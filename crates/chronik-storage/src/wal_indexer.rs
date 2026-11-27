//! WAL Indexer - Background task to convert sealed WAL segments to Tantivy indexes
//!
//! This module implements the background indexing task that:
//! 1. Monitors for sealed WAL segments
//! 2. Reads CanonicalRecord batches from sealed segments
//! 3. Writes records to Tantivy indexes
//! 4. Uploads indexes to object store
//! 5. Deletes WAL segments after successful indexing
//!
//! This enables the layered storage architecture:
//! - WAL (hot data, seconds to minutes)
//! - Tantivy (warm data, minutes to hours, searchable)
//! - Object store (cold data, hours to days, archived)

use crate::{
    canonical_record::{CanonicalRecord, CanonicalRecordEntry, TimestampType},
    tantivy_segment::TantivySegmentWriter,
    object_store::{ObjectStore, ObjectStoreConfig},
    segment_index::{SegmentIndex, SegmentMetadata as SegmentIndexMetadata, ParquetSegmentMetadata},
};
use chronik_wal::{WalManager, WalRecord};
use chronik_common::{Result, Error};
use chronik_common::metadata::traits::{MetadataStore, SegmentMetadata as MetadataSegmentMetadata};
use chronik_columnar::{
    ParquetSegmentWriter,
    ColumnarConfig,
    CompressionCodec,
    PartitioningStrategy,
    converter::{RecordBatchConverter, KafkaRecord},
    json_schema::InferredJsonSchema,
    HotDataBuffer,
    VectorIndexManager, VectorIndexConfig, HnswIndexConfig, EmbeddingPipeline,
};
use chronik_embeddings::{VectorSearchConfig, create_provider};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::RwLock;
use tokio::time::Duration;
use tracing::{info, warn, error, debug, instrument};
use std::collections::{HashMap, HashSet};

/// Consecutive indexing passes a topic must be missing from metadata before its
/// WAL directory is reclaimed.
///
/// One pass is not enough: a restarting node finishes message-WAL recovery
/// before its metadata catalog is populated, so every live topic is briefly
/// missing and reclaiming on first sight deletes live data.
const ORPHAN_CONFIRM_PASSES: u32 = 3;
use std::path::PathBuf;
use serde::{Deserialize, Serialize};

/// Configuration for the WAL indexer
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalIndexerConfig {
    /// Interval between indexing runs (seconds)
    pub interval_secs: u64,

    /// Minimum segment age before indexing (seconds)
    /// Prevents indexing segments that might still be receiving writes
    pub min_segment_age_secs: u64,

    /// Maximum number of segments to index per run
    pub max_segments_per_run: usize,

    /// Pause between segments within one run, in milliseconds. Zero (the
    /// default) indexes a backlog back to back; constrained disks can trade
    /// drain speed for headroom with `CHRONIK_WAL_INDEXER_SEGMENT_PAUSE_MS`.
    /// Runs themselves are always separated by a full `interval_secs` of idle.
    #[serde(default)]
    pub segment_pause_ms: u64,

    /// Whether to delete WAL segments after successful indexing
    pub delete_after_index: bool,

    /// Object store configuration for uploading Tantivy indexes
    pub object_store: ObjectStoreConfig,

    /// Object-store key prefix for Tantivy index archives. A key, not a
    /// filesystem path: the object store adds its own root (a bucket, or the
    /// local segments directory). Archives are found through the segment
    /// index, which stores each full key, so changing it never orphans one.
    pub index_base_path: String,

    /// Enable parallel indexing (one task per topic-partition)
    pub parallel_indexing: bool,

    /// Maximum concurrent indexing tasks
    pub max_concurrent_tasks: usize,

    /// Path to segment index persistence file
    pub segment_index_path: Option<PathBuf>,

    /// Enable auto-save for segment index
    pub segment_index_auto_save: bool,

    /// v2.2.16: Force seal segments that have been idle longer than this (seconds)
    /// This ensures low-volume topics still get indexed even if they don't
    /// produce enough data to trigger size-based segment rotation.
    /// Default: 30 seconds (same as indexer interval)
    pub stale_segment_seal_secs: u64,

    /// v2.2.21: Base path for Parquet files (columnar storage)
    /// Topics with columnar.enabled=true will write Parquet files here.
    pub columnar_base_path: String,

    /// v2.2.23: Enable object storage for Parquet files (S3/GCS/Azure)
    /// When true, Parquet files are uploaded to object storage instead of (or in addition to) local disk.
    /// Default: false (local-first)
    pub columnar_use_object_store: bool,

    /// v2.2.23: Object store configuration for Parquet files
    /// Only used when columnar_use_object_store is true.
    /// Separate from the Tantivy object_store config to allow different buckets/prefixes.
    pub columnar_object_store: Option<ObjectStoreConfig>,

    /// v2.2.23: Prefix for Parquet files in object storage
    /// Default: "columnar"
    pub columnar_s3_prefix: String,

    /// v2.2.23: Whether to keep local copy of Parquet files when uploading to object storage
    /// Default: true (keep local for faster queries)
    pub columnar_keep_local: bool,

    /// v2.2.22: Base path for vector indexes (HNSW storage)
    /// Topics with vector.enabled=true will store embeddings here.
    pub vector_base_path: String,

    /// v2.2.22: Interval for vector index snapshots (seconds)
    /// 0 to disable periodic snapshots. Default: 300 (5 minutes)
    pub vector_snapshot_interval_secs: u64,
}

impl WalIndexerConfig {
    /// Get the snapshot interval for vector indexes
    pub fn snapshot_interval_secs(&self) -> u64 {
        self.vector_snapshot_interval_secs
    }
}

impl Default for WalIndexerConfig {
    fn default() -> Self {
        Self {
            interval_secs: 30,
            min_segment_age_secs: 10,
            max_segments_per_run: 100,
            segment_pause_ms: 0,
            delete_after_index: true,
            object_store: ObjectStoreConfig::default(),
            index_base_path: "tantivy_indexes".to_string(),
            parallel_indexing: true,
            max_concurrent_tasks: 4,
            segment_index_path: Some(PathBuf::from("./data/segment_index.json")),
            segment_index_auto_save: true,
            stale_segment_seal_secs: 30, // v2.2.16: Seal stale segments after 30s
            columnar_base_path: "./data/columnar".to_string(), // v2.2.21: Parquet files
            columnar_use_object_store: false, // v2.2.23: Local-first by default
            columnar_object_store: None, // v2.2.23: Configured via env vars when enabled
            columnar_s3_prefix: "columnar".to_string(), // v2.2.23: Default prefix
            columnar_keep_local: true, // v2.2.23: Keep local copy for fast queries
            vector_base_path: "./data/vectors".to_string(), // v2.2.22: HNSW indexes
            vector_snapshot_interval_secs: 300, // v2.2.22: Snapshot every 5 minutes
        }
    }
}

/// Backoff state for a segment whose indexing pass failed.
///
/// See `WalIndexer::segment_failures` for why this exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentRetry {
    /// Consecutive failed passes.
    pub attempts: u32,
    /// Passes still to skip before trying again. Decremented once per run, so
    /// the backoff is measured in indexing cycles rather than wall-clock —
    /// the indexer only wakes on its own interval, so a deadline in seconds
    /// would round to the same thing while being harder to test.
    pub skip_passes: u32,
}

/// Failed passes after which a segment stops being retried automatically.
///
/// Reached only by a segment that fails for a reason that re-running cannot fix.
/// Its WAL copy is kept (nothing here deletes data), so the records are still
/// served from the WAL and still replicated — what stops is the pointless
/// re-indexing. Restarting the broker clears this.
const MAX_SEGMENT_ATTEMPTS: u32 = 6;

/// Cap on how many passes a segment may be made to wait, so an exponential
/// backoff cannot park a recoverable segment for hours.
const MAX_SKIP_PASSES: u32 = 32;

/// How long to wait after `attempts` consecutive failures, in indexing passes:
/// 1, 2, 4, 8 … capped. Doubling is what turns a hot loop into a trickle; the
/// cap is what keeps a transient failure from being punished like a permanent
/// one.
fn backoff_passes(attempts: u32) -> u32 {
    1u32.checked_shl(attempts.saturating_sub(1))
        .unwrap_or(MAX_SKIP_PASSES)
        .min(MAX_SKIP_PASSES)
}

/// Statistics from an indexing run
#[derive(Debug, Default, Clone)]
pub struct IndexingStats {
    /// Number of WAL segments processed
    pub segments_processed: usize,

    /// Number of records indexed
    pub records_indexed: usize,

    /// Number of Tantivy indexes created
    pub indexes_created: usize,

    /// Number of errors encountered
    pub errors: usize,

    /// Total bytes read from WAL
    pub bytes_read: u64,

    /// Total bytes written to Tantivy
    pub bytes_written: u64,

    /// Duration of indexing run (milliseconds)
    pub duration_ms: u64,
}

/// Partitions whose unarchived backlog is pressing (their hot index is
/// filling, or produce to them is being held back), and the signal that wakes
/// the indexer for them. See [`request_indexing`].
struct IndexingPressure {
    partitions: std::sync::Mutex<HashSet<(String, i32)>>,
    wake: tokio::sync::Notify,
}

static PRESSURE: std::sync::LazyLock<IndexingPressure> = std::sync::LazyLock::new(|| IndexingPressure {
    partitions: std::sync::Mutex::new(HashSet::new()),
    wake: tokio::sync::Notify::new(),
});

/// Ask for `topic`/`partition` to be archived now rather than at the next
/// scheduled run: the indexer wakes (instead of finishing its idle interval)
/// and first seals the partition's active segment, so what it holds becomes
/// archivable without waiting for a size rotation or the idle seal. Cheap and
/// idempotent; requests made during a run are served by the next one.
///
/// Used when a burst outruns archiving: the hot index keeps unarchived records
/// only up to its hard cap, so the sooner the archive starts the fewer it has
/// to drop.
pub fn request_indexing(topic: &str, partition: i32) {
    let inserted = PRESSURE
        .partitions
        .lock()
        .map(|mut p| p.insert((topic.to_string(), partition)))
        .unwrap_or(false);
    if inserted {
        PRESSURE.wake.notify_one();
    }
}

fn take_indexing_requests() -> Vec<(String, i32)> {
    PRESSURE.partitions.lock().map(|mut p| p.drain().collect()).unwrap_or_default()
}

/// A Tantivy archive built into a local tar, not yet uploaded or registered.
/// The temporary directory holding it goes with it.
struct LocalArchive {
    _dir: tempfile::TempDir,
    archive_path: PathBuf,
    base_offset: i64,
    last_offset: i64,
    min_offset: i64,
    max_offset: i64,
    min_timestamp: i64,
    max_timestamp: i64,
    record_count: usize,
}

/// The CPU-bound part of indexing one sealed segment, done ahead of its turn:
/// the segment read and decoded, its raw-tier bytes encoded and (searchable
/// topics) its Tantivy archive built. Everything that publishes (uploads,
/// catalogue entries, cold-flush notifications, Parquet, WAL deletion) still
/// happens segment by segment in offset order, under the partition's archive
/// lock; the work is used only if no truncation ran since it was read.
struct PreparedSegment {
    /// The partition's truncation epoch when the segment was read.
    truncation_epoch: u64,
    tp_records: HashMap<TopicPartition, Vec<CanonicalRecord>>,
    records_indexed: usize,
    raw: HashMap<TopicPartition, Vec<u8>>,
    archives: HashMap<TopicPartition, LocalArchive>,
}


/// Topic-partition identifier
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TopicPartition {
    pub topic: String,
    pub partition: i32,
}

impl TopicPartition {
    pub fn new(topic: String, partition: i32) -> Self {
        Self { topic, partition }
    }
}

/// A dedicated tokio runtime whose `Drop` is non-blocking.
///
/// A plain `tokio::runtime::Runtime` blocks on drop while it joins its worker
/// threads, which panics ("Cannot drop a runtime in a context where blocking is
/// not allowed") if that drop happens inside another runtime's async context —
/// exactly what occurs when the server is torn down on SIGTERM and the last
/// `Arc<WalIndexer>` is dropped from within `#[tokio::main]`. `shutdown_background()`
/// tears the runtime down without blocking, so this is safe to drop anywhere.
struct BackgroundRuntime {
    inner: Option<tokio::runtime::Runtime>,
}

impl BackgroundRuntime {
    fn new(rt: tokio::runtime::Runtime) -> Self {
        Self { inner: Some(rt) }
    }

    fn handle(&self) -> &tokio::runtime::Handle {
        self.inner.as_ref().expect("runtime present until drop").handle()
    }

    fn spawn<F>(&self, future: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.handle().spawn(future)
    }
}

impl Drop for BackgroundRuntime {
    fn drop(&mut self) {
        if let Some(rt) = self.inner.take() {
            // Non-blocking: detaches worker threads instead of joining them, so
            // this never blocks and is safe inside an async context.
            rt.shutdown_background();
        }
    }
}

/// WAL Indexer - converts sealed WAL segments to Tantivy indexes
pub struct WalIndexer {
    /// Configuration
    config: WalIndexerConfig,

    /// WAL manager for reading sealed segments
    wal_manager: Arc<WalManager>,

    /// Object store for uploading indexes
    object_store: Arc<dyn ObjectStore>,

    /// Segment index registry
    segment_index: Arc<SegmentIndex>,

    /// Metadata store for registering segment metadata
    metadata_store: Arc<dyn MetadataStore>,

    /// Set of segments currently being indexed (to avoid duplicate work)
    indexing_in_progress: Arc<RwLock<HashSet<String>>>,

    /// Issue #19: segments already indexed in this process, with the size they
    /// had at the time (`segment_id → size_bytes`).
    ///
    /// Sealed segments are immutable, so re-reading one republishes byte-for-byte
    /// identical output: the same raw segment upload and the same Parquet file.
    /// Without this guard the indexer redoes every sealed segment on every run
    /// (every 30s, forever), which is O(all WAL data) of pointless S3 traffic and
    /// CPU. The size is part of the key so a segment that somehow grows is still
    /// re-indexed. In-memory only: after a restart each segment is indexed once
    /// more, which is harmless because the output is deterministic.
    indexed_segments: Arc<RwLock<HashMap<String, u64>>>,

    /// Segments whose last pass failed, with how many times and when they may be
    /// tried again (`segment_id → SegmentRetry`).
    ///
    /// Issue #57: a segment that fails is deliberately *not* recorded in
    /// `indexed_segments`, so it is picked up again on the next run — which is
    /// right for a transient failure and ruinous for a permanent one. One
    /// segment that could not build its Tantivy index was re-read, re-tokenized,
    /// re-uploaded and re-written every 30s indefinitely, and the CPU and I/O
    /// that burned was enough to make `acks=all` produce time out. Retrying is
    /// correct; retrying at full speed forever is what took the cluster down
    /// with it.
    ///
    /// In-memory only. A restart clears the backoff, which is the behaviour we
    /// want: a new binary deserves a fresh attempt at a segment the old one
    /// could not index.
    segment_failures: Arc<RwLock<HashMap<String, SegmentRetry>>>,

    /// Statistics from last indexing run
    last_stats: Arc<RwLock<IndexingStats>>,

    /// Whether the indexer is running
    running: Arc<RwLock<bool>>,

    /// CRITICAL v2.2.10: Dedicated tokio runtime for background indexing
    /// Prevents WalIndexer from starving the main runtime's accept loop under heavy load
    /// 2 worker threads are sufficient for sequential segment processing
    runtime: Arc<BackgroundRuntime>,

    /// v2.2.16: Cache of searchable topics (refreshed periodically)
    /// Topics with config["searchable"] = "true" or CHRONIK_DEFAULT_SEARCHABLE=true
    searchable_topics: Arc<RwLock<HashSet<String>>>,

    /// v2.2.21: Cache of columnar-enabled topics (refreshed periodically)
    /// Topics with config["columnar.enabled"] = "true" or CHRONIK_DEFAULT_COLUMNAR=true
    columnar_topics: Arc<RwLock<HashSet<String>>>,

    /// v2.2.22: Cache of vector-enabled topics (refreshed periodically)
    /// Topics with config["vector.enabled"] = "true" for embedding generation
    vector_topics: Arc<RwLock<HashSet<String>>>,

    /// v2.2.22: Vector index manager for HNSW indexes per topic-partition
    /// Used by the embedding pipeline to store and search embeddings
    vector_index_manager: Arc<VectorIndexManager>,

    /// Leadership flag — shared with RaftCluster's cached_is_leader.
    /// On followers, segment metadata persistence is skipped to avoid deadlock
    /// (writing to __chronik_metadata with acks=1 blocks forever on non-leaders).
    /// Standalone/single-node: always true. Updated atomically by Raft on failover.
    is_leader: Arc<AtomicBool>,

    /// v2.4.1: Optional HotDataBuffer reference for set_flushed_offset() callback.
    /// After Parquet files are created, we notify the hot buffer so it stops reading
    /// already-persisted records from WAL, reducing memory usage.
    hot_buffer: Arc<RwLock<Option<Arc<HotDataBuffer>>>>,

    /// RP-1.1: Optional follower-progress source for the retention interlock.
    /// When present, a WAL segment holding records no live follower has yet
    /// acknowledged is kept rather than deleted after indexing. Decoupled via a
    /// trait so chronik-storage need not depend on the server's IsrTracker.
    replication_progress: Arc<RwLock<Option<Arc<dyn ReplicationProgress>>>>,

    /// How many consecutive passes each topic has looked absent from metadata.
    ///
    /// Reclaiming a topic's WAL directory the first time it cannot be found in
    /// the catalog is unsafe on startup: message-WAL recovery completes before
    /// the metadata catalog is populated, so a perfectly live topic looks
    /// orphaned for a moment. Measured on a restarting node — 2ms after
    /// `WAL recovery complete - 1 partitions loaded`:
    ///
    /// ```text
    /// WalIndexer: topic absent from metadata (orphaned) — reclaiming WAL storage topic=…
    /// Topic '…' cleanup: removed 0 partition queues, 1 sealed segments, wal_dir=true
    /// ```
    ///
    /// That deleted 140 live records. They came back only because two other
    /// replicas still had them; on a single node, or if the timing had caught
    /// every replica, they would simply be gone.
    absent_topic_passes: Arc<RwLock<HashMap<String, u32>>>,

    /// HP-1.4/HP-2.6: Listeners notified after each successful cold Tantivy
    /// commit. Each listener evicts its in-memory view of offsets that are
    /// now in cold storage. Multiple listeners are supported so both the hot
    /// text and hot vector indexes can plug in.
    ///
    /// Decoupled via a trait to avoid chronik-storage ↔ chronik-search /
    /// chronik-columnar circular dependencies.
    cold_flush_listener: Arc<RwLock<Vec<Arc<dyn ColdFlushListener>>>>,

    /// HP-2 follow-up A: Optional hot vector index for single-embed reuse.
    /// When set, the cold embedding pipeline checks this cache before calling
    /// the embedding provider, saving API calls for offsets the hot path
    /// already embedded.
    hot_vector_index: Arc<RwLock<Option<Arc<chronik_columnar::hot_vector_index::HotVectorIndex>>>>,
}

/// HP-1.4: Trait implemented by hot-index shadows (text, vector) that need
/// to be notified when an offset is safely persisted to cold storage.
///
/// Implementations must return quickly and must not block. A typical impl
/// spawns a detached task to perform eviction asynchronously.
pub trait ColdFlushListener: Send + Sync + 'static {
    /// Called after the cold Tantivy index for `(topic, partition)` has been
    /// sealed with records up to `max_offset`. The listener is responsible
    /// for evicting its in-memory view of offsets ≤ `max_offset`.
    fn notify_cold_flushed(&self, topic: String, partition: i32, max_offset: i64);
}

/// RP-1.1: Source of follower replication progress, for the retention interlock.
///
/// Indexing a WAL segment does not mean its records reached the replicas. Nothing
/// in the system re-sends a record a follower missed, so deleting WAL that has
/// not yet been replicated strands that data on one node permanently — the same
/// class of loss as deleting WAL whose object-store upload failed (v2.10.10),
/// reached by a different route.
pub trait ReplicationProgress: Send + Sync + 'static {
    /// Lowest offset acknowledged by every follower still keeping up with
    /// `(topic, partition)`.
    ///
    /// `None` means no replication is in play — single node, or nothing
    /// acknowledged yet — and callers must apply no interlock at all, so
    /// single-node deployments behave exactly as before.
    ///
    /// Followers that have fallen too far behind are expected to be excluded by
    /// the implementation: they must resync rather than pin WAL forever, or one
    /// dead node would fill the disk.
    fn min_replicated_offset(&self, topic: &str, partition: i32) -> Option<i64>;
}

