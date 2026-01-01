//! Segment Index Registry - Track Tantivy and Parquet segments for efficient query planning
//!
//! This module maintains an in-memory and persistent registry of all Tantivy and Parquet segments,
//! enabling efficient query planning for fetch requests and SQL queries. The index tracks:
//! - Segment location (object store path)
//! - Offset ranges (min/max offsets)
//! - Timestamp ranges (min/max timestamps)
//! - Record counts
//! - Creation timestamps
//!
//! The index is used by:
//! - Fetch handler to determine which Tantivy segments to query for offset/timestamp ranges
//! - DataFusion query engine to determine which Parquet files to scan for SQL queries

use chronik_common::{Result, Error};
use chronik_columnar::SegmentIndexProvider;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn, debug};

/// Metadata for a single Tantivy segment
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SegmentMetadata {
    /// Unique segment ID (generated from topic-partition-base_offset-last_offset)
    pub segment_id: String,

    /// Topic name
    pub topic: String,

    /// Partition number
    pub partition: i32,

    /// Minimum offset in this segment (inclusive)
    pub min_offset: i64,

    /// Maximum offset in this segment (inclusive)
    pub max_offset: i64,

    /// Number of records in this segment
    pub record_count: usize,

    /// Minimum timestamp in this segment
    pub min_timestamp: i64,

    /// Maximum timestamp in this segment
    pub max_timestamp: i64,

    /// Object store path (e.g., "tantivy_indexes/topic/partition-0/segment-0-1000.tar.gz")
    pub object_store_path: String,

    /// Size in bytes (tar.gz file size)
    pub size_bytes: u64,

    /// Creation timestamp (when segment was indexed)
    pub created_at: i64,

    /// Compression type used
    pub compression: String,
}

impl SegmentMetadata {
    /// Check if this segment contains the given offset
    pub fn contains_offset(&self, offset: i64) -> bool {
        offset >= self.min_offset && offset <= self.max_offset
    }

    /// Check if this segment overlaps with the given offset range
    pub fn overlaps_offset_range(&self, start: i64, end: i64) -> bool {
        // Segment overlaps if: segment_start <= range_end AND segment_end >= range_start
        self.min_offset <= end && self.max_offset >= start
    }

    /// Check if this segment overlaps with the given timestamp range
    pub fn overlaps_timestamp_range(&self, start: i64, end: i64) -> bool {
        self.min_timestamp <= end && self.max_timestamp >= start
    }
}

/// Metadata for a single Parquet segment (columnar storage)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ParquetSegmentMetadata {
    /// Unique segment ID (generated from topic-partition-base_offset-last_offset-timestamp)
    pub segment_id: String,

    /// Topic name
    pub topic: String,

    /// Partition number
    pub partition: i32,

    /// Minimum offset in this segment (inclusive)
    pub min_offset: i64,

    /// Maximum offset in this segment (inclusive)
    pub max_offset: i64,

    /// Number of records (rows) in this segment
    pub record_count: usize,

    /// Number of row groups in this Parquet file
    pub row_group_count: usize,

    /// Minimum timestamp in this segment
    pub min_timestamp: i64,

    /// Maximum timestamp in this segment
    pub max_timestamp: i64,

    /// Object store path (e.g., "parquet/topic/partition=0/2024/01/01/segment-0-1000.parquet")
    pub object_store_path: String,

    /// Size in bytes (Parquet file size)
    pub size_bytes: u64,

    /// Creation timestamp (when segment was created)
    pub created_at: i64,

    /// Compression codec used (zstd, snappy, gzip, etc.)
    pub compression: String,

    /// Time partition key if time-partitioned (e.g., "2024-01-01-12" for hourly)
    pub time_partition_key: Option<String>,

    /// Parquet file schema fingerprint (for schema evolution tracking)
    pub schema_fingerprint: Option<String>,
}

impl ParquetSegmentMetadata {
    /// Check if this segment contains the given offset
    pub fn contains_offset(&self, offset: i64) -> bool {
        offset >= self.min_offset && offset <= self.max_offset
    }

    /// Check if this segment overlaps with the given offset range
    pub fn overlaps_offset_range(&self, start: i64, end: i64) -> bool {
        self.min_offset <= end && self.max_offset >= start
    }

    /// Check if this segment overlaps with the given timestamp range
    pub fn overlaps_timestamp_range(&self, start: i64, end: i64) -> bool {
        self.min_timestamp <= end && self.max_timestamp >= start
    }

    /// Check if this segment matches a time partition key
    pub fn matches_time_partition(&self, partition_key: &str) -> bool {
        self.time_partition_key.as_deref() == Some(partition_key)
    }
}

/// Topic-partition identifier
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TopicPartition {
    pub topic: String,
    pub partition: i32,
}

impl TopicPartition {
    pub fn new(topic: String, partition: i32) -> Self {
        Self { topic, partition }
    }

    pub fn key(&self) -> String {
        format!("{}-{}", self.topic, self.partition)
    }
}

/// In-memory segment index with persistence
pub struct SegmentIndex {
    /// Map from topic-partition to list of Tantivy segments (sorted by min_offset)
    /// Using BTreeMap for automatic sorting by offset
    segments: Arc<RwLock<HashMap<TopicPartition, BTreeMap<i64, SegmentMetadata>>>>,

    /// Map from topic-partition to list of Parquet segments (sorted by min_offset)
    /// Separate storage for columnar data (used by DataFusion SQL queries)
    parquet_segments: Arc<RwLock<HashMap<TopicPartition, BTreeMap<i64, ParquetSegmentMetadata>>>>,

    /// Path to persistence file (JSON format)
    persistence_path: Option<PathBuf>,

    /// Path to Parquet index persistence file (JSON format)
    parquet_persistence_path: Option<PathBuf>,

    /// Whether auto-save is enabled
    auto_save: bool,
}

impl SegmentIndex {
    /// Create a new segment index without persistence
    pub fn new() -> Self {
        Self {
            segments: Arc::new(RwLock::new(HashMap::new())),
            parquet_segments: Arc::new(RwLock::new(HashMap::new())),
            persistence_path: None,
            parquet_persistence_path: None,
            auto_save: false,
        }
    }

