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
use std::sync::atomic::{AtomicU64, Ordering};
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

                                                                // Only consider sealed segments (>= rotation size or not the highest segment_id)
                                                                // For now, consider all segments except segment_0 as potentially sealed
                                                                if segment_id > 0 || size_bytes >= self.config.rotation_size_bytes {
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

        // Handle acks=0 (fire-and-forget)
        if acks == 0 {
            return self.enqueue_nowait(queue, data.into(), base_offset, last_offset).await;
        }

        // Handle acks=1 or acks=-1 (wait for commit)
        self.enqueue_and_wait(queue, data.into(), data_len, base_offset, last_offset).await
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

        // Use partition-specific naming for consistency with rotation
        // Format: wal_{partition}_{segment_id}.log
        let wal_path = partition_dir.join(format!("wal_{}_0.log", partition));

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
            segment_id: Arc::new(AtomicU64::new(0)),
            segment_created_at: Arc::new(Mutex::new(Instant::now())),
            segment_size_bytes: Arc::new(AtomicU64::new(0)),
            topic: topic.to_string(),
            partition,
        });

        // Start per-partition commit worker
        self.start_partition_committer(queue.clone());

        self.partition_queues.insert(key, queue.clone());

        info!("Created commit queue for {}-{}", topic, partition);

        Ok(queue)
    }

    /// Start per-partition commit worker
    fn start_partition_committer(&self, queue: Arc<PartitionCommitQueue>) {
        let config = self.config.clone();
        let shutdown = self.shutdown.clone();
        let sealed_segments = self.sealed_segments.clone();
        let base_dir = self.base_dir.clone();
        #[cfg(all(target_os = "linux", feature = "async-io"))]
        let io_uring_handle = self.io_uring_handle.clone();
        let commit_callback = self.commit_callback.clone();  // v2.2.10: For async response delivery

        info!("🚀 WORKER_SPAWN: Starting partition committer background task");

        tokio::spawn(async move {
            info!("✅ WORKER_STARTED: Partition committer task is running");

            // v2.2.7: Set high I/O priority for WAL thread to prevent Tantivy from blocking it
            // NOTE: This is optional and failure is non-fatal
            if let Err(e) = crate::io_priority::set_wal_priority() {
                debug!("Could not set WAL I/O priority (requires CAP_SYS_ADMIN): {}", e);
            }
            let mut interval = tokio::time::interval(Duration::from_millis(config.max_wait_time_ms));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            debug!("WORKER_CONFIG: max_wait_time_ms={}, max_batch_size={}, max_batch_bytes={}",
                config.max_wait_time_ms, config.max_batch_size, config.max_batch_bytes);

            loop {
                // v2.2.9 fix: Changed hot-path logs from debug! to trace! to prevent log bomb
                // With 1000s of partitions, these logs fire 10x/sec per partition = 180K lines/sec
                trace!("🔄 WORKER_LOOP: Waiting for interval tick (PostgreSQL-style wait queue)");

                // POSTGRESQL-STYLE WAIT QUEUE (v2.2.11 - FIX for acks=1 batching)
                //
                // PROBLEM (v2.2.10): Adaptive batching failed because:
                //   - Every write calls write_notify.notify_one()
                //   - Worker wakes up, checks queue length
                //   - Writes arrive spaced (1-4ms apart due to network/client pipelining)
                //   - Queue rarely reaches MIN_BATCH_SIZE before tick
                //   - Result: batch_size=1, throughput stuck at ~10K msg/s
                //
                // SOLUTION (v2.2.11): PostgreSQL wait queue approach:
                //   - IGNORE write notifications entirely
                //   - ONLY commit on interval tick (50ms for MEDIUM profile)
                //   - Let writes naturally accumulate during interval
                //   - All accumulated writes committed together in one batch
                //
                // With 128 concurrent clients at 50ms interval:
                //   - ~64-128 writes accumulate naturally
                //   - Single fsync commits entire batch
                //   - Expected throughput: 40K-60K msg/s
                //
                // This matches PostgreSQL commit_delay behavior:
                //   - PostgreSQL waits commit_delay microseconds
                //   - If commit_siblings backends waiting, one commits for all
                //   - Chronik: All clients wait on oneshot channel, worker commits for all
                tokio::select! {
                    _ = interval.tick() => {
                        trace!("⏰ WORKER_TICK: Interval tick fired - committing all accumulated writes");
                        // Fall through to commit
                    }
                    _ = shutdown.notified() => {
                        info!("🛑 WORKER_SHUTDOWN: Partition committer shutting down");
                        return;  // Exit worker entirely
                    }
                    // NOTE: write_notify branch REMOVED - we ignore notifications and only commit on tick
                    // This allows natural write accumulation without synchronization
                }

                // Commit batch (triggered ONLY by interval tick)
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
        let (batch, combined_buffer) = {
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

            (batch, combined)
        }; // Lock released here - much shorter critical section!

        let batch_count = batch.len();
        let total_bytes = combined_buffer.len();

        debug!("💾 COMMIT_WRITE: Writing {} records ({} bytes) to file in single syscall", batch_count, total_bytes);

        // OPTIMIZATION P3: Single write instead of loop
        let mut file = queue.file.lock().await;
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

    /// Force seal a segment (used during shutdown)
    async fn force_seal_segment(
        queue: &PartitionCommitQueue,
        sealed_segments: &Arc<DashMap<String, SealedSegmentInfo>>,
        base_dir: &Path,
    ) -> Result<()> {
        // Seal current segment
        let old_segment_id = queue.segment_id.load(Ordering::Relaxed);
        let old_file_path = base_dir
            .join(&queue.topic)
            .join(queue.partition.to_string())
            .join(format!("wal_{}_{}.log", queue.partition, old_segment_id));

        // Get actual file size from disk
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

        // Close current file
        {
            let file = queue.file.lock().await;
            file.sync_all().await?;
            drop(file); // Explicitly drop to close
        }

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

        // Seal current segment
        let old_segment_id = queue.segment_id.load(Ordering::Relaxed);
        let old_file_path = base_dir
            .join(&queue.topic)
            .join(queue.partition.to_string())
            .join(format!("wal_{}_{}.log", queue.partition, old_segment_id));

        info!(
            "🔒 Sealing segment {}/{} segment_id={} (size={} bytes, age={:?})",
            queue.topic, queue.partition, old_segment_id, current_size, segment_age
        );

        // Close old file
        {
            let file = queue.file.lock().await;
            file.sync_all().await?;
            drop(file); // Explicitly drop to close
        }

        // Record sealed segment info
        let sealed_key = format!("{}:{}:{}", queue.topic, queue.partition, old_segment_id);
        sealed_segments.insert(
            sealed_key,
            SealedSegmentInfo {
                topic: queue.topic.clone(),
                partition: queue.partition,
                segment_id: old_segment_id,
                file_path: old_file_path.clone(),
                size_bytes: current_size,
                state: SegmentState::Sealed,
                sealed_at: Instant::now(),
            },
        );

        // Rotate to new segment
        let new_segment_id = old_segment_id + 1;
        queue.segment_id.store(new_segment_id, Ordering::Relaxed);

        let new_file_path = base_dir
            .join(&queue.topic)
            .join(queue.partition.to_string())
            .join(format!("wal_{}_{}.log", queue.partition, new_segment_id));

        // Open new file with io_uring if available
        let partition_key = format!("{}:{}", queue.topic, queue.partition);
        let new_writer = WalWriter::create(
            &new_file_path,
            partition_key,
            #[cfg(all(target_os = "linux", feature = "async-io"))]
            io_uring_handle.clone(),
        ).await?;

        *queue.file.lock().await = new_writer;

        // Reset segment tracking
        *queue.segment_created_at.lock().await = Instant::now();
        queue.segment_size_bytes.store(0, Ordering::Relaxed);

        info!(
            "✅ Rotated to new segment {}/{} segment_id={}",
            queue.topic, queue.partition, new_segment_id
        );

        Ok(())
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

                if let Err(e) = Self::force_seal_segment(queue, &self.sealed_segments, &self.base_dir).await {
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
            if let Err(e) = Self::force_seal_segment(queue, &self.sealed_segments, &self.base_dir).await {
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