impl WalIndexer {
    /// Create a new WAL indexer
    ///
    /// `is_leader` is a shared atomic flag from RaftCluster indicating whether
    /// this node is the current leader. Followers skip segment metadata persistence
    /// to avoid deadlocking on __chronik_metadata writes. Pass `None` for standalone
    /// mode (defaults to always-true).
    pub fn new(
        config: WalIndexerConfig,
        wal_manager: Arc<WalManager>,
        object_store: Arc<dyn ObjectStore>,
        metadata_store: Arc<dyn MetadataStore>,
        is_leader: Option<Arc<AtomicBool>>,
    ) -> Self {
        // Create segment index with persistence
        let segment_index = if let Some(ref path) = config.segment_index_path {
            Arc::new(SegmentIndex::with_persistence(
                path.clone(),
                config.segment_index_auto_save,
            ))
        } else {
            Arc::new(SegmentIndex::new())
        };

        // CRITICAL v2.2.10: Create dedicated runtime for WalIndexer background task
        // This prevents indexing from starving the main runtime's accept loop
        // Pattern: High-performance servers use separate runtimes for background work
        // - Main runtime: Accept loop + request handlers (latency-critical)
        // - Background runtime: Indexing, compaction, cleanup (throughput-oriented)
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)  // 2 threads sufficient for sequential indexing
            .thread_name("wal-indexer")
            // Lower priority for every thread of this runtime, its blocking
            // pool included; threads they spawn (Tantivy writers) inherit it.
            // See crate::background.
            .on_thread_start(crate::background::lower_current_thread_priority)
            .enable_all()  // Enable time and I/O drivers
            .build()
            .expect("Failed to create WalIndexer dedicated runtime");

        info!("Created dedicated runtime for WalIndexer with 2 worker threads");

        // v2.2.22: Create vector index manager for HNSW indexes
        let vector_index_config = VectorIndexConfig {
            base_path: PathBuf::from(&config.vector_base_path),
            ..VectorIndexConfig::default()
        };
        let vector_index_manager = Arc::new(VectorIndexManager::new(vector_index_config));
        info!("Created VectorIndexManager at {}", config.vector_base_path);