    /// Create a new segment index with persistence
    pub fn with_persistence(path: PathBuf, auto_save: bool) -> Self {
        // Generate parquet index path alongside tantivy index
        let parquet_path = path.with_file_name(
            path.file_stem()
                .map(|s| format!("{}_parquet", s.to_string_lossy()))
                .unwrap_or_else(|| "segment_index_parquet".to_string())
        ).with_extension("json");

        Self {
            segments: Arc::new(RwLock::new(HashMap::new())),
            parquet_segments: Arc::new(RwLock::new(HashMap::new())),
            persistence_path: Some(path),
            parquet_persistence_path: Some(parquet_path),
            auto_save,
        }
    }

    /// Create a new segment index with explicit persistence paths
    pub fn with_persistence_paths(
        tantivy_path: PathBuf,
        parquet_path: PathBuf,
        auto_save: bool,
    ) -> Self {
        Self {
            segments: Arc::new(RwLock::new(HashMap::new())),
            parquet_segments: Arc::new(RwLock::new(HashMap::new())),
            persistence_path: Some(tantivy_path),
            parquet_persistence_path: Some(parquet_path),
            auto_save,
        }
    }

    /// Add a segment to the index
    pub async fn add_segment(&self, metadata: SegmentMetadata) -> Result<()> {
        let tp = TopicPartition::new(metadata.topic.clone(), metadata.partition);

        let mut segments = self.segments.write().await;
        let partition_segments = segments.entry(tp.clone()).or_insert_with(BTreeMap::new);

        // Insert using min_offset as key for automatic sorting
        partition_segments.insert(metadata.min_offset, metadata.clone());

        info!(
            topic = %tp.topic,
            partition = tp.partition,
            segment_id = %metadata.segment_id,
            min_offset = metadata.min_offset,
            max_offset = metadata.max_offset,
            "Added segment to index"
        );

        drop(segments); // Release lock before save

        // Auto-save if enabled
        if self.auto_save {
            self.save().await?;
        }

        Ok(())
    }

    /// Remove a segment from the index
    pub async fn remove_segment(&self, topic: &str, partition: i32, segment_id: &str) -> Result<bool> {
        let tp = TopicPartition::new(topic.to_string(), partition);

        let mut segments = self.segments.write().await;

        if let Some(partition_segments) = segments.get_mut(&tp) {
            // Find and remove the segment
            let mut removed = false;
            partition_segments.retain(|_, seg| {
                if seg.segment_id == segment_id {
                    removed = true;
                    false
                } else {
                    true
                }
            });

            if removed {
                info!(
                    topic = %topic,
                    partition = partition,
                    segment_id = %segment_id,
                    "Removed segment from index"
                );

                drop(segments); // Release lock before save

                // Auto-save if enabled
                if self.auto_save {
                    self.save().await?;
                }

                return Ok(true);
            }
        }

        Ok(false)
    }

    /// Find all segments that overlap with the given offset range
    pub async fn find_segments_by_offset_range(
        &self,
        topic: &str,
        partition: i32,
        start_offset: i64,
        end_offset: i64,
    ) -> Result<Vec<SegmentMetadata>> {
        let tp = TopicPartition::new(topic.to_string(), partition);

        let segments = self.segments.read().await;

        if let Some(partition_segments) = segments.get(&tp) {
            let matching = partition_segments
                .values()
                .filter(|seg| seg.overlaps_offset_range(start_offset, end_offset))
                .cloned()
                .collect();

            debug!(
                topic = %topic,
                partition = partition,
                start_offset = start_offset,
                end_offset = end_offset,
                count = partition_segments.len(),
                "Found matching segments by offset range"
            );

            Ok(matching)
        } else {
            Ok(Vec::new())
        }
    }

    /// Find all segments that overlap with the given timestamp range
    pub async fn find_segments_by_timestamp_range(
        &self,
        topic: &str,
        partition: i32,
        start_timestamp: i64,
        end_timestamp: i64,
    ) -> Result<Vec<SegmentMetadata>> {
        let tp = TopicPartition::new(topic.to_string(), partition);

        let segments = self.segments.read().await;

        if let Some(partition_segments) = segments.get(&tp) {
            let matching: Vec<SegmentMetadata> = partition_segments
                .values()
                .filter(|seg| seg.overlaps_timestamp_range(start_timestamp, end_timestamp))
                .cloned()
                .collect();

            debug!(
                topic = %topic,
                partition = partition,
                start_timestamp = start_timestamp,
                end_timestamp = end_timestamp,
                count = matching.len(),
                "Found matching segments by timestamp range"
            );

            Ok(matching)
        } else {
            Ok(Vec::new())
        }
    }

    /// Get all segments for a topic-partition
    pub async fn get_segments_for_partition(&self, topic: &str, partition: i32) -> Result<Vec<SegmentMetadata>> {
        let tp = TopicPartition::new(topic.to_string(), partition);

        let segments = self.segments.read().await;

        if let Some(partition_segments) = segments.get(&tp) {
            Ok(partition_segments.values().cloned().collect())
        } else {
            Ok(Vec::new())
        }
    }

    /// Get all segments across all topic-partitions
    pub async fn get_all_segments(&self) -> Result<Vec<SegmentMetadata>> {
        let segments = self.segments.read().await;

        let mut all_segments = Vec::new();
        for partition_segments in segments.values() {
            all_segments.extend(partition_segments.values().cloned());
        }

        Ok(all_segments)
    }

    // =========================================================================
    // Parquet Segment Methods (for columnar storage / SQL queries)
    // =========================================================================

    /// Add a Parquet segment to the index
    pub async fn add_parquet_segment(&self, metadata: ParquetSegmentMetadata) -> Result<()> {
        let tp = TopicPartition::new(metadata.topic.clone(), metadata.partition);

        let mut segments = self.parquet_segments.write().await;
        let partition_segments = segments.entry(tp.clone()).or_insert_with(BTreeMap::new);

        // Insert using min_offset as key for automatic sorting
        partition_segments.insert(metadata.min_offset, metadata.clone());

        info!(
            topic = %tp.topic,
            partition = tp.partition,
            segment_id = %metadata.segment_id,
            min_offset = metadata.min_offset,
            max_offset = metadata.max_offset,
            path = %metadata.object_store_path,
            "Added Parquet segment to index"
        );

        drop(segments); // Release lock before save

        // Auto-save if enabled
        if self.auto_save {
            self.save_parquet_index().await?;
        }

        Ok(())
    }

