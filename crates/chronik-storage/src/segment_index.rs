//! Segment Index Registry - Track Tantivy segments for efficient query planning
//!
//! This module maintains an in-memory and persistent registry of all Tantivy segments,
//! enabling efficient query planning for fetch requests. The index tracks:
//! - Segment location (object store path)
//! - Offset ranges (min/max offsets)
//! - Timestamp ranges (min/max timestamps)
//! - Record counts
//! - Creation timestamps
//!
//! The index is used by the fetch handler to determine which segments to query
//! for a given offset range or timestamp range.

use chronik_common::{Result, Error};
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
    /// Map from topic-partition to list of segments (sorted by min_offset)
    /// Using BTreeMap for automatic sorting by offset
    segments: Arc<RwLock<HashMap<TopicPartition, BTreeMap<i64, SegmentMetadata>>>>,

    /// Path to persistence file (JSON format)
    persistence_path: Option<PathBuf>,

    /// Whether auto-save is enabled
    auto_save: bool,
}

impl SegmentIndex {
    /// Create a new segment index without persistence
    pub fn new() -> Self {
        Self {
            segments: Arc::new(RwLock::new(HashMap::new())),
            persistence_path: None,
            auto_save: false,
        }
    }

    /// Create a new segment index with persistence
    pub fn with_persistence(path: PathBuf, auto_save: bool) -> Self {
        Self {
            segments: Arc::new(RwLock::new(HashMap::new())),
            persistence_path: Some(path),
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

    /// Get statistics about the index
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

    /// Save the index to disk (JSON format)
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

    /// Load the index from disk
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

    /// Clear all segments from the index
    pub async fn clear(&self) -> Result<()> {
        let mut segments = self.segments.write().await;
        segments.clear();

        info!("Cleared segment index");

        drop(segments);

        // Auto-save if enabled
        if self.auto_save {
            self.save().await?;
        }

        Ok(())
    }
}

impl Default for SegmentIndex {
    fn default() -> Self {
        Self::new()
    }
}

/// Statistics about the segment index
#[derive(Debug, Clone)]
pub struct IndexStats {
    pub total_partitions: usize,
    pub total_segments: usize,
    pub total_records: usize,
    pub total_bytes: u64,
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

        let all_segs = index.get_all_segments("test-topic", 0).await.unwrap();
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

        let all_segs = index.get_all_segments("test-topic", 0).await.unwrap();
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
}