        Self {
            config,
            wal_manager,
            object_store,
            segment_index,
            metadata_store,
            indexing_in_progress: Arc::new(RwLock::new(HashSet::new())),
            indexed_segments: Arc::new(RwLock::new(HashMap::new())),
            segment_failures: Arc::new(RwLock::new(HashMap::new())),
            last_stats: Arc::new(RwLock::new(IndexingStats::default())),
            running: Arc::new(RwLock::new(false)),
            runtime: Arc::new(BackgroundRuntime::new(runtime)),
            searchable_topics: Arc::new(RwLock::new(HashSet::new())),
            columnar_topics: Arc::new(RwLock::new(HashSet::new())),
            vector_topics: Arc::new(RwLock::new(HashSet::new())),
            vector_index_manager,
            is_leader: is_leader.unwrap_or_else(|| Arc::new(AtomicBool::new(true))),
            hot_buffer: Arc::new(RwLock::new(None)),
            replication_progress: Arc::new(RwLock::new(None)),
            absent_topic_passes: Arc::new(RwLock::new(HashMap::new())),
            cold_flush_listener: Arc::new(RwLock::new(Vec::new())),
            hot_vector_index: Arc::new(RwLock::new(None)),
        }
    }

    /// HP-2 follow-up A: Attach the hot vector index so the cold embedding
    /// pipeline can reuse already-embedded vectors.
    pub async fn set_hot_vector_index(
        &self,
        index: Arc<chronik_columnar::hot_vector_index::HotVectorIndex>,
    ) {
        *self.hot_vector_index.write().await = Some(index);
        info!("HotVectorIndex wired to WalIndexer for single-embed reuse");
    }

    /// v2.4.1: Set the HotDataBuffer reference for flushed offset notifications.
    /// Called from main.rs after both WalIndexer and HotDataBuffer are created.
    pub async fn set_hot_buffer(&self, buffer: Arc<HotDataBuffer>) {
        // #41: seed the buffer's per-partition flushed offset from what cold
        // already holds, BEFORE it serves anything. `flushed_offsets` lives only
        // in memory, so after a restart it starts at 0 and the buffer — rebuilt
        // from the WAL trailing window — re-serves offsets already flushed to
        // cold, which `hot UNION ALL cold` then double-counts. An actively
        // produced topic self-corrects on its next flush, but a static one never
        // does. Seeding from the durable cold high-water mark restores the
        // invariant (hot serves only what cold does not) immediately.
        self.seed_hot_flushed_offsets(&buffer).await;
        *self.hot_buffer.write().await = Some(buffer);
        info!("HotDataBuffer wired to WalIndexer for flushed offset notifications");
    }

    /// Seed a hot buffer's per-partition `flushed_offset` from the cold Parquet
    /// high-water mark, so it never re-serves already-flushed offsets after a
    /// restart. Advances only; never lowers a watermark the buffer already has.
    async fn seed_hot_flushed_offsets(&self, buffer: &Arc<HotDataBuffer>) {
        let topics = match self.metadata_store.list_topics().await {
            Ok(topics) => topics,
            Err(e) => {
                debug!(error = %e, "Could not list topics to seed hot flushed offsets");
                return;
            }
        };
        let mut seeded = 0usize;
        for topic in &topics {
            let segments = match self
                .metadata_store
                .list_parquet_segments(&topic.name, None)
                .await
            {
                Ok(segments) => segments,
                Err(_) => continue,
            };
            let mut cold_max: HashMap<i32, i64> = HashMap::new();
            for seg in segments {
                let entry = cold_max.entry(seg.partition).or_insert(i64::MIN);
                if seg.max_offset > *entry {
                    *entry = seg.max_offset;
                }
            }
            for (partition, max) in cold_max {
                if buffer.get_flushed_offset(&topic.name, partition) < max {
                    buffer.set_flushed_offset(&topic.name, partition, max);
                    seeded += 1;
                }
            }
        }
        if seeded > 0 {
            info!(
                partitions = seeded,
                "Seeded hot buffer flushed offsets from cold on startup (#41)"
            );
        }
    }

    /// HP-1.4/HP-2.6: Attach a cold-flush listener. Multiple calls append
    /// listeners — they all fire (in order) after every successful cold
    /// Tantivy commit.
    pub async fn set_cold_flush_listener(&self, listener: Arc<dyn ColdFlushListener>) {
        self.cold_flush_listener.write().await.push(listener);
        info!("ColdFlushListener added to WalIndexer");
    }

    /// v2.2.16: Refresh the searchable topics cache from metadata store
    pub async fn refresh_searchable_topics(&self) -> Result<()> {
        let topics = self.metadata_store.list_topics().await
            .map_err(|e| Error::Internal(format!("Failed to list topics: {}", e)))?;

        let mut searchable = HashSet::new();
        for topic in topics {
            if topic.config.is_searchable() {
                info!(topic = %topic.name, "Topic marked as searchable");
                searchable.insert(topic.name.clone());
            }
        }

        let count = searchable.len();
        *self.searchable_topics.write().await = searchable;
        info!(count, "Refreshed searchable topics cache");
        Ok(())
    }

    /// v2.2.16: Check if a topic should be indexed for search
    pub async fn is_topic_searchable(&self, topic: &str) -> bool {
        self.searchable_topics.read().await.contains(topic)
    }

    /// v2.2.16: Register a topic as searchable (called when topic created)
    pub async fn register_searchable_topic(&self, topic: &str) {
        info!(topic, "Registering searchable topic");
        self.searchable_topics.write().await.insert(topic.to_string());
    }

    /// v2.2.16: Unregister a topic as searchable (called when topic deleted or config changed)
    pub async fn unregister_searchable_topic(&self, topic: &str) {
        info!(topic, "Unregistering searchable topic");
        self.searchable_topics.write().await.remove(topic);
    }

    /// v2.2.21: Refresh the columnar topics cache from metadata store
    pub async fn refresh_columnar_topics(&self) -> Result<()> {
        let topics = self.metadata_store.list_topics().await
            .map_err(|e| Error::Internal(format!("Failed to list topics: {}", e)))?;

        let mut columnar = HashSet::new();
        for topic in topics {
            if topic.config.is_columnar_enabled() {
                info!(topic = %topic.name, "Topic marked as columnar-enabled");
                columnar.insert(topic.name.clone());
            }
        }

        let count = columnar.len();
        *self.columnar_topics.write().await = columnar;
        info!(count, "Refreshed columnar topics cache");
        Ok(())
    }

    /// v2.2.21: Check if a topic should have columnar (Parquet) storage
    pub async fn is_topic_columnar(&self, topic: &str) -> bool {
        self.columnar_topics.read().await.contains(topic)
    }

    /// v2.2.21: Register a topic as columnar-enabled (called when topic created)
    pub async fn register_columnar_topic(&self, topic: &str) {
        info!(topic, "Registering columnar topic");
        self.columnar_topics.write().await.insert(topic.to_string());
    }

    /// v2.2.21: Unregister a topic as columnar-enabled (called when topic deleted or config changed)
    pub async fn unregister_columnar_topic(&self, topic: &str) {
        info!(topic, "Unregistering columnar topic");
        self.columnar_topics.write().await.remove(topic);
    }

    /// v2.2.22: Refresh the vector topics cache from metadata store
    pub async fn refresh_vector_topics(&self) -> Result<()> {
        let topics = self.metadata_store.list_topics().await
            .map_err(|e| Error::Internal(format!("Failed to list topics: {}", e)))?;

        let mut vector = HashSet::new();
        for topic in topics {
            if topic.config.is_vector_enabled() {
                info!(topic = %topic.name, "Topic marked as vector-enabled");
                vector.insert(topic.name.clone());
            }
        }

        let count = vector.len();
        *self.vector_topics.write().await = vector;
        info!(count, "Refreshed vector topics cache");
        Ok(())
    }

    /// v2.2.22: Check if a topic should have vector embeddings generated
    pub async fn is_topic_vector_enabled(&self, topic: &str) -> bool {
        self.vector_topics.read().await.contains(topic)
    }

    /// v2.2.22: Register a topic as vector-enabled (called when topic created)
    pub async fn register_vector_topic(&self, topic: &str) {
        info!(topic, "Registering vector topic");
        self.vector_topics.write().await.insert(topic.to_string());
    }

    /// v2.2.22: Unregister a topic as vector-enabled (called when topic deleted or config changed)
    pub async fn unregister_vector_topic(&self, topic: &str) {
        info!(topic, "Unregistering vector topic");
        self.vector_topics.write().await.remove(topic);
    }

    /// Get reference to segment index
    pub fn segment_index(&self) -> &Arc<SegmentIndex> {
        &self.segment_index
    }

    /// Archive `batches` of `topic`/`partition` as one Tantivy segment —
    /// uploaded with its term filter and registered, exactly as the indexer
    /// archives the WAL. For records the indexer missed (re-archived from the
    /// log); the batches must hold only offsets no archive covers yet.
    /// Returns (bytes written, highest offset archived).
    /// Build `batches` into one archive and upload it, unregistered (see
    /// `build_archive`): for a reindex, which swaps it in for the archive it
    /// rebuilds.
    pub async fn build_unregistered_archive(
        &self,
        topic: &str,
        partition: i32,
        batches: Vec<CanonicalRecord>,
        key_suffix: &str,
    ) -> Result<SegmentIndexMetadata> {
        let tp = TopicPartition::new(topic.to_string(), partition);
        Self::build_archive(&self.config, &self.object_store, &tp, batches, Some(key_suffix)).await
    }

    pub async fn archive_batches(
        &self,
        topic: &str,
        partition: i32,
        batches: Vec<CanonicalRecord>,
    ) -> Result<(u64, i64)> {
        let tp = TopicPartition::new(topic.to_string(), partition);
        Self::create_tantivy_index(&self.config, &self.object_store, &self.segment_index, &tp, batches, None).await
    }

    /// Object store holding this node's cold Tantivy/Parquet segments. Exposed
    /// so `/_search` can download+open a topic's cold Tantivy segments (the
    /// WalIndexer writes them here via `object_store.put`; nothing else can
    /// resolve the real physical/S3 location of those archives).
    pub fn object_store(&self) -> Arc<dyn ObjectStore> {
        Arc::clone(&self.object_store)
    }

    /// v2.2.22: Get reference to vector index manager
    pub fn vector_index_manager(&self) -> &Arc<VectorIndexManager> {
        &self.vector_index_manager
    }

    /// v2.2.23: Get reference to WAL manager (for hot buffer)
    pub fn wal_manager(&self) -> &Arc<WalManager> {
        &self.wal_manager
    }

    /// Start the background indexing task
    ///
    /// CRITICAL v2.4.0: This method returns immediately and does NOT block on
    /// loading segment indexes or vector indexes from disk. All heavy I/O
    /// (segment index deserialization, HNSW index rebuild) is deferred to the
    /// background task's warm-up phase. This ensures the server can bind to
    /// Kafka and HTTP ports within seconds, even with hundreds of thousands of
    /// messages in the WAL. Without this, liveness probes in k8s kill the pod
    /// before it finishes loading large HNSW indexes.
    #[instrument(skip(self))]
    pub async fn start(&self) -> Result<()> {
        let mut running = self.running.write().await;
        if *running {
            warn!("WAL indexer already running");
            return Ok(());
        }
        *running = true;
        drop(running);

        info!(
            interval_secs = self.config.interval_secs,
            "Starting WAL indexer background task on dedicated runtime \
             (index loading deferred to background warm-up)"
        );

        let config = self.config.clone();
        let wal_manager = Arc::clone(&self.wal_manager);
        let object_store = Arc::clone(&self.object_store);
        let segment_index = Arc::clone(&self.segment_index);
        let metadata_store = Arc::clone(&self.metadata_store);
        let indexing_in_progress = Arc::clone(&self.indexing_in_progress);
        let indexed_segments = Arc::clone(&self.indexed_segments);
        let segment_failures = Arc::clone(&self.segment_failures);
        let last_stats = Arc::clone(&self.last_stats);
        let running = Arc::clone(&self.running);
        let runtime = Arc::clone(&self.runtime);
        let vector_index_manager = Arc::clone(&self.vector_index_manager);
        let is_leader = Arc::clone(&self.is_leader);
        let hot_buffer = Arc::clone(&self.hot_buffer);
        let replication_progress = Arc::clone(&self.replication_progress);
        let absent_topic_passes = Arc::clone(&self.absent_topic_passes);
        let cold_flush_listener = Arc::clone(&self.cold_flush_listener);
        let hot_vector_index = Arc::clone(&self.hot_vector_index);
        let snapshot_interval = self.config.snapshot_interval_secs();

        // CRITICAL v2.2.10: Spawn on dedicated runtime, NOT main runtime
        // This guarantees accept loop can never be starved by indexing work
        //
        // v2.4.0: Segment index + vector index loading moved INTO this spawn
        // block so server startup is never blocked by disk I/O or HNSW rebuild
        runtime.spawn(async move {
            // === Warm-up phase: load persisted state from disk ===
            // This runs in the background while the server is already accepting
            // Kafka connections and HTTP requests on port 6092.
            let warmup_start = std::time::Instant::now();
            info!("WAL indexer warm-up: loading persisted indexes from disk...");

            // Load segment index from disk if persistence is configured
            if let Err(e) = segment_index.load().await {
                warn!(error = %e, "Failed to load segment index, starting with empty index");
            }

            // v2.2.22: Load vector indexes from disk (includes HNSW rebuild)
            match vector_index_manager.load_all_indexes().await {
                Ok(count) => {
                    if count > 0 {
                        info!(loaded = count, "Loaded vector indexes from disk");
                    }
                }
                Err(e) => {
                    warn!(error = %e, "Failed to load vector indexes, starting with empty indexes");
                }
            }

            // v2.2.22: Start vector index snapshot task
            if snapshot_interval > 0 {
                vector_index_manager.start_snapshot_task(snapshot_interval);
            }

            let warmup_elapsed = warmup_start.elapsed();
            info!(
                duration_ms = warmup_elapsed.as_millis() as u64,
                "WAL indexer warm-up complete, entering periodic indexing loop"
            );

            // === Periodic indexing loop ===
            //
            // A full interval of idle AFTER each run, not a fixed-rate tick.
            //
            // This was `interval().tick()`, whose default missed-tick behaviour
            // is Burst: a run that outlasts the period (a backlog of 100
            // segments takes 15–75 s against a 30 s period) leaves ticks owed,
            // and they fire immediately, one after another. So exactly when a
            // node had the most to catch up on, the indexer ran back to back
            // with no gap at all — reported on a homelab cluster draining a
            // 587 MB backlog after an upgrade, where it held the disk long
            // enough to starve Raft. Sleeping after the run guarantees the rest
            // of the node `interval_secs` of headroom between runs however long
            // a run takes; in steady state a run lasts milliseconds, so
            // freshness is unchanged.
            let idle_between_runs = Duration::from_secs(config.interval_secs);
            let mut first_run = true;

            loop {
                if !first_run {
                    // A partition under pressure (request_indexing) cuts the
                    // idle short: records it has not archived are about to
                    // become unsearchable, which outweighs the headroom.
                    tokio::select! {
                        _ = tokio::time::sleep(idle_between_runs) => {}
                        _ = PRESSURE.wake.notified() => {}
                    }
                }
                first_run = false;

                // Check if still running
                let is_running = *running.read().await;
                if !is_running {
                    info!("WAL indexer stopped");
                    break;
                }

                // Run indexing
                debug!("WAL indexer tick - checking for sealed segments");

                match Self::index_sealed_segments_internal(
                    &config,
                    &wal_manager,
                    &object_store,
                    &segment_index,
                    &metadata_store,
                    &indexing_in_progress,
                    &indexed_segments,
                    &segment_failures,
                    &vector_index_manager,
                    &is_leader,
                    &hot_buffer,
                    &replication_progress,
                    &absent_topic_passes,
                    &cold_flush_listener,
                    &hot_vector_index,
                ).await {
                    Ok(stats) => {
                        if stats.segments_processed > 0 {
                            info!(
                                segments = stats.segments_processed,
                                records = stats.records_indexed,
                                indexes = stats.indexes_created,
                                errors = stats.errors,
                                duration_ms = stats.duration_ms,
                                "WAL indexing run complete"
                            );
                        }
                        *last_stats.write().await = stats;
                    }
                    Err(e) => {
                        error!(error = %e, "WAL indexing run failed");
                    }
                }
            }
        });

        Ok(())
    }

    /// Stop the background indexing task
    pub async fn stop(&self) {
        info!("Stopping WAL indexer");
        *self.running.write().await = false;
    }

    /// Get statistics from last indexing run
    pub async fn get_stats(&self) -> IndexingStats {
        self.last_stats.read().await.clone()
    }

    /// Check if indexer is running
    pub async fn is_running(&self) -> bool {
        *self.running.read().await
    }

    /// Run one indexing cycle immediately (used during shutdown)
    pub async fn run_once(&self) -> Result<IndexingStats> {
        info!("Running WAL indexer once (on-demand)");
        Self::index_sealed_segments_internal(
            &self.config,
            &self.wal_manager,
            &self.object_store,
            &self.segment_index,
            &self.metadata_store,
            &self.indexing_in_progress,
            &self.indexed_segments,
            &self.segment_failures,
            &self.vector_index_manager,
            &self.is_leader,
            &self.hot_buffer,
            &self.replication_progress,
            &self.absent_topic_passes,
            &self.cold_flush_listener,
            &self.hot_vector_index,
        ).await
    }

    /// Index sealed segments (internal implementation)
    #[instrument(skip(config, wal_manager, object_store, segment_index, metadata_store, indexing_in_progress, indexed_segments, segment_failures, vector_index_manager, is_leader, hot_buffer, replication_progress, cold_flush_listener, hot_vector_index))]
    #[allow(clippy::too_many_arguments)] // the indexer's state, passed to a free-standing pass
    async fn index_sealed_segments_internal(
        config: &WalIndexerConfig,
        wal_manager: &Arc<WalManager>,
        object_store: &Arc<dyn ObjectStore>,
        segment_index: &Arc<SegmentIndex>,
        metadata_store: &Arc<dyn MetadataStore>,
        indexing_in_progress: &Arc<RwLock<HashSet<String>>>,
        indexed_segments: &Arc<RwLock<HashMap<String, u64>>>,
        segment_failures: &Arc<RwLock<HashMap<String, SegmentRetry>>>,
        vector_index_manager: &Arc<VectorIndexManager>,
        is_leader: &Arc<AtomicBool>,
        hot_buffer: &Arc<RwLock<Option<Arc<HotDataBuffer>>>>,
        replication_progress: &Arc<RwLock<Option<Arc<dyn ReplicationProgress>>>>,
        absent_topic_passes: &Arc<RwLock<HashMap<String, u32>>>,
        cold_flush_listener: &Arc<RwLock<Vec<Arc<dyn ColdFlushListener>>>>,
        hot_vector_index: &Arc<RwLock<Option<Arc<chronik_columnar::hot_vector_index::HotVectorIndex>>>>,
    ) -> Result<IndexingStats> {
        let start_time = std::time::Instant::now();
        let mut stats = IndexingStats::default();

        // v2.2.16: Force-seal stale segments before checking for sealed segments
        // This ensures low-volume topics get indexed even if they don't produce
        // enough data to trigger size-based segment rotation.
        if config.stale_segment_seal_secs > 0 {
            let sealed_count = wal_manager.seal_stale_segments(config.stale_segment_seal_secs).await;
            if sealed_count > 0 {
                debug!("Sealed {} stale segments before indexing", sealed_count);
            }
        }
        // Partitions under pressure: seal what they hold now (see request_indexing).
        for (topic, partition) in take_indexing_requests() {
            match wal_manager.group_commit_wal().seal_partition(&topic, partition).await {
                Ok(_) => debug!(topic = %topic, partition, "Sealed a partition under indexing pressure"),
                Err(e) => warn!(topic = %topic, partition, "Could not seal a partition under indexing pressure: {}", e),
            }
        }

        // Get list of sealed segments from WAL manager (v1.3.47+: direct call)
        let sealed_segments = wal_manager.get_sealed_segments_with_size();

        if sealed_segments.is_empty() {
            debug!("No sealed segments to index");
            return Ok(stats);
        }

        // Filter out segments already being indexed, and those already indexed
        // at this exact size — a sealed segment is immutable, so re-reading it
        // would republish byte-identical output (issue #19).
        let mut segments_to_index = Vec::new();
        let mut segment_sizes: HashMap<String, u64> = HashMap::new();
        let mut already_done = 0usize;
        {
            let in_progress = indexing_in_progress.read().await;
            let done = indexed_segments.read().await;
            for (segment, size) in sealed_segments {
                if in_progress.contains(&segment) {
                    continue;
                }
                if done.get(&segment) == Some(&size) {
                    already_done += 1;
                    continue;
                }
                segment_sizes.insert(segment.clone(), size);
                segments_to_index.push(segment);
            }
        }

        // Hold back segments that are serving a backoff, and drop the ones that
        // have exhausted their attempts (issue #57). Without this a segment that
        // cannot be indexed is retried at full speed every pass forever, and the
        // I/O that costs is enough to time out `acks=all` produce on the whole
        // broker.
        let (held_back, quarantined) = {
            let mut failures = segment_failures.write().await;
            let before = segments_to_index.len();
            let mut quarantined = 0usize;

            segments_to_index.retain(|segment| match failures.get_mut(segment) {
                None => true,
                Some(retry) if retry.attempts >= MAX_SEGMENT_ATTEMPTS => {
                    quarantined += 1;
                    false
                }
                Some(retry) if retry.skip_passes > 0 => {
                    retry.skip_passes -= 1;
                    false
                }
                Some(_) => true,
            });

            (before - segments_to_index.len() - quarantined, quarantined)
        };

        if held_back > 0 || quarantined > 0 {
            warn!(
                backing_off = held_back,
                quarantined,
                max_attempts = MAX_SEGMENT_ATTEMPTS,
                "Some WAL segments were not indexed this pass (previous failures). Their WAL \
                 copies are kept, so their records are still served and replicated; a restart \
                 retries them."
            );
        }

        if segments_to_index.is_empty() {
            debug!(
                already_indexed = already_done,
                "No new sealed segments to index"
            );
            return Ok(stats);
        }

        info!(
            count = segments_to_index.len(),
            already_indexed = already_done,
            "Found sealed WAL segments to index"
        );

        // Oldest first within each partition, then limit the run. The WAL lists
        // sealed segments in no particular order, so a backlog was indexed —
        // and cut to `max_segments_per_run` — in arbitrary order, and the hot
        // buffer's flushed offset (the contiguous run cold holds) jumped ahead
        // of segments still waiting.
        segments_to_index.sort_by_cached_key(|segment| segment_order_key(segment));
        segments_to_index.truncate(config.max_segments_per_run);

        if segments_to_index.is_empty() {
            debug!("All sealed segments already being indexed");
            return Ok(stats);
        }

        // Orphan reclamation: drop segments whose topic no longer exists in
        // metadata. DeleteTopics removes the topic from metadata and (v2.7.4+)
        // its WAL dir, but WAL recovery on restart re-registers any on-disk
        // segments that predate the fix — so the indexer would grind deleted
        // topics forever (this is what clogged the cluster after the
        // LongMemEval eval churned ~9K tenants). When we see an orphaned topic,
        // cleanup_topic() removes its whole WAL dir from disk (freeing storage)
        // and we skip its segments. Per-topic existence is cached for the run.
        {
            let mut checked: std::collections::HashMap<String, bool> = std::collections::HashMap::new();
            let mut orphan_topics: HashSet<String> = HashSet::new();
            for segment_id in &segments_to_index {
                let topic = segment_id.split(':').next().unwrap_or("").to_string();
                if topic.is_empty() {
                    continue;
                }
                let exists = match checked.get(&topic) {
                    Some(&e) => e,
                    None => {
                        let e = metadata_store
                            .get_topic(&topic)
                            .await
                            .ok()
                            .flatten()
                            .is_some();
                        checked.insert(topic.clone(), e);
                        e
                    }
                };
                if !exists {
                    orphan_topics.insert(topic);
                }
            }

            // Absent once is not absent. Message-WAL recovery finishes before
            // the metadata catalog is populated, so on a restarting node every
            // live topic is briefly missing from the catalog — and deleting its
            // WAL directory on that basis destroys live data. Require the topic
            // to be missing on `ORPHAN_CONFIRM_PASSES` consecutive passes, which
            // spans several indexing intervals; a genuinely deleted topic is
            // still reclaimed, just not within milliseconds of a restart.
            {
                let mut passes = absent_topic_passes.write().await;
                orphan_topics = Self::confirm_orphans(&mut passes, orphan_topics);
            }

            if !orphan_topics.is_empty() {
                for t in &orphan_topics {
                    warn!(topic = %t, passes = ORPHAN_CONFIRM_PASSES,
                        "WalIndexer: topic absent from metadata on every recent pass (orphaned) — reclaiming WAL storage");
                    wal_manager.cleanup_topic(t).await;
                }
                segments_to_index
                    .retain(|s| !orphan_topics.contains(s.split(':').next().unwrap_or("")));

                // Forget what we indexed for those topics. A topic recreated
                // under the same name restarts at segment 0, and a stale
                // `segment_id → size` entry that happened to match would make
                // us skip the new topic's first segment entirely.
                let mut done = indexed_segments.write().await;
                done.retain(|segment_id, _| {
                    !orphan_topics.contains(segment_id.split(':').next().unwrap_or(""))
                });
            }

            // Absent but not yet confirmed: leave it for a later pass rather
            // than archive it. Either the topic was just deleted — and its
            // deletion purges the archive, which this pass would refill — or
            // this node has not heard of it yet, and will index it with its
            // real configuration once it has. Not a failure: no backoff.
            segments_to_index.retain(|segment_id| {
                let topic = segment_id.split(':').next().unwrap_or("");
                checked.get(topic).copied().unwrap_or(true)
            });
        }
        if segments_to_index.is_empty() {
            debug!("All pending segments were orphaned (topics deleted) — reclaimed, nothing to index");
            return Ok(stats);
        }

        // Mark segments as being indexed
        {
            let mut in_progress = indexing_in_progress.write().await;
            for segment in &segments_to_index {
                in_progress.insert(segment.clone());
            }
        }

        // Process each segment, in order. With build concurrency above 1 the
        // next segments are prepared (read, encoded, archive built) on their
        // own tasks while earlier ones are published, so a partition with a
        // backlog archives several segments' worth of CPU at once instead of
        // one segment at a time.
        let window = Self::archive_build_concurrency(config);
        let mut ahead: std::collections::VecDeque<tokio::task::JoinHandle<Option<PreparedSegment>>> =
            std::collections::VecDeque::new();
        let mut next_to_prepare = 0;
        for (n, segment_id) in segments_to_index.iter().enumerate() {
            if window > 1 {
                while next_to_prepare < segments_to_index.len() && next_to_prepare < n + window {
                    ahead.push_back(tokio::spawn(Self::prepare_segment(
                        wal_manager.clone(),
                        metadata_store.clone(),
                        segments_to_index[next_to_prepare].clone(),
                    )));
                    next_to_prepare += 1;
                }
            }
            let prepared = match ahead.pop_front() {
                Some(handle) => handle.await.ok().flatten(),
                None => None,
            };
            if n > 0 && config.segment_pause_ms > 0 {
                tokio::time::sleep(Duration::from_millis(config.segment_pause_ms)).await;
            }
            let errors_before = stats.errors;
            match Self::index_segment(
                config,
                wal_manager,
                object_store,
                segment_index,
                metadata_store,
                vector_index_manager,
                segment_id,
                &mut stats,
                is_leader,
                hot_buffer,
                replication_progress,
                cold_flush_listener,
                hot_vector_index,
                prepared,
            ).await {
                Ok(_) => {
                    debug!(segment = %segment_id, "Successfully indexed segment");
                    // Record only fully clean passes: a segment whose upload or
                    // Parquet write failed must be retried on the next run.
                    if stats.errors == errors_before {
                        if let Some(size) = segment_sizes.get(segment_id) {
                            indexed_segments
                                .write()
                                .await
                                .insert(segment_id.clone(), *size);
                        }
                        // A clean pass clears the backoff. The next failure
                        // starts from one attempt again, so a segment that
                        // occasionally fails is never slowly starved.
                        segment_failures.write().await.remove(segment_id);
                    } else {
                        Self::record_segment_failure(segment_failures, segment_id).await;
                    }
                }
                Err(e) => {
                    error!(segment = %segment_id, error = %e, "Failed to index segment");
                    stats.errors += 1;
                    Self::record_segment_failure(segment_failures, segment_id).await;
                }
            }
        }

        // Remove from in-progress set
        {
            let mut in_progress = indexing_in_progress.write().await;
            for segment in &segments_to_index {
                in_progress.remove(segment);
            }
        }

        stats.duration_ms = start_time.elapsed().as_millis() as u64;
        Ok(stats)
    }

    /// Note a failed pass for `segment_id` and set how long it waits.
    ///
    /// The backoff doubles per consecutive failure and is announced at the point
    /// it starts mattering, so an operator reading the log sees the indexer
    /// deciding to slow down rather than a stream of identical errors.
    async fn record_segment_failure(
        segment_failures: &Arc<RwLock<HashMap<String, SegmentRetry>>>,
        segment_id: &str,
    ) {
        let mut failures = segment_failures.write().await;
        let entry = failures
            .entry(segment_id.to_string())
            .or_insert(SegmentRetry { attempts: 0, skip_passes: 0 });
        entry.attempts = entry.attempts.saturating_add(1);
        entry.skip_passes = backoff_passes(entry.attempts);

        if entry.attempts >= MAX_SEGMENT_ATTEMPTS {
            error!(
                segment = %segment_id,
                attempts = entry.attempts,
                "WAL segment has failed indexing on every attempt — quarantining it. Its WAL \
                 copy is KEPT, so no records are lost and consumers still read them; what stops \
                 is re-indexing it every pass. Search and SQL will not see this segment's range \
                 until the cause is fixed and the broker restarted."
            );
        } else {
            warn!(
                segment = %segment_id,
                attempts = entry.attempts,
                skip_passes = entry.skip_passes,
                "WAL segment failed indexing — backing off before the next attempt"
            );
        }
    }

    /// Whether `index_segment` may delete the source WAL segment after a pass.
    /// Only when deletion is enabled AND the pass recorded no new errors: a
    /// raw-segment upload failure bumps the error count and continues, so a
    /// dirty pass must KEEP the WAL copy (the caller also declines to mark such
    /// a segment "indexed", so the next run re-uploads it). Deleting on a dirty
    /// pass would lose the data from BOTH the WAL and the object store.
    fn may_delete_wal_segment(delete_after_index: bool, errors_at_start: usize, errors_now: usize) -> bool {
        delete_after_index && errors_now == errors_at_start
    }

    /// `(topic, partition)` of a WAL segment id `topic:partition:id`. Kafka topic
    /// names cannot contain `:`, so splitting from the right is exact.
    /// How many sealed segments are prepared at once
    /// (`CHRONIK_ARCHIVE_BUILD_CONCURRENCY`). Default: a quarter of the cores
    /// the process may use, at most `max_concurrent_tasks` and at least 1;
    /// each build is a Tantivy writer of up to 3 threads and 50 MB, plus the
    /// segment's records in memory.
    fn archive_build_concurrency(config: &WalIndexerConfig) -> usize {
        if let Some(n) = std::env::var("CHRONIK_ARCHIVE_BUILD_CONCURRENCY")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
        {
            return n.max(1);
        }
        let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
        (cores / 4).clamp(1, config.max_concurrent_tasks.max(1))
    }

    fn segment_partition(segment_id: &str) -> Option<(String, i32)> {
        let mut parts = segment_id.rsplitn(3, ':');
        let _id = parts.next()?;
        let partition = parts.next()?.parse::<i32>().ok()?;
        let topic = parts.next()?;
        (!topic.is_empty()).then(|| (topic.to_string(), partition))
    }

    /// The offset ranges this partition's cold Parquet segments cover, sorted
    /// and merged (touching ranges joined), or empty if it has none.
    ///
    /// This is the durable, restart-surviving record of what cold already
    /// holds, used to keep Parquet flushing idempotent (#41): a segment is
    /// never written for offsets a prior one covers. It used to be reduced to
    /// the highest offset, and everything at or below it was skipped — so when
    /// sealed segments were indexed out of order (a backlog, a segment held
    /// back by a failure backoff), the first high one written made every lower
    /// one look done, and their records never reached Parquet.
    async fn cold_ranges(
        metadata_store: &Arc<dyn MetadataStore>,
        tp: &TopicPartition,
    ) -> Result<Vec<(i64, i64)>> {
        let segments = metadata_store
            .list_parquet_segments(&tp.topic, Some(tp.partition))
            .await
            .map_err(|e| Error::Internal(format!("Failed to list Parquet segments: {}", e)))?;
        Ok(merge_ranges(segments.iter().map(|s| (s.min_offset, s.max_offset)).collect()))
    }

    /// Which of this pass's missing topics may actually have their WAL deleted.
    ///
    /// A topic must be missing from metadata on `ORPHAN_CONFIRM_PASSES`
    /// consecutive passes. Any topic that reappears has its count cleared, so
    /// the passes must be consecutive rather than cumulative.
    ///
    /// One pass is not enough because message-WAL recovery completes before the
    /// metadata catalog is populated: on a restarting node every live topic is
    /// briefly absent, and reclaiming on first sight deleted 140 live records
    /// two milliseconds after recovery reported them loaded.
    fn confirm_orphans(
        passes: &mut HashMap<String, u32>,
        candidates: HashSet<String>,
    ) -> HashSet<String> {
        // A topic present this time starts again from zero.
        passes.retain(|topic, _| candidates.contains(topic));

        let mut confirmed = HashSet::new();
        for topic in candidates {
            let seen = passes.entry(topic.clone()).or_insert(0);
            *seen += 1;
            if *seen >= ORPHAN_CONFIRM_PASSES {
                confirmed.insert(topic);
            } else {
                debug!(
                    topic = %topic, pass = *seen,
                    "Topic missing from metadata — waiting for confirmation before reclaiming its WAL"
                );
            }
        }
        confirmed
    }

    /// RP-1.1 retention interlock: whether every record in this pass has reached
    /// the followers.
    ///
    /// `tp_max_offsets` is the highest offset indexed per partition in this
    /// segment. A partition holds the segment back when a live follower has not
    /// acknowledged that far, because nothing re-sends what a follower missed —
    /// deleting here would strand the data on one node with no recovery path.
    ///
    /// With no progress source (single node, or replication not configured) this
    /// returns true and behaviour is unchanged.
    fn fully_replicated(
        progress: Option<&Arc<dyn ReplicationProgress>>,
        tp_max_offsets: &[(TopicPartition, i64)],
    ) -> Result<()> {
        let Some(progress) = progress else {
            return Ok(());
        };

        for (tp, max_offset) in tp_max_offsets {
            // None = no live follower tracked → no interlock for this partition.
            if let Some(acked) = progress.min_replicated_offset(&tp.topic, tp.partition) {
                if acked < *max_offset {
                    return Err(Error::Internal(format!(
                        "{}-{} replicated only to offset {} of {}",
                        tp.topic, tp.partition, acked, max_offset
                    )));
                }
            }
        }
        Ok(())
    }

    /// Attach a follower-progress source for the retention interlock (RP-1.1).
    pub async fn set_replication_progress(&self, progress: Arc<dyn ReplicationProgress>) {
        *self.replication_progress.write().await = Some(progress);
        info!("Replication progress wired to WalIndexer — WAL retention now waits for followers");
    }

    /// Group a segment's records by topic-partition; also returns how many
    /// records it holds. V1 records carry no topic-partition and are skipped
    /// (handled by the legacy path).
    fn decode_segment(records: Vec<WalRecord>) -> Result<(HashMap<TopicPartition, Vec<CanonicalRecord>>, usize)> {
        let mut tp_records: HashMap<TopicPartition, Vec<CanonicalRecord>> = HashMap::new();
        let mut count = 0;
        for record in records {
            match record {
                WalRecord::V1 { .. } => {
                    debug!("Skipping V1 record (legacy format)");
                }
                WalRecord::V2 { topic, partition, canonical_data, .. } => {
                    let canonical_record: CanonicalRecord = bincode::deserialize(&canonical_data)
                        .map_err(|e| Error::Internal(format!("Failed to deserialize CanonicalRecord: {}", e)))?;
                    count += canonical_record.records.len();
                    tp_records.entry(TopicPartition::new(topic, partition)).or_default().push(canonical_record);
                }
            }
        }
        Ok((tp_records, count))
    }

    /// The CPU-bound part of indexing `segment_id`, run ahead of its turn (see
    /// [`PreparedSegment`]). `None` when there is nothing worth preparing or
    /// anything fails: `index_segment` then does the work itself and reports
    /// what went wrong.
    async fn prepare_segment(
        wal_manager: Arc<WalManager>,
        metadata_store: Arc<dyn MetadataStore>,
        segment_id: String,
    ) -> Option<PreparedSegment> {
        let (topic, partition) = Self::segment_partition(&segment_id)?;
        // Sampled under the archive lock: a truncation holds it from before it
        // bumps the epoch until it is done, so the segment read below is not
        // one a truncation is halfway through.
        let truncation_epoch = {
            let lock = wal_manager.archive_lock(&topic, partition);
            let _guard = lock.lock().await;
            wal_manager.truncation_epoch(&topic, partition)
        };
        let started = std::time::Instant::now();
        let records = wal_manager.read_segment(&segment_id).await.ok()?;
        let read = started.elapsed();
        if records.is_empty() {
            return None;
        }
        let (tp_records, records_indexed) = Self::decode_segment(records).ok()?;
        let decoded = started.elapsed();
        let mut searchable = HashSet::new();
        for tp in tp_records.keys() {
            let config = metadata_store.get_topic(&tp.topic).await.ok().flatten().map(|t| t.config).unwrap_or_default();
            if config.is_searchable() {
                searchable.insert(tp.clone());
            }
        }
        // Encoding and indexing run on blocking threads (they inherit the
        // indexer's lowered priority), so several segments build at once.
        tokio::task::spawn_blocking(move || {
            let mut raw = HashMap::new();
            let mut archives = HashMap::new();
            for (tp, records) in &tp_records {
                if let Ok(bytes) = crate::archive::encode_raw_segment(records) {
                    raw.insert(tp.clone(), bytes);
                }
                if searchable.contains(tp) && !records.is_empty() {
                    match Self::build_archive_local(tp, records.clone()) {
                        Ok(local) => {
                            archives.insert(tp.clone(), local);
                        }
                        Err(e) => debug!(topic = %tp.topic, partition = tp.partition, "archive not prepared: {}", e),
                    }
                }
            }
            debug!(
                segment = %segment_id,
                read_ms = read.as_millis() as u64,
                decode_ms = (decoded - read).as_millis() as u64,
                build_ms = (started.elapsed() - decoded).as_millis() as u64,
                "Prepared segment"
            );
            PreparedSegment { truncation_epoch, tp_records, records_indexed, raw, archives }
        })
        .await
        .ok()
    }

    /// Index a single sealed WAL segment
    #[instrument(skip(config, wal_manager, object_store, segment_index, metadata_store, vector_index_manager, stats, is_leader, hot_buffer, replication_progress, cold_flush_listener, hot_vector_index, prepared))]
    #[allow(clippy::too_many_arguments)] // the indexer's state, passed to a free-standing pass
    async fn index_segment(
        config: &WalIndexerConfig,
        wal_manager: &Arc<WalManager>,
        object_store: &Arc<dyn ObjectStore>,
        segment_index: &Arc<SegmentIndex>,
        metadata_store: &Arc<dyn MetadataStore>,
        vector_index_manager: &Arc<VectorIndexManager>,
        segment_id: &str,
        stats: &mut IndexingStats,
        is_leader: &Arc<AtomicBool>,
        hot_buffer: &Arc<RwLock<Option<Arc<HotDataBuffer>>>>,
        replication_progress: &Arc<RwLock<Option<Arc<dyn ReplicationProgress>>>>,
        cold_flush_listener: &Arc<RwLock<Vec<Arc<dyn ColdFlushListener>>>>,
        hot_vector_index: &Arc<RwLock<Option<Arc<chronik_columnar::hot_vector_index::HotVectorIndex>>>>,
        prepared: Option<PreparedSegment>,
    ) -> Result<()> {
        info!(segment = %segment_id, "Indexing WAL segment");

        // Snapshot the error count so we can gate WAL-segment deletion on a
        // clean pass. A raw-segment upload failure only logs + bumps
        // `stats.errors` and continues (see STEP 1), so without this guard the
        // WAL segment below would be deleted even though its data never reached
        // the object store — losing it from BOTH tiers. The caller already
        // declines to mark such a segment "indexed" (retries next run); keeping
        // the WAL copy until the pass is clean is what makes that retry able to
        // re-upload it.
        let errors_at_start = stats.errors;

        // Hold the partition's archive lock from reading the segment until its
        // raw, Tantivy and Parquet copies are published, so a follower cannot
        // truncate the log in between and leave this pass re-archiving records
        // it discarded. Released before embedding, which can take minutes and
        // feeds only vector search.
        let archive_guard = match Self::segment_partition(segment_id) {
            Some((topic, partition)) => {
                let guard = wal_manager.archive_lock(&topic, partition).lock_owned().await;
                // Deleted while this pass waited for the lock: its purge has
                // run or is queued behind us. Archiving now would put the
                // records back under a name a new topic may take.
                if metadata_store.get_topic(&topic).await.ok().flatten().is_none() {
                    debug!(segment = %segment_id, "Topic no longer in the catalog — not archiving");
                    stats.segments_processed += 1;
                    return Ok(());
                }
                Some(guard)
            }
            None => None,
        };

        // Work prepared ahead counts only if no truncation ran since the
        // segment was read: a truncation bumps the epoch before it changes
        // anything, and takes the lock held here, so an unchanged epoch means
        // the records read then are the records the log holds now.
        let prepared = prepared.filter(|p| {
            let current = Self::segment_partition(segment_id)
                .map(|(topic, partition)| wal_manager.truncation_epoch(&topic, partition));
            let valid = current == Some(p.truncation_epoch);
            if !valid {
                info!(segment = %segment_id, "Partition truncated since the segment was prepared — indexing it again");
            }
            valid
        });
        let (tp_records, mut raw_prepared, mut archives_prepared) = match prepared {
            Some(p) => {
                stats.records_indexed += p.records_indexed;
                (p.tp_records, p.raw, p.archives)
            }
            None => {
                // Read all records from the segment (v1.3.47+: direct call)
                let records = wal_manager.read_segment(segment_id).await
                    .map_err(|e| Error::Internal(format!("Failed to read segment {}: {}", segment_id, e)))?;
                if records.is_empty() {
                    info!(segment = %segment_id, "Segment is empty, skipping");
                    stats.segments_processed += 1;
                    return Ok(());
                }
                let (tp_records, records_indexed) = Self::decode_segment(records)?;
                stats.records_indexed += records_indexed;
                (tp_records, HashMap::new(), HashMap::new())
            }
        };

        // Collect vector embedding work for concurrent processing after the main loop
        let mut vector_work_items: Vec<(TopicPartition, Vec<CanonicalRecord>)> = Vec::new();

        // RP-1.1: highest offset this segment carries per partition, so the
        // retention interlock below can ask whether followers have it yet.
        let mut tp_max_offsets: Vec<(TopicPartition, i64)> = tp_records
            .iter()
            .map(|(tp, records)| {
                let max = records.iter().map(|r| r.last_offset()).max().unwrap_or(-1);
                (tp.clone(), max)
            })
            .collect();
        tp_max_offsets.retain(|(_, max)| *max >= 0);

        // Process each topic-partition: upload raw segments + create Tantivy indexes
        for (tp, canonical_records) in tp_records {
            // Which tiers this topic gets.
            // v2.2.16: Only index searchable topics for Tantivy
            // v2.2.21: Only create Parquet files for columnar-enabled topics
            // v2.2.22: Only generate embeddings for vector-enabled topics
            let (is_searchable, is_columnar, is_vector) = {
                // Look up topic config from metadata store. If topic not yet in metadata
                // (e.g., follower node hasn't received TopicCreated event yet), fall back
                // to a default TopicConfig which respects env var defaults like
                // CHRONIK_DEFAULT_VECTOR_ENABLED, CHRONIK_DEFAULT_COLUMNAR, etc.
                let config = metadata_store.get_topic(&tp.topic).await
                    .ok()
                    .flatten()
                    .map(|t| t.config)
                    .unwrap_or_default();
                (config.is_searchable(), config.is_columnar_enabled(), config.is_vector_enabled())
            };

            // The Tantivy archive (searchable topics) is built on its own task
            // while this one writes the raw segment: they read the same records
            // and write different files, and both are CPU-bound (indexing;
            // zstd), so on the indexer's two workers they overlap instead of
            // adding up.
            // When the archive was built ahead (prepare_segment) only its upload
            // and registration are left.
            let prebuilt = archives_prepared.remove(&tp);
            let tantivy = is_searchable.then(|| {
                let (config, object_store, segment_index, tp) =
                    (config.clone(), object_store.clone(), segment_index.clone(), tp.clone());
                let records = if prebuilt.is_some() { Vec::new() } else { canonical_records.clone() };
                tokio::spawn(async move {
                    Self::create_tantivy_index(&config, &object_store, &segment_index, &tp, records, prebuilt).await
                })
            });

            // STEP 1: Upload raw segment data to S3 (Tier 2: warm storage)
            // This preserves the actual message data for consumption
            match Self::upload_raw_segment(
                object_store,
                metadata_store,
                &tp,
                &canonical_records,
                is_leader,
                raw_prepared.remove(&tp),
            ).await {
                Ok(segment_bytes) => {
                    info!(
                        topic = %tp.topic,
                        partition = tp.partition,
                        bytes = segment_bytes,
                        records = canonical_records.len(),
                        "Uploaded raw segment to S3"
                    );
                    stats.bytes_written += segment_bytes;
                }
                Err(e) => {
                    error!(
                        topic = %tp.topic,
                        partition = tp.partition,
                        error = %e,
                        "Failed to upload raw segment - CRITICAL DATA LOSS RISK!"
                    );
                    stats.errors += 1;
                    // Tantivy indexing continues even if raw upload fails
                    // (at least we'll have searchable index)
                }
            }

            // Clone records for each remaining step that needs them; the last
            // one (vector) takes the original.
            let records_for_columnar = if is_columnar {
                Some(canonical_records.clone())
            } else {
                None
            };
            let records_for_vector = if is_vector {
                Some(canonical_records)
            } else {
                drop(canonical_records);
                None
            };

            // STEP 2a: the Tantivy index (searchable topics only)
            if let Some(tantivy) = tantivy {
                let result = tantivy
                    .await
                    .unwrap_or_else(|e| Err(Error::Internal(format!("Tantivy archive task failed: {}", e))));
                match result {
                    Ok((bytes_written, max_offset)) => {
                        info!(
                            topic = %tp.topic,
                            partition = tp.partition,
                            bytes = bytes_written,
                            max_offset = max_offset,
                            "Created Tantivy index (searchable topic)"
                        );
                        stats.indexes_created += 1;
                        stats.bytes_written += bytes_written;

                        // HP-1.4/HP-2.6: notify all listeners (hot text, hot vector)
                        // that everything up to max_offset is now safely in cold
                        // storage — they can evict.
                        if max_offset >= 0 {
                            for listener in cold_flush_listener.read().await.iter() {
                                listener.notify_cold_flushed(
                                    tp.topic.clone(),
                                    tp.partition,
                                    max_offset,
                                );
                            }
                        }
                    }
                    Err(e) => {
                        error!(
                            topic = %tp.topic,
                            partition = tp.partition,
                            error = %e,
                            "Failed to create Tantivy index"
                        );
                        stats.errors += 1;
                    }
                }
            } else {
                debug!(
                    topic = %tp.topic,
                    partition = tp.partition,
                    "Skipping Tantivy index (non-searchable topic)"
                );
            }

            // STEP 2b: Create Parquet file (columnar-enabled topics only)
            // v2.2.21: Async Parquet generation for SQL analytics
            if let Some(records) = records_for_columnar {
                match Self::create_parquet_segment(
                    config,
                    object_store,
                    metadata_store,
                    segment_index,
                    &tp,
                    records,
                ).await {
                    Ok((bytes_written, max_offset)) => {
                        info!(
                            topic = %tp.topic,
                            partition = tp.partition,
                            bytes = bytes_written,
                            max_offset = max_offset,
                            "Created Parquet file (columnar-enabled topic)"
                        );
                        stats.bytes_written += bytes_written;

                        // v2.4.1: Notify hot buffer that records up to max_offset
                        // are now in Parquet, so they can be skipped in WAL reads.
                        if max_offset >= 0 {
                            if let Some(hb) = hot_buffer.read().await.as_ref() {
                                hb.set_flushed_offset(&tp.topic, tp.partition, max_offset);
                            }
                        }
                    }
                    Err(e) => {
                        error!(
                            topic = %tp.topic,
                            partition = tp.partition,
                            error = %e,
                            "Failed to create Parquet file"
                        );
                        stats.errors += 1;
                    }
                }
            } else {
                debug!(
                    topic = %tp.topic,
                    partition = tp.partition,
                    "Skipping Parquet file (non-columnar topic)"
                );
            }

            // STEP 2c: Collect vector embedding work for concurrent processing
            // v2.4.0: Defer embedding to run concurrently across partitions
            if let Some(records) = records_for_vector {
                vector_work_items.push((tp.clone(), records));
            } else {
                debug!(
                    topic = %tp.topic,
                    partition = tp.partition,
                    "Skipping vector embeddings (non-vector topic)"
                );
            }
        }

        drop(archive_guard);

        // STEP 3: Process all vector embeddings concurrently across partitions
        // v2.4.0: Up to 3 partitions embed in parallel, reducing total wall-clock time
        if !vector_work_items.is_empty() {
            let max_partition_concurrency = std::env::var("CHRONIK_VECTOR_PARTITION_CONCURRENCY")
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(3)
                .clamp(1, 8);

            // HP-2 follow-up A: snapshot the hot vector index once.
            let hot_vec_for_reuse = hot_vector_index.read().await.clone();

            if vector_work_items.len() == 1 || max_partition_concurrency <= 1 {
                // Serial: single partition or concurrency disabled
                for (tp, records) in vector_work_items {
                    match Self::process_vector_embeddings(
                        config,
                        metadata_store,
                        vector_index_manager,
                        &tp,
                        records,
                        hot_vec_for_reuse.clone(),
                    ).await {
                        Ok(embeddings_generated) => {
                            info!(
                                topic = %tp.topic,
                                partition = tp.partition,
                                embeddings = embeddings_generated,
                                "Generated vector embeddings (vector-enabled topic)"
                            );
                        }
                        Err(e) => {
                            error!(
                                topic = %tp.topic,
                                partition = tp.partition,
                                error = %e,
                                "Failed to generate vector embeddings"
                            );
                            stats.errors += 1;
                        }
                    }
                }
            } else {
                // Concurrent: multiple partitions embed in parallel
                let semaphore = Arc::new(tokio::sync::Semaphore::new(max_partition_concurrency));
                let mut join_set = tokio::task::JoinSet::new();

                for (tp, records) in vector_work_items {
                    let permit = semaphore.clone().acquire_owned().await
                        .map_err(|e| Error::Internal(format!("Semaphore closed: {}", e)))?;
                    let config = config.clone();
                    let metadata_store = Arc::clone(metadata_store);
                    let vector_index_manager = Arc::clone(vector_index_manager);
                    let hot_vec_for_task = hot_vec_for_reuse.clone();

                    join_set.spawn(async move {
                        let result = Self::process_vector_embeddings(
                            &config,
                            &metadata_store,
                            &vector_index_manager,
                            &tp,
                            records,
                            hot_vec_for_task,
                        ).await;
                        drop(permit);
                        (tp, result)
                    });
                }

                while let Some(join_result) = join_set.join_next().await {
                    match join_result {
                        Ok((tp, Ok(embeddings_generated))) => {
                            info!(
                                topic = %tp.topic,
                                partition = tp.partition,
                                embeddings = embeddings_generated,
                                "Generated vector embeddings (concurrent, vector-enabled topic)"
                            );
                        }
                        Ok((tp, Err(e))) => {
                            error!(
                                topic = %tp.topic,
                                partition = tp.partition,
                                error = %e,
                                "Failed to generate vector embeddings (concurrent)"
                            );
                            stats.errors += 1;
                        }
                        Err(e) => {
                            error!(error = %e, "Vector embedding task panicked");
                            stats.errors += 1;
                        }
                    }
                }
            }
        }

        // Delete WAL segment if configured (v1.3.47+: direct call) — but ONLY
        // on a clean pass. If any step for this segment errored (most
        // critically a raw-segment upload to the object store), keep the WAL
        // copy so the durable data isn't lost from both tiers; the caller won't
        // mark this segment indexed, so the next run retries and re-uploads it.
        // RP-1.1 retention interlock: indexed does not mean replicated. Nothing
        // re-sends a record a follower missed, so deleting WAL the followers do
        // not have yet strands it on one node permanently.
        let replication_gap = {
            let guard = replication_progress.read().await;
            Self::fully_replicated(guard.as_ref(), &tp_max_offsets).err()
        };

        if let Some(gap) = replication_gap {
            warn!(
                segment = %segment_id,
                reason = %gap,
                "Keeping WAL segment — not yet replicated to followers"
            );
        } else if Self::may_delete_wal_segment(config.delete_after_index, errors_at_start, stats.errors) {
            let removed = wal_manager.delete_segment(segment_id).await
                .map_err(|e| Error::Internal(format!("Failed to delete segment {}: {}", segment_id, e)))?;
            if removed {
                info!(segment = %segment_id, "Deleted WAL segment after indexing");
            } else {
                warn!(segment = %segment_id, "WAL segment indexed but no file was deleted (not tracked or already gone)");
            }
        } else if config.delete_after_index {
            warn!(
                segment = %segment_id,
                errors = stats.errors - errors_at_start,
                "Keeping WAL segment (indexing/upload had errors) — will retry next run"
            );
        }

        stats.segments_processed += 1;
        Ok(())
    }

    /// Upload raw segment data to S3 for message consumption (Tier 2: warm storage)
    ///
    /// This stores the actual CanonicalRecords (bincode-serialized) so consumers can
    /// fetch messages from S3 when they're no longer in local WAL/segments.
    #[instrument(skip(object_store, metadata_store, canonical_records, is_leader, encoded))]
    async fn upload_raw_segment(
        object_store: &Arc<dyn ObjectStore>,
        metadata_store: &Arc<dyn MetadataStore>,
        tp: &TopicPartition,
        canonical_records: &[CanonicalRecord],
        is_leader: &Arc<AtomicBool>,
        encoded: Option<Vec<u8>>,
    ) -> Result<u64> {
        if canonical_records.is_empty() {
            return Ok(0);
        }

        // Calculate offset range for this segment
        let min_offset = canonical_records.iter()
            .map(|r| r.min_offset())
            .min()
            .unwrap_or(0);
        let max_offset = canonical_records.iter()
            .map(|r| r.last_offset())
            .max()
            .unwrap_or(0);

        // The batches exactly as the WAL holds them (original wire bytes
        // included, so fetch serves them with their producer CRCs), compressed.
        let serialized_data = match encoded {
            Some(bytes) => bytes,
            None => crate::archive::encode_raw_segment(canonical_records)
                .map_err(|e| Error::Internal(format!("Failed to encode raw segment: {}", e)))?,
        };

        let data_size = serialized_data.len() as u64;

        // Upload to S3 with path: segments/{topic}/{partition}/{min_offset}-{max_offset}.segment
        let object_key = crate::archive::archived_segment_key(&tp.topic, tp.partition, min_offset, max_offset);

        let data_bytes = bytes::Bytes::from(serialized_data);
        object_store.put(&object_key, data_bytes).await
            .map_err(|e| Error::Internal(format!("Failed to upload raw segment to S3: {}", e)))?;

        info!(
            topic = %tp.topic,
            partition = tp.partition,
            min_offset = min_offset,
            max_offset = max_offset,
            object_key = %object_key,
            bytes = data_size,
            "Uploaded raw segment data to S3"
        );

        // Register segment metadata in the metadata store for disaster recovery.
        // This allows high watermarks to be restored on startup.
        // Gated by `is_leader` only when a caller passes one; the server passes
        // none, because segment records live in each node's own metadata WAL.
        if is_leader.load(Ordering::Relaxed) {
            // The partition is part of the id: records are keyed by (topic, id),
            // and two partitions archiving the same offset range (common with
            // round-robin producers) otherwise replaced each other's record.
            let segment_id = format!("p{}-{}-{}", tp.partition, min_offset, max_offset);
            let segment_metadata = MetadataSegmentMetadata {
                segment_id,
                topic: tp.topic.clone(),
                partition: tp.partition as u32,
                start_offset: min_offset,
                end_offset: max_offset,
                size: data_size as i64,
                record_count: canonical_records.iter().map(|r| r.records.len() as i64).sum(),
                path: object_key.clone(),
                created_at: chrono::Utc::now(),
            };

            match metadata_store.persist_segment_metadata(segment_metadata).await {
                Ok(()) => {
                    debug!(
                        topic = %tp.topic,
                        partition = tp.partition,
                        min_offset = min_offset,
                        max_offset = max_offset,
                        "Registered segment metadata"
                    );
                }
                Err(e) => {
                    warn!(
                        topic = %tp.topic,
                        partition = tp.partition,
                        min_offset = min_offset,
                        max_offset = max_offset,
                        error = %e,
                        "Failed to register segment metadata (non-fatal, S3 upload succeeded)"
                    );
                }
            }
        }

        Ok(data_size)
    }

    /// Create Tantivy index from CanonicalRecords
    #[instrument(skip(config, object_store, segment_index, canonical_records, prebuilt))]
    async fn create_tantivy_index(
        config: &WalIndexerConfig,
        object_store: &Arc<dyn ObjectStore>,
        segment_index: &Arc<SegmentIndex>,
        tp: &TopicPartition,
        canonical_records: Vec<CanonicalRecord>,
        prebuilt: Option<LocalArchive>,
    ) -> Result<(u64, i64)> {
        let segment_metadata = match prebuilt {
            Some(local) => Self::upload_archive(config, object_store, tp, local, None).await?,
            None if canonical_records.is_empty() => return Ok((0, -1)),
            None => Self::build_archive(config, object_store, tp, canonical_records, None).await?,
        };
        let (segment_id, bytes_written, max_offset) =
            (segment_metadata.segment_id.clone(), segment_metadata.size_bytes, segment_metadata.max_offset);
        segment_index.add_segment(segment_metadata).await
            .map_err(|e| Error::Internal(format!("Failed to register segment in index: {}", e)))?;

        info!(
            topic = %tp.topic,
            partition = tp.partition,
            segment_id = %segment_id,
            "Registered segment in index"
        );

        Ok((bytes_written, max_offset))
    }

    /// Build `canonical_records` into one Tantivy archive and upload it with
    /// its term filter, without registering it: the catalogue entry to add (or
    /// to swap in for others, as a reindex does). `key_suffix` makes the file
    /// name distinct from an archive of the same range it will replace.
    async fn build_archive(
        config: &WalIndexerConfig,
        object_store: &Arc<dyn ObjectStore>,
        tp: &TopicPartition,
        canonical_records: Vec<CanonicalRecord>,
        key_suffix: Option<&str>,
    ) -> Result<SegmentIndexMetadata> {
        let local = {
            let tp = tp.clone();
            tokio::task::spawn_blocking(move || Self::build_archive_local(&tp, canonical_records))
                .await
                .map_err(|e| Error::Internal(format!("Tantivy archive build task failed: {}", e)))??
        };
        Self::upload_archive(config, object_store, tp, local, key_suffix).await
    }

    /// The CPU half of [`Self::build_archive`]: index the records into a tar
    /// (and its term filter) in a temporary directory. Blocking.
    fn build_archive_local(tp: &TopicPartition, canonical_records: Vec<CanonicalRecord>) -> Result<LocalArchive> {
        if canonical_records.is_empty() {
            return Err(Error::Internal("no records to archive".into()));
        }

        // Get base offset and calculate ranges from all records
        let base_offset = canonical_records[0].base_offset;
        let mut min_offset = i64::MAX;
        let mut max_offset = i64::MIN;
        let mut min_timestamp = i64::MAX;
        let mut max_timestamp = i64::MIN;
        let mut record_count = 0;

        for record in &canonical_records {
            min_offset = min_offset.min(record.min_offset());
            max_offset = max_offset.max(record.last_offset());
            min_timestamp = min_timestamp.min(record.min_timestamp());
            max_timestamp = max_timestamp.max(record.max_timestamp);
            record_count += record.records.len();
        }

        let mut writer = TantivySegmentWriter::new(tp.topic.clone(), tp.partition, base_offset)?;
        for canonical_record in canonical_records {
            writer.write_batch(&canonical_record)?;
        }

        let dir = tempfile::tempdir()
            .map_err(|e| Error::Internal(format!("Failed to create temp dir: {}", e)))?;
        let (archive_path, metadata) = writer.commit_and_serialize(dir.path())?;

        info!(
            topic = %tp.topic,
            partition = tp.partition,
            record_count = metadata.record_count,
            base_offset = metadata.base_offset,
            last_offset = metadata.last_offset,
            "Committed Tantivy index (record_count={})",
            metadata.record_count
        );

        Ok(LocalArchive {
            _dir: dir,
            archive_path,
            base_offset: metadata.base_offset,
            last_offset: metadata.last_offset,
            min_offset,
            max_offset,
            min_timestamp,
            max_timestamp,
            record_count,
        })
    }

    /// The I/O half of [`Self::build_archive`]: upload a built archive and its
    /// term filter; returns the catalogue entry (not registered).
    async fn upload_archive(
        config: &WalIndexerConfig,
        object_store: &Arc<dyn ObjectStore>,
        tp: &TopicPartition,
        local: LocalArchive,
        key_suffix: Option<&str>,
    ) -> Result<SegmentIndexMetadata> {
        let archive_data = tokio::fs::read(&local.archive_path).await
            .map_err(|e| Error::Internal(format!("Failed to read the archive: {}", e)))?;
        let bytes_written = archive_data.len() as u64;

        let object_key = format!(
            "{}/{}/partition-{}/segment-{}-{}{}.tar",
            config.index_base_path,
            tp.topic,
            tp.partition,
            local.base_offset,
            local.last_offset,
            key_suffix.map(|s| format!("-{s}")).unwrap_or_default()
        );

        object_store.put(&object_key, bytes::Bytes::from(archive_data)).await
            .map_err(|e| Error::Internal(format!("Failed to upload index: {}", e)))?;

        info!(
            topic = %tp.topic,
            partition = tp.partition,
            object_key = %object_key,
            bytes = bytes_written,
            "Uploaded Tantivy index to object store"
        );

        // Its term filter, when one was built: searches skip the archive
        // without opening it when a required word is absent. Best effort — a
        // missing filter only means the archive is always opened (and the
        // search builds one the first time it does).
        let filter_path = crate::term_filter::term_filter_key(&local.archive_path.to_string_lossy());
        if let Ok(filter) = tokio::fs::read(&filter_path).await {
            let key = crate::term_filter::term_filter_key(&object_key);
            if let Err(e) = object_store.put(&key, bytes::Bytes::from(filter)).await {
                warn!(object_key = %key, "Failed to upload term filter: {}", e);
            }
        }

        Ok(SegmentIndexMetadata {
            segment_id: format!("{}-{}-{}-{}", tp.topic, tp.partition, local.min_offset, local.max_offset),
            topic: tp.topic.clone(),
            partition: tp.partition,
            min_offset: local.min_offset,
            max_offset: local.max_offset,
            record_count: local.record_count,
            min_timestamp: local.min_timestamp,
            max_timestamp: local.max_timestamp,
            object_store_path: object_key,
            size_bytes: bytes_written,
            created_at: chrono::Utc::now().timestamp(),
            compression: "snappy".to_string(),
        })
    }

    /// v2.2.21: Create Parquet file from CanonicalRecords for columnar storage
    ///
    /// Converts CanonicalRecords to Arrow RecordBatches and writes to Parquet format.
    /// Uses time-based partitioning (hourly by default) for efficient query pruning.
    /// Registers the new Parquet segment with the SegmentIndex for SQL query planning.
    ///
    /// Writes for one partition are serialized (`parquet_flush_lock`) from
    /// reading what cold covers to registering the new segment: the indexer
    /// and the columnar catch-up both write here, and two writers that each
    /// saw a range uncovered would both write it — two files for one range,
    /// every row counted twice (#41).
    ///
    /// Returns (bytes_written, end of the contiguous run cold now holds).
    #[instrument(skip(config, object_store, metadata_store, segment_index, canonical_records))]
    async fn create_parquet_segment(
        config: &WalIndexerConfig,
        object_store: &Arc<dyn ObjectStore>,
        metadata_store: &Arc<dyn MetadataStore>,
        segment_index: &Arc<SegmentIndex>,
        tp: &TopicPartition,
        canonical_records: Vec<CanonicalRecord>,
    ) -> Result<(u64, i64)> {
        let lock = parquet_flush_lock(tp);
        let _held = lock.lock().await;
        Self::create_parquet_segment_locked(config, object_store, metadata_store, segment_index, tp, canonical_records)
            .await
    }

    async fn create_parquet_segment_locked(
        config: &WalIndexerConfig,
        object_store: &Arc<dyn ObjectStore>,
        metadata_store: &Arc<dyn MetadataStore>,
        segment_index: &Arc<SegmentIndex>,
        tp: &TopicPartition,
        canonical_records: Vec<CanonicalRecord>,
    ) -> Result<(u64, i64)> {
        if canonical_records.is_empty() {
            return Ok((0, -1));
        }

        // Get topic config to read columnar settings
        let topic_config = metadata_store.get_topic(&tp.topic).await
            .map_err(|e| Error::Internal(format!("Failed to get topic config: {}", e)))?;

        let columnar_config = if let Some(ref topic) = topic_config {
            Self::build_columnar_config(&topic.config)
        } else {
            ColumnarConfig::default()
        };

        // Convert CanonicalRecords to KafkaRecords for the converter
        // Note: Embeddings are generated asynchronously and populated later
        // when vector search is enabled for this topic
        let mut kafka_records: Vec<KafkaRecord> = canonical_records
            .iter()
            .flat_map(|cr| {
                cr.records.iter().map(|entry| KafkaRecord {
                    topic: tp.topic.clone(),
                    partition: tp.partition,
                    offset: entry.offset,
                    timestamp_ms: entry.timestamp,
                    timestamp_type: match cr.timestamp_type {
                        TimestampType::CreateTime => 0,
                        TimestampType::LogAppendTime => 1,
                    },
                    key: entry.key.clone(),
                    value: entry.value.clone().unwrap_or_default(),
                    headers: entry.headers.iter()
                        .map(|h| (h.key.clone(), h.value.clone()))
                        .collect(),
                    embedding: None, // TODO: Populate from embedding service for vector-enabled topics
                })
            })
            .collect();

        if kafka_records.is_empty() {
            debug!(
                topic = %tp.topic,
                partition = tp.partition,
                "No records to write to Parquet"
            );
            return Ok((0, -1));
        }

        // Idempotent flush (#41): never re-write offsets that are already in a
        // cold segment. The in-memory "already indexed" guard is empty after a
        // restart, so the indexer re-reads sealed WAL segments it has already
        // flushed; a follower that re-pulled a partition re-seals overlapping
        // ranges. Writing those again created a *second* Parquet segment covering
        // offsets an older one already held, and the cold scan counted each
        // offset once per copy. The ranges cold covers live in the metadata
        // store (they survive restarts), so drop exactly the offsets inside
        // them — not everything below the highest, which lost whole segments
        // whenever they were indexed out of order.
        //
        // What is returned for the hot buffer is the end of the contiguous run
        // cold holds from its lowest offset (`cold_prefix_end`): the hot buffer
        // hides every offset at or below it, so it must never move past a gap
        // a segment indexed later will fill.
        let mut cold = Self::cold_ranges(metadata_store, tp).await?;
        let before = kafka_records.len();
        kafka_records.retain(|r| !range_covers(&cold, r.offset));
        let dropped = before - kafka_records.len();
        if dropped > 0 {
            debug!(
                topic = %tp.topic,
                partition = tp.partition,
                dropped,
                remaining = kafka_records.len(),
                "Skipping offsets already present in cold Parquet (idempotent flush)"
            );
        }
        if kafka_records.is_empty() {
            // Everything here is already in cold. Nothing to write.
            return Ok((0, cold_prefix_end(&cold)));
        }

        // Calculate offset and timestamp ranges for naming
        let min_offset = kafka_records.iter().map(|r| r.offset).min().unwrap_or(0);
        let max_offset = kafka_records.iter().map(|r| r.offset).max().unwrap_or(0);
        let min_timestamp = kafka_records.iter().map(|r| r.timestamp_ms).min().unwrap_or(0);
        let max_timestamp = kafka_records.iter().map(|r| r.timestamp_ms).max().unwrap_or(0);

        // The typed columns this topic projects out of its values (issue #73;
        // chronik-columnar/src/projection.rs). In `auto` mode (the default)
        // they are the top-level fields inferred from the data, stored in the
        // topic config as `columnar.json_schema` so every node and every
        // segment agree. The stored list only grows: a field first seen now is
        // appended. It used to be inferred once, from the first flush's first
        // 100 records, and never again, so a field that appeared later was
        // never a column.
        let mut topic_cfg: std::collections::HashMap<String, String> =
            topic_config.as_ref().map(|tc| tc.config.config.clone()).unwrap_or_default();
        if topic_config.is_some()
            && chronik_columnar::projection::ProjectionMode::of(&topic_cfg).ok()
                == Some(chronik_columnar::projection::ProjectionMode::Auto)
        {
            // A sample spread across the batch, not just its head.
            let step = (kafka_records.len() / 100).max(1);
            let sample_values: Vec<&[u8]> =
                kafka_records.iter().step_by(step).take(100).map(|r| r.value.as_slice()).collect();
            let stored = topic_cfg
                .get(chronik_columnar::projection::INFERRED_KEY)
                .and_then(|s| InferredJsonSchema::from_config_string(s));
            let grown = match (&stored, InferredJsonSchema::infer(&sample_values, 64)) {
                (None, Some(inferred)) => Some(inferred),
                (Some(stored), Some(inferred)) => stored.extended_with(&inferred, 64),
                _ => None,
            };
            if let (Some(grown), Some(tc)) = (grown, topic_config.as_ref()) {
                let config_str = grown.to_config_string();
                info!(
                    topic = %tp.topic,
                    schema = %config_str,
                    "Columnar fields inferred from the data (columnar.json_schema)"
                );
                let mut updated_config = tc.config.clone();
                updated_config
                    .config
                    .insert(chronik_columnar::projection::INFERRED_KEY.to_string(), config_str.clone());
                if let Err(e) = metadata_store.update_topic(&tp.topic, updated_config).await {
                    warn!(
                        topic = %tp.topic,
                        error = %e,
                        "Failed to persist inferred JSON schema (will re-infer next time)"
                    );
                }
                topic_cfg.insert(chronik_columnar::projection::INFERRED_KEY.to_string(), config_str);
            }
        }
        let (columns, problem) = chronik_columnar::projection::effective_columns(&topic_cfg);
        if let Some(problem) = problem {
            warn!(topic = %tp.topic, "columnar projection config ignored in part: {}", problem);
        }

        // Convert to Arrow RecordBatch (envelope + projected columns)
        let converter = RecordBatchConverter::new();
        let batch = converter
            .convert_with_columns(&kafka_records, &columns)
            .map_err(|e| Error::Internal(format!("Failed to convert to RecordBatch: {}", e)))?;

        // Create Parquet writer and write to bytes
        let schema = (*batch.schema()).clone();
        let writer = ParquetSegmentWriter::new(schema, columnar_config.clone());
        let (parquet_bytes, stats) = writer.write_to_bytes(&[batch])
            .map_err(|e| Error::Internal(format!("Failed to write Parquet: {}", e)))?;

        let bytes_written = parquet_bytes.len() as u64;

        // Generate the local file path with time partitioning
        let local_file_path = Self::generate_parquet_path(
            &config.columnar_base_path,
            &tp.topic,
            tp.partition,
            min_offset,
            max_offset,
            min_timestamp,
            &columnar_config,
        );

        // v2.2.23: Determine which paths to use based on configuration
        // - If columnar_use_object_store is false: write locally only
        // - If columnar_use_object_store is true and columnar_keep_local is true: write to both
        // - If columnar_use_object_store is true and columnar_keep_local is false: write to object store only
        let write_local = !config.columnar_use_object_store || config.columnar_keep_local;
        let write_to_object_store = config.columnar_use_object_store && config.columnar_object_store.is_some();

        // Write to local filesystem if needed
        if write_local {
            let path = std::path::Path::new(&local_file_path);
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await
                    .map_err(|e| Error::Internal(format!("Failed to create Parquet directory: {}", e)))?;
            }
            tokio::fs::write(&local_file_path, &parquet_bytes).await
                .map_err(|e| Error::Internal(format!("Failed to write Parquet file: {}", e)))?;

            debug!(
                topic = %tp.topic,
                partition = tp.partition,
                file_path = %local_file_path,
                bytes = bytes_written,
                "Wrote Parquet segment to local filesystem"
            );
        }

        // v2.2.23: Generate object store path and upload if configured
        let final_path = if write_to_object_store {
            let object_store_path = Self::generate_parquet_s3_path(
                &config.columnar_s3_prefix,
                &tp.topic,
                tp.partition,
                min_offset,
                max_offset,
                min_timestamp,
                &columnar_config,
            );

            // Upload to object store (S3/GCS/Azure)
            // Note: object_store.put() is async and handles multipart uploads for large files
            if let Err(e) = object_store.put(&object_store_path, parquet_bytes.clone().into()).await {
                error!(
                    topic = %tp.topic,
                    partition = tp.partition,
                    object_path = %object_store_path,
                    error = %e,
                    "Failed to upload Parquet segment to object storage"
                );
                // Fall back to local path if upload fails
                if write_local {
                    warn!("Using local path as fallback after object store upload failure");
                    local_file_path.clone()
                } else {
                    return Err(Error::Internal(format!("Failed to upload Parquet to object storage: {}", e)));
                }
            } else {
                info!(
                    topic = %tp.topic,
                    partition = tp.partition,
                    object_path = %object_store_path,
                    bytes = bytes_written,
                    "Uploaded Parquet segment to object storage"
                );
                // Use object store path for metadata (DataFusion can query S3 directly)
                object_store_path
            }
        } else {
            local_file_path.clone()
        };

        info!(
            topic = %tp.topic,
            partition = tp.partition,
            file_path = %final_path,
            rows = stats.num_rows,
            row_groups = stats.num_row_groups,
            bytes = bytes_written,
            offset_range = format!("{}-{}", min_offset, max_offset),
            local = write_local,
            object_store = write_to_object_store,
            "Created Parquet segment for columnar storage"
        );

        // Register Parquet segment with SegmentIndex for SQL query planning
        let time_partition_key = Self::extract_time_partition_key(min_timestamp, &columnar_config);
        let parquet_metadata = ParquetSegmentMetadata {
            segment_id: format!("{}-{}-{}-{}-parquet", tp.topic, tp.partition, min_offset, max_offset),
            topic: tp.topic.clone(),
            partition: tp.partition,
            min_offset,
            max_offset,
            record_count: kafka_records.len(),
            row_group_count: stats.num_row_groups,
            min_timestamp,
            max_timestamp,
            object_store_path: final_path.clone(), // v2.2.23: Use final path (local or S3)
            size_bytes: bytes_written,
            created_at: chrono::Utc::now().timestamp(),
            compression: columnar_config.compression.to_string(),
            time_partition_key,
            schema_fingerprint: None, // TODO: Add schema fingerprinting for evolution tracking
        };

        segment_index.add_parquet_segment(parquet_metadata.clone()).await
            .map_err(|e| Error::Internal(format!("Failed to register Parquet segment: {}", e)))?;

        // Persist Parquet segment metadata for SQL handler discovery
        let common_metadata = chronik_common::metadata::ParquetSegmentMetadata {
            segment_id: parquet_metadata.segment_id,
            topic: parquet_metadata.topic,
            partition: parquet_metadata.partition,
            min_offset: parquet_metadata.min_offset,
            max_offset: parquet_metadata.max_offset,
            record_count: parquet_metadata.record_count,
            row_group_count: parquet_metadata.row_group_count,
            min_timestamp: parquet_metadata.min_timestamp,
            max_timestamp: parquet_metadata.max_timestamp,
            object_store_path: parquet_metadata.object_store_path,
            size_bytes: parquet_metadata.size_bytes,
            created_at: parquet_metadata.created_at,
            compression: parquet_metadata.compression,
            time_partition_key: parquet_metadata.time_partition_key,
            schema_fingerprint: parquet_metadata.schema_fingerprint,
        };

        // Issue #19: this is NOT optional. The SQL cold table is built from
        // `get_parquet_paths`, so a segment whose metadata never lands is
        // invisible to queries — and if we returned Ok here the caller would
        // mark the WAL segment indexed (never retrying) *and* advance the hot
        // buffer's flushed offset past those rows, dropping them from both
        // tiers. Failing keeps the rows served from hot until a later run
        // succeeds; re-running rewrites the identical Parquet file.
        metadata_store
            .persist_parquet_segment(common_metadata)
            .await
            .map_err(|e| {
                Error::Internal(format!(
                    "Failed to persist Parquet segment metadata for {}-{}: {}",
                    tp.topic, tp.partition, e
                ))
            })?;

        debug!(
            topic = %tp.topic,
            partition = tp.partition,
            min_offset = min_offset,
            max_offset = max_offset,
            "Registered Parquet segment in index and metadata store"
        );

        cold.push((min_offset, max_offset));
        Ok((bytes_written, cold_prefix_end(&merge_ranges(cold))))
    }

    /// v2.2.22: Process vector embeddings for CanonicalRecords
    ///
    /// Extracts text from the configured field, generates embeddings via the configured
    /// provider, and stores them in the vector index for semantic search.
    ///
    /// The embedding field is configured via topic config:
    /// - `vector.field` = "value" (default) | "key" | JSON path like "$.message.text"
    /// - `vector.provider` = "openai" | "external" | "local"
    /// - `vector.model` = model name (e.g., "text-embedding-3-small")
    #[instrument(skip(config, metadata_store, vector_index_manager, canonical_records, hot_vector_index))]
    async fn process_vector_embeddings(
        config: &WalIndexerConfig,
        metadata_store: &Arc<dyn MetadataStore>,
        vector_index_manager: &Arc<VectorIndexManager>,
        tp: &TopicPartition,
        canonical_records: Vec<CanonicalRecord>,
        hot_vector_index: Option<Arc<chronik_columnar::hot_vector_index::HotVectorIndex>>,
    ) -> Result<usize> {
        Self::process_vector_embeddings_with_model(
            config, metadata_store, vector_index_manager, tp, canonical_records, None, hot_vector_index,
        ).await
        .map(|o| o.embedded)
    }

    /// Process vector embeddings with an optional model ID override.
    ///
    /// When `model_id` is Some, vectors are stored under that model's index
    /// (enabling multi-model A/B testing and re-embedding). When None, the
    /// default model is used.
    async fn process_vector_embeddings_with_model(
        _config: &WalIndexerConfig,
        metadata_store: &Arc<dyn MetadataStore>,
        vector_index_manager: &Arc<VectorIndexManager>,
        tp: &TopicPartition,
        canonical_records: Vec<CanonicalRecord>,
        model_id: Option<&str>,
        hot_vector_index: Option<Arc<chronik_columnar::hot_vector_index::HotVectorIndex>>,
    ) -> Result<EmbedOutcome> {
        if canonical_records.is_empty() {
            return Ok(EmbedOutcome::default());
        }

        // Get topic config to read vector settings
        let topic_config = metadata_store.get_topic(&tp.topic).await
            .map_err(|e| Error::Internal(format!("Failed to get topic config: {}", e)))?;

        let topic_meta = match topic_config {
            Some(t) => t,
            None => {
                warn!(topic = %tp.topic, "Topic not found for vector processing");
                return Ok(EmbedOutcome::default());
            }
        };

        // Parse VectorSearchConfig from topic config
        let vector_config = VectorSearchConfig::from_topic_config(&topic_meta.config.config)
            .map_err(|e| Error::Internal(format!("Invalid vector config: {}", e)))?;

        // Get the field to extract text from (default: "value")
        let field = &vector_config.field;

        // Extract text from records based on field configuration
        let mut texts: Vec<(i64, String)> = Vec::new(); // (offset, text)

        for cr in &canonical_records {
            for entry in &cr.records {
                let text = Self::extract_text_from_field(field, entry);
                if let Some(t) = text {
                    if !t.is_empty() {
                        texts.push((entry.offset, t));
                    }
                }
            }
        }

        if texts.is_empty() {
            debug!(
                topic = %tp.topic,
                partition = tp.partition,
                "No text extracted from records for embedding"
            );
            return Ok(EmbedOutcome::default());
        }

        let text_count = texts.len();
        info!(
            topic = %tp.topic,
            partition = tp.partition,
            texts = text_count,
            field = field,
            provider = %vector_config.embedding.provider,
            model = %vector_config.embedding.model,
            "Extracted {} texts for embedding generation",
            text_count
        );

        // Create embedding provider from config
        let provider = match create_provider(&vector_config) {
            Ok(p) => p,
            Err(e) => {
                // Not fatal — the rest of the segment still indexes. But say
                // what it costs and what to set, because the caller logs
                // "embeddings=0" right after this and that reads as success:
                // the topic stays permanently unsearchable by vector while
                // every other signal looks healthy.
                warn!(
                    topic = %tp.topic,
                    partition = tp.partition,
                    provider = %vector_config.embedding.provider,
                    error = %e,
                    "No embedding provider, so this topic's records will NOT be \
                     vector-searchable. Set vector.provider (with vector.endpoint \
                     for 'external', or OPENAI_API_KEY for 'openai') on the topic, \
                     or CHRONIK_EMBEDDING_PROVIDER/_ENDPOINT on the server"
                );
                return Ok(EmbedOutcome { texts: text_count, failed: text_count, ..Default::default() });
            }
        };

        // Register topic with vector index manager if not already registered
        if !vector_index_manager.is_topic_registered(&tp.topic).await {
            let hnsw_config = HnswIndexConfig::from_vector_config(&vector_config);
            vector_index_manager.register_topic(&tp.topic, hnsw_config).await;
        }

        // Create embedding pipeline with configured concurrency.
        // HP-2 follow-up A: attach hot vector index for single-embed reuse.
        let mut pipeline = EmbeddingPipeline::with_concurrency(
            provider,
            Arc::clone(vector_index_manager),
            vector_config.batch_size,
            vector_config.embedding_concurrency,
        );
        if let Some(hot) = hot_vector_index {
            pipeline = pipeline.with_hot_vector_index(hot);
        }

        // Process messages through the pipeline (model-aware when specified)
        match pipeline.process_messages_for_model(&tp.topic, tp.partition, texts, model_id).await {
            Ok(stats) => {
                info!(
                    topic = %tp.topic,
                    partition = tp.partition,
                    total = stats.total_messages,
                    embedded = stats.embedded,
                    failed = stats.failed,
                    batches = stats.batches_processed,
                    tokens = stats.total_tokens,
                    "Embedding pipeline completed"
                );
                if stats.failed > 0 {
                    warn!(
                        topic = %tp.topic,
                        partition = tp.partition,
                        failed = stats.failed,
                        embedded = stats.embedded,
                        "records not embedded (provider error); the vector catch-up retries them from the log"
                    );
                }
                Ok(EmbedOutcome { texts: text_count, embedded: stats.embedded, failed: stats.failed })
            }
            Err(e) => {
                error!(
                    topic = %tp.topic,
                    partition = tp.partition,
                    error = %e,
                    "Embedding pipeline failed"
                );
                Err(Error::Internal(format!("Embedding pipeline failed: {}", e)))
            }
        }
    }

    /// Extract text from a record field based on field configuration
    ///
    /// Field can be:
    /// - "value" - Extract from record value (decoded as UTF-8)
    /// - "key" - Extract from record key (decoded as UTF-8)
    /// - "$.path.to.field" - JSON path extraction from value
    fn extract_text_from_field(field: &str, entry: &CanonicalRecordEntry) -> Option<String> {
        match field {
            "value" => {
                // Extract from value, treating as UTF-8 text
                entry.value.as_ref().and_then(|v| String::from_utf8(v.clone()).ok())
            }
            "key" => {
                // Extract from key, treating as UTF-8 text
                entry.key.as_ref().and_then(|k| String::from_utf8(k.clone()).ok())
            }
            path if path.starts_with("$.") => {
                // JSON path extraction from value
                entry.value.as_ref().and_then(|v| {
                    // Parse value as JSON
                    let json_str = String::from_utf8(v.clone()).ok()?;
                    let json: serde_json::Value = serde_json::from_str(&json_str).ok()?;

                    // Navigate JSON path (simple implementation for common paths)
                    // Full JSONPath support would require a library like jsonpath-rust
                    Self::extract_json_path(&json, path)
                })
            }
            _ => {
                // Unknown field type, try as value
                entry.value.as_ref().and_then(|v| String::from_utf8(v.clone()).ok())
            }
        }
    }

    /// Simple JSON path extraction (supports basic dot notation)
    ///
    /// Examples:
    /// - "$.message" -> json["message"]
    /// - "$.data.text" -> json["data"]["text"]
    /// - "$.items[0].content" -> json["items"][0]["content"]
    fn extract_json_path(json: &serde_json::Value, path: &str) -> Option<String> {
        // Remove "$." prefix
        let path = path.strip_prefix("$.")?;

        let mut current = json;

        for part in path.split('.') {
            // Handle array indexing like "items[0]"
            if let Some(bracket_pos) = part.find('[') {
                let field_name = &part[..bracket_pos];
                let index_str = &part[bracket_pos + 1..part.len() - 1];
                let index: usize = index_str.parse().ok()?;

                current = current.get(field_name)?.get(index)?;
            } else {
                current = current.get(part)?;
            }
        }

        // Convert final value to string
        match current {
            serde_json::Value::String(s) => Some(s.clone()),
            serde_json::Value::Number(n) => Some(n.to_string()),
            serde_json::Value::Bool(b) => Some(b.to_string()),
            _ => Some(current.to_string()), // Arrays/objects as JSON string
        }
    }

    /// Extract time partition key from timestamp based on partitioning strategy
    fn extract_time_partition_key(timestamp_ms: i64, config: &ColumnarConfig) -> Option<String> {
        match config.partitioning {
            PartitioningStrategy::None => None,
            PartitioningStrategy::Hourly => {
                let hour = timestamp_ms / (1000 * 60 * 60);
                Some(format!("{:010}", hour))
            }
            PartitioningStrategy::Daily => {
                let day = timestamp_ms / (1000 * 60 * 60 * 24);
                Some(format!("{:010}", day))
            }
        }
    }

    /// v2.2.21: Build ColumnarConfig from TopicConfig settings
    fn build_columnar_config(topic_config: &chronik_common::metadata::traits::TopicConfig) -> ColumnarConfig {
        let mut config = ColumnarConfig {
            // Compression from the topic config
            compression: match topic_config.columnar_compression() {
                "none" => CompressionCodec::None,
                "snappy" => CompressionCodec::Snappy,
                "gzip" => CompressionCodec::Gzip,
                "lz4" => CompressionCodec::Lz4,
                "zstd" => CompressionCodec::Zstd,
                _ => CompressionCodec::Zstd, // Default
            },
            ..Default::default()
        };

        // Set row group size
        config.row_group_size = topic_config.columnar_row_group_size();

        // Set partitioning strategy
        config.partitioning = match topic_config.columnar_partitioning() {
            "none" => PartitioningStrategy::None,
            "hourly" => PartitioningStrategy::Hourly,
            "daily" => PartitioningStrategy::Daily,
            _ => PartitioningStrategy::Hourly, // Default
        };

        config
    }

    /// v2.2.21: Generate Parquet file path with time-based partitioning
    fn generate_parquet_path(
        base_path: &str,
        topic: &str,
        partition: i32,
        min_offset: i64,
        max_offset: i64,
        min_timestamp: i64,
        config: &ColumnarConfig,
    ) -> String {
        match config.partitioning {
            PartitioningStrategy::None => {
                format!(
                    "{}/{}/partition={}/{:020}-{:020}.parquet",
                    base_path, topic, partition, min_offset, max_offset
                )
            }
            PartitioningStrategy::Hourly => {
                // Convert timestamp to hour bucket (milliseconds to hours since epoch)
                let hour = min_timestamp / (1000 * 60 * 60);
                format!(
                    "{}/{}/partition={}/hour={:010}/{:020}-{:020}.parquet",
                    base_path, topic, partition, hour, min_offset, max_offset
                )
            }
            PartitioningStrategy::Daily => {
                // Convert timestamp to day bucket (milliseconds to days since epoch)
                let day = min_timestamp / (1000 * 60 * 60 * 24);
                format!(
                    "{}/{}/partition={}/day={:010}/{:020}-{:020}.parquet",
                    base_path, topic, partition, day, min_offset, max_offset
                )
            }
        }
    }

    /// v2.2.23: Generate Parquet file path for object storage (S3/GCS/Azure)
    ///
    /// Generates paths suitable for cloud object storage with time-based partitioning.
    /// Path format: `{prefix}/{topic}/partition={partition}/[hour|day=X/]{offset_range}.parquet`
    ///
    /// This path format is compatible with:
    /// - Amazon S3 (`s3://bucket/...`)
    /// - Google Cloud Storage (`gs://bucket/...`)
    /// - Azure Blob Storage (`az://container/...`)
    /// - Local object store emulator (MinIO, Azurite)
    fn generate_parquet_s3_path(
        prefix: &str,
        topic: &str,
        partition: i32,
        min_offset: i64,
        max_offset: i64,
        min_timestamp: i64,
        config: &ColumnarConfig,
    ) -> String {
        // Use the same partitioning logic as local paths, but with the S3 prefix
        match config.partitioning {
            PartitioningStrategy::None => {
                format!(
                    "{}/{}/partition={}/{:020}-{:020}.parquet",
                    prefix, topic, partition, min_offset, max_offset
                )
            }
            PartitioningStrategy::Hourly => {
                let hour = min_timestamp / (1000 * 60 * 60);
                format!(
                    "{}/{}/partition={}/hour={:010}/{:020}-{:020}.parquet",
                    prefix, topic, partition, hour, min_offset, max_offset
                )
            }
            PartitioningStrategy::Daily => {
                let day = min_timestamp / (1000 * 60 * 60 * 24);
                format!(
                    "{}/{}/partition={}/day={:010}/{:020}-{:020}.parquet",
                    prefix, topic, partition, day, min_offset, max_offset
                )
            }
        }
    }

    /// v2.3.1: Backfill vector embeddings from existing Tier 2 raw segments.
    ///
    /// When vector embedding is enabled after data is already loaded, existing
    /// sealed segments have been processed (Tantivy/Parquet) and deleted from the WAL.
    /// This method reads those segments from object store and feeds them through the
    /// embedding pipeline, skipping offsets already in the HNSW index.
    ///
    /// Returns stats about the backfill operation.
    /// Embed `records` of a vector-enabled topic into its default-model index,
    /// skipping offsets the index already holds. Used by the vector catch-up
    /// (chronik-server `vector_catchup`), which re-reads from the log what the
    /// seal-time pass failed to embed or never saw.
    pub async fn embed_records(
        &self,
        topic: &str,
        partition: i32,
        records: Vec<CanonicalRecord>,
    ) -> Result<EmbedOutcome> {
        let tp = TopicPartition::new(topic.to_string(), partition);
        let hot_vec = self.hot_vector_index.read().await.clone();
        Self::process_vector_embeddings_with_model(
            &self.config,
            &self.metadata_store,
            &self.vector_index_manager,
            &tp,
            records,
            None,
            hot_vec,
        )
        .await
    }

    /// Write records read back from the log to this partition's Parquet, as
    /// the seal-time pass would have, skipping offsets cold already covers.
    /// Used by the columnar catch-up. Returns (bytes written, end of the
    /// contiguous run cold now holds); the SQL hot buffer is told that end.
    pub async fn write_parquet_records(
        &self,
        topic: &str,
        partition: i32,
        records: Vec<CanonicalRecord>,
    ) -> Result<(u64, i64)> {
        let tp = TopicPartition::new(topic.to_string(), partition);
        let (bytes, flushed) = Self::create_parquet_segment(
            &self.config,
            &self.object_store,
            &self.metadata_store,
            &self.segment_index,
            &tp,
            records,
        )
        .await?;
        if flushed >= 0 {
            if let Some(hb) = self.hot_buffer.read().await.as_ref() {
                hb.set_flushed_offset(topic, partition, flushed);
            }
        }
        Ok((bytes, flushed))
    }

    /// The offset ranges this partition's Parquet segments cover, merged.
    pub async fn parquet_coverage(&self, topic: &str, partition: i32) -> Result<Vec<(i64, i64)>> {
        Self::cold_ranges(&self.metadata_store, &TopicPartition::new(topic.to_string(), partition)).await
    }

    pub async fn backfill_vector_embeddings(
        &self,
        topic: &str,
        partition: Option<i32>,
        model_id: Option<&str>,
    ) -> Result<BackfillStats> {
        let mut stats = BackfillStats { model_id: model_id.map(|s| s.to_string()), ..Default::default() };
        let start_time = std::time::Instant::now();

        // 1. Verify topic exists and has vector enabled
        let topic_meta = self.metadata_store.get_topic(topic).await
            .map_err(|e| Error::Internal(format!("Failed to get topic: {}", e)))?
            .ok_or_else(|| Error::Internal(format!("Topic '{}' not found", topic)))?;

        if !topic_meta.config.is_vector_enabled() {
            return Err(Error::Internal(format!(
                "Topic '{}' does not have vector search enabled", topic
            )));
        }

        info!(topic = %topic, partition = ?partition, "Starting vector embedding backfill");

        // 2. List raw segments from metadata store
        let segments = self.metadata_store.list_segments(topic, partition.map(|p| p as u32)).await
            .map_err(|e| Error::Internal(format!("Failed to list segments: {}", e)))?;

        if segments.is_empty() {
            info!(topic = %topic, "No segments found for backfill");
            stats.duration_secs = start_time.elapsed().as_secs();
            return Ok(stats);
        }

        info!(
            topic = %topic,
            segment_count = segments.len(),
            "Found segments for vector backfill"
        );

        // 3. Process each segment
        for segment in &segments {
            // Check if we should skip this partition
            if let Some(p) = partition {
                if segment.partition as i32 != p {
                    continue;
                }
            }

            // Check HNSW index last_offset to skip already-embedded segments
            let tp = TopicPartition::new(topic.to_string(), segment.partition as i32);
            let last_indexed_offset = self.vector_index_manager
                .get_partition_stats(&tp.topic, tp.partition).await
                .and_then(|s| s.last_offset)
                .unwrap_or(-1);

            if segment.end_offset <= last_indexed_offset {
                debug!(
                    topic = %topic,
                    partition = segment.partition,
                    segment_end = segment.end_offset,
                    last_indexed = last_indexed_offset,
                    "Skipping segment (already embedded)"
                );
                stats.skipped_segments += 1;
                continue;
            }

            // Read raw segment from object store
            let data = match self.object_store.get(&segment.path).await {
                Ok(d) => d,
                Err(e) => {
                    warn!(
                        topic = %topic,
                        partition = segment.partition,
                        path = %segment.path,
                        error = %e,
                        "Failed to read segment from object store, skipping"
                    );
                    stats.errors += 1;
                    continue;
                }
            };

            // Decode the raw segment (compressed or plain)
            let canonical_records: Vec<CanonicalRecord> = match crate::archive::decode_raw_segment(&data) {
                Ok(r) => r,
                Err(e) => {
                    warn!(
                        topic = %topic,
                        partition = segment.partition,
                        path = %segment.path,
                        error = %e,
                        "Failed to deserialize segment, skipping"
                    );
                    stats.errors += 1;
                    continue;
                }
            };

            if canonical_records.is_empty() {
                continue;
            }

            // Run through the vector embedding pipeline.
            // HP-2 follow-up A: pass the hot vector index so already-embedded
            // offsets skip the provider call.
            let hot_vec = self.hot_vector_index.read().await.clone();
            match Self::process_vector_embeddings_with_model(
                &self.config,
                &self.metadata_store,
                &self.vector_index_manager,
                &tp,
                canonical_records,
                model_id,
                hot_vec,
            ).await {
                Ok(outcome) => {
                    let embeddings_generated = outcome.embedded;
                    info!(
                        topic = %topic,
                        partition = segment.partition,
                        segment = %segment.segment_id,
                        embeddings = embeddings_generated,
                        "Backfill: generated embeddings for segment"
                    );
                    stats.vectors_generated += embeddings_generated;
                    stats.segments_processed += 1;
                }
                Err(e) => {
                    error!(
                        topic = %topic,
                        partition = segment.partition,
                        segment = %segment.segment_id,
                        error = %e,
                        "Backfill: failed to generate embeddings for segment"
                    );
                    stats.errors += 1;
                }
            }
        }

        stats.duration_secs = start_time.elapsed().as_secs();
        info!(
            topic = %topic,
            segments_processed = stats.segments_processed,
            vectors_generated = stats.vectors_generated,
            skipped = stats.skipped_segments,
            errors = stats.errors,
            duration_secs = stats.duration_secs,
            "Vector embedding backfill complete"
        );

        Ok(stats)
    }
}