    /// Remove a Parquet segment from the index
    pub async fn remove_parquet_segment(&self, topic: &str, partition: i32, segment_id: &str) -> Result<bool> {
        let tp = TopicPartition::new(topic.to_string(), partition);

        let mut segments = self.parquet_segments.write().await;

        if let Some(partition_segments) = segments.get_mut(&tp) {
            let mut removed = false;
            partition_segments.retain(|_, seg| {
                if seg.segment_id == segment_id {
                    removed = true;
                    false
                } else {
                    true
                }
            });

            if removed {
                info!(
                    topic = %topic,
                    partition = partition,
                    segment_id = %segment_id,
                    "Removed Parquet segment from index"
                );

                drop(segments);

                if self.auto_save {
                    self.save_parquet_index().await?;
                }

                return Ok(true);
            }
        }

        Ok(false)
    }

    /// Find all Parquet segments that overlap with the given offset range
    pub async fn find_parquet_segments_by_offset_range(
        &self,
        topic: &str,
        partition: i32,
        start_offset: i64,
        end_offset: i64,
    ) -> Result<Vec<ParquetSegmentMetadata>> {
        let tp = TopicPartition::new(topic.to_string(), partition);

        let segments = self.parquet_segments.read().await;

        if let Some(partition_segments) = segments.get(&tp) {
            let matching = partition_segments
                .values()
                .filter(|seg| seg.overlaps_offset_range(start_offset, end_offset))
                .cloned()
                .collect();

            debug!(
                topic = %topic,
                partition = partition,
                start_offset = start_offset,
                end_offset = end_offset,
                "Found matching Parquet segments by offset range"
            );

            Ok(matching)
        } else {
            Ok(Vec::new())
        }
    }

    /// Find all Parquet segments that overlap with the given timestamp range
    pub async fn find_parquet_segments_by_timestamp_range(
        &self,
        topic: &str,
        partition: i32,
        start_timestamp: i64,
        end_timestamp: i64,
    ) -> Result<Vec<ParquetSegmentMetadata>> {
        let tp = TopicPartition::new(topic.to_string(), partition);

        let segments = self.parquet_segments.read().await;

        if let Some(partition_segments) = segments.get(&tp) {
            let matching: Vec<ParquetSegmentMetadata> = partition_segments
                .values()
                .filter(|seg| seg.overlaps_timestamp_range(start_timestamp, end_timestamp))
                .cloned()
                .collect();

            debug!(
                topic = %topic,
                partition = partition,
                start_timestamp = start_timestamp,
                end_timestamp = end_timestamp,
                count = matching.len(),
                "Found matching Parquet segments by timestamp range"
            );

            Ok(matching)
        } else {
            Ok(Vec::new())
        }
    }

    /// Find Parquet segments by time partition key (for time-partitioned topics)
    pub async fn find_parquet_segments_by_time_partition(
        &self,
        topic: &str,
        time_partition_key: &str,
    ) -> Result<Vec<ParquetSegmentMetadata>> {
        let segments = self.parquet_segments.read().await;

        let mut matching = Vec::new();

        for (tp, partition_segments) in segments.iter() {
            if tp.topic == topic {
                for seg in partition_segments.values() {
                    if seg.matches_time_partition(time_partition_key) {
                        matching.push(seg.clone());
                    }
                }
            }
        }

        debug!(
            topic = %topic,
            time_partition_key = %time_partition_key,
            count = matching.len(),
            "Found Parquet segments by time partition"
        );

        Ok(matching)
    }

    /// Get all Parquet segments for a topic-partition
    pub async fn get_parquet_segments_for_partition(
        &self,
        topic: &str,
        partition: i32,
    ) -> Result<Vec<ParquetSegmentMetadata>> {
        let tp = TopicPartition::new(topic.to_string(), partition);

        let segments = self.parquet_segments.read().await;

        if let Some(partition_segments) = segments.get(&tp) {
            Ok(partition_segments.values().cloned().collect())
        } else {
            Ok(Vec::new())
        }
    }

    /// Get all Parquet file paths for a topic (used by DataFusion query engine)
    /// Returns paths sorted by partition and offset for deterministic query results
    pub async fn get_parquet_paths(&self, topic: &str) -> Result<Vec<String>> {
        let segments = self.parquet_segments.read().await;

        let mut paths = Vec::new();

        // Collect all paths for this topic
        for (tp, partition_segments) in segments.iter() {
            if tp.topic == topic {
                for seg in partition_segments.values() {
                    paths.push(seg.object_store_path.clone());
                }
            }
        }

        // Sort by path for deterministic ordering
        paths.sort();

        debug!(
            topic = %topic,
            count = paths.len(),
            "Retrieved Parquet paths for topic"
        );

        Ok(paths)
    }

    /// Get Parquet file paths for a topic filtered by time partition keys
    /// Useful for time-based queries (e.g., "last 24 hours")
    pub async fn get_parquet_paths_for_time_range(
        &self,
        topic: &str,
        time_partition_keys: &[String],
    ) -> Result<Vec<String>> {
        let segments = self.parquet_segments.read().await;

        let mut paths = Vec::new();

        for (tp, partition_segments) in segments.iter() {
            if tp.topic == topic {
                for seg in partition_segments.values() {
                    if let Some(ref key) = seg.time_partition_key {
                        if time_partition_keys.contains(key) {
                            paths.push(seg.object_store_path.clone());
                        }
                    }
                }
            }
        }

        paths.sort();

        debug!(
            topic = %topic,
            time_partitions = ?time_partition_keys,
            count = paths.len(),
            "Retrieved Parquet paths for topic with time filter"
        );

        Ok(paths)
    }

    /// Get Parquet file paths for a topic filtered by offset range (across all partitions)
    /// Used by TopicQueryService for partition pruning in SQL queries
    pub async fn get_parquet_paths_by_offset_range(
        &self,
        topic: &str,
        min_offset: Option<i64>,
        max_offset: Option<i64>,
    ) -> Result<Vec<String>> {
        let segments = self.parquet_segments.read().await;

        let mut paths = Vec::new();

        for (tp, partition_segments) in segments.iter() {
            if tp.topic == topic {
                for seg in partition_segments.values() {
                    let matches = match (min_offset, max_offset) {
                        (Some(min), Some(max)) => seg.overlaps_offset_range(min, max),
                        (Some(min), None) => seg.max_offset >= min,
                        (None, Some(max)) => seg.min_offset <= max,
                        (None, None) => true,
                    };
                    if matches {
                        paths.push(seg.object_store_path.clone());
                    }
                }
            }
        }

        paths.sort();

        debug!(
            topic = %topic,
            min_offset = ?min_offset,
            max_offset = ?max_offset,
            count = paths.len(),
            "Retrieved Parquet paths for topic by offset range"
        );

        Ok(paths)
    }

