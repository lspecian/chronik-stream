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
    schema::kafka_message_schema,
    VectorIndexManager, VectorIndexConfig, HnswIndexConfig, EmbeddingPipeline,
};
use chronik_embeddings::{VectorSearchConfig, create_provider};
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::time::{interval, Duration};
use tracing::{info, warn, error, debug, instrument};
use std::collections::{HashMap, HashSet};
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

    /// Statistics from last indexing run
    last_stats: Arc<RwLock<IndexingStats>>,

    /// Whether the indexer is running
    running: Arc<RwLock<bool>>,

    /// CRITICAL v2.2.10: Dedicated tokio runtime for background indexing
    /// Prevents WalIndexer from starving the main runtime's accept loop under heavy load
    /// 2 worker threads are sufficient for sequential segment processing
    runtime: Arc<tokio::runtime::Runtime>,

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
}

impl WalIndexer {
    /// Create a new WAL indexer
    pub fn new(
        config: WalIndexerConfig,
        wal_manager: Arc<WalManager>,
        object_store: Arc<dyn ObjectStore>,
        metadata_store: Arc<dyn MetadataStore>,
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
            last_stats: Arc::new(RwLock::new(IndexingStats::default())),
            running: Arc::new(RwLock::new(false)),
            runtime: Arc::new(runtime),
            searchable_topics: Arc::new(RwLock::new(HashSet::new())),
            columnar_topics: Arc::new(RwLock::new(HashSet::new())),
            vector_topics: Arc::new(RwLock::new(HashSet::new())),
            vector_index_manager,
        }
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

    /// v2.2.22: Get reference to vector index manager
    pub fn vector_index_manager(&self) -> &Arc<VectorIndexManager> {
        &self.vector_index_manager
    }

    /// v2.2.23: Get reference to WAL manager (for hot buffer)
    pub fn wal_manager(&self) -> &Arc<WalManager> {
        &self.wal_manager
    }

