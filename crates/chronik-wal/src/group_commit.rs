//! Group commit implementation for WAL durability
//!
//! This module implements PostgreSQL-style group commit to achieve both high throughput
//! and zero data loss. Multiple concurrent writes are batched together and committed
//! with a single fsync, amortizing the cost of disk synchronization.
//!
//! Key features:
//! - Zero data loss for acks=1 and acks=-1
//! - Optional fire-and-forget for acks=0
//! - Bounded memory with backpressure
//! - Adaptive batching based on throughput
//! - Per-partition commit queues

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::RwLock;  // v2.2.10: For interior mutability of commit_callback
use std::time::{Duration, Instant};
use bytes::Bytes;
use dashmap::DashMap;
use tokio::fs::{File, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex, oneshot, Notify};
use tracing::{debug, info, warn, error, trace, instrument};

use crate::error::{Result, WalError};
use crate::record::WalRecord;
use crate::truncate::{SegmentVerdict, TruncateOutcome};
use chronik_monitoring::MetricsRecorder;

#[cfg(all(target_os = "linux", feature = "async-io"))]
use crate::io_uring_thread::IoUringThreadHandle;
#[cfg(all(target_os = "linux", feature = "async-io"))]
use std::sync::Arc as StdArc;

/// v2.2.10: Callback invoked after successful batch commit
/// Enables async response delivery without blocking on WAL fsync
/// Args: (topic, partition, min_offset, max_offset)
pub type CommitCallback = Arc<dyn Fn(&str, i32, i64, i64) + Send + Sync>;

/// Unified WAL writer abstraction (either io_uring thread or tokio::fs::File)
enum WalWriter {
    #[cfg(all(target_os = "linux", feature = "async-io"))]
    IoUring {
        handle: StdArc<IoUringThreadHandle>,
        partition_key: String,
    },
    Standard(File),
}

impl WalWriter {
    /// Create a new WAL writer at the given path
    async fn create(
        path: impl AsRef<Path>,
        partition_key: String,
        #[cfg(all(target_os = "linux", feature = "async-io"))]
        io_uring_handle: Option<StdArc<IoUringThreadHandle>>,
    ) -> Result<Self> {
        #[cfg(all(target_os = "linux", feature = "async-io"))]
        {
            if let Some(handle) = io_uring_handle {
                // Create file via io_uring thread
                match handle.create_file(partition_key.clone(), path.as_ref().to_path_buf()).await {
                    Ok(_) => {
                        info!("✨ io_uring WAL writer created (10x faster I/O): {:?}", path.as_ref());
                        return Ok(WalWriter::IoUring {
                            handle,
                            partition_key,
                        });
                    }
                    Err(e) => {
                        warn!("io_uring unavailable, falling back to standard I/O: {}", e);
                    }
                }
            }
        }

        // Fallback to standard I/O
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        Ok(WalWriter::Standard(file))
    }

    /// Write data to WAL
    async fn write_all(&mut self, data: &[u8]) -> Result<()> {
        match self {
            #[cfg(all(target_os = "linux", feature = "async-io"))]
            WalWriter::IoUring { handle, partition_key } => {
                handle.write(partition_key.clone(), Bytes::copy_from_slice(data)).await
            }
            WalWriter::Standard(file) => {
                file.write_all(data).await?;
                Ok(())
            }
        }
    }

    /// Fsync WAL
    async fn sync_all(&self) -> Result<()> {
        match self {
            #[cfg(all(target_os = "linux", feature = "async-io"))]
            WalWriter::IoUring { handle, partition_key } => {
                handle.sync(partition_key.clone()).await
            }
            WalWriter::Standard(file) => {
                file.sync_all().await?;
                Ok(())
            }
        }
    }
}

/// Configuration for group commit behavior
#[derive(Debug, Clone)]
pub struct GroupCommitConfig {
    /// Maximum number of writes per batch (prevents unbounded queue growth)
    pub max_batch_size: usize,

    /// Maximum bytes per batch (memory limit)
    pub max_batch_bytes: usize,

    /// Maximum time to wait before forcing a commit (latency bound)
    pub max_wait_time_ms: u64,

    /// Maximum queue depth per partition (backpressure threshold)
    pub max_queue_depth: usize,

    /// Enable metrics collection
    pub enable_metrics: bool,

    /// Enable segment rotation (for S3 archival)
    pub enable_rotation: bool,

    /// Maximum segment size before rotation (bytes)
    pub rotation_size_bytes: u64,

    /// Maximum segment age before rotation (seconds)
    pub rotation_age_secs: u64,
}

/// State of a WAL segment in its lifecycle
#[derive(Debug, Clone, PartialEq)]
pub enum SegmentState {
    /// Currently accepting writes
    Active,
    /// Immutable, ready for indexing/archival (Quickwit's "Staged")
    Sealed,
    /// Uploaded to S3, local copy can be deleted (Quickwit's "Published")
    Archived,
}

/// Metadata about a WAL segment
#[derive(Debug, Clone)]
pub struct SegmentMetadata {
    pub segment_id: u64,
    pub created_at: Instant,
    pub size_bytes: u64,
    pub file_path: PathBuf,
    pub state: SegmentState,
}

/// Sealed segment info for archival (public API)
#[derive(Debug, Clone)]
pub struct SealedSegmentInfo {
    pub topic: String,
    pub partition: i32,
    pub segment_id: u64,
    pub file_path: PathBuf,
    pub size_bytes: u64,
    pub state: SegmentState,
    pub sealed_at: Instant,
}

impl Default for GroupCommitConfig {
    fn default() -> Self {
        // Check for profile override via environment
        if let Ok(profile) = std::env::var("CHRONIK_WAL_PROFILE") {
            return match profile.to_lowercase().as_str() {
                "low" | "small" | "container" => Self::low_resource(),
                "medium" | "balanced" => Self::medium_resource(),
                "high" | "aggressive" | "dedicated" => Self::high_resource(),
                "ultra" | "maximum" | "throughput" => Self::ultra_resource(),
                _ => {
                    warn!("Unknown CHRONIK_WAL_PROFILE '{}', using low profile (default)", profile);
                    Self::low_resource()
                }
            };
        }

        // Default to low profile: optimized for low-latency workloads
        // Note: Metadata WAL always uses HIGH via CHRONIK_METADATA_WAL_PROFILE
        Self::low_resource()
    }
}

impl GroupCommitConfig {
    /// Parse rotation size from environment variable or use default
    /// Supports: "100KB", "250MB", "1GB", or raw bytes "268435456"
    ///
    /// Example: CHRONIK_WAL_ROTATION_SIZE=100MB
    fn parse_rotation_size() -> u64 {
        match std::env::var("CHRONIK_WAL_ROTATION_SIZE") {
            Ok(val) => {
                let val = val.trim().to_uppercase();

                // Try to parse with unit suffix
                if val.ends_with("KB") {
                    val.trim_end_matches("KB").trim().parse::<u64>().unwrap_or(250) * 1024
                } else if val.ends_with("MB") {
                    val.trim_end_matches("MB").trim().parse::<u64>().unwrap_or(250) * 1024 * 1024
                } else if val.ends_with("GB") {
                    val.trim_end_matches("GB").trim().parse::<u64>().unwrap_or(1) * 1024 * 1024 * 1024
                } else {
                    // Try parsing as raw bytes
                    val.parse::<u64>().unwrap_or(250 * 1024 * 1024)
                }
            }
            Err(_) => 250 * 1024 * 1024,  // Default: 250MB
        }
    }

    /// Low resource profile (containers, small VMs: <= 1 CPU, < 512MB RAM)
    pub fn low_resource() -> Self {
        Self {
            max_batch_size: 500,            // 500 writes per batch
            max_batch_bytes: 5_000_000,     // 5MB per batch
            max_wait_time_ms: 2,            // 2ms latency
            max_queue_depth: 2_500,         // 2.5K queue depth
            enable_metrics: true,
            enable_rotation: true,          // Enable S3 archival
            rotation_size_bytes: Self::parse_rotation_size(),
            rotation_age_secs: 30 * 60,     // 30 minutes
        }
    }

    /// Medium resource profile (typical servers: 2-4 CPUs, 512MB-4GB RAM)
    pub fn medium_resource() -> Self {
        Self {
            max_batch_size: 2_000,          // 2K writes per batch
            max_batch_bytes: 15_000_000,    // 15MB per batch
            max_wait_time_ms: 10,           // 10ms latency
            max_queue_depth: 10_000,        // 10K queue depth
            enable_metrics: true,
            enable_rotation: true,          // Enable S3 archival
            rotation_size_bytes: Self::parse_rotation_size(),
            rotation_age_secs: 30 * 60,     // 30 minutes
        }
    }

    /// High resource profile (dedicated servers: 4+ CPUs, 4GB+ RAM)
    /// Default profile: balanced throughput and reduced disk I/O with 100ms batching
    pub fn high_resource() -> Self {
        Self {
            max_batch_size: 10_000,         // 10K writes per batch
            max_batch_bytes: 50_000_000,    // 50MB per batch
            max_wait_time_ms: 50,           // 50ms latency (reduce disk I/O)
            max_queue_depth: 50_000,        // 50K queue depth
            enable_metrics: true,
            enable_rotation: true,          // Enable S3 archival
            rotation_size_bytes: Self::parse_rotation_size(),
            rotation_age_secs: 30 * 60,     // 30 minutes
        }
    }

    /// Ultra resource profile (maximum throughput: 16+ CPUs, 16GB+ RAM)
    /// Uses 100ms batching window for massive throughput at cost of higher latency
    /// Trade-off: ~100ms p99 latency for potentially 2-3x higher throughput
    pub fn ultra_resource() -> Self {
        Self {
            max_batch_size: 20_000,         // 20K writes per batch (4x high profile)
            max_batch_bytes: 100_000_000,   // 100MB per batch (4x high profile)
            max_wait_time_ms: 100,          // 100ms latency (20x high profile - maximum batching)
            max_queue_depth: 100_000,       // 100K queue depth (4x high profile)
            enable_metrics: true,
            enable_rotation: true,          // Enable S3 archival
            rotation_size_bytes: 512 * 1024 * 1024,  // 512MB (larger for ultra)
            rotation_age_secs: 30 * 60,     // 30 minutes
        }
    }

    /// Create custom configuration
    pub fn custom(
        max_batch_size: usize,
        max_batch_bytes: usize,
        max_wait_time_ms: u64,
        max_queue_depth: usize,
    ) -> Self {
        Self {
            max_batch_size,
            max_batch_bytes,
            max_wait_time_ms,
            max_queue_depth,
            enable_metrics: true,
            enable_rotation: true,
            rotation_size_bytes: 256 * 1024 * 1024,
            rotation_age_secs: 30 * 60,
        }
    }
}

/// A pending write waiting for group commit
struct PendingWrite {
    /// Pre-serialized WAL record data
    data: Bytes,

    /// Channel to notify caller when commit completes
    response_tx: Option<oneshot::Sender<Result<()>>>,

    /// v2.2.10: Metadata for async response callbacks
    /// Base offset of this batch (for ResponsePipeline notification)
    base_offset: i64,

    /// Last offset of this batch (for ResponsePipeline notification)
    last_offset: i64,
}

/// Per-partition commit queue
struct PartitionCommitQueue {
    /// Pending writes waiting for commit
    pending: Mutex<VecDeque<PendingWrite>>,

    /// Total bytes in queue (for memory tracking) - CHANGED to AtomicU64 for lock-free updates
    total_queued_bytes: AtomicU64,

    /// File handle for this partition's WAL
    file: Arc<Mutex<WalWriter>>,

    /// Last fsync timestamp
    last_fsync: Mutex<Instant>,

    /// Notification for new writes
    write_notify: Arc<Notify>,

    /// Metrics
    metrics: Arc<CommitMetrics>,