    /// Get Parquet file paths for a topic filtered by timestamp range (across all partitions)
    /// Used by TopicQueryService for partition pruning in SQL queries
    pub async fn get_parquet_paths_by_timestamp_range(
        &self,
        topic: &str,
        min_timestamp: Option<i64>,
        max_timestamp: Option<i64>,
    ) -> Result<Vec<String>> {
        let segments = self.parquet_segments.read().await;

        let mut paths = Vec::new();

        for (tp, partition_segments) in segments.iter() {
            if tp.topic == topic {
                for seg in partition_segments.values() {
                    let matches = match (min_timestamp, max_timestamp) {
                        (Some(min), Some(max)) => seg.overlaps_timestamp_range(min, max),
                        (Some(min), None) => seg.max_timestamp >= min,
                        (None, Some(max)) => seg.min_timestamp <= max,
                        (None, None) => true,
                    };
                    if matches {
                        paths.push(seg.object_store_path.clone());
                    }
                }
            }
        }

        paths.sort();

        debug!(
            topic = %topic,
            min_timestamp = ?min_timestamp,
            max_timestamp = ?max_timestamp,
            count = paths.len(),
            "Retrieved Parquet paths for topic by timestamp range"
        );

        Ok(paths)
    }

    /// Check if a topic has any Parquet segments
    pub async fn has_parquet_segments(&self, topic: &str) -> Result<bool> {
        let segments = self.parquet_segments.read().await;

        for tp in segments.keys() {
            if tp.topic == topic {
                return Ok(true);
            }
        }

        Ok(false)
    }

    /// Get all Parquet segments across all topic-partitions
    pub async fn get_all_parquet_segments(&self) -> Result<Vec<ParquetSegmentMetadata>> {
        let segments = self.parquet_segments.read().await;

        let mut all_segments = Vec::new();
        for partition_segments in segments.values() {
            all_segments.extend(partition_segments.values().cloned());
        }

        Ok(all_segments)
    }

    /// Get statistics about the index (Tantivy segments only, use get_combined_stats for both)
    pub async fn get_stats(&self) -> IndexStats {
        let segments = self.segments.read().await;

        let total_partitions = segments.len();
        let mut total_segments = 0;
        let mut total_records = 0;
        let mut total_bytes = 0;

        for partition_segments in segments.values() {
            total_segments += partition_segments.len();
            for seg in partition_segments.values() {
                total_records += seg.record_count;
                total_bytes += seg.size_bytes;
            }
        }

        IndexStats {
            total_partitions,
            total_segments,
            total_records,
            total_bytes,
        }
    }

    /// Get statistics about Parquet segments
    pub async fn get_parquet_stats(&self) -> ParquetIndexStats {
        let segments = self.parquet_segments.read().await;

        let total_partitions = segments.len();
        let mut total_segments = 0;
        let mut total_records = 0;
        let mut total_bytes = 0;
        let mut total_row_groups = 0;

        for partition_segments in segments.values() {
            total_segments += partition_segments.len();
            for seg in partition_segments.values() {
                total_records += seg.record_count;
                total_bytes += seg.size_bytes;
                total_row_groups += seg.row_group_count;
            }
        }

        ParquetIndexStats {
            total_partitions,
            total_segments,
            total_records,
            total_bytes,
            total_row_groups,
        }
    }

    /// Get combined statistics for both Tantivy and Parquet segments
    pub async fn get_combined_stats(&self) -> CombinedIndexStats {
        let tantivy = self.get_stats().await;
        let parquet = self.get_parquet_stats().await;

        CombinedIndexStats { tantivy, parquet }
    }

