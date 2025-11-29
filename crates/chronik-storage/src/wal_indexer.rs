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
    canonical_record::{CanonicalRecord, CanonicalRecordEntry},
    tantivy_segment::{TantivySegmentWriter, SegmentMetadata},
    object_store::{ObjectStore, ObjectStoreConfig},
    segment_index::{SegmentIndex, SegmentMetadata as SegmentIndexMetadata},
};
use chronik_wal::{WalManager, WalRecord};
use chronik_common::{Result, Error};
use chronik_common::metadata::traits::{MetadataStore, SegmentMetadata as MetadataSegmentMetadata};
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

    /// Get reference to segment index
    pub fn segment_index(&self) -> &Arc<SegmentIndex> {
        &self.segment_index
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
        ).await
    }

    /// Index sealed segments (internal implementation)
    #[instrument(skip(config, wal_manager, object_store, segment_index, metadata_store, indexing_in_progress))]
    async fn index_sealed_segments_internal(
        config: &WalIndexerConfig,
        wal_manager: &Arc<WalManager>,
        object_store: &Arc<dyn ObjectStore>,
        segment_index: &Arc<SegmentIndex>,
        metadata_store: &Arc<dyn MetadataStore>,
        indexing_in_progress: &Arc<RwLock<HashSet<String>>>,
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
    #[instrument(skip(config, wal_manager, object_store, segment_index, metadata_store, stats))]
    async fn index_segment(
        config: &WalIndexerConfig,
        wal_manager: &Arc<WalManager>,
        object_store: &Arc<dyn ObjectStore>,
        segment_index: &Arc<SegmentIndex>,
        metadata_store: &Arc<dyn MetadataStore>,
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

            // STEP 2: Create Tantivy index (Tier 3: cold storage with search)
            // v2.2.16: Only index searchable topics
            let is_searchable = {
                // Check if topic is in searchable set by looking up metadata
                // For performance, we check the env var directly here
                // The WalIndexer.searchable_topics cache is used at a higher level
                let topic_searchable_config = metadata_store.get_topic(&tp.topic).await
                    .map(|opt| opt.map(|t| t.config.is_searchable()).unwrap_or(false))
                    .unwrap_or(false);
                topic_searchable_config
            };

            if is_searchable {
                match Self::create_tantivy_index(
                    config,
                    object_store,
                    segment_index,
                    &tp,
                    canonical_records,
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

    // TODO: Add integration tests with real WAL segments and Tantivy indexes
}