    /// Start the background indexing task
    #[instrument(skip(self))]
    pub async fn start(&self) -> Result<()> {
        let mut running = self.running.write().await;
        if *running {
            warn!("WAL indexer already running");
            return Ok(());
        }
        *running = true;
        drop(running);

        // Load segment index from disk if persistence is configured
        if let Err(e) = self.segment_index.load().await {
            warn!(error = %e, "Failed to load segment index, starting with empty index");
        }

        // v2.2.22: Load vector indexes from disk
        match self.vector_index_manager.load_all_indexes().await {
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
        let snapshot_interval = self.config.snapshot_interval_secs();
        if snapshot_interval > 0 {
            self.vector_index_manager.start_snapshot_task(snapshot_interval);
        }

        info!(
            interval_secs = self.config.interval_secs,
            "Starting WAL indexer background task on dedicated runtime"
        );

        let config = self.config.clone();
        let wal_manager = Arc::clone(&self.wal_manager);
        let object_store = Arc::clone(&self.object_store);
        let segment_index = Arc::clone(&self.segment_index);
        let metadata_store = Arc::clone(&self.metadata_store);
        let indexing_in_progress = Arc::clone(&self.indexing_in_progress);
        let last_stats = Arc::clone(&self.last_stats);
        let running = Arc::clone(&self.running);
        let runtime = Arc::clone(&self.runtime);
        let vector_index_manager = Arc::clone(&self.vector_index_manager);

        // CRITICAL v2.2.10: Spawn on dedicated runtime, NOT main runtime
        // This guarantees accept loop can never be starved by indexing work
        runtime.spawn(async move {
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
                    &vector_index_manager,
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
            &self.vector_index_manager,
        ).await
    }

    /// Index sealed segments (internal implementation)
    #[instrument(skip(config, wal_manager, object_store, segment_index, metadata_store, indexing_in_progress, vector_index_manager))]
    async fn index_sealed_segments_internal(
        config: &WalIndexerConfig,
        wal_manager: &Arc<WalManager>,
        object_store: &Arc<dyn ObjectStore>,
        segment_index: &Arc<SegmentIndex>,
        metadata_store: &Arc<dyn MetadataStore>,
        indexing_in_progress: &Arc<RwLock<HashSet<String>>>,
        vector_index_manager: &Arc<VectorIndexManager>,
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
        let sealed_segments = wal_manager.get_sealed_segments();

        if sealed_segments.is_empty() {
            debug!("No sealed segments to index");
            return Ok(stats);
        }

        info!(count = sealed_segments.len(), "Found sealed WAL segments to index");

        // Filter out segments already being indexed
        let mut segments_to_index = Vec::new();
        {
            let in_progress = indexing_in_progress.read().await;
            for segment in sealed_segments {
                if !in_progress.contains(&segment) {
                    segments_to_index.push(segment);
                }
            }
        }

        // Limit number of segments per run
        segments_to_index.truncate(config.max_segments_per_run);

        if segments_to_index.is_empty() {
            debug!("All sealed segments already being indexed");
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
            match Self::index_segment(
                config,
                wal_manager,
                object_store,
                segment_index,
                metadata_store,
                vector_index_manager,
                segment_id,
                &mut stats,
            ).await {
                Ok(_) => {
                    debug!(segment = %segment_id, "Successfully indexed segment");
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

    /// Index a single sealed WAL segment
    #[instrument(skip(config, wal_manager, object_store, segment_index, metadata_store, vector_index_manager, stats))]
    async fn index_segment(
        config: &WalIndexerConfig,
        wal_manager: &Arc<WalManager>,
        object_store: &Arc<dyn ObjectStore>,
        segment_index: &Arc<SegmentIndex>,
        metadata_store: &Arc<dyn MetadataStore>,
        vector_index_manager: &Arc<VectorIndexManager>,
        segment_id: &str,
        stats: &mut IndexingStats,
    ) -> Result<()> {
        info!(segment = %segment_id, "Indexing WAL segment");

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

        // Process each topic-partition: upload raw segments + create Tantivy indexes
        for (tp, canonical_records) in tp_records {
            // STEP 1: Upload raw segment data to S3 (Tier 2: warm storage)
            // This preserves the actual message data for consumption
            match Self::upload_raw_segment(
                object_store,
                metadata_store,
                &tp,
                &canonical_records,
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
                // Check if topic is in searchable set by looking up metadata
                // For performance, we check the config directly here
                let topic_config = metadata_store.get_topic(&tp.topic).await
                    .map(|opt| opt.map(|t| (
                        t.config.is_searchable(),
                        t.config.is_columnar_enabled(),
                        t.config.is_vector_enabled(),
                    )).unwrap_or((false, false, false)))
                    .unwrap_or((false, false, false));
                topic_config
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
                    Ok(bytes_written) => {
                        info!(
                            topic = %tp.topic,
                            partition = tp.partition,
                            bytes = bytes_written,
                            "Created Tantivy index (searchable topic)"
                        );
                        stats.indexes_created += 1;
                        stats.bytes_written += bytes_written;
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
                    Ok(bytes_written) => {
                        info!(
                            topic = %tp.topic,
                            partition = tp.partition,
                            bytes = bytes_written,
                            "Created Parquet file (columnar-enabled topic)"
                        );
                        stats.bytes_written += bytes_written;
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

            // STEP 2c: Generate vector embeddings (vector-enabled topics only)
            // v2.2.22: Async embedding generation for semantic search
            if let Some(records) = records_for_vector {
                match Self::process_vector_embeddings(
                    config,
                    metadata_store,
                    vector_index_manager,
                    &tp,
                    records,
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
            } else {
                debug!(
                    topic = %tp.topic,
                    partition = tp.partition,
                    "Skipping vector embeddings (non-vector topic)"
                );
            }
        }

        // Delete WAL segment if configured (v1.3.47+: direct call)
        if config.delete_after_index {
            wal_manager.delete_segment(segment_id).await
                .map_err(|e| Error::Internal(format!("Failed to delete segment {}: {}", segment_id, e)))?;
            info!(segment = %segment_id, "Deleted WAL segment after indexing");
        }

        stats.segments_processed += 1;
        Ok(())
    }

    /// Upload raw segment data to S3 for message consumption (Tier 2: warm storage)
    ///
    /// This stores the actual CanonicalRecords (bincode-serialized) so consumers can
    /// fetch messages from S3 when they're no longer in local WAL/segments.
    #[instrument(skip(object_store, metadata_store, canonical_records))]
    async fn upload_raw_segment(
        object_store: &Arc<dyn ObjectStore>,
        metadata_store: &Arc<dyn MetadataStore>,
        tp: &TopicPartition,
        canonical_records: &[CanonicalRecord],
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

        // Register segment metadata in the metadata store
        // This allows high watermarks to be restored on startup
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

        metadata_store.persist_segment_metadata(segment_metadata).await
            .map_err(|e| Error::Internal(format!("Failed to register segment metadata: {}", e)))?;

        info!(
            topic = %tp.topic,
            partition = tp.partition,
            min_offset = min_offset,
            max_offset = max_offset,
            "Registered segment metadata"
        );

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
    ) -> Result<u64> {
        if canonical_records.is_empty() {
            return Ok(0);
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

        Ok(bytes_written)
    }

    /// v2.2.21: Create Parquet file from CanonicalRecords for columnar storage
    ///
    /// Converts CanonicalRecords to Arrow RecordBatches and writes to Parquet format.
    /// Uses time-based partitioning (hourly by default) for efficient query pruning.
    /// Registers the new Parquet segment with the SegmentIndex for SQL query planning.
    #[instrument(skip(config, object_store, metadata_store, segment_index, canonical_records))]
    async fn create_parquet_segment(
        config: &WalIndexerConfig,
        object_store: &Arc<dyn ObjectStore>,
        metadata_store: &Arc<dyn MetadataStore>,
        segment_index: &Arc<SegmentIndex>,
        tp: &TopicPartition,
        canonical_records: Vec<CanonicalRecord>,
    ) -> Result<u64> {
        if canonical_records.is_empty() {
            return Ok(0);
        }

        // Get topic config to read columnar settings
        let topic_config = metadata_store.get_topic(&tp.topic).await
            .map_err(|e| Error::Internal(format!("Failed to get topic config: {}", e)))?;

        let columnar_config = if let Some(topic) = topic_config {
            Self::build_columnar_config(&topic.config)
        } else {
            ColumnarConfig::default()
        };

        // Convert CanonicalRecords to KafkaRecords for the converter
        // Note: Embeddings are generated asynchronously and populated later
        // when vector search is enabled for this topic
        let kafka_records: Vec<KafkaRecord> = canonical_records
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
            return Ok(0);
        }

        // Calculate offset and timestamp ranges for naming
        let min_offset = kafka_records.iter().map(|r| r.offset).min().unwrap_or(0);
        let max_offset = kafka_records.iter().map(|r| r.offset).max().unwrap_or(0);
        let min_timestamp = kafka_records.iter().map(|r| r.timestamp_ms).min().unwrap_or(0);
        let max_timestamp = kafka_records.iter().map(|r| r.timestamp_ms).max().unwrap_or(0);

        // Convert to Arrow RecordBatch
        let converter = RecordBatchConverter::new();
        let batch = converter.convert(&kafka_records)
            .map_err(|e| Error::Internal(format!("Failed to convert to RecordBatch: {}", e)))?;

        // Create Parquet writer and write to bytes
        let schema = kafka_message_schema();
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

        // v2.2.22: Also persist to metadata store for SQL handler access
        // The SQL handler uses metadata_store.get_parquet_paths() to discover tables
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

        if let Err(e) = metadata_store.persist_parquet_segment(common_metadata).await {
            warn!(
                topic = %tp.topic,
                partition = tp.partition,
                error = %e,
                "Failed to persist Parquet segment to metadata store (SQL queries may not see this segment)"
            );
        }

        debug!(
            topic = %tp.topic,
            partition = tp.partition,
            min_offset = min_offset,
            max_offset = max_offset,
            "Registered Parquet segment in index and metadata store"
        );

        Ok(bytes_written)
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
    #[instrument(skip(_config, metadata_store, vector_index_manager, canonical_records))]
    async fn process_vector_embeddings(
        _config: &WalIndexerConfig,
        metadata_store: &Arc<dyn MetadataStore>,
        vector_index_manager: &Arc<VectorIndexManager>,
        tp: &TopicPartition,
        canonical_records: Vec<CanonicalRecord>,
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

        // Create embedding pipeline
        let pipeline = EmbeddingPipeline::new(
            provider,
            Arc::clone(vector_index_manager),
            vector_config.batch_size,
        );

        // Process messages through the pipeline
        match pipeline.process_messages(&tp.topic, tp.partition, texts).await {
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

        let records_written = WalIndexer::create_parquet_segment(
            &indexer_config,
            &object_store,
            &metadata_store,
            &segment_index,
            &tp,
            records,
        ).await.unwrap();

        // 8. Verify bytes were written (create_parquet_segment returns bytes, not record count)
        assert!(records_written > 0, "Expected bytes to be written to Parquet file");

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