    /// Save the Tantivy index to disk (JSON format)
    pub async fn save(&self) -> Result<()> {
        if let Some(ref path) = self.persistence_path {
            let segments = self.segments.read().await;

            // Convert to serializable format
            let serializable: HashMap<String, Vec<SegmentMetadata>> = segments
                .iter()
                .map(|(tp, segs)| {
                    (tp.key(), segs.values().cloned().collect())
                })
                .collect();

            let json = serde_json::to_string_pretty(&serializable)
                .map_err(|e| Error::Internal(format!("Failed to serialize index: {}", e)))?;

            // Ensure parent directory exists
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await
                    .map_err(|e| Error::Internal(format!("Failed to create index directory: {}", e)))?;
            }

            // Write to temp file first, then rename (atomic)
            let temp_path = path.with_extension("tmp");
            tokio::fs::write(&temp_path, json).await
                .map_err(|e| Error::Internal(format!("Failed to write index: {}", e)))?;

            tokio::fs::rename(&temp_path, path).await
                .map_err(|e| Error::Internal(format!("Failed to rename index file: {}", e)))?;

            debug!(path = %path.display(), "Saved segment index to disk");

            Ok(())
        } else {
            Ok(()) // No persistence configured
        }
    }

    /// Save the Parquet index to disk (JSON format)
    pub async fn save_parquet_index(&self) -> Result<()> {
        if let Some(ref path) = self.parquet_persistence_path {
            let segments = self.parquet_segments.read().await;

            // Convert to serializable format
            let serializable: HashMap<String, Vec<ParquetSegmentMetadata>> = segments
                .iter()
                .map(|(tp, segs)| {
                    (tp.key(), segs.values().cloned().collect())
                })
                .collect();

            let json = serde_json::to_string_pretty(&serializable)
                .map_err(|e| Error::Internal(format!("Failed to serialize Parquet index: {}", e)))?;

            // Ensure parent directory exists
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await
                    .map_err(|e| Error::Internal(format!("Failed to create Parquet index directory: {}", e)))?;
            }

            // Write to temp file first, then rename (atomic)
            let temp_path = path.with_extension("tmp");
            tokio::fs::write(&temp_path, json).await
                .map_err(|e| Error::Internal(format!("Failed to write Parquet index: {}", e)))?;

            tokio::fs::rename(&temp_path, path).await
                .map_err(|e| Error::Internal(format!("Failed to rename Parquet index file: {}", e)))?;

            debug!(path = %path.display(), "Saved Parquet segment index to disk");

            Ok(())
        } else {
            Ok(()) // No persistence configured
        }
    }

    /// Save both Tantivy and Parquet indexes to disk
    pub async fn save_all(&self) -> Result<()> {
        self.save().await?;
        self.save_parquet_index().await?;
        Ok(())
    }

    /// Load the Tantivy index from disk
    pub async fn load(&self) -> Result<()> {
        if let Some(ref path) = self.persistence_path {
            if !path.exists() {
                info!("Segment index file does not exist, starting with empty index");
                return Ok(());
            }

            let json = tokio::fs::read_to_string(path).await
                .map_err(|e| Error::Internal(format!("Failed to read index: {}", e)))?;

            let serializable: HashMap<String, Vec<SegmentMetadata>> = serde_json::from_str(&json)
                .map_err(|e| Error::Internal(format!("Failed to deserialize index: {}", e)))?;

            let mut segments = self.segments.write().await;
            segments.clear();

            // Convert back to internal format
            for (key, segs) in serializable {
                // Parse topic-partition key
                let parts: Vec<&str> = key.rsplitn(2, '-').collect();
                if parts.len() != 2 {
                    warn!("Invalid topic-partition key in index: {}", key);
                    continue;
                }

                let partition: i32 = match parts[0].parse() {
                    Ok(p) => p,
                    Err(_) => {
                        warn!("Invalid partition number in key: {}", key);
                        continue;
                    }
                };

                let topic = parts[1].to_string();
                let tp = TopicPartition::new(topic, partition);

                let mut btree = BTreeMap::new();
                for seg in segs {
                    btree.insert(seg.min_offset, seg);
                }

                segments.insert(tp, btree);
            }

            info!(
                path = %path.display(),
                partitions = segments.len(),
                "Loaded segment index from disk"
            );

            Ok(())
        } else {
            Ok(()) // No persistence configured
        }
    }

    /// Load the Parquet index from disk
    pub async fn load_parquet_index(&self) -> Result<()> {
        if let Some(ref path) = self.parquet_persistence_path {
            if !path.exists() {
                info!("Parquet index file does not exist, starting with empty Parquet index");
                return Ok(());
            }

            let json = tokio::fs::read_to_string(path).await
                .map_err(|e| Error::Internal(format!("Failed to read Parquet index: {}", e)))?;

            let serializable: HashMap<String, Vec<ParquetSegmentMetadata>> = serde_json::from_str(&json)
                .map_err(|e| Error::Internal(format!("Failed to deserialize Parquet index: {}", e)))?;

            let mut segments = self.parquet_segments.write().await;
            segments.clear();

            // Convert back to internal format
            for (key, segs) in serializable {
                // Parse topic-partition key
                let parts: Vec<&str> = key.rsplitn(2, '-').collect();
                if parts.len() != 2 {
                    warn!("Invalid topic-partition key in Parquet index: {}", key);
                    continue;
                }

                let partition: i32 = match parts[0].parse() {
                    Ok(p) => p,
                    Err(_) => {
                        warn!("Invalid partition number in Parquet index key: {}", key);
                        continue;
                    }
                };

                let topic = parts[1].to_string();
                let tp = TopicPartition::new(topic, partition);

                let mut btree = BTreeMap::new();
                for seg in segs {
                    btree.insert(seg.min_offset, seg);
                }

                segments.insert(tp, btree);
            }

            info!(
                path = %path.display(),
                partitions = segments.len(),
                "Loaded Parquet segment index from disk"
            );

            Ok(())
        } else {
            Ok(()) // No persistence configured
        }
    }

    /// Load both Tantivy and Parquet indexes from disk
    pub async fn load_all(&self) -> Result<()> {
        self.load().await?;
        self.load_parquet_index().await?;
        Ok(())
    }

    /// Clear all Tantivy segments from the index
    pub async fn clear(&self) -> Result<()> {
        let mut segments = self.segments.write().await;
        segments.clear();

        info!("Cleared Tantivy segment index");

        drop(segments);

        // Auto-save if enabled
        if self.auto_save {
            self.save().await?;
        }

        Ok(())
    }

    /// Clear all Parquet segments from the index
    pub async fn clear_parquet(&self) -> Result<()> {
        let mut segments = self.parquet_segments.write().await;
        segments.clear();

        info!("Cleared Parquet segment index");

        drop(segments);

        // Auto-save if enabled
        if self.auto_save {
            self.save_parquet_index().await?;
        }

        Ok(())
    }

    /// Clear all segments (both Tantivy and Parquet) from the index
    pub async fn clear_all(&self) -> Result<()> {
        self.clear().await?;
        self.clear_parquet().await?;
        Ok(())
    }
}

impl Default for SegmentIndex {
    fn default() -> Self {
        Self::new()
    }
}

/// Statistics about the Tantivy segment index
#[derive(Debug, Clone)]
pub struct IndexStats {
    pub total_partitions: usize,
    pub total_segments: usize,
    pub total_records: usize,
    pub total_bytes: u64,
}

/// Statistics about the Parquet segment index
#[derive(Debug, Clone)]
pub struct ParquetIndexStats {
    pub total_partitions: usize,
    pub total_segments: usize,
    pub total_records: usize,
    pub total_bytes: u64,
    pub total_row_groups: usize,
}

/// Combined statistics for both Tantivy and Parquet indexes
#[derive(Debug, Clone)]
pub struct CombinedIndexStats {
    pub tantivy: IndexStats,
    pub parquet: ParquetIndexStats,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_segment(topic: &str, partition: i32, min_offset: i64, max_offset: i64) -> SegmentMetadata {
        SegmentMetadata {
            segment_id: format!("{}-{}-{}-{}", topic, partition, min_offset, max_offset),
            topic: topic.to_string(),
            partition,
            min_offset,
            max_offset,
            record_count: (max_offset - min_offset + 1) as usize,
            min_timestamp: min_offset * 1000,
            max_timestamp: max_offset * 1000,
            object_store_path: format!("segments/{}/{}/segment-{}-{}.tar.gz", topic, partition, min_offset, max_offset),
            size_bytes: 1024 * 1024,
            created_at: chrono::Utc::now().timestamp(),
            compression: "snappy".to_string(),
        }
    }

