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
    canonical_record::{CanonicalRecord, CanonicalRecordEntry, TimestampType, RecordHeader},
    tantivy_segment::{TantivySegmentWriter, SegmentMetadata},
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
    schema::kafka_message_schema,
    HotDataBuffer,
    VectorIndexManager, VectorIndexConfig, HnswIndexConfig, EmbeddingPipeline,
};
use chronik_embeddings::{VectorSearchConfig, create_provider};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::RwLock;
use tokio::time::{interval, Duration};
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

    /// Whether to delete WAL segments after successful indexing
    pub delete_after_index: bool,

    /// Object store configuration for uploading Tantivy indexes
    pub object_store: ObjectStoreConfig,

    /// Base path for Tantivy indexes
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
            delete_after_index: true,
            object_store: ObjectStoreConfig::default(),
            index_base_path: "./data/indexes".to_string(),
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
            let mut interval_timer = interval(Duration::from_secs(config.interval_secs));

            loop {
                interval_timer.tick().await;

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
    #[instrument(skip(config, wal_manager, object_store, segment_index, metadata_store, indexing_in_progress, indexed_segments, vector_index_manager, is_leader, hot_buffer, replication_progress, cold_flush_listener, hot_vector_index))]
    async fn index_sealed_segments_internal(
        config: &WalIndexerConfig,
        wal_manager: &Arc<WalManager>,
        object_store: &Arc<dyn ObjectStore>,
        segment_index: &Arc<SegmentIndex>,
        metadata_store: &Arc<dyn MetadataStore>,
        indexing_in_progress: &Arc<RwLock<HashSet<String>>>,
        indexed_segments: &Arc<RwLock<HashMap<String, u64>>>,
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

        // Limit number of segments per run
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

        // Process each segment
        for segment_id in &segments_to_index {
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
                    }
                }
                Err(e) => {
                    error!(segment = %segment_id, error = %e, "Failed to index segment");
                    stats.errors += 1;
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

    /// Whether `index_segment` may delete the source WAL segment after a pass.
    /// Only when deletion is enabled AND the pass recorded no new errors: a
    /// raw-segment upload failure bumps the error count and continues, so a
    /// dirty pass must KEEP the WAL copy (the caller also declines to mark such
    /// a segment "indexed", so the next run re-uploads it). Deleting on a dirty
    /// pass would lose the data from BOTH the WAL and the object store.
    fn may_delete_wal_segment(delete_after_index: bool, errors_at_start: usize, errors_now: usize) -> bool {
        delete_after_index && errors_now == errors_at_start
    }

    /// Highest offset already written to a cold Parquet segment for this
    /// partition, or `None` if it has no cold segments yet.
    ///
    /// This is the durable, restart-surviving record of what cold already holds,
    /// used to keep Parquet flushing idempotent (a segment is never written for
    /// offsets a prior one covers). `None` vs `Some(-1)` do not need
    /// distinguishing here — both mean "nothing to skip".
    async fn cold_high_water_mark(
        metadata_store: &Arc<dyn MetadataStore>,
        tp: &TopicPartition,
    ) -> Option<i64> {
        let segments = metadata_store
            .list_parquet_segments(&tp.topic, Some(tp.partition))
            .await
            .ok()?;
        segments.iter().map(|s| s.max_offset).max()
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

    /// Index a single sealed WAL segment
    #[instrument(skip(config, wal_manager, object_store, segment_index, metadata_store, vector_index_manager, stats, is_leader, hot_buffer, replication_progress, cold_flush_listener, hot_vector_index))]
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

        // Read all records from the segment (v1.3.47+: direct call)
        let records = wal_manager.read_segment(segment_id).await
            .map_err(|e| Error::Internal(format!("Failed to read segment {}: {}", segment_id, e)))?;

        if records.is_empty() {
            info!(segment = %segment_id, "Segment is empty, skipping");
            stats.segments_processed += 1;
            return Ok(());
        }

        // Group records by topic-partition
        let mut tp_records: HashMap<TopicPartition, Vec<CanonicalRecord>> = HashMap::new();

        for record in records {
            match record {
                WalRecord::V1 { .. } => {
                    // V1 records don't have topic-partition info in the record itself
                    // We need to get this from the segment metadata
                    // For now, skip V1 records (they're handled by legacy path)
                    debug!("Skipping V1 record (legacy format)");
                    continue;
                }
                WalRecord::V2 { topic, partition, canonical_data, .. } => {
                    // Deserialize CanonicalRecord from bincode
                    let canonical_record: CanonicalRecord = bincode::deserialize(&canonical_data)
                        .map_err(|e| Error::Internal(format!("Failed to deserialize CanonicalRecord: {}", e)))?;

                    let record_count = canonical_record.records.len();
                    let tp = TopicPartition::new(topic.clone(), partition);
                    tp_records.entry(tp).or_insert_with(Vec::new).push(canonical_record);

                    stats.records_indexed += record_count;
                }
            }
        }

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
            // STEP 1: Upload raw segment data to S3 (Tier 2: warm storage)
            // This preserves the actual message data for consumption
            match Self::upload_raw_segment(
                object_store,
                metadata_store,
                &tp,
                &canonical_records,
                is_leader,
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
                    // Continue to Tantivy indexing even if raw upload fails
                    // (at least we'll have searchable index)
                }
            }

            // STEP 2: Check topic configuration for searchable, columnar, and vector
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

            // Clone records for each processing step that needs them
            // The last step in order (vector > columnar > searchable) can use the original
            let records_for_tantivy = if is_searchable {
                Some(canonical_records.clone())
            } else {
                None
            };
            let records_for_columnar = if is_columnar {
                Some(canonical_records.clone())
            } else {
                None
            };
            // Vector gets the original to avoid extra clone
            let records_for_vector = if is_vector {
                Some(canonical_records)
            } else {
                // We still need to drop canonical_records
                drop(canonical_records);
                None
            };

            // STEP 2a: Create Tantivy index (searchable topics only)
            if let Some(records) = records_for_tantivy {
                match Self::create_tantivy_index(
                    config,
                    object_store,
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

        // STEP 3: Process all vector embeddings concurrently across partitions
        // v2.4.0: Up to 3 partitions embed in parallel, reducing total wall-clock time
        if !vector_work_items.is_empty() {
            let max_partition_concurrency = std::env::var("CHRONIK_VECTOR_PARTITION_CONCURRENCY")
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(3)
                .max(1)
                .min(8);

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
            wal_manager.delete_segment(segment_id).await
                .map_err(|e| Error::Internal(format!("Failed to delete segment {}: {}", segment_id, e)))?;
            info!(segment = %segment_id, "Deleted WAL segment after indexing");
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
    #[instrument(skip(object_store, metadata_store, canonical_records, is_leader))]
    async fn upload_raw_segment(
        object_store: &Arc<dyn ObjectStore>,
        metadata_store: &Arc<dyn MetadataStore>,
        tp: &TopicPartition,
        canonical_records: &[CanonicalRecord],
        is_leader: &Arc<AtomicBool>,
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

        // Serialize all CanonicalRecords using bincode
        // This preserves the exact data including compressed_records_wire_bytes
        let serialized_data = bincode::serialize(canonical_records)
            .map_err(|e| Error::Internal(format!("Failed to serialize canonical records: {}", e)))?;

        let data_size = serialized_data.len() as u64;

        // Upload to S3 with path: segments/{topic}/{partition}/{min_offset}-{max_offset}.segment
        let object_key = format!(
            "segments/{}/{}/{}-{}.segment",
            tp.topic,
            tp.partition,
            min_offset,
            max_offset
        );

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
        // Only persist on leader — followers don't own __chronik_metadata partition.
        if is_leader.load(Ordering::Relaxed) {
            let segment_id = format!("{}-{}", min_offset, max_offset);
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
    #[instrument(skip(config, object_store, segment_index, canonical_records))]
    async fn create_tantivy_index(
        config: &WalIndexerConfig,
        object_store: &Arc<dyn ObjectStore>,
        segment_index: &Arc<SegmentIndex>,
        tp: &TopicPartition,
        canonical_records: Vec<CanonicalRecord>,
    ) -> Result<(u64, i64)> {
        if canonical_records.is_empty() {
            return Ok((0, -1));
        }

        // Get base offset and calculate ranges from all records
        let base_offset = canonical_records[0].base_offset;
        let mut min_offset = i64::MAX;
        let mut max_offset = i64::MIN;
        let mut min_timestamp = i64::MAX;
        let mut max_timestamp = i64::MIN;
        let mut total_record_count = 0;

        for record in &canonical_records {
            min_offset = min_offset.min(record.min_offset());
            max_offset = max_offset.max(record.last_offset());
            min_timestamp = min_timestamp.min(record.min_timestamp());
            max_timestamp = max_timestamp.max(record.max_timestamp);
            total_record_count += record.records.len();
        }

        // Create TantivySegmentWriter
        let mut writer = TantivySegmentWriter::new(tp.topic.clone(), tp.partition, base_offset)?;

        // Write all batches (CPU-intensive: 285K records)
        for canonical_record in canonical_records {
            writer.write_batch(&canonical_record)?;
        }

        // Create output directory for tar.gz
        let temp_dir = tempfile::tempdir()
            .map_err(|e| Error::Internal(format!("Failed to create temp dir: {}", e)))?;

        // Commit and serialize to tar.gz (CPU + I/O intensive)
        let (tar_gz_path, metadata) = writer.commit_and_serialize(temp_dir.path())?;

        info!(
            topic = %tp.topic,
            partition = tp.partition,
            record_count = metadata.record_count,
            base_offset = metadata.base_offset,
            last_offset = metadata.last_offset,
            "Committed Tantivy index (record_count={})",
            metadata.record_count
        );

        // Read tar.gz file (blocking I/O)
        let tar_gz_data = tokio::fs::read(&tar_gz_path).await
            .map_err(|e| Error::Internal(format!("Failed to read tar.gz: {}", e)))?;
        let bytes_written = tar_gz_data.len() as u64;

        // Upload to object store
        let object_key = format!(
            "{}/{}/partition-{}/segment-{}-{}.tar.gz",
            config.index_base_path,
            tp.topic,
            tp.partition,
            metadata.base_offset,
            metadata.last_offset
        );

        let data_bytes = bytes::Bytes::from(tar_gz_data);
        object_store.put(&object_key, data_bytes).await
            .map_err(|e| Error::Internal(format!("Failed to upload index: {}", e)))?;

        info!(
            topic = %tp.topic,
            partition = tp.partition,
            object_key = %object_key,
            bytes = bytes_written,
            "Uploaded Tantivy index to object store"
        );

        // Register segment in index
        let segment_id = format!("{}-{}-{}-{}", tp.topic, tp.partition, min_offset, max_offset);
        let segment_metadata = SegmentIndexMetadata {
            segment_id: segment_id.clone(),
            topic: tp.topic.clone(),
            partition: tp.partition,
            min_offset,
            max_offset,
            record_count: total_record_count,
            min_timestamp,
            max_timestamp,
            object_store_path: object_key.clone(),
            size_bytes: bytes_written,
            created_at: chrono::Utc::now().timestamp(),
            compression: "snappy".to_string(),
        };

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

    /// v2.2.21: Create Parquet file from CanonicalRecords for columnar storage
    ///
    /// Converts CanonicalRecords to Arrow RecordBatches and writes to Parquet format.
    /// Uses time-based partitioning (hourly by default) for efficient query pruning.
    /// Registers the new Parquet segment with the SegmentIndex for SQL query planning.
    #[instrument(skip(config, object_store, metadata_store, segment_index, canonical_records))]
    /// Returns (bytes_written, max_offset) on success.
    async fn create_parquet_segment(
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
        // offset once per copy. The cold high-water mark lives in the metadata
        // store (it survives restarts), so trim to offsets strictly above it.
        //
        // `highest_offset` (before trimming) is still returned so the hot buffer
        // evicts up to it: those rows are in cold either way, whether this pass
        // wrote them or a previous one did.
        let highest_offset = kafka_records.iter().map(|r| r.offset).max().unwrap_or(0);
        let cold_watermark = Self::cold_high_water_mark(metadata_store, tp).await;
        if let Some(wm) = cold_watermark {
            let before = kafka_records.len();
            kafka_records.retain(|r| r.offset > wm);
            let dropped = before - kafka_records.len();
            if dropped > 0 {
                debug!(
                    topic = %tp.topic,
                    partition = tp.partition,
                    cold_watermark = wm,
                    dropped,
                    remaining = kafka_records.len(),
                    "Skipping offsets already present in cold Parquet (idempotent flush)"
                );
            }
            if kafka_records.is_empty() {
                // Everything here is already in cold. Nothing to write, but the
                // data is durable in cold, so report the high offset for eviction.
                return Ok((0, highest_offset));
            }
        }

        // Calculate offset and timestamp ranges for naming
        let min_offset = kafka_records.iter().map(|r| r.offset).min().unwrap_or(0);
        let max_offset = kafka_records.iter().map(|r| r.offset).max().unwrap_or(0);
        let min_timestamp = kafka_records.iter().map(|r| r.timestamp_ms).min().unwrap_or(0);
        let max_timestamp = kafka_records.iter().map(|r| r.timestamp_ms).max().unwrap_or(0);

        // Check if topic has JSON schema for columnar extraction
        let json_schema = if let Some(ref tc) = topic_config {
            if let Some(schema_str) = tc.config.json_schema() {
                InferredJsonSchema::from_config_string(schema_str)
            } else {
                // First time: infer schema from sample values and persist
                let sample_values: Vec<&[u8]> = kafka_records
                    .iter()
                    .take(100)
                    .map(|r| r.value.as_slice())
                    .collect();
                if let Some(inferred) = InferredJsonSchema::infer(&sample_values, 64) {
                    let config_str = inferred.to_config_string();
                    debug!(
                        topic = %tp.topic,
                        schema = %config_str,
                        "Inferred JSON schema for columnar storage"
                    );
                    // Persist to topic config (best-effort, don't block on failure)
                    let mut updated_config = tc.config.clone();
                    updated_config.config.insert(
                        "columnar.json_schema".to_string(),
                        config_str,
                    );
                    if let Err(e) = metadata_store.update_topic(&tp.topic, updated_config).await {
                        warn!(
                            topic = %tp.topic,
                            error = %e,
                            "Failed to persist inferred JSON schema (will re-infer next time)"
                        );
                    }
                    Some(inferred)
                } else {
                    None
                }
            }
        } else {
            None
        };

        // Convert to Arrow RecordBatch (with JSON columns if schema available)
        let converter = RecordBatchConverter::new();
        let batch = if let Some(ref js) = json_schema {
            converter.convert_with_json(&kafka_records, js)
                .map_err(|e| Error::Internal(format!("Failed to convert to RecordBatch: {}", e)))?
        } else {
            converter.convert(&kafka_records)
                .map_err(|e| Error::Internal(format!("Failed to convert to RecordBatch: {}", e)))?
        };

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

        Ok((bytes_written, max_offset))
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
    ) -> Result<usize> {
        if canonical_records.is_empty() {
            return Ok(0);
        }

        // Get topic config to read vector settings
        let topic_config = metadata_store.get_topic(&tp.topic).await
            .map_err(|e| Error::Internal(format!("Failed to get topic config: {}", e)))?;

        let topic_meta = match topic_config {
            Some(t) => t,
            None => {
                warn!(topic = %tp.topic, "Topic not found for vector processing");
                return Ok(0);
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
            return Ok(0);
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
                // Log error but don't fail - embedding is optional enhancement
                warn!(
                    topic = %tp.topic,
                    partition = tp.partition,
                    error = %e,
                    "Failed to create embedding provider, skipping embedding generation"
                );
                return Ok(0);
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
                Ok(stats.embedded)
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
        let mut config = ColumnarConfig::default();

        // Set compression from topic config
        config.compression = match topic_config.columnar_compression() {
            "none" => CompressionCodec::None,
            "snappy" => CompressionCodec::Snappy,
            "gzip" => CompressionCodec::Gzip,
            "lz4" => CompressionCodec::Lz4,
            "zstd" => CompressionCodec::Zstd,
            _ => CompressionCodec::Zstd, // Default
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
    pub async fn backfill_vector_embeddings(
        &self,
        topic: &str,
        partition: Option<i32>,
        model_id: Option<&str>,
    ) -> Result<BackfillStats> {
        let mut stats = BackfillStats::default();
        stats.model_id = model_id.map(|s| s.to_string());
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

            // Deserialize CanonicalRecords
            let canonical_records: Vec<CanonicalRecord> = match bincode::deserialize(&data) {
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
                Ok(embeddings_generated) => {
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