    /// Current segment ID
    segment_id: Arc<AtomicU64>,

    /// Current segment creation time
    segment_created_at: Arc<Mutex<Instant>>,

    /// Current segment size in bytes
    segment_size_bytes: Arc<AtomicU64>,

    /// Topic name (for rotation)
    topic: String,

    /// Partition number (for rotation)
    partition: i32,

    /// Bumped by every suffix truncation (RP-3.3).
    ///
    /// `commit_batch` drains the pending queue and *then* takes the writer
    /// lock, so a batch can be in flight — off the queue, not yet on disk —
    /// when a truncation acquires both locks and cuts the log. Writing that
    /// batch afterwards would re-append records the truncation just decided
    /// were divergent. The worker reads this counter when it drains and again
    /// once it holds the writer; a change between the two means the batch
    /// straddles a truncation and must be dropped rather than written.
    truncation_epoch: Arc<AtomicU64>,

    /// The most recently appended records, so reading the tail of the log costs
    /// no I/O. See `TailCache`.
    tail: Mutex<TailCache>,

    /// Highest offset the commit worker has written and fsynced.
    ///
    /// The cache is filled at *append* time, which is the only point where
    /// arrival order is still known — but a queued write is not yet on disk,
    /// and some never get there: `commit_batch` discards a batch that straddles
    /// a truncation. Serving those from memory would let a replica read records
    /// no file will ever hold. So the cache may only answer at or below this,
    /// which makes it strictly an accelerator over durable state.
    committed_through: Arc<AtomicI64>,
}

/// A bounded, in-memory suffix of a partition's log.
///
/// Every fetch — a follower replicating, a consumer keeping up — asks for
/// records near the log end, and `read_from` answered by reading the *entire*
/// active segment file off disk and parsing it from byte zero, materialising
/// every record's payload before discarding the ones below the requested
/// offset. Segments rotate at 250MB, so that cost grew with the segment: on a
/// three-node cluster it measured 2.5–4.5ms to return 2 records, it was the
/// whole of `acks=all`'s fetch latency, and it is why replicated throughput
/// visibly degraded *within* a ten-second run (RP-9).
///
/// Records are pushed in append order and evicted from the front, so the cache
/// is always a contiguous suffix of the log. That is what makes it safe to
/// serve from: either it covers the requested offset, or the read falls back to
/// the file. A truncation clears it (RP-3.3) — a cut log must not be answered
/// from records the cut removed.
///
/// The file remains the source of truth. This never acknowledges a write, and
/// nothing here changes when a record is considered durable.
struct TailCache {
    /// Held in offset order, which is not always arrival order.
    records: VecDeque<WalRecord>,
    bytes: usize,
    max_bytes: usize,
    /// Lowest offset from which the cache runs gap-free to the end. A read
    /// below this cannot be served, because the answer would skip a hole.
    contiguous_from: Option<i64>,
}

/// Per-partition budget for `TailCache`, from `CHRONIK_WAL_TAIL_CACHE_BYTES`.
///
/// The total is this times the number of partitions this node writes, so a node
/// carrying thousands of partitions should lower it. `0` disables the cache and
/// sends every read back to the file scan.
fn tail_cache_bytes() -> usize {
    const DEFAULT: usize = 2 * 1024 * 1024;
    match std::env::var("CHRONIK_WAL_TAIL_CACHE_BYTES") {
        Ok(v) => v.trim().parse().unwrap_or(DEFAULT),
        Err(_) => DEFAULT,
    }
}

impl TailCache {
    fn new(max_bytes: usize) -> Self {
        Self {
            records: VecDeque::new(),
            bytes: 0,
            max_bytes,
            contiguous_from: None,
        }
    }

    /// Walk back from the log end while each record continues the next, and
    /// return where that run starts. Only needed after an out-of-order arrival.
    fn recompute_contiguous_from(&mut self) {
        let mut start = None;
        let mut expected: Option<i64> = None;
        for record in self.records.iter().rev() {
            match expected {
                Some(e) if record.get_last_offset() + 1 != e => break,
                _ => {}
            }
            expected = Some(record.get_base_offset());
            start = expected;
        }
        self.contiguous_from = start;
    }

    /// Record just appended to the log, with the serialised size already known.
    ///
    /// Contiguity is *enforced*, not assumed. Offsets are assigned before the
    /// WAL append, and two produces to the same partition can therefore reach
    /// this point in an order that does not match their offsets. A cache that
    /// merely assumed order would then answer "I cover offset N" out of records
    /// that skip past it, and a caller asking for the whole log would silently
    /// receive part of it — which is exactly how this cache first went wrong,
    /// as an intermittent failure of the RP-3.3 divergence test.
    ///
    /// So anything that is not the next offset drops the cache. The file is
    /// still authoritative; the only cost of being wrong here is a miss.
    fn push(&mut self, record: WalRecord, size: usize) {
        if self.max_bytes == 0 {
            return;
        }

        let base = record.get_base_offset();

        // The overwhelming majority: this append continues the log end.
        if self
            .records
            .back()
            .map_or(true, |b| b.get_last_offset() + 1 == base)
        {
            if self.records.is_empty() {
                self.contiguous_from = Some(base);
            }
            self.records.push_back(record);
            self.bytes += size;
        } else {
            // Out of order, and legitimately so: offsets are assigned before
            // the WAL append, so two produces to one partition can reach this
            // point in an order that does not match their offsets. Under 64
            // concurrent producers that is not an edge case, it is most of
            // them.
            //
            // An earlier version refused these and held the cache empty, which
            // is safe but useless — the cache never served a single read and
            // the fetch path fell back to the full-segment scan it exists to
            // avoid. Ordering the record into place keeps the cache a true
            // mirror of the log without giving up on it.
            let pos = self.records.partition_point(|r| r.get_base_offset() < base);
            if self
                .records
                .get(pos)
                .is_some_and(|r| r.get_base_offset() == base)
            {
                return; // already held; a duplicate must not double-count bytes
            }
            self.records.insert(pos, record);
            self.bytes += size;
            self.recompute_contiguous_from();
        }

        while self.bytes > self.max_bytes && self.records.len() > 1 {
            if let Some(evicted) = self.records.pop_front() {
                self.bytes = self.bytes.saturating_sub(evicted.heap_size());
            }
            // Dropping the front can only raise where the gap-free run starts.
            if let Some(front) = self.records.front() {
                let front_base = front.get_base_offset();
                self.contiguous_from = Some(match self.contiguous_from {
                    Some(c) => c.max(front_base),
                    None => front_base,
                });
            }
        }
    }

    /// Forget everything, including where the log was. Used when the log itself
    /// moved under us — a truncation, or an append that never landed — so the
    /// next append re-anchors the cache wherever the log now ends.
    fn clear(&mut self) {
        self.records.clear();
        self.bytes = 0;
        self.contiguous_from = None;
    }

    /// Records from `offset` onward, or `None` if the cache does not reach back
    /// that far — in which case the caller must read the file.
    ///
    /// Answering a read the cache only partly covers would silently drop the
    /// records below its start, so "partly" has to count as a miss.
    /// `durable_through` is the highest offset the commit worker has fsynced;
    /// records above it are queued, not written, and must not be served.
    fn read_from(
        &self,
        offset: i64,
        max_records: usize,
        durable_through: i64,
    ) -> Option<Vec<WalRecord>> {
        // Serve only from within the gap-free run that reaches the log end.
        // Below it the cache has a hole, and a short answer there would be read
        // as the whole of the log.
        if offset < self.contiguous_from? {
            return None;
        }

        let mut out = Vec::new();
        let mut messages = 0usize;
        for record in &self.records {
            if record.get_last_offset() < offset {
                continue;
            }
            // Stop at the durable end rather than skipping past it: the records
            // beyond are a contiguous run too, and serving the ones after a
            // not-yet-committed batch would leave a hole in the answer.
            if record.get_last_offset() > durable_through {
                break;
            }
            if messages >= max_records {
                break;
            }
            messages += record.get_record_count().max(1) as usize;
            out.push(record.clone());
        }

        // Nothing durable to give. Say "miss" rather than "empty": the file may
        // well have records this cache is not yet allowed to serve, and an
        // empty answer would be taken for the end of the log.
        if out.is_empty() {
            return None;
        }
        Some(out)
    }
}

/// Commit metrics for observability
#[derive(Debug, Default)]
struct CommitMetrics {
    total_commits: std::sync::atomic::AtomicU64,
    total_writes: std::sync::atomic::AtomicU64,
    total_bytes: std::sync::atomic::AtomicU64,
    total_fsync_time_us: std::sync::atomic::AtomicU64,
    backpressure_events: std::sync::atomic::AtomicU64,
}

/// Group commit WAL manager
pub struct GroupCommitWal {
    /// Per-partition commit queues
    partition_queues: Arc<DashMap<(String, i32), Arc<PartitionCommitQueue>>>,

    /// Sealed segments ready for archival (topic:partition:segment_id → info)
    sealed_segments: Arc<DashMap<String, SealedSegmentInfo>>,

    /// Configuration
    config: GroupCommitConfig,

    /// Base directory for WAL files
    base_dir: PathBuf,

    /// Shutdown signal
    shutdown: Arc<Notify>,

    /// io_uring thread handle (Linux + async-io feature only)
    #[cfg(all(target_os = "linux", feature = "async-io"))]
    io_uring_handle: Option<StdArc<IoUringThreadHandle>>,

    /// v2.2.10: Optional callback invoked after successful batch commit
    /// Enables async response delivery for acks=1 without blocking on fsync
    /// Wrapped in Arc<RwLock<>> for interior mutability (can be set after construction)
    commit_callback: Arc<RwLock<Option<CommitCallback>>>,

    /// v2.4.0: Runtime handle captured at construction time.
    /// Commit workers are ALWAYS spawned on this handle's runtime, regardless
    /// of which runtime initiates the first write to a partition.
    /// This prevents deadlock when WalIndexer (on its dedicated 2-thread runtime)
    /// writes to __chronik_metadata — without this, the commit worker would be
    /// spawned on the WalIndexer's runtime and starve when those 2 threads are
    /// saturated with Tantivy/Parquet/embedding work.
    runtime_handle: tokio::runtime::Handle,
}

impl GroupCommitWal {
    /// Create a new group commit WAL manager
    pub fn new(base_dir: PathBuf, config: GroupCommitConfig) -> Self {
        // Spawn io_uring thread if available
        #[cfg(all(target_os = "linux", feature = "async-io"))]
        let io_uring_handle = match IoUringThreadHandle::spawn() {
            Ok(handle) => {
                info!("✨ io_uring thread spawned for 10x faster WAL writes");
                Some(StdArc::new(handle))
            }
            Err(e) => {
                warn!("Failed to spawn io_uring thread, falling back to standard I/O: {}", e);
                None
            }
        };

        let wal = Self {
            partition_queues: Arc::new(DashMap::new()),
            sealed_segments: Arc::new(DashMap::new()),
            config,
            base_dir: base_dir.clone(),
            shutdown: Arc::new(Notify::new()),
            #[cfg(all(target_os = "linux", feature = "async-io"))]
            io_uring_handle,
            commit_callback: Arc::new(RwLock::new(None)),  // v2.2.10: Interior mutability for async responses
            runtime_handle: tokio::runtime::Handle::current(),
        };

        // Discover existing sealed segments from filesystem
        wal.discover_sealed_segments();

        info!("Group commit WAL initialized: max_batch={}, max_wait={}ms, max_queue={}, rotation={}",
              wal.config.max_batch_size, wal.config.max_wait_time_ms, wal.config.max_queue_depth,
              if wal.config.enable_rotation { "enabled" } else { "disabled" });

        wal
    }