    #[tokio::test]
    async fn test_add_and_get_segments() {
        let index = SegmentIndex::new();

        let seg1 = create_test_segment("test-topic", 0, 0, 99);
        let seg2 = create_test_segment("test-topic", 0, 100, 199);

        index.add_segment(seg1.clone()).await.unwrap();
        index.add_segment(seg2.clone()).await.unwrap();

        let all_segs = index.get_all_segments().await.unwrap();
        assert_eq!(all_segs.len(), 2);
        assert_eq!(all_segs[0].min_offset, 0);
        assert_eq!(all_segs[1].min_offset, 100);
    }

    #[tokio::test]
    async fn test_find_by_offset_range() {
        let index = SegmentIndex::new();

        index.add_segment(create_test_segment("test-topic", 0, 0, 99)).await.unwrap();
        index.add_segment(create_test_segment("test-topic", 0, 100, 199)).await.unwrap();
        index.add_segment(create_test_segment("test-topic", 0, 200, 299)).await.unwrap();

        // Query range that overlaps first two segments
        let matching = index.find_segments_by_offset_range("test-topic", 0, 50, 150).await.unwrap();
        assert_eq!(matching.len(), 2);

        // Query range within single segment
        let matching = index.find_segments_by_offset_range("test-topic", 0, 10, 20).await.unwrap();
        assert_eq!(matching.len(), 1);

        // Query range that spans all segments
        let matching = index.find_segments_by_offset_range("test-topic", 0, 0, 299).await.unwrap();
        assert_eq!(matching.len(), 3);
    }

    #[tokio::test]
    async fn test_remove_segment() {
        let index = SegmentIndex::new();

        let seg = create_test_segment("test-topic", 0, 0, 99);
        index.add_segment(seg.clone()).await.unwrap();

        let removed = index.remove_segment("test-topic", 0, &seg.segment_id).await.unwrap();
        assert!(removed);

        let all_segs = index.get_all_segments().await.unwrap();
        assert_eq!(all_segs.len(), 0);
    }

    #[tokio::test]
    async fn test_segment_metadata_overlap() {
        let seg = create_test_segment("test", 0, 100, 199);

        // Test contains_offset
        assert!(seg.contains_offset(100));
        assert!(seg.contains_offset(150));
        assert!(seg.contains_offset(199));
        assert!(!seg.contains_offset(99));
        assert!(!seg.contains_offset(200));

        // Test overlaps_offset_range
        assert!(seg.overlaps_offset_range(50, 150));   // Overlaps left
        assert!(seg.overlaps_offset_range(150, 250));  // Overlaps right
        assert!(seg.overlaps_offset_range(120, 180));  // Inside
        assert!(seg.overlaps_offset_range(0, 300));    // Contains
        assert!(!seg.overlaps_offset_range(0, 99));    // Before
        assert!(!seg.overlaps_offset_range(200, 300)); // After
    }

    #[tokio::test]
    async fn test_get_stats() {
        let index = SegmentIndex::new();

        index.add_segment(create_test_segment("topic1", 0, 0, 99)).await.unwrap();
        index.add_segment(create_test_segment("topic1", 1, 0, 99)).await.unwrap();
        index.add_segment(create_test_segment("topic2", 0, 0, 99)).await.unwrap();

        let stats = index.get_stats().await;
        assert_eq!(stats.total_partitions, 3);
        assert_eq!(stats.total_segments, 3);
        assert_eq!(stats.total_records, 300); // 100 records per segment
    }

    // =========================================================================
    // Parquet Segment Tests
    // =========================================================================

    fn create_test_parquet_segment(
        topic: &str,
        partition: i32,
        min_offset: i64,
        max_offset: i64,
        time_partition: Option<&str>,
    ) -> ParquetSegmentMetadata {
        ParquetSegmentMetadata {
            segment_id: format!("{}-{}-{}-{}-parquet", topic, partition, min_offset, max_offset),
            topic: topic.to_string(),
            partition,
            min_offset,
            max_offset,
            record_count: (max_offset - min_offset + 1) as usize,
            row_group_count: 1,
            min_timestamp: min_offset * 1000,
            max_timestamp: max_offset * 1000,
            object_store_path: format!(
                "parquet/{}/partition={}/{}/segment-{}-{}.parquet",
                topic,
                partition,
                time_partition.unwrap_or("none"),
                min_offset,
                max_offset
            ),
            size_bytes: 2 * 1024 * 1024, // 2MB
            created_at: chrono::Utc::now().timestamp(),
            compression: "zstd".to_string(),
            time_partition_key: time_partition.map(|s| s.to_string()),
            schema_fingerprint: None,
        }
    }

    #[tokio::test]
    async fn test_add_and_get_parquet_segments() {
        let index = SegmentIndex::new();

        let seg1 = create_test_parquet_segment("test-topic", 0, 0, 99, Some("2024-01-01"));
        let seg2 = create_test_parquet_segment("test-topic", 0, 100, 199, Some("2024-01-01"));

        index.add_parquet_segment(seg1.clone()).await.unwrap();
        index.add_parquet_segment(seg2.clone()).await.unwrap();

        let all_segs = index.get_all_parquet_segments().await.unwrap();
        assert_eq!(all_segs.len(), 2);
        assert_eq!(all_segs[0].min_offset, 0);
        assert_eq!(all_segs[1].min_offset, 100);
    }

    #[tokio::test]
    async fn test_get_parquet_paths() {
        let index = SegmentIndex::new();

        index.add_parquet_segment(create_test_parquet_segment("topic1", 0, 0, 99, Some("2024-01-01"))).await.unwrap();
        index.add_parquet_segment(create_test_parquet_segment("topic1", 0, 100, 199, Some("2024-01-02"))).await.unwrap();
        index.add_parquet_segment(create_test_parquet_segment("topic1", 1, 0, 99, Some("2024-01-01"))).await.unwrap();
        index.add_parquet_segment(create_test_parquet_segment("topic2", 0, 0, 99, None)).await.unwrap();

        // Get all paths for topic1
        let paths = index.get_parquet_paths("topic1").await.unwrap();
        assert_eq!(paths.len(), 3);

        // Get all paths for topic2
        let paths = index.get_parquet_paths("topic2").await.unwrap();
        assert_eq!(paths.len(), 1);
    }