/// What one embedding step did with a batch of records.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct EmbedOutcome {
    /// Records that yielded text to embed (the rest have nothing to embed).
    pub texts: usize,
    /// Vectors added.
    pub embedded: usize,
    /// Texts not embedded: provider errors, or no provider at all.
    pub failed: usize,
}

/// Statistics for a vector embedding backfill operation
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct BackfillStats {
    pub segments_processed: usize,
    pub vectors_generated: usize,
    pub skipped_segments: usize,
    pub errors: usize,
    pub duration_secs: u64,
    /// Model ID used for this backfill (None = default model)
    pub model_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_indexer_config_default() {
        let config = WalIndexerConfig::default();
        assert_eq!(config.interval_secs, 30);
        assert_eq!(config.min_segment_age_secs, 10);
        assert!(config.delete_after_index);
    }

    fn topics(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    /// A topic missing from metadata once is NOT reclaimed.
    ///
    /// This is the restart case: message-WAL recovery finishes before the
    /// catalog is populated, so a live topic is briefly missing. Reclaiming on
    /// first sight deleted 140 live records two milliseconds after recovery
    /// reported them loaded — they returned only because other replicas had
    /// them.
    #[test]
    fn a_topic_missing_once_is_not_reclaimed() {
        let mut passes = HashMap::new();
        assert!(WalIndexer::confirm_orphans(&mut passes, topics(&["orders"])).is_empty());
    }

    /// Missing on enough consecutive passes and it is reclaimed — a genuinely
    /// deleted topic must not pin its WAL forever.
    #[test]
    fn a_topic_missing_on_every_pass_is_eventually_reclaimed() {
        let mut passes = HashMap::new();
        for _ in 1..ORPHAN_CONFIRM_PASSES {
            assert!(WalIndexer::confirm_orphans(&mut passes, topics(&["orders"])).is_empty());
        }
        assert_eq!(
            WalIndexer::confirm_orphans(&mut passes, topics(&["orders"])),
            topics(&["orders"])
        );
    }

    /// The passes must be CONSECUTIVE. A topic that reappears — metadata caught
    /// up, or it was recreated — starts again from zero, so intermittent
    /// absence never accumulates into a deletion.
    #[test]
    fn a_topic_that_reappears_starts_over() {
        let mut passes = HashMap::new();
        for _ in 1..ORPHAN_CONFIRM_PASSES {
            WalIndexer::confirm_orphans(&mut passes, topics(&["orders"]));
        }

        // Seen again: not a candidate this pass.
        WalIndexer::confirm_orphans(&mut passes, topics(&[]));

        assert!(
            WalIndexer::confirm_orphans(&mut passes, topics(&["orders"])).is_empty(),
            "an intermittently-missing topic must never accumulate its way to deletion"
        );
    }

    /// Topics are counted independently.
    #[test]
    fn confirmation_is_per_topic() {
        let mut passes = HashMap::new();
        for _ in 1..ORPHAN_CONFIRM_PASSES {
            WalIndexer::confirm_orphans(&mut passes, topics(&["old"]));
        }
        let confirmed = WalIndexer::confirm_orphans(&mut passes, topics(&["old", "new"]));
        assert_eq!(confirmed, topics(&["old"]), "'new' has only been missing once");
    }

    /// Follower progress stub: returns whatever the test dictates per partition.
    struct StubProgress(std::collections::HashMap<(String, i32), Option<i64>>);

    impl ReplicationProgress for StubProgress {
        fn min_replicated_offset(&self, topic: &str, partition: i32) -> Option<i64> {
            self.0
                .get(&(topic.to_string(), partition))
                .copied()
                .flatten()
        }
    }

    fn stub(entries: &[(&str, i32, Option<i64>)]) -> Arc<dyn ReplicationProgress> {
        let mut m = std::collections::HashMap::new();
        for (t, p, v) in entries {
            m.insert((t.to_string(), *p), *v);
        }
        Arc::new(StubProgress(m))
    }

    /// RP-1.1: a WAL segment must survive until its records reach the followers.
    ///
    /// Indexing is not replication. Nothing re-sends what a follower missed, so
    /// deleting un-replicated WAL strands that data on a single node with no
    /// recovery path — the same loss shape as v2.10.10 (deleting WAL after a
    /// failed object-store upload), reached from a different direction.
    #[test]
    fn retention_interlock_holds_unreplicated_segments() {
        let tp = TopicPartition::new("orders".to_string(), 0);

        // No progress source at all (single node) → no interlock, unchanged behaviour.
        assert!(WalIndexer::fully_replicated(None, &[(tp.clone(), 100)]).is_ok());

        // Followers have everything → safe to delete.
        let caught_up = stub(&[("orders", 0, Some(100))]);
        assert!(WalIndexer::fully_replicated(Some(&caught_up), &[(tp.clone(), 100)]).is_ok());

        // Followers are behind → hold the segment.
        let behind = stub(&[("orders", 0, Some(99))]);
        assert!(WalIndexer::fully_replicated(Some(&behind), &[(tp.clone(), 100)]).is_err());

        // No live follower tracked for the partition → no interlock. A dead
        // replica must not pin WAL forever; it has to resync instead.
        let unknown = stub(&[("orders", 0, None)]);
        assert!(WalIndexer::fully_replicated(Some(&unknown), &[(tp.clone(), 100)]).is_ok());
    }

    /// A segment spanning partitions is held if ANY partition is behind — the
    /// segment is deleted as a unit, so one lagging partition protects all of it.
    #[test]
    fn retention_interlock_is_per_segment_not_per_partition() {
        let a = TopicPartition::new("orders".to_string(), 0);
        let b = TopicPartition::new("orders".to_string(), 1);

        let mixed = stub(&[("orders", 0, Some(100)), ("orders", 1, Some(5))]);
        assert!(
            WalIndexer::fully_replicated(Some(&mixed), &[(a.clone(), 100), (b.clone(), 50)]).is_err(),
            "partition 1 is behind, so the whole segment must be kept"
        );

        let both = stub(&[("orders", 0, Some(100)), ("orders", 1, Some(50))]);
        assert!(WalIndexer::fully_replicated(Some(&both), &[(a, 100), (b, 50)]).is_ok());
    }

    #[test]
    fn segment_partition_parses_the_id_from_the_right() {
        assert_eq!(WalIndexer::segment_partition("default-traces:0:7770"), Some(("default-traces".to_string(), 0)));
        assert_eq!(WalIndexer::segment_partition("a.b-c:12:1"), Some(("a.b-c".to_string(), 12)));
        assert_eq!(WalIndexer::segment_partition("nopartition"), None);
        assert_eq!(WalIndexer::segment_partition(":1:2"), None);
    }

    #[test]
    fn may_delete_wal_segment_only_on_clean_pass() {
        // Clean pass (no new errors) with deletion enabled -> may delete.
        assert!(WalIndexer::may_delete_wal_segment(true, 5, 5));
        // New errors during the pass (e.g. a failed raw-segment upload) -> KEEP
        // the WAL copy so the next run can re-upload it. Deleting here would
        // lose the data from both the WAL and the object store.
        assert!(!WalIndexer::may_delete_wal_segment(true, 5, 6));
        // Deletion disabled -> never delete, regardless of errors.
        assert!(!WalIndexer::may_delete_wal_segment(false, 5, 5));
        assert!(!WalIndexer::may_delete_wal_segment(false, 5, 6));
    }

    /// Issue #57: the retry had no bound, so one segment that could not be
    /// indexed re-ran every 30s forever and the I/O starved `acks=all` produce.
    #[test]
    fn the_retry_backoff_doubles_and_is_capped() {
        assert_eq!(backoff_passes(1), 1, "the first retry is the next pass");
        assert_eq!(backoff_passes(2), 2);
        assert_eq!(backoff_passes(3), 4);
        assert_eq!(backoff_passes(4), 8);

        // Capped, so a segment that recovers is not parked for hours.
        assert_eq!(backoff_passes(20), MAX_SKIP_PASSES);
        // And no shift overflow at the extreme.
        assert_eq!(backoff_passes(u32::MAX), MAX_SKIP_PASSES);
        assert_eq!(
            backoff_passes(0),
            1,
            "a zero-attempt entry must still wait a pass rather than spin"
        );
    }

    /// A failure must set a wait, a further failure must lengthen it, and a
    /// clean pass must clear it — otherwise a segment that fails once is slowly
    /// starved for the rest of the process's life.
    #[tokio::test]
    async fn a_failure_backs_off_and_a_clean_pass_clears_it() {
        let failures: Arc<RwLock<HashMap<String, SegmentRetry>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let seg = "default-logs:0:4";

        WalIndexer::record_segment_failure(&failures, seg).await;
        assert_eq!(
            failures.read().await.get(seg),
            Some(&SegmentRetry { attempts: 1, skip_passes: 1 })
        );

        WalIndexer::record_segment_failure(&failures, seg).await;
        WalIndexer::record_segment_failure(&failures, seg).await;
        let after_three = failures.read().await.get(seg).cloned().unwrap();
        assert_eq!(after_three.attempts, 3);
        assert_eq!(
            after_three.skip_passes, 4,
            "the wait must grow with consecutive failures"
        );

        // What the clean-pass branch does.
        failures.write().await.remove(seg);
        assert!(failures.read().await.get(seg).is_none());

        // A fresh failure starts over rather than resuming the old backoff.
        WalIndexer::record_segment_failure(&failures, seg).await;
        assert_eq!(failures.read().await.get(seg).unwrap().attempts, 1);
    }

    /// The quarantine threshold has to be reachable by repeated failure, and it
    /// must not be so low that a couple of transient object-store errors stop a
    /// segment being indexed at all.
    #[test]
    fn quarantine_is_reachable_but_not_hair_trigger() {
        const {
            assert!(MAX_SEGMENT_ATTEMPTS >= 4, "a transient failure must get several attempts")
        };
        // The passes a segment waits before its final attempt, summed, is the
        // grace period an operator gets to notice the warnings.
        let total_waited: u32 = (1..MAX_SEGMENT_ATTEMPTS).map(backoff_passes).sum();
        assert!(
            total_waited >= 15,
            "quarantine must take many passes to reach, got {total_waited}"
        );
    }

    #[test]
    fn test_topic_partition_equality() {
        let tp1 = TopicPartition::new("test".to_string(), 0);
        let tp2 = TopicPartition::new("test".to_string(), 0);
        let tp3 = TopicPartition::new("test".to_string(), 1);

        assert_eq!(tp1, tp2);
        assert_ne!(tp1, tp3);
    }

    #[test]
    fn test_generate_parquet_path_no_partitioning() {
        let config = ColumnarConfig {
            partitioning: PartitioningStrategy::None,
            ..ColumnarConfig::default()
        };

        let path = WalIndexer::generate_parquet_path(
            "/data/columnar",
            "my-topic",
            0,
            100,
            199,
            1704067200000, // 2024-01-01T00:00:00Z
            &config,
        );

        assert!(path.starts_with("/data/columnar/my-topic/partition=0/"));
        assert!(path.ends_with("00000000000000000100-00000000000000000199.parquet"));
    }

    #[test]
    fn test_generate_parquet_path_hourly_partitioning() {
        let config = ColumnarConfig {
            partitioning: PartitioningStrategy::Hourly,
            ..ColumnarConfig::default()
        };

        let path = WalIndexer::generate_parquet_path(
            "/data/columnar",
            "my-topic",
            2,
            0,
            99,
            1704067200000, // 2024-01-01T00:00:00Z = hour 473352
            &config,
        );

        assert!(path.contains("/partition=2/"));
        assert!(path.contains("/hour="));
        assert!(path.ends_with(".parquet"));
    }

    #[test]
    fn test_generate_parquet_path_daily_partitioning() {
        let config = ColumnarConfig {
            partitioning: PartitioningStrategy::Daily,
            ..ColumnarConfig::default()
        };

        let path = WalIndexer::generate_parquet_path(
            "/data/columnar",
            "events",
            1,
            1000,
            2000,
            1704067200000, // 2024-01-01 = day 19723
            &config,
        );

        assert!(path.contains("/partition=1/"));
        assert!(path.contains("/day="));
        assert!(path.ends_with(".parquet"));
    }

    #[test]
    fn test_extract_time_partition_key() {
        // Test hourly partitioning - returns just the hour number as a zero-padded string
        let hourly_config = ColumnarConfig {
            partitioning: PartitioningStrategy::Hourly,
            ..ColumnarConfig::default()
        };
        let hourly_key = WalIndexer::extract_time_partition_key(1704067200000, &hourly_config);
        assert!(hourly_key.is_some());
        // 1704067200000 ms = 1704067200 s = 473352 hours
        assert_eq!(hourly_key.unwrap(), "0000473352");

        // Test daily partitioning - returns just the day number as a zero-padded string
        let daily_config = ColumnarConfig {
            partitioning: PartitioningStrategy::Daily,
            ..ColumnarConfig::default()
        };
        let daily_key = WalIndexer::extract_time_partition_key(1704067200000, &daily_config);
        assert!(daily_key.is_some());
        // 1704067200000 ms = 1704067200 s = 19723 days
        assert_eq!(daily_key.unwrap(), "0000019723");

        // Test no partitioning
        let none_config = ColumnarConfig {
            partitioning: PartitioningStrategy::None,
            ..ColumnarConfig::default()
        };
        let none_key = WalIndexer::extract_time_partition_key(1704067200000, &none_config);
        assert!(none_key.is_none());
    }

    // v2.2.22: Full integration test for create_parquet_segment
    #[tokio::test]
    async fn test_create_parquet_segment_integration() {
        use crate::object_store::{LocalBackend, ObjectStoreConfig, StorageBackend, ObjectStore};
        use chronik_common::metadata::{InMemoryMetadataStore, TopicConfig};
        use tempfile::tempdir;
        use std::collections::HashMap;

        // 1. Create a temp directory for object store
        let temp_dir = tempdir().unwrap();
        let object_store_path = temp_dir.path().join("parquet");
        std::fs::create_dir_all(&object_store_path).unwrap();

        // 2. Create LocalBackend object store
        let config = ObjectStoreConfig {
            backend: StorageBackend::Local { path: object_store_path.to_string_lossy().to_string() },
            ..Default::default()
        };
        let object_store: Arc<dyn ObjectStore> = Arc::new(LocalBackend::new(config).await.unwrap());

        // 3. Create InMemoryMetadataStore with columnar-enabled topic
        let metadata_store: Arc<dyn MetadataStore> = Arc::new(InMemoryMetadataStore::new());
        let mut topic_config_map = HashMap::new();
        topic_config_map.insert("columnar.enabled".to_string(), "true".to_string());
        topic_config_map.insert("columnar.format".to_string(), "parquet".to_string());
        topic_config_map.insert("columnar.compression".to_string(), "zstd".to_string());

        let topic_config = TopicConfig {
            partition_count: 1,
            replication_factor: 1,
            retention_ms: None,
            segment_bytes: 1024 * 1024 * 1024, // 1GB
            config: topic_config_map,
        };
        metadata_store.create_topic("test-columnar-topic", topic_config).await.unwrap();

        // 4. Create SegmentIndex
        let segment_index = Arc::new(SegmentIndex::new());

        // 5. Create WalIndexerConfig
        let indexer_config = WalIndexerConfig {
            columnar_base_path: object_store_path.to_string_lossy().to_string(),
            ..Default::default()
        };

        // 6. Create CanonicalRecords with test data
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;

        let records = vec![
            CanonicalRecord {
                base_offset: 0,
                partition_leader_epoch: 0,
                producer_id: -1,
                producer_epoch: -1,
                base_sequence: -1,
                is_transactional: false,
                is_control: false,
                compression: crate::canonical_record::CompressionType::None,
                timestamp_type: TimestampType::CreateTime,
                base_timestamp: now,
                max_timestamp: now + 200,
                records: vec![
                    CanonicalRecordEntry {
                        offset: 0,
                        timestamp: now,
                        key: Some(b"key-0".to_vec()),
                        value: Some(b"{\"id\": 0, \"message\": \"Hello\"}".to_vec()),
                        headers: vec![],
                        attributes: 0,
                    },
                    CanonicalRecordEntry {
                        offset: 1,
                        timestamp: now + 100,
                        key: Some(b"key-1".to_vec()),
                        value: Some(b"{\"id\": 1, \"message\": \"World\"}".to_vec()),
                        headers: vec![],
                        attributes: 0,
                    },
                    CanonicalRecordEntry {
                        offset: 2,
                        timestamp: now + 200,
                        key: Some(b"key-2".to_vec()),
                        value: Some(b"{\"id\": 2, \"message\": \"Test\"}".to_vec()),
                        headers: vec![],
                        attributes: 0,
                    },
                ],
                compressed_records_wire_bytes: None,
                original_v1_wire_format: None,
                original_v2_wire_format: None,
            },
        ];

        // 7. Create TopicPartition and call create_parquet_segment
        let tp = TopicPartition::new("test-columnar-topic".to_string(), 0);

        let (bytes_written, max_offset) = WalIndexer::create_parquet_segment(
            &indexer_config,
            &object_store,
            &metadata_store,
            &segment_index,
            &tp,
            records,
        ).await.unwrap();

        // 8. Verify bytes were written (create_parquet_segment returns (bytes, max_offset))
        assert!(bytes_written > 0, "Expected bytes to be written to Parquet file");
        assert!(max_offset >= 0, "Expected valid max_offset from Parquet segment");

        // 9. Verify Parquet segment was registered in SegmentIndex
        let parquet_paths = segment_index.get_parquet_paths("test-columnar-topic").await.unwrap();
        assert!(!parquet_paths.is_empty(), "Expected at least one Parquet path registered");

        // 10. Verify the Parquet file exists on disk
        // Find files in the object store path
        let mut found_parquet = false;
        for entry in walkdir::WalkDir::new(&object_store_path)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if entry.path().extension().map(|e| e == "parquet").unwrap_or(false) {
                found_parquet = true;
                // Verify it's a valid Parquet file by checking magic bytes
                let file_content = std::fs::read(entry.path()).unwrap();
                assert!(file_content.len() > 4, "Parquet file too small");
                assert_eq!(&file_content[0..4], b"PAR1", "Invalid Parquet magic bytes");
            }
        }
        assert!(found_parquet, "Expected to find a Parquet file in object store path");
    }

    // v2.4.1: Test that JSON columns appear in Parquet output
    #[tokio::test]
    async fn test_create_parquet_segment_with_json_columns() {
        use crate::object_store::{LocalBackend, ObjectStoreConfig, StorageBackend, ObjectStore};
        use chronik_common::metadata::{InMemoryMetadataStore, TopicConfig};
        use tempfile::tempdir;
        use std::collections::HashMap;

        let temp_dir = tempdir().unwrap();
        let object_store_path = temp_dir.path().join("parquet");
        std::fs::create_dir_all(&object_store_path).unwrap();

        let config = ObjectStoreConfig {
            backend: StorageBackend::Local { path: object_store_path.to_string_lossy().to_string() },
            ..Default::default()
        };
        let object_store: Arc<dyn ObjectStore> = Arc::new(LocalBackend::new(config).await.unwrap());

        let metadata_store: Arc<dyn MetadataStore> = Arc::new(InMemoryMetadataStore::new());
        let mut topic_config_map = HashMap::new();
        topic_config_map.insert("columnar.enabled".to_string(), "true".to_string());

        let topic_config = TopicConfig {
            partition_count: 1,
            replication_factor: 1,
            retention_ms: None,
            segment_bytes: 1024 * 1024 * 1024,
            config: topic_config_map,
        };
        metadata_store.create_topic("json-test-topic", topic_config).await.unwrap();

        let segment_index = Arc::new(SegmentIndex::new());
        let indexer_config = WalIndexerConfig {
            columnar_base_path: object_store_path.to_string_lossy().to_string(),
            ..Default::default()
        };

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;

        // Create JSON records with typed fields
        let records = vec![
            CanonicalRecord {
                base_offset: 0,
                partition_leader_epoch: 0,
                producer_id: -1,
                producer_epoch: -1,
                base_sequence: -1,
                is_transactional: false,
                is_control: false,
                compression: crate::canonical_record::CompressionType::None,
                timestamp_type: TimestampType::CreateTime,
                base_timestamp: now,
                max_timestamp: now + 200,
                records: vec![
                    CanonicalRecordEntry {
                        offset: 0,
                        timestamp: now,
                        key: Some(b"k0".to_vec()),
                        value: Some(br#"{"user_id":1,"name":"Alice","score":9.5,"active":true}"#.to_vec()),
                        headers: vec![],
                        attributes: 0,
                    },
                    CanonicalRecordEntry {
                        offset: 1,
                        timestamp: now + 100,
                        key: Some(b"k1".to_vec()),
                        value: Some(br#"{"user_id":2,"name":"Bob","score":8.0,"active":false}"#.to_vec()),
                        headers: vec![],
                        attributes: 0,
                    },
                    CanonicalRecordEntry {
                        offset: 2,
                        timestamp: now + 200,
                        key: Some(b"k2".to_vec()),
                        value: Some(br#"{"user_id":3,"name":"Charlie","score":7.2,"active":true}"#.to_vec()),
                        headers: vec![],
                        attributes: 0,
                    },
                ],
                compressed_records_wire_bytes: None,
                original_v1_wire_format: None,
                original_v2_wire_format: None,
            },
        ];

        let tp = TopicPartition::new("json-test-topic".to_string(), 0);
        let (bytes_written, max_offset) = WalIndexer::create_parquet_segment(
            &indexer_config,
            &object_store,
            &metadata_store,
            &segment_index,
            &tp,
            records,
        ).await.unwrap();

        assert!(bytes_written > 0);
        assert_eq!(max_offset, 2);

        // Verify JSON schema was persisted to topic config
        let updated_topic = metadata_store.get_topic("json-test-topic").await.unwrap().unwrap();
        let schema_str = updated_topic.config.json_schema()
            .expect("JSON schema should have been persisted to topic config");
        assert!(schema_str.contains("user_id"));
        assert!(schema_str.contains("name"));
        assert!(schema_str.contains("score"));
        assert!(schema_str.contains("active"));

        // Verify the Parquet file has JSON columns by reading it
        let mut parquet_path = None;
        for entry in walkdir::WalkDir::new(&object_store_path)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if entry.path().extension().map(|e| e == "parquet").unwrap_or(false) {
                parquet_path = Some(entry.path().to_path_buf());
                break;
            }
        }
        let parquet_path = parquet_path.expect("Should find Parquet file");

        // Read the Parquet file schema — this is the critical verification
        let reader = chronik_columnar::ParquetSegmentReader::new(Default::default());
        let schema = reader.read_schema(&parquet_path).unwrap();
        let field_names: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();

        // Base columns should be present
        assert!(field_names.contains(&"_topic"), "Missing _topic column");
        assert!(field_names.contains(&"_partition"), "Missing _partition column");
        assert!(field_names.contains(&"_offset"), "Missing _offset column");
        assert!(field_names.contains(&"_value"), "Missing _value column");

        // JSON-inferred columns should also be present
        assert!(field_names.contains(&"user_id"), "Missing user_id JSON column, fields: {:?}", field_names);
        assert!(field_names.contains(&"name"), "Missing name JSON column, fields: {:?}", field_names);
        assert!(field_names.contains(&"score"), "Missing score JSON column, fields: {:?}", field_names);
        assert!(field_names.contains(&"active"), "Missing active JSON column, fields: {:?}", field_names);

        // Verify row count from Parquet metadata
        let metadata = reader.read_metadata(&parquet_path).unwrap();
        let total_rows: i64 = metadata.row_groups().iter().map(|rg| rg.num_rows()).sum();
        assert_eq!(total_rows, 3, "Expected 3 rows in Parquet file");
    }

    /// #41: re-flushing a range already in cold must not write a second,
    /// overlapping Parquet segment. This is the restart case — the in-memory
    /// "already indexed" guard is gone, so the indexer re-reads sealed WAL
    /// segments it has already flushed. The durable cold high-water mark makes
    /// the re-flush a no-op, and genuinely new offsets still get written.
    #[tokio::test]
    async fn re_flushing_already_flushed_offsets_writes_no_duplicate_segment() {
        use crate::object_store::{LocalBackend, ObjectStoreConfig, StorageBackend, ObjectStore};
        use chronik_common::metadata::{InMemoryMetadataStore, TopicConfig};
        use tempfile::tempdir;
        use std::collections::HashMap;

        let temp_dir = tempdir().unwrap();
        let base = temp_dir.path().join("parquet");
        std::fs::create_dir_all(&base).unwrap();
        let object_store: Arc<dyn ObjectStore> = Arc::new(
            LocalBackend::new(ObjectStoreConfig {
                backend: StorageBackend::Local { path: base.to_string_lossy().to_string() },
                ..Default::default()
            })
            .await
            .unwrap(),
        );
        let metadata_store: Arc<dyn MetadataStore> = Arc::new(InMemoryMetadataStore::new());
        let mut cfg = HashMap::new();
        cfg.insert("columnar.enabled".to_string(), "true".to_string());
        metadata_store
            .create_topic(
                "reflush",
                TopicConfig {
                    partition_count: 1,
                    replication_factor: 1,
                    retention_ms: None,
                    segment_bytes: 1 << 30,
                    config: cfg,
                },
            )
            .await
            .unwrap();
        let segment_index = Arc::new(SegmentIndex::new());
        let indexer_config = WalIndexerConfig {
            columnar_base_path: base.to_string_lossy().to_string(),
            ..Default::default()
        };
        let tp = TopicPartition::new("reflush".to_string(), 0);

        // A canonical batch covering [first_off, first_off+n).
        let batch = |first_off: i64, n: i64| -> Vec<CanonicalRecord> {
            vec![CanonicalRecord {
                base_offset: first_off,
                partition_leader_epoch: 0,
                producer_id: -1,
                producer_epoch: -1,
                base_sequence: -1,
                is_transactional: false,
                is_control: false,
                compression: crate::canonical_record::CompressionType::None,
                timestamp_type: TimestampType::CreateTime,
                base_timestamp: 1000,
                max_timestamp: 1000 + n,
                records: (0..n)
                    .map(|i| CanonicalRecordEntry {
                        offset: first_off + i,
                        timestamp: 1000 + i,
                        key: None,
                        value: Some(format!("v{}", first_off + i).into_bytes()),
                        headers: vec![],
                        attributes: 0,
                    })
                    .collect(),
                compressed_records_wire_bytes: None,
                original_v1_wire_format: None,
                original_v2_wire_format: None,
            }]
        };

        let seg_count = |ms: &Arc<dyn MetadataStore>| {
            let ms = ms.clone();
            async move { ms.list_parquet_segments("reflush", Some(0)).await.unwrap().len() }
        };

        // First flush of [0,3): writes and registers one segment.
        let (b1, m1) = WalIndexer::create_parquet_segment(
            &indexer_config, &object_store, &metadata_store, &segment_index, &tp, batch(0, 3),
        ).await.unwrap();
        assert!(b1 > 0, "first flush should write bytes");
        assert_eq!(m1, 2, "highest offset is 2");
        assert_eq!(seg_count(&metadata_store).await, 1);

        // Re-flush of the SAME [0,3) (restart re-index): no new segment, but it
        // still reports the high offset so the hot buffer can evict.
        let (b2, m2) = WalIndexer::create_parquet_segment(
            &indexer_config, &object_store, &metadata_store, &segment_index, &tp, batch(0, 3),
        ).await.unwrap();
        assert_eq!(b2, 0, "re-flush must write nothing");
        assert_eq!(m2, 2, "re-flush still reports the high offset for eviction");
        assert_eq!(seg_count(&metadata_store).await, 1, "no duplicate segment");

        // A batch that overlaps the tail AND extends past it: only the new
        // offsets [3,5) are written, as one non-overlapping segment.
        let (b3, m3) = WalIndexer::create_parquet_segment(
            &indexer_config, &object_store, &metadata_store, &segment_index, &tp, batch(2, 3),
        ).await.unwrap();
        assert!(b3 > 0, "the new tail should be written");
        assert_eq!(m3, 4, "highest offset is now 4");
        assert_eq!(seg_count(&metadata_store).await, 2, "one more, non-overlapping segment");
    }

    // v2.2.22: Vector text extraction tests

    #[test]
    fn test_extract_text_from_value_field() {
        let entry = CanonicalRecordEntry {
            offset: 0,
            timestamp: 0,
            key: None,
            value: Some(b"Hello, world!".to_vec()),
            headers: vec![],
            attributes: 0,
        };

        let result = WalIndexer::extract_text_from_field("value", &entry);
        assert_eq!(result, Some("Hello, world!".to_string()));
    }

    #[test]
    fn test_extract_text_from_key_field() {
        let entry = CanonicalRecordEntry {
            offset: 0,
            timestamp: 0,
            key: Some(b"my-key".to_vec()),
            value: Some(b"my-value".to_vec()),
            headers: vec![],
            attributes: 0,
        };

        let result = WalIndexer::extract_text_from_field("key", &entry);
        assert_eq!(result, Some("my-key".to_string()));
    }

    #[test]
    fn test_extract_text_from_missing_value() {
        let entry = CanonicalRecordEntry {
            offset: 0,
            timestamp: 0,
            key: None,
            value: None,
            headers: vec![],
            attributes: 0,
        };

        let result = WalIndexer::extract_text_from_field("value", &entry);
        assert!(result.is_none());
    }

    #[test]
    fn test_extract_json_path_simple() {
        let json = serde_json::json!({
            "message": "Hello from JSON!"
        });

        let result = WalIndexer::extract_json_path(&json, "$.message");
        assert_eq!(result, Some("Hello from JSON!".to_string()));
    }

    #[test]
    fn test_extract_json_path_nested() {
        let json = serde_json::json!({
            "data": {
                "text": "Nested value"
            }
        });

        let result = WalIndexer::extract_json_path(&json, "$.data.text");
        assert_eq!(result, Some("Nested value".to_string()));
    }

    #[test]
    fn test_extract_json_path_with_array_index() {
        let json = serde_json::json!({
            "items": [
                {"content": "First"},
                {"content": "Second"}
            ]
        });

        let result = WalIndexer::extract_json_path(&json, "$.items[0].content");
        assert_eq!(result, Some("First".to_string()));

        let result2 = WalIndexer::extract_json_path(&json, "$.items[1].content");
        assert_eq!(result2, Some("Second".to_string()));
    }

    #[test]
    fn test_extract_json_path_from_record() {
        let json_value = serde_json::json!({
            "event": {
                "message": "User logged in"
            }
        });

        let entry = CanonicalRecordEntry {
            offset: 0,
            timestamp: 0,
            key: None,
            value: Some(json_value.to_string().into_bytes()),
            headers: vec![],
            attributes: 0,
        };

        let result = WalIndexer::extract_text_from_field("$.event.message", &entry);
        assert_eq!(result, Some("User logged in".to_string()));
    }

    #[test]
    fn test_extract_json_path_invalid_path() {
        let json = serde_json::json!({
            "message": "Hello"
        });

        let result = WalIndexer::extract_json_path(&json, "$.nonexistent");
        assert!(result.is_none());
    }

    #[test]
    fn test_extract_json_path_number_value() {
        let json = serde_json::json!({
            "count": 42
        });

        let result = WalIndexer::extract_json_path(&json, "$.count");
        assert_eq!(result, Some("42".to_string()));
    }

    #[test]
    fn test_extract_json_path_boolean_value() {
        let json = serde_json::json!({
            "active": true
        });

        let result = WalIndexer::extract_json_path(&json, "$.active");
        assert_eq!(result, Some("true".to_string()));
    }

    // v2.2.23: S3 path generation tests

    #[test]
    fn test_generate_parquet_s3_path_no_partitioning() {
        let config = ColumnarConfig {
            partitioning: PartitioningStrategy::None,
            ..ColumnarConfig::default()
        };

        let path = WalIndexer::generate_parquet_s3_path(
            "columnar",
            "my-topic",
            0,
            100,
            199,
            1704067200000, // 2024-01-01T00:00:00Z
            &config,
        );

        assert_eq!(
            path,
            "columnar/my-topic/partition=0/00000000000000000100-00000000000000000199.parquet"
        );
    }

    #[test]
    fn test_generate_parquet_s3_path_hourly_partitioning() {
        let config = ColumnarConfig {
            partitioning: PartitioningStrategy::Hourly,
            ..ColumnarConfig::default()
        };

        let path = WalIndexer::generate_parquet_s3_path(
            "s3-prefix",
            "events",
            2,
            0,
            99,
            1704067200000, // 2024-01-01T00:00:00Z = hour 473352
            &config,
        );

        // Verify it has the correct structure
        assert!(path.starts_with("s3-prefix/events/partition=2/hour="));
        assert!(path.contains("/hour="));
        assert!(path.ends_with(".parquet"));
    }

    #[test]
    fn test_generate_parquet_s3_path_daily_partitioning() {
        let config = ColumnarConfig {
            partitioning: PartitioningStrategy::Daily,
            ..ColumnarConfig::default()
        };

        let path = WalIndexer::generate_parquet_s3_path(
            "my-bucket/columnar",
            "logs",
            1,
            1000,
            2000,
            1704067200000, // 2024-01-01 = day 19723
            &config,
        );

        // Verify it has the correct structure
        assert!(path.starts_with("my-bucket/columnar/logs/partition=1/day="));
        assert!(path.contains("/day="));
        assert!(path.ends_with(".parquet"));
    }

    #[test]
    fn test_generate_parquet_s3_path_matches_local_structure() {
        // Verify S3 path structure matches local path structure
        let config = ColumnarConfig {
            partitioning: PartitioningStrategy::Hourly,
            ..ColumnarConfig::default()
        };

        let local_path = WalIndexer::generate_parquet_path(
            "/data/columnar",
            "test-topic",
            0,
            100,
            200,
            1704067200000,
            &config,
        );

        let s3_path = WalIndexer::generate_parquet_s3_path(
            "columnar",
            "test-topic",
            0,
            100,
            200,
            1704067200000,
            &config,
        );

        // Strip the base paths and compare the rest
        let local_suffix = local_path.strip_prefix("/data/columnar/").unwrap();
        let s3_suffix = s3_path.strip_prefix("columnar/").unwrap();

        assert_eq!(local_suffix, s3_suffix, "S3 and local paths should have identical structure after prefix");
    }
}

/// Sort key for a sealed segment id (`topic:partition:sequence`): by topic and
/// partition, then by sequence numerically, so each partition's segments come
/// oldest first. An id of another shape sorts after, by its text.
fn segment_order_key(segment: &str) -> (String, i64, u64, String) {
    let mut parts = segment.rsplitn(3, ':');
    let seq = parts.next().and_then(|s| s.parse::<u64>().ok());
    let partition = parts.next().and_then(|p| p.parse::<i64>().ok());
    match (parts.next(), partition, seq) {
        (Some(topic), Some(partition), Some(seq)) => (topic.to_string(), partition, seq, String::new()),
        _ => (String::from("\u{10FFFF}"), i64::MAX, u64::MAX, segment.to_string()),
    }
}

/// Inclusive offset ranges sorted and merged; ranges that overlap or touch
/// (`next.min == prev.max + 1`) become one.
fn merge_ranges(mut ranges: Vec<(i64, i64)>) -> Vec<(i64, i64)> {
    ranges.retain(|(lo, hi)| lo <= hi);
    ranges.sort_unstable();
    let mut merged: Vec<(i64, i64)> = Vec::with_capacity(ranges.len());
    for (lo, hi) in ranges {
        match merged.last_mut() {
            Some(last) if lo <= last.1.saturating_add(1) => last.1 = last.1.max(hi),
            _ => merged.push((lo, hi)),
        }
    }
    merged
}

/// Whether `offset` lies inside one of the merged ranges.
fn range_covers(merged: &[(i64, i64)], offset: i64) -> bool {
    let i = merged.partition_point(|(lo, _)| *lo <= offset);
    i > 0 && offset <= merged[i - 1].1
}

/// End of the contiguous run the merged ranges hold from their lowest offset,
/// or -1 if there are none.
fn cold_prefix_end(merged: &[(i64, i64)]) -> i64 {
    merged.first().map(|(_, hi)| *hi).unwrap_or(-1)
}

#[cfg(test)]
mod cold_range_tests {
    use super::*;

    #[test]
    fn ranges_merge_when_they_overlap_or_touch() {
        assert_eq!(merge_ranges(vec![(10, 19), (0, 9), (30, 39), (35, 50)]), vec![(0, 19), (30, 50)]);
        assert_eq!(merge_ranges(vec![]), vec![]);
    }

    #[test]
    fn a_lower_segment_indexed_after_a_higher_one_is_not_covered() {
        // Segment 7 written first: offsets of segments 0..6 must still be written.
        let cold = merge_ranges(vec![(700, 799)]);
        assert!(!range_covers(&cold, 0));
        assert!(!range_covers(&cold, 699));
        assert!(range_covers(&cold, 700));
        assert!(range_covers(&cold, 799));
        assert!(!range_covers(&cold, 800));
    }

    #[test]
    fn the_flushed_offset_stops_at_the_first_gap() {
        assert_eq!(cold_prefix_end(&merge_ranges(vec![(0, 99), (200, 299)])), 99);
        assert_eq!(cold_prefix_end(&merge_ranges(vec![(0, 99), (100, 199), (200, 299)])), 299);
        assert_eq!(cold_prefix_end(&[]), -1);
    }

    #[test]
    fn sealed_segments_sort_oldest_first_per_partition() {
        let mut ids = vec!["t:0:51", "t:0:7", "t:1:2", "s:0:10", "t:0:10", "odd"];
        ids.sort_by_cached_key(|s| segment_order_key(s));
        assert_eq!(ids, vec!["s:0:10", "t:0:7", "t:0:10", "t:0:51", "t:1:2", "odd"]);
    }
}

/// The lock serializing Parquet writes for one partition (see
/// `WalIndexer::create_parquet_segment`).
fn parquet_flush_lock(tp: &TopicPartition) -> Arc<tokio::sync::Mutex<()>> {
    type Locks = dashmap::DashMap<(String, i32), Arc<tokio::sync::Mutex<()>>>;
    static LOCKS: std::sync::LazyLock<Locks> = std::sync::LazyLock::new(dashmap::DashMap::new);
    LOCKS.entry((tp.topic.clone(), tp.partition)).or_default().clone()
}

#[cfg(test)]
mod prepare_tests {
    use super::*;
    use crate::object_store::{LocalBackend, ObjectStoreConfig, StorageBackend};
    use chronik_common::metadata::{InMemoryMetadataStore, TopicConfig};

    const TOPIC: &str = "prep";

    /// One batch of `n` records from `base`, as produce writes it to the WAL.
    fn batch(base: i64, n: i64) -> CanonicalRecord {
        CanonicalRecord {
            base_offset: base,
            partition_leader_epoch: 0,
            producer_id: -1,
            producer_epoch: -1,
            base_sequence: -1,
            is_transactional: false,
            is_control: false,
            compression: crate::canonical_record::CompressionType::None,
            timestamp_type: TimestampType::CreateTime,
            base_timestamp: 1_700_000_000_000 + base,
            max_timestamp: 1_700_000_000_000 + base + n - 1,
            records: (base..base + n)
                .map(|o| CanonicalRecordEntry {
                    offset: o,
                    timestamp: 1_700_000_000_000 + o,
                    key: None,
                    value: Some(format!("{{\"n\":{o}}}").into_bytes()),
                    headers: vec![],
                    attributes: 0,
                })
                .collect(),
            compressed_records_wire_bytes: None,
            original_v1_wire_format: None,
            original_v2_wire_format: None,
        }
    }

    async fn indexer(dir: &std::path::Path) -> WalIndexer {
        let wal_config = chronik_wal::config::WalConfig { data_dir: dir.join("wal"), ..Default::default() };
        let wal = Arc::new(WalManager::new(wal_config).await.unwrap());
        for b in 0..10 {
            let data = bincode::serialize(&batch(b * 10, 10)).unwrap();
            wal.append_canonical(TOPIC.to_string(), 0, data, b * 10, b * 10 + 9, 10).await.unwrap();
        }
        assert_eq!(wal.seal_stale_segments(0).await, 1);

        let store_dir = dir.join("store");
        std::fs::create_dir_all(&store_dir).unwrap();
        let object_store: Arc<dyn ObjectStore> = Arc::new(
            LocalBackend::new(ObjectStoreConfig {
                backend: StorageBackend::Local { path: store_dir.to_string_lossy().to_string() },
                ..Default::default()
            })
            .await
            .unwrap(),
        );
        let metadata: Arc<dyn MetadataStore> = Arc::new(InMemoryMetadataStore::new());
        let mut config = HashMap::new();
        config.insert("searchable".to_string(), "true".to_string());
        metadata
            .create_topic(
                TOPIC,
                TopicConfig {
                    partition_count: 1,
                    replication_factor: 1,
                    retention_ms: None,
                    segment_bytes: 1 << 30,
                    config,
                },
            )
            .await
            .unwrap();
        let config = WalIndexerConfig {
            segment_index_path: None,
            vector_base_path: dir.join("vectors").to_string_lossy().to_string(),
            columnar_base_path: dir.join("columnar").to_string_lossy().to_string(),
            ..Default::default()
        };
        WalIndexer::new(config, wal, object_store, metadata, None)
    }

    async fn finish(w: &WalIndexer, segment: &str, prepared: Option<PreparedSegment>) {
        let mut stats = IndexingStats::default();
        WalIndexer::index_segment(
            &w.config,
            &w.wal_manager,
            &w.object_store,
            &w.segment_index,
            &w.metadata_store,
            &w.vector_index_manager,
            segment,
            &mut stats,
            &w.is_leader,
            &w.hot_buffer,
            &w.replication_progress,
            &w.cold_flush_listener,
            &w.hot_vector_index,
            prepared,
        )
        .await
        .unwrap();
        assert_eq!(stats.errors, 0);
    }

    async fn archived(w: &WalIndexer) -> Vec<(i64, i64, usize)> {
        let mut v: Vec<_> = w
            .segment_index
            .get_segments_for_partition(TOPIC, 0)
            .await
            .unwrap_or_default()
            .iter()
            .map(|s| (s.min_offset, s.max_offset, s.record_count))
            .collect();
        v.sort();
        v
    }

    fn sealed(w: &WalIndexer) -> String {
        let segments = w.wal_manager.get_sealed_segments_with_size();
        assert_eq!(segments.len(), 1);
        segments[0].0.clone()
    }

    /// Prepared work is what gets published when nothing changed.
    #[tokio::test]
    async fn a_prepared_segment_is_published_as_built() {
        let dir = tempfile::tempdir().unwrap();
        let w = indexer(dir.path()).await;
        let segment = sealed(&w);
        let prepared = WalIndexer::prepare_segment(w.wal_manager.clone(), w.metadata_store.clone(), segment.clone())
            .await
            .expect("a sealed segment of a searchable topic is prepared");
        assert_eq!(prepared.records_indexed, 100);
        assert_eq!(prepared.archives.len(), 1, "the archive is built ahead");
        assert_eq!(prepared.raw.len(), 1, "the raw segment is encoded ahead");

        finish(&w, &segment, Some(prepared)).await;
        assert_eq!(archived(&w).await, vec![(0, 99, 100)]);
    }

    /// A truncation between preparing a segment and publishing it must not
    /// publish what the truncation discarded: the prepared archive covers
    /// offsets the log no longer holds.
    #[tokio::test]
    async fn a_truncation_after_prepare_discards_the_prepared_work() {
        let dir = tempfile::tempdir().unwrap();
        let w = indexer(dir.path()).await;
        let segment = sealed(&w);
        let prepared = WalIndexer::prepare_segment(w.wal_manager.clone(), w.metadata_store.clone(), segment.clone())
            .await
            .unwrap();
        assert_eq!(prepared.archives.len(), 1);

        // What a follower does when it learns its log diverged.
        {
            let lock = w.wal_manager.archive_lock(TOPIC, 0);
            let _guard = lock.lock().await;
            w.wal_manager.truncate_to(TOPIC, 0, 50).await.unwrap();
        }

        finish(&w, &segment, Some(prepared)).await;
        let got = archived(&w).await;
        assert!(
            got.iter().all(|&(_, max, _)| max < 50),
            "offsets the truncation removed were archived from stale prepared work: {got:?}"
        );
    }
}