    /// Create a new group commit WAL manager with a callback for async response delivery (v2.2.10)
    pub fn with_callback(base_dir: PathBuf, config: GroupCommitConfig, callback: CommitCallback) -> Self {
        // Spawn io_uring thread if available
        #[cfg(all(target_os = "linux", feature = "async-io"))]
        let io_uring_handle = match IoUringThreadHandle::spawn() {
            Ok(handle) => {
                info!("✨ io_uring thread spawned for 10x faster WAL writes");
                Some(StdArc::new(handle))
            }
            Err(e) => {
                warn!("Failed to spawn io_uring thread, falling back to standard I/O: {}", e);
                None
            }
        };

        let wal = Self {
            partition_queues: Arc::new(DashMap::new()),
            sealed_segments: Arc::new(DashMap::new()),
            config,
            base_dir: base_dir.clone(),
            shutdown: Arc::new(Notify::new()),
            #[cfg(all(target_os = "linux", feature = "async-io"))]
            io_uring_handle,
            commit_callback: Arc::new(RwLock::new(Some(callback))),  // v2.2.10: Interior mutability for async responses
            runtime_handle: tokio::runtime::Handle::current(),
        };

        // Discover existing sealed segments from filesystem
        wal.discover_sealed_segments();

        info!("Group commit WAL initialized with async callback: max_batch={}, max_wait={}ms, max_queue={}, rotation={}",
              wal.config.max_batch_size, wal.config.max_wait_time_ms, wal.config.max_queue_depth,
              if wal.config.enable_rotation { "enabled" } else { "disabled" });

        wal
    }

    /// Set the commit callback for async response delivery (v2.2.10)
    /// Allows setting callback after construction for flexible initialization ordering
    /// This enables WalManager to be created first, then ResponsePipeline wired later
    pub fn set_commit_callback(&self, callback: CommitCallback) {
        if let Ok(mut guard) = self.commit_callback.write() {
            *guard = Some(callback);
            info!("✅ Commit callback registered for async response delivery (v2.2.10 - CRITICAL FIX #7)");
        } else {
            warn!("Failed to acquire write lock for commit_callback - callback NOT set");
        }
    }