    #[tokio::test]
    async fn test_get_parquet_paths_for_time_range() {
        let index = SegmentIndex::new();

        index.add_parquet_segment(create_test_parquet_segment("topic1", 0, 0, 99, Some("2024-01-01"))).await.unwrap();
        index.add_parquet_segment(create_test_parquet_segment("topic1", 0, 100, 199, Some("2024-01-02"))).await.unwrap();
        index.add_parquet_segment(create_test_parquet_segment("topic1", 1, 0, 99, Some("2024-01-01"))).await.unwrap();

        // Get paths for specific time partition
        let time_keys = vec!["2024-01-01".to_string()];
        let paths = index.get_parquet_paths_for_time_range("topic1", &time_keys).await.unwrap();
        assert_eq!(paths.len(), 2); // Two partitions with same date
    }

    #[tokio::test]
    async fn test_find_parquet_segments_by_offset_range() {
        let index = SegmentIndex::new();

        index.add_parquet_segment(create_test_parquet_segment("topic1", 0, 0, 99, None)).await.unwrap();
        index.add_parquet_segment(create_test_parquet_segment("topic1", 0, 100, 199, None)).await.unwrap();
        index.add_parquet_segment(create_test_parquet_segment("topic1", 0, 200, 299, None)).await.unwrap();

        // Query range that overlaps first two segments
        let matching = index.find_parquet_segments_by_offset_range("topic1", 0, 50, 150).await.unwrap();
        assert_eq!(matching.len(), 2);
    }

    #[tokio::test]
    async fn test_remove_parquet_segment() {
        let index = SegmentIndex::new();

        let seg = create_test_parquet_segment("test-topic", 0, 0, 99, None);
        index.add_parquet_segment(seg.clone()).await.unwrap();

        let removed = index.remove_parquet_segment("test-topic", 0, &seg.segment_id).await.unwrap();
        assert!(removed);

        let all_segs = index.get_all_parquet_segments().await.unwrap();
        assert_eq!(all_segs.len(), 0);
    }

    #[tokio::test]
    async fn test_parquet_stats() {
        let index = SegmentIndex::new();

        index.add_parquet_segment(create_test_parquet_segment("topic1", 0, 0, 99, None)).await.unwrap();
        index.add_parquet_segment(create_test_parquet_segment("topic1", 1, 0, 99, None)).await.unwrap();
        index.add_parquet_segment(create_test_parquet_segment("topic2", 0, 0, 99, None)).await.unwrap();

        let stats = index.get_parquet_stats().await;
        assert_eq!(stats.total_partitions, 3);
        assert_eq!(stats.total_segments, 3);
        assert_eq!(stats.total_records, 300); // 100 records per segment
        assert_eq!(stats.total_row_groups, 3); // 1 row group per segment
    }

    #[tokio::test]
    async fn test_combined_stats() {
        let index = SegmentIndex::new();

        // Add Tantivy segments
        index.add_segment(create_test_segment("topic1", 0, 0, 99)).await.unwrap();

        // Add Parquet segments
        index.add_parquet_segment(create_test_parquet_segment("topic1", 0, 0, 99, None)).await.unwrap();

        let combined = index.get_combined_stats().await;
        assert_eq!(combined.tantivy.total_segments, 1);
        assert_eq!(combined.parquet.total_segments, 1);
    }

    #[tokio::test]
    async fn test_parquet_segment_metadata_time_partition() {
        let seg = create_test_parquet_segment("test", 0, 0, 99, Some("2024-01-15-12"));

        assert!(seg.matches_time_partition("2024-01-15-12"));
        assert!(!seg.matches_time_partition("2024-01-15-13"));
        assert!(!seg.matches_time_partition("2024-01-16-12"));
    }
}

// =========================================================================
// SegmentIndexProvider Implementation
// =========================================================================
// Implements the trait from chronik-columnar to enable TopicQueryService
// to use SegmentIndex for partition pruning in SQL queries.

#[async_trait::async_trait]
impl SegmentIndexProvider for SegmentIndex {
    async fn get_parquet_paths(&self, topic: &str) -> anyhow::Result<Vec<String>> {
        self.get_parquet_paths(topic)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to get Parquet paths: {}", e))
    }

    async fn get_parquet_paths_by_offset_range(
        &self,
        topic: &str,
        min_offset: Option<i64>,
        max_offset: Option<i64>,
    ) -> anyhow::Result<Vec<String>> {
        SegmentIndex::get_parquet_paths_by_offset_range(self, topic, min_offset, max_offset)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to get Parquet paths by offset range: {}", e))
    }

    async fn get_parquet_paths_by_timestamp_range(
        &self,
        topic: &str,
        min_timestamp: Option<i64>,
        max_timestamp: Option<i64>,
    ) -> anyhow::Result<Vec<String>> {
        SegmentIndex::get_parquet_paths_by_timestamp_range(self, topic, min_timestamp, max_timestamp)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to get Parquet paths by timestamp range: {}", e))
    }

    async fn get_parquet_paths_by_time_partitions(
        &self,
        topic: &str,
        time_partition_keys: &[String],
    ) -> anyhow::Result<Vec<String>> {
        self.get_parquet_paths_for_time_range(topic, time_partition_keys)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to get Parquet paths by time partitions: {}", e))
    }

    async fn has_parquet_segments(&self, topic: &str) -> anyhow::Result<bool> {
        SegmentIndex::has_parquet_segments(self, topic)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to check for Parquet segments: {}", e))
    }
}

#[cfg(test)]
mod segment_index_provider_tests {
    use super::*;

    fn create_test_parquet_segment(
        topic: &str,
        partition: i32,
        min_offset: i64,
        max_offset: i64,
        time_partition: Option<&str>,
    ) -> ParquetSegmentMetadata {
        ParquetSegmentMetadata {
            segment_id: format!("{}-{}-{}-{}-parquet", topic, partition, min_offset, max_offset),
            topic: topic.to_string(),
            partition,
            min_offset,
            max_offset,
            record_count: (max_offset - min_offset + 1) as usize,
            row_group_count: 1,
            min_timestamp: min_offset * 1000,
            max_timestamp: max_offset * 1000,
            object_store_path: format!(
                "parquet/{}/partition={}/{}/segment-{}-{}.parquet",
                topic,
                partition,
                time_partition.unwrap_or("none"),
                min_offset,
                max_offset
            ),
            size_bytes: 2 * 1024 * 1024,
            created_at: chrono::Utc::now().timestamp(),
            compression: "zstd".to_string(),
            time_partition_key: time_partition.map(|s| s.to_string()),
            schema_fingerprint: None,
        }
    }