    /// Discover sealed segments from filesystem (called on startup)
    fn discover_sealed_segments(&self) {
        use std::fs;

        if !self.config.enable_rotation {
            return;
        }

        let mut discovered = 0;

        // Scan data/wal/topic/partition/ directories
        if let Ok(topic_dirs) = fs::read_dir(&self.base_dir) {
            for topic_entry in topic_dirs.flatten() {
                if let Ok(topic_name) = topic_entry.file_name().into_string() {
                    // Skip non-directories or __meta
                    if topic_name.starts_with("__") || !topic_entry.path().is_dir() {
                        continue;
                    }

                    // Scan partition directories
                    if let Ok(partition_dirs) = fs::read_dir(topic_entry.path()) {
                        for partition_entry in partition_dirs.flatten() {
                            if let Ok(partition_str) = partition_entry.file_name().into_string() {
                                if let Ok(partition) = partition_str.parse::<i32>() {
                                    // Scan WAL files in this partition
                                    if let Ok(files) = fs::read_dir(partition_entry.path()) {
                                        for file_entry in files.flatten() {
                                            let file_name = file_entry.file_name();
                                            let file_name_str = file_name.to_string_lossy();

                                            // Match pattern: wal_PARTITION_SEGMENT.log
                                            if file_name_str.starts_with("wal_") && file_name_str.ends_with(".log") {
                                                // Extract segment_id from filename
                                                if let Some(segment_str) = file_name_str.strip_prefix("wal_").and_then(|s| s.strip_suffix(".log")) {
                                                    let parts: Vec<&str> = segment_str.split('_').collect();
                                                    if parts.len() == 2 {
                                                        if let Ok(segment_id) = parts[1].parse::<u64>() {
                                                            // Get file metadata
                                                            if let Ok(metadata) = file_entry.metadata() {
                                                                let size_bytes = metadata.len();

                                                                // Consider a segment sealed if:
                                                                // - segment_id > 0 (not the first segment), OR
                                                                // - size >= rotation threshold, OR
                                                                // - segment has data (size > 0) — covers recovered
                                                                //   segments from topics with a single segment
                                                                if segment_id > 0 || size_bytes >= self.config.rotation_size_bytes || size_bytes > 0 {
                                                                    let sealed_key = format!("{}:{}:{}", topic_name, partition, segment_id);
                                                                    self.sealed_segments.insert(
                                                                        sealed_key,
                                                                        SealedSegmentInfo {
                                                                            topic: topic_name.clone(),
                                                                            partition,
                                                                            segment_id,
                                                                            file_path: file_entry.path(),
                                                                            size_bytes,
                                                                            state: SegmentState::Sealed,
                                                                            sealed_at: Instant::now(),
                                                                        },
                                                                    );
                                                                    discovered += 1;
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        if discovered > 0 {
            info!("Discovered {} sealed segments from filesystem", discovered);
        }
    }

    /// Append a record with specified acknowledgment mode
    #[instrument(skip(self, record), fields(topic = %topic, partition = partition, acks = acks))]
    pub async fn append(
        &self,
        topic: String,
        partition: i32,
        record: WalRecord,
        acks: i16,
    ) -> Result<()> {
        // v2.2.10: Extract offset metadata from WalRecord before serialization (for async response callbacks)
        let (base_offset, last_offset) = match &record {
            WalRecord::V2 { base_offset, last_offset, .. } => (*base_offset, *last_offset),
            WalRecord::V1 { offset, .. } => (*offset, *offset),  // V1 has single offset
        };

        // Serialize record
        let data = record.to_bytes()?;
        let data_len = data.len();

        // Get or create partition queue
        let queue = self.get_or_create_queue(&topic, partition).await?;

        // Cache the tail before enqueuing, not after: `enqueue_and_wait` does
        // not return until the commit worker has fsynced, and a follower's fetch
        // for this offset can arrive in that window. Ordering is still the
        // append order — this is the same task that is about to enqueue — and a
        // record that fails to enqueue is removed again below.
        let heap = record.heap_size();
        {
            let mut tail = queue.tail.lock().await;
            tail.push(record, heap);
        }

        // Handle acks=0 (fire-and-forget)
        let result = if acks == 0 {
            self.enqueue_nowait(Arc::clone(&queue), data.into(), base_offset, last_offset).await
        } else {
            self.enqueue_and_wait(Arc::clone(&queue), data.into(), data_len, base_offset, last_offset).await
        };

        if result.is_err() {
            // The log does not have this record, so neither may the cache — it
            // is only ever allowed to be a suffix of what was written.
            queue.tail.lock().await.clear();
        }

        result
    }

    /// Enqueue without waiting (acks=0 mode)
    ///
    /// Elastic buffer for acks=0 with high threshold
    /// - Monitor queue size but DON'T block/error for acks=0 (fire-and-forget semantics)
    /// - Queue grows elastically during bursts, background committer drains at its own pace
    /// - Warn only if queue exceeds 10x batch size (very large accumulation)
    /// - Ultra profile: 10MB batch size × 10 = 100MB warning threshold
    /// - This prevents false "message loss" from backpressure errors
    async fn enqueue_nowait(
        &self,
        queue: Arc<PartitionCommitQueue>,
        data: Bytes,
        base_offset: i64,  // v2.2.10: For async response callbacks
        last_offset: i64,  // v2.2.10: For async response callbacks
    ) -> Result<()> {
        let data_len = data.len();

        // Monitor backpressure but don't block - elastic buffer with high threshold
        let queued_bytes = queue.total_queued_bytes.load(Ordering::Relaxed);
        if queued_bytes >= (self.config.max_batch_bytes * 10) as u64 {
            queue.metrics.backpressure_events.fetch_add(1, Ordering::Relaxed);
            warn!(
                "acks=0 queue growing large: {} MB (elastic threshold {} MB), continuing",
                queued_bytes / 1_000_000,
                (self.config.max_batch_bytes * 10) / 1_000_000
            );
        }

        // Enqueue without response channel
        let mut pending = queue.pending.lock().await;
        pending.push_back(PendingWrite {
            data,
            response_tx: None,
            base_offset,  // v2.2.10: Offset metadata
            last_offset,  // v2.2.10: Offset metadata
        });
        drop(pending);

        // Update queue size atomically (lock-free)
        queue.total_queued_bytes.fetch_add(data_len as u64, Ordering::Relaxed);

        // Notify committer
        queue.write_notify.notify_one();

        Ok(())
    }

    /// Enqueue and wait for commit (acks=1 or acks=-1 mode)
    ///
    /// PERFORMANCE OPTIMIZATION (P1): Reduced lock acquisitions from 4 to 1
    /// - Before: 4 separate lock acquisitions (backpressure check, enqueue, bytes update, commit check)
    /// - After: 1 lock acquisition combining all operations
    /// - Expected gain: 2-4x throughput at high concurrency
    async fn enqueue_and_wait(
        &self,
        queue: Arc<PartitionCommitQueue>,
        data: Bytes,
        data_len: usize,
        base_offset: i64,  // v2.2.10: For async response callbacks
        last_offset: i64,  // v2.2.10: For async response callbacks
    ) -> Result<()> {
        debug!("🟢 ENQUEUE_START: Enqueuing write with {} bytes", data_len);

        // Skip backpressure check for critical Raft topics
        // Raft HardState/ConfState writes MUST succeed for cluster coordination
        let is_raft_topic = queue.topic.starts_with("__raft") || queue.topic.starts_with("__chronik_metadata");

        // Create response channel before locking
        let (tx, rx) = oneshot::channel();
        debug!("📫 ENQUEUE_CHANNEL: Created oneshot channel for fsync confirmation");

        // OPTIMIZATION: Single lock acquisition combines 4 operations
        // 1. Backpressure check
        // 2. Enqueue write
        // 3. Commit decision (was in should_commit_now)
        // 4. Return queue depth
        let should_commit = {
            let mut pending = queue.pending.lock().await;

            // Check backpressure (inside lock to avoid TOCTOU race)
            if !is_raft_topic && pending.len() >= self.config.max_queue_depth {
                queue.metrics.backpressure_events.fetch_add(1, Ordering::Relaxed);
                warn!("🔴 BACKPRESSURE: Queue depth {} exceeded max {}", pending.len(), self.config.max_queue_depth);
                return Err(WalError::Backpressure(format!("Queue depth {} exceeds limit {}", pending.len(), self.config.max_queue_depth)));
            }

            // Enqueue write
            pending.push_back(PendingWrite {
                data,
                response_tx: Some(tx),
                base_offset,  // v2.2.10: Offset metadata
                last_offset,  // v2.2.10: Offset metadata
            });
            let queue_depth = pending.len();
            info!("📥 ENQUEUE_ADDED: Enqueued write (with wait), queue depth now: {}", queue_depth);

            // Update bytes atomically (lock-free after this point)
            let total_bytes = queue.total_queued_bytes.fetch_add(data_len as u64, Ordering::Relaxed) + data_len as u64;
            debug!("📊 ENQUEUE_BYTES: Total queued bytes now: {}", total_bytes);

            // Check if we should commit immediately (inline instead of separate function)
            queue_depth >= self.config.max_batch_size || total_bytes >= self.config.max_batch_bytes as u64
        }; // Lock released here

        // Notify committer
        queue.write_notify.notify_one();
        info!("🔔 ENQUEUE_NOTIFY: Notified commit worker, should_commit={}", should_commit);

        // If batch is full, trigger commit immediately (don't wait for timer)
        if should_commit {
            info!("⚡ ENQUEUE_IMMEDIATE: Triggering immediate commit due to batch size");
        }

        // Wait for commit confirmation
        info!("⏳ ENQUEUE_WAIT: Waiting for fsync confirmation on oneshot channel...");
        let result = rx.await
            .map_err(|_| WalError::CommitFailed("Response channel closed".into()))?;
        info!("✅ ENQUEUE_DONE: Fsync confirmed! Returning success to caller");
        result
    }

    /// Get or create a partition queue
    async fn get_or_create_queue(
        &self,
        topic: &str,
        partition: i32,
    ) -> Result<Arc<PartitionCommitQueue>> {
        let key = (topic.to_string(), partition);

        if let Some(queue) = self.partition_queues.get(&key) {
            return Ok(queue.clone());
        }

        // Create new queue
        let partition_dir = self.base_dir.join(topic).join(partition.to_string());
        tokio::fs::create_dir_all(&partition_dir).await?;

        // Scan existing WAL files to find the next available segment_id.
        // After recovery, discover_sealed_segments() marks existing segments as sealed,
        // so we must start a NEW segment to avoid overwriting sealed data.
        let mut next_segment_id: u64 = 0;
        let prefix = format!("wal_{}_", partition);
        if let Ok(mut entries) = tokio::fs::read_dir(&partition_dir).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                if let Some(name) = entry.file_name().to_str() {
                    if name.starts_with(&prefix) && name.ends_with(".log") {
                        if let Some(seg_str) = name.strip_prefix(&prefix).and_then(|s| s.strip_suffix(".log")) {
                            if let Ok(seg_id) = seg_str.parse::<u64>() {
                                next_segment_id = next_segment_id.max(seg_id + 1);
                            }
                        }
                    }
                }
            }
        }

        // Format: wal_{partition}_{segment_id}.log
        let wal_path = partition_dir.join(format!("wal_{}_{}.log", partition, next_segment_id));

        let partition_key = format!("{}:{}", topic, partition);
        let writer = WalWriter::create(
            &wal_path,
            partition_key,
            #[cfg(all(target_os = "linux", feature = "async-io"))]
            self.io_uring_handle.clone(),
        ).await?;

        let queue = Arc::new(PartitionCommitQueue {
            pending: Mutex::new(VecDeque::new()),
            total_queued_bytes: AtomicU64::new(0),  // Now atomic instead of Mutex
            file: Arc::new(Mutex::new(writer)),
            last_fsync: Mutex::new(Instant::now()),
            write_notify: Arc::new(Notify::new()),
            metrics: Arc::new(CommitMetrics::default()),
            segment_id: Arc::new(AtomicU64::new(next_segment_id)),
            segment_created_at: Arc::new(Mutex::new(Instant::now())),
            segment_size_bytes: Arc::new(AtomicU64::new(0)),
            topic: topic.to_string(),
            partition,
            truncation_epoch: Arc::new(AtomicU64::new(0)),
            tail: Mutex::new(TailCache::new(tail_cache_bytes())),
            committed_through: Arc::new(AtomicI64::new(i64::MIN)),
        });

        // Start per-partition commit worker
        self.start_partition_committer(queue.clone());

        self.partition_queues.insert(key, queue.clone());

        if next_segment_id > 0 {
            info!("Created commit queue for {}-{} (segment_id={}, skipping {} existing segments)",
                topic, partition, next_segment_id, next_segment_id);
        } else {
            info!("Created commit queue for {}-{}", topic, partition);
        }

        Ok(queue)
    }

    /// Start per-partition commit worker
    fn start_partition_committer(&self, queue: Arc<PartitionCommitQueue>) {
        let config = self.config.clone();
        let shutdown = self.shutdown.clone();
        let sealed_segments = self.sealed_segments.clone();
        let base_dir = self.base_dir.clone();
        let partition_queues = self.partition_queues.clone();
        #[cfg(all(target_os = "linux", feature = "async-io"))]
        let io_uring_handle = self.io_uring_handle.clone();
        let commit_callback = self.commit_callback.clone();  // v2.2.10: For async response delivery

        info!("🚀 WORKER_SPAWN: Starting partition committer on construction-time runtime (prevents WalIndexer starvation)");

        // v2.4.0: CRITICAL — spawn on the runtime captured at construction time,
        // NOT tokio::spawn() which uses the *current* runtime.
        // Without this, if a WalIndexer thread (on its dedicated 2-thread runtime)
        // triggers the first write to __chronik_metadata, the commit worker would
        // be spawned on the WalIndexer's runtime. When those 2 threads are saturated
        // with Tantivy/Parquet/S3 work, the commit worker starves → deadlock.
        self.runtime_handle.spawn(async move {
            info!("✅ WORKER_STARTED: Partition committer task is running");

            // v2.2.7: Set high I/O priority for WAL thread to prevent Tantivy from blocking it
            // NOTE: This is optional and failure is non-fatal
            if let Err(e) = crate::io_priority::set_wal_priority() {
                debug!("Could not set WAL I/O priority (requires CAP_SYS_ADMIN): {}", e);
            }
            let mut interval = tokio::time::interval(Duration::from_millis(config.max_wait_time_ms));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            let worker_key = (queue.topic.clone(), queue.partition);

            debug!("WORKER_CONFIG: max_wait_time_ms={}, max_batch_size={}, max_batch_bytes={}",
                config.max_wait_time_ms, config.max_batch_size, config.max_batch_bytes);

            loop {
                // Check if partition queue was removed (topic deleted)
                if !partition_queues.contains_key(&worker_key) {
                    info!("🛑 WORKER_EXIT: Partition queue removed (topic deleted), committer exiting: {}:{}",
                        queue.topic, queue.partition);
                    return;
                }
                // v2.2.9 fix: Changed hot-path logs from debug! to trace! to prevent log bomb
                // With 1000s of partitions, these logs fire 10x/sec per partition = 180K lines/sec
                trace!("🔄 WORKER_LOOP: Waiting for interval tick (PostgreSQL-style wait queue)");

                // HYBRID COMMIT STRATEGY (v2.4.0 — fixes deadlock under load)
                //
                // History:
                //   v2.2.10: Notify-only → batch_size=1, stuck at ~10K msg/s
                //   v2.2.11: Interval-only (PostgreSQL-style) → great batching but
                //            DEADLOCKS under high indexing load because the tokio
                //            runtime delays the interval tick when saturated with
                //            WalIndexer work (Tantivy, Parquet, S3 uploads).
                //            Callers block on oneshot rx.await forever.
                //
                // FIX (v2.4.0): Hybrid approach — use BOTH interval AND notify:
                //   - interval.tick(): Ensures regular commits at configured rate
                //   - write_notify: Ensures no write waits forever when runtime is busy
                //   - commit_batch handles empty queue gracefully (returns immediately)
                //   - High-throughput topics: interval fires first, batches stay large
                //   - Low-throughput topics (e.g. __chronik_metadata): notify ensures
                //     writes complete promptly instead of waiting for delayed tick
                tokio::select! {
                    _ = interval.tick() => {
                        trace!("⏰ WORKER_TICK: Interval tick fired");
                    }
                    _ = queue.write_notify.notified() => {
                        trace!("🔔 WORKER_NOTIFY: Write notification received");
                    }
                    _ = shutdown.notified() => {
                        info!("🛑 WORKER_SHUTDOWN: Partition committer shutting down");
                        return;
                    }
                }

                // Commit batch (triggered by interval tick OR write notification)
                trace!("📝 WORKER_COMMIT: Committing accumulated writes");
                if let Err(e) = Self::commit_batch(
                    &queue,
                    &config,
                    &sealed_segments,
                    &base_dir,
                    #[cfg(all(target_os = "linux", feature = "async-io"))]
                    &io_uring_handle,
                    &commit_callback,  // v2.2.10: Pass callback for async response delivery
                ).await {
                    error!("❌ WORKER_ERROR: Commit batch failed: {}", e);
                } else {
                    trace!("✅ WORKER_SUCCESS: commit_batch completed successfully");
                }
            }
        });

        info!("🎯 WORKER_SPAWNED: tokio::spawn returned (worker should be running in background)");
    }


    /// Commit a batch of writes with single fsync
    ///
    /// PERFORMANCE OPTIMIZATION (P3): Optimized fsync path
    /// - Before: Lock held during write loop + blocking fsync
    /// - After: Pre-combine buffer + single write + non-blocking fsync
    /// - Expected gain: 40-60% throughput improvement
    #[instrument(skip(queue, config, sealed_segments, base_dir, io_uring_handle, commit_callback), fields(batch_size, bytes, fsync_us))]
    async fn commit_batch(
        queue: &PartitionCommitQueue,
        config: &GroupCommitConfig,
        sealed_segments: &Arc<DashMap<String, SealedSegmentInfo>>,
        base_dir: &Path,
        #[cfg(all(target_os = "linux", feature = "async-io"))]
        io_uring_handle: &Option<StdArc<IoUringThreadHandle>>,
        commit_callback: &Arc<RwLock<Option<CommitCallback>>>,  // v2.2.10: Interior mutability for post-construction callback setting
    ) -> Result<()> {
        debug!("🔵 COMMIT_START: Entering commit_batch");
        let start = Instant::now();

        // OPTIMIZATION P3: Drain queue and pre-combine all writes into single buffer
        // This minimizes lock hold time and enables single write syscall
        let (batch, combined_buffer, drained_at_epoch) = {
            let mut pending = queue.pending.lock().await;

            if pending.is_empty() {
                debug!("⚪ COMMIT_EMPTY: commit_batch called but queue is empty, nothing to do");
                return Ok(());
            }

            let queue_depth = pending.len();
            let batch_size = std::cmp::min(pending.len(), config.max_batch_size);
            let mut batch = Vec::with_capacity(batch_size);

            info!("📦 COMMIT_DRAIN: Draining {} writes from queue (total depth: {}) for batch commit", batch_size, queue_depth);

            // Pre-allocate buffer for combined writes (avoids reallocations)
            let mut combined = Vec::new();

            for _ in 0..batch_size {
                if let Some(write) = pending.pop_front() {
                    combined.extend_from_slice(&write.data);
                    batch.push(write);
                }
            }

            // Read under the same lock that guards the drain, so the epoch and
            // the batch describe the same instant.
            (batch, combined, queue.truncation_epoch.load(Ordering::SeqCst))
        }; // Lock released here - much shorter critical section!

        let batch_count = batch.len();
        let total_bytes = combined_buffer.len();

        debug!("💾 COMMIT_WRITE: Writing {} records ({} bytes) to file in single syscall", batch_count, total_bytes);

        // OPTIMIZATION P3: Single write instead of loop
        let mut file = queue.file.lock().await;

        // RP-3.3: a truncation ran while this batch was in flight. Its records
        // are at or above the cut by construction — the truncation drained the
        // queue for exactly that reason — so writing them now would undo it.
        if queue.truncation_epoch.load(Ordering::SeqCst) != drained_at_epoch {
            drop(file);
            queue.total_queued_bytes.fetch_sub(total_bytes as u64, Ordering::Relaxed);
            // These records are not going to disk, so the tail cache must not
            // keep answering reads with them. `truncate_to` clears the cache,
            // but a batch already drained is past that point — it is dropped
            // *here*, after the clear, and would otherwise be left in memory as
            // records no replica could ever fetch from the file.
            queue.tail.lock().await.clear();
            warn!(
                topic = %queue.topic,
                partition = queue.partition,
                writes = batch_count,
                bytes = total_bytes,
                "Discarding in-flight batch: the log was truncated beneath it"
            );
            for write in batch {
                if let Some(tx) = write.response_tx {
                    let _ = tx.send(Err(WalError::CommitFailed(
                        "log truncated while this write was in flight".into(),
                    )));
                }
            }
            return Ok(());
        }

        file.write_all(&combined_buffer).await?;

        debug!("🔄 COMMIT_FSYNC: Starting fsync for {} bytes", total_bytes);
        // Single fsync for entire batch ⭐
        file.sync_all().await?;
        drop(file);
        debug!("✅ COMMIT_FSYNC_DONE: fsync completed successfully");

        // Update last fsync time
        {
            let mut last_fsync = queue.last_fsync.lock().await;
            *last_fsync = Instant::now();
        }

        // Update queue size atomically (lock-free)
        queue.total_queued_bytes.fetch_sub(total_bytes as u64, Ordering::Relaxed);

        // These records are now on disk, so the tail cache may serve them.
        if let Some(max_offset) = batch.iter().map(|w| w.last_offset).max() {
            queue.committed_through.fetch_max(max_offset, Ordering::SeqCst);
        }

        let fsync_duration = start.elapsed();

        // Update segment size
        queue.segment_size_bytes.fetch_add(total_bytes as u64, Ordering::Relaxed);

        // Update metrics
        if config.enable_metrics {
            queue.metrics.total_commits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            queue.metrics.total_writes.fetch_add(batch_count as u64, std::sync::atomic::Ordering::Relaxed);
            queue.metrics.total_bytes.fetch_add(total_bytes as u64, std::sync::atomic::Ordering::Relaxed);
            queue.metrics.total_fsync_time_us.fetch_add(fsync_duration.as_micros() as u64, std::sync::atomic::Ordering::Relaxed);

            // Record to unified metrics for Prometheus (v2.1.0)
            MetricsRecorder::record_wal_batch(batch_count as u64);
        }

        // Record metrics in span
        tracing::Span::current()
            .record("batch_size", batch_count)
            .record("bytes", total_bytes)
            .record("fsync_us", fsync_duration.as_micros() as u64);

        info!(
            "✅ Group commit: {} writes, {} bytes, fsync took {:?}",
            batch_count, total_bytes, fsync_duration
        );

        // v2.2.10: Invoke commit callback for async response delivery (if configured)
        if let Ok(callback_guard) = commit_callback.read() {
            if let Some(callback) = callback_guard.as_ref() {
                if !batch.is_empty() {
                    // Extract offset range from batch metadata
                    let min_offset = batch.iter().map(|w| w.base_offset).min().unwrap_or(0);
                    let max_offset = batch.iter().map(|w| w.last_offset).max().unwrap_or(0);

                    info!("🔔 INVOKING_CALLBACK: topic={}, partition={}, offsets={}-{}, batch_size={}",
                          &queue.topic, queue.partition, min_offset, max_offset, batch.len());

                    callback(&queue.topic, queue.partition, min_offset, max_offset);

                    info!("✅ CALLBACK_COMPLETED: topic={}, partition={}", &queue.topic, queue.partition);
                }
            } else {
                warn!("⚠️  NO_CALLBACK_SET: topic={}, partition={} - responses will timeout!",
                      &queue.topic, queue.partition);
            }
        }

        // Notify all waiters
        debug!("📢 COMMIT_NOTIFY: Notifying {} waiters that fsync is complete", batch_count);
        for write in batch {
            if let Some(tx) = write.response_tx {
                let _ = tx.send(Ok(()));
            }
        }
        debug!("All {} waiters notified successfully", batch_count);

        // Check if we should seal and rotate segment
        if let Err(e) = Self::check_and_seal_segment(
            queue,
            config,
            sealed_segments,
            base_dir,
            #[cfg(all(target_os = "linux", feature = "async-io"))]
            io_uring_handle,
        ).await {
            error!("Failed to check/seal segment: {}", e);
            // Don't fail the commit, just log the error
        }

        Ok(())
    }

    /// Open a fresh segment file for a partition and reset its size/age counters.
    ///
    /// Shared by size/age-triggered rotation and idle sealing: a segment that
    /// has been handed to the indexer must stop growing, otherwise the indexer
    /// re-reads it (from offset 0) on every run and republishes an ever-larger,
    /// overlapping Parquet/raw segment — which double-counts rows in SQL.
    /// Caller must already hold `queue.file` — that guard is the handoff gate.
    /// `commit_batch` takes the same lock to write, so holding it across
    /// seal-then-rotate is what stops a commit from landing in a file that has
    /// already been recorded as sealed (its recorded size would then be short,
    /// and the indexer — which skips segments it has seen at that size — would
    /// never come back for the extra records).
    async fn rotate_to_new_segment(
        queue: &PartitionCommitQueue,
        file: &mut WalWriter,
        base_dir: &Path,
        #[cfg(all(target_os = "linux", feature = "async-io"))]
        io_uring_handle: &Option<StdArc<IoUringThreadHandle>>,
    ) -> Result<()> {
        let old_segment_id = queue.segment_id.load(Ordering::Relaxed);
        let new_segment_id = old_segment_id + 1;

        let new_file_path = base_dir
            .join(&queue.topic)
            .join(queue.partition.to_string())
            .join(format!("wal_{}_{}.log", queue.partition, new_segment_id));

        let partition_key = format!("{}:{}", queue.topic, queue.partition);
        // Create before publishing: if this fails the partition keeps writing to
        // the old segment, which is still the one `segment_id` names.
        let new_writer = WalWriter::create(
            &new_file_path,
            partition_key,
            #[cfg(all(target_os = "linux", feature = "async-io"))]
            io_uring_handle.clone(),
        )
        .await?;

        *file = new_writer;
        queue.segment_id.store(new_segment_id, Ordering::Relaxed);

        // Reset segment tracking
        *queue.segment_created_at.lock().await = Instant::now();
        queue.segment_size_bytes.store(0, Ordering::Relaxed);

        info!(
            "✅ Rotated to new segment {}/{} segment_id={}",
            queue.topic, queue.partition, new_segment_id
        );

        Ok(())
    }

    /// Force seal a segment (used during shutdown and idle sealing).
    ///
    /// When `rotate` is true the partition continues on a *new* segment file,
    /// leaving the sealed one immutable. Shutdown passes false — nothing will
    /// write again, and a fresh empty file would only be cleaned up later.
    async fn force_seal_segment(
        queue: &PartitionCommitQueue,
        sealed_segments: &Arc<DashMap<String, SealedSegmentInfo>>,
        base_dir: &Path,
        rotate: bool,
        #[cfg(all(target_os = "linux", feature = "async-io"))]
        io_uring_handle: &Option<StdArc<IoUringThreadHandle>>,
    ) -> Result<()> {
        // Hold the writer lock across sync → measure → record → rotate. Commits
        // take this same lock, so nothing can append to the segment between the
        // size we record and the moment it stops being the active file.
        let mut file = queue.file.lock().await;

        let old_segment_id = queue.segment_id.load(Ordering::Relaxed);
        let old_file_path = base_dir
            .join(&queue.topic)
            .join(queue.partition.to_string())
            .join(format!("wal_{}_{}.log", queue.partition, old_segment_id));

        file.sync_all().await?;

        // Size after the fsync, so the recorded size covers every durable byte.
        let file_size = if old_file_path.exists() {
            tokio::fs::metadata(&old_file_path).await
                .map(|m| m.len())
                .unwrap_or(0)
        } else {
            0
        };

        if file_size == 0 {
            // No data in file, skip sealing
            return Ok(());
        }

        info!(
            "🔒 Force sealing segment {}/{} segment_id={} (file_size={} bytes)",
            queue.topic, queue.partition, old_segment_id, file_size
        );

        // Record sealed segment info
        let sealed_key = format!("{}:{}:{}", queue.topic, queue.partition, old_segment_id);
        sealed_segments.insert(
            sealed_key,
            SealedSegmentInfo {
                topic: queue.topic.clone(),
                partition: queue.partition,
                segment_id: old_segment_id,
                file_path: old_file_path.clone(),
                size_bytes: file_size,
                state: SegmentState::Sealed,
                sealed_at: Instant::now(),
            },
        );

        info!(
            "✅ Sealed segment {}/{} segment_id={} (file_size={} bytes)",
            queue.topic, queue.partition, old_segment_id, file_size
        );

        if rotate {
            Self::rotate_to_new_segment(
                queue,
                &mut file,
                base_dir,
                #[cfg(all(target_os = "linux", feature = "async-io"))]
                io_uring_handle,
            )
            .await?;
        }

        Ok(())
    }

    /// Check if current segment should be sealed and rotate if needed
    async fn check_and_seal_segment(
        queue: &PartitionCommitQueue,
        config: &GroupCommitConfig,
        sealed_segments: &Arc<DashMap<String, SealedSegmentInfo>>,
        base_dir: &Path,
        #[cfg(all(target_os = "linux", feature = "async-io"))]
        io_uring_handle: &Option<StdArc<IoUringThreadHandle>>,
    ) -> Result<()> {
        // Skip if rotation disabled
        if !config.enable_rotation {
            return Ok(());
        }

        let current_size = queue.segment_size_bytes.load(Ordering::Relaxed);
        let segment_age = {
            let created_at = queue.segment_created_at.lock().await;
            created_at.elapsed()
        };

        let should_seal = current_size >= config.rotation_size_bytes
            || segment_age.as_secs() >= config.rotation_age_secs;

        if !should_seal {
            return Ok(());
        }

        // Same writer handoff as force_seal_segment: hold the lock from the
        // fsync through the rotation so no commit lands in a file that has
        // already been recorded as sealed at a given size.
        let mut file = queue.file.lock().await;

        let old_segment_id = queue.segment_id.load(Ordering::Relaxed);
        let old_file_path = base_dir
            .join(&queue.topic)
            .join(queue.partition.to_string())
            .join(format!("wal_{}_{}.log", queue.partition, old_segment_id));

        info!(
            "🔒 Sealing segment {}/{} segment_id={} (size={} bytes, age={:?})",
            queue.topic, queue.partition, old_segment_id, current_size, segment_age
        );

        file.sync_all().await?;

        // Re-read the counter under the lock: a commit may have landed between
        // the should_seal check and acquiring the writer.
        let sealed_size = queue.segment_size_bytes.load(Ordering::Relaxed);

        // Record sealed segment info
        let sealed_key = format!("{}:{}:{}", queue.topic, queue.partition, old_segment_id);
        sealed_segments.insert(
            sealed_key,
            SealedSegmentInfo {
                topic: queue.topic.clone(),
                partition: queue.partition,
                segment_id: old_segment_id,
                file_path: old_file_path.clone(),
                size_bytes: sealed_size,
                state: SegmentState::Sealed,
                sealed_at: Instant::now(),
            },
        );

        // Rotate to new segment
        Self::rotate_to_new_segment(
            queue,
            &mut file,
            base_dir,
            #[cfg(all(target_os = "linux", feature = "async-io"))]
            io_uring_handle,
        )
        .await
    }

    /// Discard every record at or above `target_offset` from a partition's WAL.
    ///
    /// This is the destructive half of RP-3.3: a follower that fetched records
    /// from a leader which then lost the election holds a tail that was never
    /// committed, and must remove it before it can converge. Nothing else in
    /// this crate removes records from the tail — `delete_records_before` and
    /// rotation both work from the front.
    ///
    /// # Guarantees
    ///
    /// - No record containing an offset at or above `target_offset` survives.
    /// - The log ends on a record boundary.
    /// - The returned [`TruncateOutcome::new_log_end_offset`] is where the log
    ///   *actually* ends, which is **at or below** `target_offset`: a target
    ///   landing inside a batch takes that whole batch, since keeping it would
    ///   keep records above the target. Resume from the returned value.
    /// - A target at or above the current end changes nothing on disk.
    ///
    /// # Quiescing
    ///
    /// The partition is stopped for the duration by holding its pending-queue
    /// and writer locks — the same two `commit_batch` takes, in the same order.
    /// Buffered writes are dropped and their callers told so; they describe a
    /// log that no longer exists. A batch already drained by the commit worker
    /// is caught by the truncation epoch instead, since it is past the queue
    /// lock by then.
    ///
    /// # What this does not undo
    ///
    /// Truncation is local to this node's WAL. If the `WalIndexer` already
    /// uploaded a sealed segment covering the discarded records, that object
    /// remains in the store under its own `{min}-{max}` key — re-indexing the
    /// shortened segment writes a *different* key rather than replacing it.
    /// Callers must not truncate a partition whose divergent tail has been
    /// published; see `docs/ROADMAP_REPLICATION.md` (RP-3.3).
    /// The offset this partition's log ends at *on disk* — one past the highest
    /// offset the commit worker has written and fsynced.
    ///
    /// `None` when nothing has been committed under this process, in which case
    /// the caller has no better answer here than whatever it already had.
    pub fn durable_end_offset(&self, topic: &str, partition: i32) -> Option<i64> {
        let queue = {
            let entry = self.partition_queues.get(&(topic.to_string(), partition))?;
            Arc::clone(entry.value())
        };
        match queue.committed_through.load(Ordering::SeqCst) {
            i64::MIN => None,
            through => Some(through + 1),
        }
    }

    /// Records from `offset` onward, served out of the partition's in-memory
    /// tail without touching the file.
    ///
    /// `None` means the cache cannot answer — the partition is unknown here, or
    /// the request reaches further back than the cache holds — and the caller
    /// must fall back to reading segments. See `TailCache`.
    pub async fn read_tail(
        &self,
        topic: &str,
        partition: i32,
        offset: i64,
        max_records: usize,
    ) -> Option<Vec<WalRecord>> {
        // Clone the Arc out and drop the map reference before awaiting: holding
        // a DashMap guard across an await is how this codebase has deadlocked
        // before.
        let queue = {
            let entry = self.partition_queues.get(&(topic.to_string(), partition))?;
            Arc::clone(entry.value())
        };

        let durable_through = queue.committed_through.load(Ordering::SeqCst);
        let tail = queue.tail.lock().await;
        tail.read_from(offset, max_records, durable_through)
    }

    pub async fn truncate_to(
        &self,
        topic: &str,
        partition: i32,
        target_offset: i64,
    ) -> Result<TruncateOutcome> {
        let queue = self.get_or_create_queue(topic, partition).await?;

        // Bump before taking any lock: a batch the commit worker has already
        // drained is past the queue lock and can only be caught here.
        queue.truncation_epoch.fetch_add(1, Ordering::SeqCst);

        // Lock order matches commit_batch (pending → file); taking both is what
        // makes the partition quiet.
        let mut pending = queue.pending.lock().await;
        let mut file = queue.file.lock().await;

        let dropped: Vec<PendingWrite> = pending.drain(..).collect();
        queue.total_queued_bytes.store(0, Ordering::Relaxed);

        // A cut log must not be answered out of records the cut removed. The
        // cache holds only a suffix, which is exactly the part a truncation
        // takes, so there is nothing worth salvaging — drop it and let the next
        // read rebuild from the file.
        queue.tail.lock().await.clear();
        // The log is now durable only up to the cut. Leaving this where it was
        // would let records cached after the truncation be served before they
        // are written, because they sit below the old, higher mark.
        queue
            .committed_through
            .store(target_offset.saturating_sub(1), Ordering::SeqCst);

        // Everything committed so far must be visible to the scan below.
        file.sync_all().await?;

        let mut outcome = self
            .truncate_partition_files(topic, partition, target_offset)
            .await?;
        outcome.buffered_writes_dropped = dropped.len();

        if outcome.touched_disk() {
            // The segment that survived a cut is now immutable. Reopening it
            // for append would hand the indexer a file that shrank and then
            // grew, which its "same id, same size ⇒ already done" bookkeeping
            // reads as new work over old bytes. A fresh segment also keeps
            // segment ids monotonic, so no deleted id is ever reused.
            Self::rotate_to_new_segment(
                &queue,
                &mut file,
                &self.base_dir,
                #[cfg(all(target_os = "linux", feature = "async-io"))]
                &self.io_uring_handle,
            )
            .await?;

            info!(
                topic = %topic,
                partition = partition,
                requested = target_offset,
                new_end = ?outcome.new_log_end_offset,
                segments_deleted = outcome.segments_deleted,
                bytes_discarded = outcome.bytes_discarded,
                buffered_dropped = outcome.buffered_writes_dropped,
                "Truncated WAL suffix"
            );
        } else {
            debug!(
                topic = %topic,
                partition = partition,
                requested = target_offset,
                "WAL suffix truncation was a no-op — log already ends at or below the target"
            );
        }

        drop(file);
        drop(pending);

        // Only after the locks are released, so a woken caller cannot re-enter
        // the partition while it is still being rebuilt.
        for write in dropped {
            if let Some(tx) = write.response_tx {
                let _ = tx.send(Err(WalError::CommitFailed(
                    "log truncated before this write was committed".into(),
                )));
            }
        }

        Ok(outcome)
    }

    /// The file surgery behind [`Self::truncate_to`]. Caller must hold the
    /// partition's writer lock.
    ///
    /// Segment ids and offsets both increase with write order, so the target
    /// can be placed by reading only each segment's *first* record: segments
    /// entirely below it are kept untouched, segments entirely at or above it
    /// are deleted whole, and at most one segment straddles the target and is
    /// scanned. That matters — a full scan of every segment would be linear in
    /// the size of the log, and these reach tens of gigabytes.
    async fn truncate_partition_files(
        &self,
        topic: &str,
        partition: i32,
        target_offset: i64,
    ) -> Result<TruncateOutcome> {
        let partition_dir = self.base_dir.join(topic).join(partition.to_string());
        if !partition_dir.exists() {
            // Not "the log is empty" — "I looked in the wrong place, or there is
            // genuinely nothing here". The caller cannot tell those apart from
            // the outcome alone, and it acts on the answer by discarding a log,
            // so say which directory was searched.
            warn!(
                topic = %topic,
                partition = partition,
                dir = %partition_dir.display(),
                "Truncation found no partition directory — reporting an empty log"
            );
            return Ok(TruncateOutcome::untouched(None));
        }

        let mut segments: Vec<(u64, PathBuf)> = Vec::new();
        let prefix = format!("wal_{}_", partition);
        let mut entries = tokio::fs::read_dir(&partition_dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if let Some(id) = name
                    .strip_prefix(&prefix)
                    .and_then(|rest| rest.strip_suffix(".log"))
                    .and_then(|s| s.parse::<u64>().ok())
                {
                    segments.push((id, path));
                }
            }
        }
        segments.sort_by_key(|(id, _)| *id);

        if segments.is_empty() {
            warn!(
                topic = %topic,
                partition = partition,
                dir = %partition_dir.display(),
                prefix = %prefix,
                "Truncation found no segment files — reporting an empty log"
            );
        }

        // Place the target. The segment that can straddle it is the *last* one
        // whose first record sits below it — not the first one at or above it,
        // which is the segment after the straddler. Getting this backwards
        // leaves the straddler's own above-target records alive.
        // What the scan is actually working with. A truncation that removes
        // nothing is indistinguishable from one that had nothing to remove
        // unless the inventory is visible, and the caller acts on the answer by
        // discarding a log.
        for (id, path) in &segments {
            let size = tokio::fs::metadata(path).await.map(|m| m.len()).unwrap_or(0);
            // Computed before the macro: a `?`-formatted temporary held across
            // an await makes the whole future non-Send.
            let first = Self::first_record_offset(path).await?;
            // `info!`, not `debug!`: a truncation happens once per divergence,
            // it deletes data, and when it reports "nothing to do" the inputs
            // are the only way to tell a correct no-op from a scan that was
            // looking at the wrong files.
            info!(
                topic = %topic, partition = partition, segment = id, bytes = size,
                first_offset = first.unwrap_or(-1),
                "truncation inventory"
            );
        }

        let mut straddler: Option<usize> = None;
        for (index, (_, path)) in segments.iter().enumerate() {
            match Self::first_record_offset(path).await? {
                // An empty segment carries no offsets and cannot place
                // anything. The active segment is routinely empty.
                None => continue,
                Some(base) if base < target_offset => straddler = Some(index),
                // Ids ascend with write order, so offsets do too: from here on
                // every segment starts at or above the target.
                Some(_) => break,
            }
        }

        let mut outcome = TruncateOutcome::untouched(None);
        let mut last_kept_offset: Option<i64> = None;

        // Where wholesale deletion starts. With no straddler, nothing on disk
        // is below the target and the whole log goes.
        let delete_from = match straddler {
            None => 0,
            Some(index) => {
                let (segment_id, path) = &segments[index];
                let data = tokio::fs::read(path).await?;

                match crate::truncate::plan_segment_truncation(&data, target_offset) {
                    SegmentVerdict::Empty => {}
                    SegmentVerdict::KeepWhole { last_offset } => {
                        last_kept_offset = Some(last_offset);
                    }
                    SegmentVerdict::DeleteWhole => {
                        outcome.bytes_discarded += data.len() as u64;
                        outcome.segments_deleted += 1;
                        self.remove_truncated_segment(topic, partition, *segment_id, path)
                            .await?;
                    }
                    SegmentVerdict::Cut {
                        keep_bytes,
                        last_offset,
                    } => {
                        last_kept_offset = Some(last_offset);
                        outcome.bytes_discarded += data.len() as u64 - keep_bytes;

                        // Async throughout: `sync_all` on a segment can stall
                        // for milliseconds, and this runs on the runtime that
                        // is also serving every other partition's commits.
                        let file = tokio::fs::OpenOptions::new()
                            .write(true)
                            .open(path)
                            .await?;
                        file.set_len(keep_bytes).await?;
                        file.sync_all().await?;
                        drop(file);

                        // The registry advertises sizes to the indexer; a stale
                        // one sends it reading past the new end of the file.
                        self.resize_sealed_segment(topic, partition, *segment_id, keep_bytes);
                    }
                }
                index + 1
            }
        };

        let mut deleted: Vec<usize> = Vec::new();
        for (index, (segment_id, path)) in segments.iter().enumerate().skip(delete_from) {
            let size = tokio::fs::metadata(path).await.map(|m| m.len()).unwrap_or(0);
            if size == 0 {
                // Nothing to discard, and this is usually the segment the
                // writer is holding open. Unlinking it would leave the writer
                // appending into an orphaned inode — bytes accepted, fsynced,
                // and unreadable by anyone.
                continue;
            }
            outcome.bytes_discarded += size;
            outcome.segments_deleted += 1;
            deleted.push(index);
            self.remove_truncated_segment(topic, partition, *segment_id, path)
                .await?;
        }

        // Where the log ends now: the last record in the last SURVIVING segment.
        //
        // Scan every survivor, not `segments[..delete_from - 1]`. That range
        // excluded the straddler and everything the delete loop skipped for
        // being empty, so in the case where nothing was cut at all it looked at
        // no files, found nothing, and reported "the log is empty".
        //
        // `None` then means two incompatible things — "nothing survived" and "I
        // did not touch anything" — and the caller collapses both to offset 0.
        // A truncation that legitimately had no work to do therefore told the
        // follower its log was empty, and the follower re-replicated the whole
        // partition from scratch instead of keeping the records it already had.
        // Observed on two of three divergence runs.
        if last_kept_offset.is_none() {
            for (index, (_, path)) in segments.iter().enumerate().rev() {
                if deleted.contains(&index) {
                    continue;
                }
                if let Some(last) = Self::last_record_offset(path).await? {
                    last_kept_offset = Some(last);
                    break;
                }
            }
        }

        outcome.new_log_end_offset = last_kept_offset.map(|last| last + 1);
        Ok(outcome)
    }

    /// Delete a segment file and forget it, so nothing later reads a path that
    /// is gone or indexes a segment that was discarded.
    async fn remove_truncated_segment(
        &self,
        topic: &str,
        partition: i32,
        segment_id: u64,
        path: &Path,
    ) -> Result<()> {
        self.sealed_segments
            .remove(&format!("{}:{}:{}", topic, partition, segment_id));
        match tokio::fs::remove_file(path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Correct a sealed segment's recorded size after it was shortened.
    fn resize_sealed_segment(&self, topic: &str, partition: i32, segment_id: u64, size: u64) {
        if let Some(mut entry) = self
            .sealed_segments
            .get_mut(&format!("{}:{}:{}", topic, partition, segment_id))
        {
            entry.size_bytes = size;
        }
    }

    /// The base offset of a segment's first record, reading only as much of the
    /// file as that record occupies. `None` for an empty or torn segment.
    async fn first_record_offset(path: &Path) -> Result<Option<i64>> {
        use tokio::io::AsyncReadExt;

        let mut file = match tokio::fs::File::open(path).await {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };

        let mut header = [0u8; crate::truncate::RECORD_HEADER_LEN];
        if file.read_exact(&mut header).await.is_err() {
            return Ok(None); // shorter than one header: empty or torn
        }
        let size = match crate::truncate::declared_record_size(&header) {
            Some(size) => size,
            None => return Ok(None),
        };

        let mut buf = vec![0u8; size];
        buf[..crate::truncate::RECORD_HEADER_LEN].copy_from_slice(&header);
        if file
            .read_exact(&mut buf[crate::truncate::RECORD_HEADER_LEN..])
            .await
            .is_err()
        {
            return Ok(None); // record is truncated on disk
        }

        Ok(crate::truncate::first_record_range(&buf).map(|(base, _)| base))
    }

    /// The highest offset a segment holds. Only needed to report where the log
    /// ends when the straddling segment kept nothing.
    async fn last_record_offset(path: &Path) -> Result<Option<i64>> {
        let data = match tokio::fs::read(path).await {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        // i64::MAX keeps every record, so the plan reports the segment's end.
        match crate::truncate::plan_segment_truncation(&data, i64::MAX) {
            SegmentVerdict::KeepWhole { last_offset } => Ok(Some(last_offset)),
            _ => Ok(None),
        }
    }

    /// Get metrics for a partition
    pub fn get_metrics(&self, topic: &str, partition: i32) -> Option<PartitionMetrics> {
        let key = (topic.to_string(), partition);
        self.partition_queues.get(&key).map(|queue| {
            let metrics = &queue.metrics;
            PartitionMetrics {
                total_commits: metrics.total_commits.load(std::sync::atomic::Ordering::Relaxed),
                total_writes: metrics.total_writes.load(std::sync::atomic::Ordering::Relaxed),
                total_bytes: metrics.total_bytes.load(std::sync::atomic::Ordering::Relaxed),
                avg_fsync_time_us: if metrics.total_commits.load(std::sync::atomic::Ordering::Relaxed) > 0 {
                    metrics.total_fsync_time_us.load(std::sync::atomic::Ordering::Relaxed) /
                    metrics.total_commits.load(std::sync::atomic::Ordering::Relaxed)
                } else {
                    0
                },
                backpressure_events: metrics.backpressure_events.load(std::sync::atomic::Ordering::Relaxed),
            }
        })
    }

    /// Get list of sealed segments ready for archival
    pub fn get_sealed_segments(&self) -> Vec<SealedSegmentInfo> {
        self.sealed_segments
            .iter()
            .filter(|entry| entry.value().state == SegmentState::Sealed)
            .map(|entry| entry.value().clone())
            .collect()
    }

    /// Mark segment as archived after successful S3 upload
    pub fn mark_segment_archived(&self, topic: &str, partition: i32, segment_id: u64) {
        let key = format!("{}:{}:{}", topic, partition, segment_id);
        if let Some(mut entry) = self.sealed_segments.get_mut(&key) {
            entry.state = SegmentState::Archived;
            info!("Marked segment {}/{} segment_id={} as Archived", topic, partition, segment_id);
        }
    }

    /// Force-delete a sealed segment's on-disk file and drop its tracking entry,
    /// regardless of archive state. Used by the Kafka DeleteRecords API to reclaim
    /// disk for segments that fall entirely below a partition's new log start offset.
    /// The caller MUST NOT pass the active (currently-written) segment.
    pub fn delete_segment_file(&self, topic: &str, partition: i32, segment_id: u64) -> Result<()> {
        let key = format!("{}:{}:{}", topic, partition, segment_id);
        // Drop tracking (if present) and recover the file path from it.
        let tracked = self.sealed_segments.remove(&key);
        let path = if let Some((_, info)) = &tracked {
            info.file_path.clone()
        } else {
            self.base_dir
                .join(topic)
                .join(partition.to_string())
                .join(format!("wal_{}_{}.log", partition, segment_id))
        };
        if path.exists() {
            std::fs::remove_file(&path)?;
            info!("DeleteRecords: removed WAL segment file {:?}", path);
        }
        Ok(())
    }

    /// Remove archived segment (called after S3 upload and local cleanup)
    pub fn remove_segment(&self, topic: &str, partition: i32, segment_id: u64) -> Result<()> {
        let key = format!("{}:{}:{}", topic, partition, segment_id);

        if let Some((_, segment_info)) = self.sealed_segments.remove(&key) {
            // Delete local file if configured
            if segment_info.state == SegmentState::Archived {
                if let Err(e) = std::fs::remove_file(&segment_info.file_path) {
                    warn!("Failed to delete archived segment file {:?}: {}", segment_info.file_path, e);
                } else {
                    info!("Deleted archived segment file {:?}", segment_info.file_path);
                }
            }
        }

        Ok(())
    }

    /// Seal segments that have been idle for longer than the threshold.
    /// This ensures data gets indexed even if topics go quiet.
    /// Returns the number of segments sealed.
    pub async fn seal_stale_segments(&self, max_idle_secs: u64) -> usize {
        let mut sealed_count = 0;
        let threshold = Duration::from_secs(max_idle_secs);

        for entry in self.partition_queues.iter() {
            let ((topic, partition), queue) = entry.pair();

            // Check segment age
            let segment_age = {
                let created_at = queue.segment_created_at.lock().await;
                created_at.elapsed()
            };

            // Only seal if segment is older than threshold AND has data
            let current_size = queue.segment_size_bytes.load(std::sync::atomic::Ordering::Relaxed);
            if segment_age >= threshold && current_size > 0 {
                let segment_id = queue.segment_id.load(std::sync::atomic::Ordering::Relaxed);
                info!(
                    "⏰ Sealing stale segment {}/{} segment_id={} (idle={:?}, size={})",
                    topic, partition, segment_id, segment_age, current_size
                );

                // Rotate: the sealed file is about to be handed to the indexer
                // and must not keep accepting writes.
                if let Err(e) = Self::force_seal_segment(
                    queue,
                    &self.sealed_segments,
                    &self.base_dir,
                    true,
                    #[cfg(all(target_os = "linux", feature = "async-io"))]
                    &self.io_uring_handle,
                )
                .await
                {
                    error!("Failed to seal stale segment {}-{}: {}", topic, partition, e);
                } else {
                    sealed_count += 1;
                }
            }
        }

        if sealed_count > 0 {
            info!("Sealed {} stale segments (idle > {}s)", sealed_count, max_idle_secs);
        }

        sealed_count
    }

    /// Clean up all partition queues and sealed segments for a deleted topic.
    /// Called when a topic is deleted via the Kafka DeleteTopics API.
    /// Removes partition_queues entries (commit workers will exit on next loop)
    /// and removes sealed_segments entries for the topic.
    pub async fn cleanup_topic(&self, topic: &str) {
        // Collect partition IDs for this topic
        let partitions: Vec<i32> = self.partition_queues
            .iter()
            .filter_map(|entry| {
                let ((t, p), _) = entry.pair();
                if t == topic { Some(*p) } else { None }
            })
            .collect();

        // Remove partition queues (commit workers check for removal and exit)
        for partition in &partitions {
            let key = (topic.to_string(), *partition);
            if self.partition_queues.remove(&key).is_some() {
                info!("Cleaned up partition queue for deleted topic: {}:{}", topic, partition);
            }
        }

        // Remove sealed segments for this topic
        let sealed_keys: Vec<String> = self.sealed_segments
            .iter()
            .filter_map(|entry| {
                if entry.value().topic == topic {
                    Some(entry.key().clone())
                } else {
                    None
                }
            })
            .collect();

        for key in &sealed_keys {
            self.sealed_segments.remove(key);
        }

        // Reclaim ON-DISK storage for the deleted topic. Previously cleanup only
        // dropped in-memory state (queues + sealed-segment tracking) but left the
        // `wal_*.log` files on disk under `base_dir/<topic>/`, so a deleted
        // topic's data lingered forever (~44GB/node accrued from the LongMemEval
        // eval), and the WalIndexer kept grinding those orphaned segments. Remove
        // the whole topic dir — all partitions + active + sealed segment files.
        let topic_dir = self.base_dir.join(topic);
        let removed_dir = if topic_dir.exists() {
            match std::fs::remove_dir_all(&topic_dir) {
                Ok(()) => true,
                Err(e) => {
                    warn!("cleanup_topic: failed to remove WAL dir {:?}: {}", topic_dir, e);
                    false
                }
            }
        } else {
            false
        };

        if !partitions.is_empty() || !sealed_keys.is_empty() || removed_dir {
            info!(
                "Topic '{}' cleanup: removed {} partition queues, {} sealed segments, wal_dir={}",
                topic, partitions.len(), sealed_keys.len(), removed_dir
            );
        }
    }

    /// Shutdown the group commit WAL
    pub async fn shutdown(&self) {
        info!("Shutting down group commit WAL...");
        self.shutdown.notify_waiters();

        // Give workers time to finish pending commits
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Force seal all active segments (CRITICAL for segment flush test)
        // This ensures all data is written to disk before shutdown
        info!("Force sealing all active WAL segments...");
        let queue_count = self.partition_queues.len();
        info!("Found {} partition queues to seal", queue_count);

        for entry in self.partition_queues.iter() {
            let ((topic, partition), queue) = entry.pair();

            // Get current segment size (note: this is unflushed buffer size, not file size)
            let current_size = queue.segment_size_bytes.load(std::sync::atomic::Ordering::Relaxed);
            let segment_id = queue.segment_id.load(std::sync::atomic::Ordering::Relaxed);

            info!("Processing {}-{} segment {} (buffer_size={} bytes)",
                  topic, partition, segment_id, current_size);

            // Force seal current segment regardless of size
            // (files may have data from previous fsyncs even if buffer is empty)
            // No rotation on shutdown: nothing will write again.
            if let Err(e) = Self::force_seal_segment(
                queue,
                &self.sealed_segments,
                &self.base_dir,
                false,
                #[cfg(all(target_os = "linux", feature = "async-io"))]
                &self.io_uring_handle,
            )
            .await
            {
                error!("Failed to seal segment {}-{}: {}", topic, partition, e);
            } else {
                info!("Successfully processed seal for {}-{}", topic, partition);
            }
        }
        info!("Finished sealing {} partitions", queue_count);

        info!("Group commit WAL shutdown complete");
    }

    /// Get current segment state for a topic/partition (for checkpointing)
    ///
    /// Returns (segment_id, position) where:
    /// - segment_id: Current WAL segment number
    /// - position: Current byte position within the segment (unflushed buffer size)
    pub fn get_segment_state(&self, topic: &str, partition: i32) -> Option<(u64, u64)> {
        self.partition_queues.get(&(topic.to_string(), partition))
            .map(|queue| {
                let segment_id = queue.segment_id.load(std::sync::atomic::Ordering::Relaxed);
                let position = queue.segment_size_bytes.load(std::sync::atomic::Ordering::Relaxed);
                (segment_id, position)
            })
    }

    /// Get the base directory for WAL files
    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }
}

/// Public metrics structure
#[derive(Debug, Clone)]
pub struct PartitionMetrics {
    pub total_commits: u64,
    pub total_writes: u64,
    pub total_bytes: u64,
    pub avg_fsync_time_us: u64,
    pub backpressure_events: u64,
}

#[cfg(test)]
mod truncation_race_tests {
    use super::*;

    /// The one interleaving that can silently resurrect truncated records.
    ///
    /// `commit_batch` drains the pending queue and *then* takes the writer
    /// lock, so a batch can be off the queue but not yet on disk when a
    /// truncation runs. Holding the writer lock from the test pins the worker
    /// in exactly that window: it has drained, it is blocked, and the epoch
    /// moves underneath it. Without the guard it would write afterwards, and
    /// the records the truncation just removed would be back on disk with
    /// nothing to indicate they had ever gone.
    #[tokio::test]
    async fn a_batch_drained_before_a_truncation_is_not_written_after_it() {
        let dir = tempfile::tempdir().unwrap();
        let wal = GroupCommitWal::new(dir.path().to_path_buf(), GroupCommitConfig::default());
        let (topic, partition) = ("race", 0);

        let queue = wal.get_or_create_queue(topic, partition).await.unwrap();
        let segment = dir
            .path()
            .join(topic)
            .join(partition.to_string())
            .join(format!("wal_{}_0.log", partition));

        // Block the writer before anything can reach it.
        let file_guard = queue.file.lock().await;

        let record = WalRecord::new_v2(topic.to_string(), partition, vec![7u8; 64], 0, 0, 1);
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = queue.pending.lock().await;
            pending.push_back(PendingWrite {
                data: Bytes::from(record.to_bytes().unwrap()),
                response_tx: Some(tx),
                base_offset: 0,
                last_offset: 0,
            });
        }
        queue.write_notify.notify_one();

        // Wait for the commit worker to drain the queue. It cannot get further
        // than the writer lock, which this test holds.
        for _ in 0..1_000 {
            if queue.pending.lock().await.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert!(
            queue.pending.lock().await.is_empty(),
            "the commit worker never drained the queue; the race is not set up"
        );

        // A truncation runs while that batch is in flight.
        queue.truncation_epoch.fetch_add(1, Ordering::SeqCst);
        drop(file_guard);

        let result = tokio::time::timeout(Duration::from_secs(5), rx)
            .await
            .expect("the waiter must be answered, not left hanging")
            .expect("the response channel must not be dropped silently");
        assert!(
            result.is_err(),
            "a write discarded by a truncation must be reported as failed, not as committed"
        );

        // Give the worker a moment to do anything else it might have planned.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let size = std::fs::metadata(&segment).map(|m| m.len()).unwrap_or(0);
        assert_eq!(size, 0, "the stale batch was written to disk anyway");
    }

    /// The guard must not fire when no truncation happened, or every ordinary
    /// commit would be dropped and the WAL would accept nothing at all.
    #[tokio::test]
    async fn an_ordinary_batch_still_commits() {
        let dir = tempfile::tempdir().unwrap();
        let wal = GroupCommitWal::new(dir.path().to_path_buf(), GroupCommitConfig::default());
        let (topic, partition) = ("no-race", 0);

        let record = WalRecord::new_v2(topic.to_string(), partition, vec![1u8; 32], 0, 0, 1);
        wal.append(topic.to_string(), partition, record, 1)
            .await
            .expect("a write with no truncation in sight must commit");

        let segment = dir
            .path()
            .join(topic)
            .join(partition.to_string())
            .join(format!("wal_{}_0.log", partition));
        assert!(std::fs::metadata(&segment).unwrap().len() > 0);
    }
}

#[cfg(test)]
mod tail_cache_tests {
    use super::*;

    fn record(base: i64, last: i64, payload: usize) -> WalRecord {
        WalRecord::new_v2(
            "t".to_string(),
            0,
            vec![0u8; payload],
            base,
            last,
            (last - base + 1) as i32,
        )
    }

    fn push(cache: &mut TailCache, base: i64, last: i64, payload: usize) {
        let r = record(base, last, payload);
        let size = r.heap_size();
        cache.push(r, size);
    }

    #[test]
    fn serves_a_read_that_starts_inside_what_it_holds() {
        let mut cache = TailCache::new(1 << 20);
        push(&mut cache, 0, 9, 16);
        push(&mut cache, 10, 19, 16);
        push(&mut cache, 20, 29, 16);

        let got = cache.read_from(10, 1000, i64::MAX).expect("cache starts at 0, so it covers 10");
        assert_eq!(
            got.iter().map(|r| r.get_base_offset()).collect::<Vec<_>>(),
            vec![10, 20],
            "the batch ending at 9 is entirely below the requested offset"
        );
    }

    /// The rule the whole design rests on: answering a read the cache only
    /// partly covers would silently drop the records below its start, and the
    /// caller would take the short answer as the whole log.
    #[test]
    fn a_read_starting_before_the_cache_is_a_miss_not_a_short_answer() {
        let mut cache = TailCache::new(1 << 20);
        push(&mut cache, 100, 109, 16);
        push(&mut cache, 110, 119, 16);

        assert!(
            cache.read_from(50, 1000, i64::MAX).is_none(),
            "offset 50 predates the cache; the file must answer this"
        );
        assert!(cache.read_from(100, 1000, i64::MAX).is_some(), "its own start is covered");
    }

    #[test]
    fn eviction_keeps_the_cache_a_contiguous_suffix() {
        // Budget for roughly two records, so the third evicts the first.
        let one = record(0, 0, 512).heap_size();
        let mut cache = TailCache::new(one * 2 + one / 2);
        push(&mut cache, 0, 9, 512);
        push(&mut cache, 10, 19, 512);
        push(&mut cache, 20, 29, 512);

        assert!(
            cache.read_from(0, 1000, i64::MAX).is_none(),
            "offset 0 was evicted, so the cache can no longer answer for it"
        );
        let got = cache.read_from(10, 1000, i64::MAX).expect("the surviving suffix still answers");
        assert_eq!(
            got.iter().map(|r| r.get_base_offset()).collect::<Vec<_>>(),
            vec![10, 20],
            "what survives eviction is contiguous and reaches the log end"
        );
    }

    /// Offsets are assigned before the WAL append, so two produces to one
    /// partition can arrive here out of order. The cache must not then claim to
    /// cover a range it has a hole in — a caller asking for the whole log would
    /// get part of it and treat that as all of it.
    #[test]
    fn a_gap_is_served_from_above_it_and_missed_from_below() {
        let mut cache = TailCache::new(1 << 20);
        push(&mut cache, 0, 9, 16);
        push(&mut cache, 30, 39, 16); // 10..29 never arrived

        assert!(
            cache.read_from(0, 1000, i64::MAX).is_none(),
            "offset 0 must miss: answering it would silently skip 10..29"
        );
        let got = cache.read_from(30, 1000, i64::MAX).expect("above the gap it reaches the log end");
        assert_eq!(got.len(), 1);

        // Filling the hole makes the whole range answerable again.
        push(&mut cache, 10, 29, 16);
        let got = cache.read_from(0, 1000, i64::MAX).expect("no hole left");
        assert_eq!(
            got.iter().map(|r| r.get_base_offset()).collect::<Vec<_>>(),
            vec![0, 10, 30],
            "and it comes back in offset order, not arrival order"
        );
    }

    /// Offsets are assigned before the WAL append, so two produces to one
    /// partition routinely reach the cache in an order that does not match
    /// their offsets — under concurrency that is most of them, not an edge
    /// case. Refusing those emptied the cache permanently and every fetch fell
    /// back to the full-segment scan the cache exists to avoid.
    #[test]
    fn out_of_order_arrival_is_ordered_into_place() {
        let mut cache = TailCache::new(1 << 20);
        push(&mut cache, 10, 19, 16);
        push(&mut cache, 0, 9, 16); // overtook its predecessor

        let got = cache.read_from(0, 1000, i64::MAX).expect("both records are held, and they join up");
        assert_eq!(
            got.iter().map(|r| r.get_base_offset()).collect::<Vec<_>>(),
            vec![0, 10]
        );
    }

    /// The cache is filled at append time, before the commit worker has written
    /// anything — and some queued writes never reach the file at all, because
    /// `commit_batch` discards a batch that straddles a truncation. Serving
    /// those would let a replica read records no file will ever hold.
    #[test]
    fn records_past_the_durable_end_are_not_served() {
        let mut cache = TailCache::new(1 << 20);
        push(&mut cache, 0, 9, 16);
        push(&mut cache, 10, 19, 16);
        push(&mut cache, 20, 29, 16);

        // Only the first two batches have been fsynced.
        let got = cache.read_from(0, 1000, 19).expect("the durable part is servable");
        assert_eq!(
            got.iter().map(|r| r.get_last_offset()).collect::<Vec<_>>(),
            vec![9, 19],
            "the queued-but-unwritten batch must be withheld"
        );
    }

    /// "Nothing durable yet" is a miss, not an empty log — an empty answer
    /// would be read as the end of the log while the file still has records.
    #[test]
    fn a_cache_with_nothing_durable_yet_reports_a_miss() {
        let mut cache = TailCache::new(1 << 20);
        push(&mut cache, 0, 9, 16);

        assert!(
            cache.read_from(0, 1000, i64::MIN).is_none(),
            "before the commit worker writes anything the file must answer"
        );
    }

    #[test]
    fn a_duplicate_append_is_ignored() {
        let mut cache = TailCache::new(1 << 20);
        push(&mut cache, 0, 9, 16);
        push(&mut cache, 20, 29, 16);
        push(&mut cache, 20, 29, 16); // same batch again

        let got = cache.read_from(20, 1000, i64::MAX).expect("covered");
        assert_eq!(got.len(), 1, "the duplicate must not be stored twice");
    }

    #[test]
    fn a_cleared_cache_answers_nothing() {
        let mut cache = TailCache::new(1 << 20);
        push(&mut cache, 0, 9, 16);
        cache.clear();
        assert!(
            cache.read_from(0, 1000, i64::MAX).is_none(),
            "truncation clears the cache; every read must go back to the file"
        );
    }

    #[test]
    fn a_zero_budget_disables_the_cache_entirely() {
        let mut cache = TailCache::new(0);
        push(&mut cache, 0, 9, 16);
        assert!(cache.read_from(0, 1000, i64::MAX).is_none());
    }

    #[test]
    fn max_records_counts_messages_not_batches() {
        let mut cache = TailCache::new(1 << 20);
        push(&mut cache, 0, 9, 16);   // 10 messages
        push(&mut cache, 10, 19, 16); // 10 messages
        push(&mut cache, 20, 29, 16); // 10 messages

        let got = cache.read_from(0, 15, i64::MAX).expect("covered");
        assert_eq!(
            got.len(),
            2,
            "the limit is in messages, so it stops after the batch that crosses it"
        );
    }

    /// The cache exists to answer without I/O, and a truncation must reach it
    /// even though the records it holds were never rejected by the writer.
    #[tokio::test]
    async fn truncation_invalidates_the_tail_cache() {
        let dir = tempfile::tempdir().unwrap();
        let wal = GroupCommitWal::new(dir.path().to_path_buf(), GroupCommitConfig::default());

        for i in 0..5i64 {
            wal.append("t".to_string(), 0, record(i * 10, i * 10 + 9, 64), 1)
                .await
                .unwrap();
        }
        assert!(
            wal.read_tail("t", 0, 0, 1000).await.is_some(),
            "appends populate the cache"
        );

        wal.truncate_to("t", 0, 20).await.unwrap();

        assert!(
            wal.read_tail("t", 0, 0, 1000).await.is_none(),
            "after a cut the cache must not answer — the file is authoritative again"
        );
    }

    #[tokio::test]
    async fn an_unknown_partition_is_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        let wal = GroupCommitWal::new(dir.path().to_path_buf(), GroupCommitConfig::default());
        assert!(wal.read_tail("never-written", 0, 0, 10).await.is_none());
    }
}