    #[tokio::test]
    async fn test_provider_get_parquet_paths() {
        let index = SegmentIndex::new();

        index.add_parquet_segment(create_test_parquet_segment("topic1", 0, 0, 99, None))
            .await
            .unwrap();
        index.add_parquet_segment(create_test_parquet_segment("topic1", 0, 100, 199, None))
            .await
            .unwrap();
        index.add_parquet_segment(create_test_parquet_segment("topic2", 0, 0, 99, None))
            .await
            .unwrap();

        // Use trait methods
        let provider: &dyn SegmentIndexProvider = &index;

        let paths = provider.get_parquet_paths("topic1").await.unwrap();
        assert_eq!(paths.len(), 2);

        let paths = provider.get_parquet_paths("topic2").await.unwrap();
        assert_eq!(paths.len(), 1);

        let paths = provider.get_parquet_paths("topic3").await.unwrap();
        assert_eq!(paths.len(), 0);
    }

    #[tokio::test]
    async fn test_provider_offset_range_filter() {
        let index = SegmentIndex::new();

        index.add_parquet_segment(create_test_parquet_segment("topic1", 0, 0, 99, None))
            .await
            .unwrap();
        index.add_parquet_segment(create_test_parquet_segment("topic1", 0, 100, 199, None))
            .await
            .unwrap();
        index.add_parquet_segment(create_test_parquet_segment("topic1", 0, 200, 299, None))
            .await
            .unwrap();

        let provider: &dyn SegmentIndexProvider = &index;

        // Query offset 50-150 should match first two segments
        let paths = provider
            .get_parquet_paths_by_offset_range("topic1", Some(50), Some(150))
            .await
            .unwrap();
        assert_eq!(paths.len(), 2);

        // Query offset 250+ should match only third segment
        let paths = provider
            .get_parquet_paths_by_offset_range("topic1", Some(250), None)
            .await
            .unwrap();
        assert_eq!(paths.len(), 1);

        // Query offset up to 50 should match only first segment
        let paths = provider
            .get_parquet_paths_by_offset_range("topic1", None, Some(50))
            .await
            .unwrap();
        assert_eq!(paths.len(), 1);

        // No filter should return all
        let paths = provider
            .get_parquet_paths_by_offset_range("topic1", None, None)
            .await
            .unwrap();
        assert_eq!(paths.len(), 3);
    }

    #[tokio::test]
    async fn test_provider_timestamp_range_filter() {
        let index = SegmentIndex::new();

        // Segments with timestamps 0-99000, 100000-199000, 200000-299000
        index.add_parquet_segment(create_test_parquet_segment("topic1", 0, 0, 99, None))
            .await
            .unwrap();
        index.add_parquet_segment(create_test_parquet_segment("topic1", 0, 100, 199, None))
            .await
            .unwrap();
        index.add_parquet_segment(create_test_parquet_segment("topic1", 0, 200, 299, None))
            .await
            .unwrap();

        let provider: &dyn SegmentIndexProvider = &index;

        // Query timestamp 50000-150000 should match first two segments
        let paths = provider
            .get_parquet_paths_by_timestamp_range("topic1", Some(50000), Some(150000))
            .await
            .unwrap();
        assert_eq!(paths.len(), 2);

        // Query timestamp 250000+ should match only third segment
        let paths = provider
            .get_parquet_paths_by_timestamp_range("topic1", Some(250000), None)
            .await
            .unwrap();
        assert_eq!(paths.len(), 1);
    }

    #[tokio::test]
    async fn test_provider_time_partition_filter() {
        let index = SegmentIndex::new();

        index.add_parquet_segment(create_test_parquet_segment("topic1", 0, 0, 99, Some("2024-01-01")))
            .await
            .unwrap();
        index.add_parquet_segment(create_test_parquet_segment("topic1", 0, 100, 199, Some("2024-01-02")))
            .await
            .unwrap();
        index.add_parquet_segment(create_test_parquet_segment("topic1", 1, 0, 99, Some("2024-01-01")))
            .await
            .unwrap();

        let provider: &dyn SegmentIndexProvider = &index;

        // Query only 2024-01-01 should match two segments
        let paths = provider
            .get_parquet_paths_by_time_partitions("topic1", &["2024-01-01".to_string()])
            .await
            .unwrap();
        assert_eq!(paths.len(), 2);

        // Query both days should match all three
        let paths = provider
            .get_parquet_paths_by_time_partitions(
                "topic1",
                &["2024-01-01".to_string(), "2024-01-02".to_string()],
            )
            .await
            .unwrap();
        assert_eq!(paths.len(), 3);
    }

    #[tokio::test]
    async fn test_provider_has_parquet_segments() {
        let index = SegmentIndex::new();

        index.add_parquet_segment(create_test_parquet_segment("topic1", 0, 0, 99, None))
            .await
            .unwrap();

        let provider: &dyn SegmentIndexProvider = &index;

        assert!(provider.has_parquet_segments("topic1").await.unwrap());
        assert!(!provider.has_parquet_segments("topic2").await.unwrap());
    }

    #[tokio::test]
    async fn test_provider_cross_partition_filter() {
        let index = SegmentIndex::new();

        // Add segments across multiple partitions
        index.add_parquet_segment(create_test_parquet_segment("topic1", 0, 0, 99, None))
            .await
            .unwrap();
        index.add_parquet_segment(create_test_parquet_segment("topic1", 1, 0, 99, None))
            .await
            .unwrap();
        index.add_parquet_segment(create_test_parquet_segment("topic1", 2, 0, 99, None))
            .await
            .unwrap();

        let provider: &dyn SegmentIndexProvider = &index;

        // All partitions should be included when querying by offset
        let paths = provider
            .get_parquet_paths_by_offset_range("topic1", Some(0), Some(100))
            .await
            .unwrap();
        assert_eq!(paths.len(), 3);
    }
}
