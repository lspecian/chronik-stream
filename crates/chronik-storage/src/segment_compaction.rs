//! Segment Compaction - Cleanup and optimization of Tantivy segments
//!
//! This module implements segment compaction strategies to:
//! 1. Delete old segments based on retention policies (time-based, size-based)
//! 2. Clean up orphaned segments
//! 3. Provide stats on segments that can be compacted
//!
//! Future enhancements (v1.3.36+):
//! - Merge small segments into larger ones
//! - Rewrite segments with better compression
//! - Deduplicate overlapping segments

use crate::{
    object_store::ObjectStore,
    segment_index::{SegmentIndex, SegmentMetadata, TopicPartition},
};
use chronik_common::{Result, Error};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{info, warn, debug, instrument};

/// Compaction strategy
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum CompactionStrategy {
    /// Delete segments older than specified seconds
    TimeBasedRetention {
        /// Maximum age in seconds
        max_age_secs: i64,
    },

    /// Delete segments until total size is below threshold
    SizeBasedRetention {
        /// Maximum total size in bytes per topic-partition
        max_size_bytes: u64,
    },

    /// Delete segments beyond a certain offset (keep only recent offsets)
    OffsetBasedRetention {
        /// Keep only segments with max_offset >= (current_high_watermark - keep_offsets)
        keep_offsets: i64,
    },

    /// Hybrid: combine time and size limits
    Hybrid {
        max_age_secs: Option<i64>,
        max_size_bytes: Option<u64>,
        keep_offsets: Option<i64>,
    },
}

impl Default for CompactionStrategy {
    fn default() -> Self {
        CompactionStrategy::Hybrid {
            max_age_secs: Some(7 * 24 * 3600), // 7 days
            max_size_bytes: Some(10 * 1024 * 1024 * 1024), // 10 GB per partition
            keep_offsets: Some(1_000_000), // Keep last 1M offsets
        }
    }
}

/// Configuration for segment compaction
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionConfig {
    /// Compaction strategy to use
    pub strategy: CompactionStrategy,

    /// Dry run mode (log what would be deleted without actually deleting)
    pub dry_run: bool,

    /// Delete segments from object store
    pub delete_from_object_store: bool,

    /// Remove segments from index
    pub remove_from_index: bool,

    /// Maximum number of segments to delete per compaction run (safety limit)
    pub max_deletions_per_run: usize,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            strategy: CompactionStrategy::default(),
            dry_run: false,
            delete_from_object_store: true,
            remove_from_index: true,
            max_deletions_per_run: 100,
        }
    }
}

/// Statistics from a compaction run
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct CompactionStats {
    /// Number of segments identified for deletion
    pub segments_identified: usize,

    /// Number of segments actually deleted
    pub segments_deleted: usize,

    /// Total bytes freed
    pub bytes_freed: u64,

    /// Number of errors encountered
    pub errors: usize,

    /// Duration of compaction run (milliseconds)
    pub duration_ms: u64,

    /// Dry run mode
    pub dry_run: bool,
}

/// Segment compactor
pub struct SegmentCompactor {
    config: CompactionConfig,
    segment_index: Arc<SegmentIndex>,
    object_store: Arc<dyn ObjectStore>,
}

impl SegmentCompactor {
    /// Create a new segment compactor
    pub fn new(
        config: CompactionConfig,
        segment_index: Arc<SegmentIndex>,
        object_store: Arc<dyn ObjectStore>,
    ) -> Self {
        Self {
            config,
            segment_index,
            object_store,
        }
    }

    /// Run compaction on all topic-partitions
    #[instrument(skip(self))]
    pub async fn compact_all(&self) -> Result<CompactionStats> {
        let start = SystemTime::now();
        let mut stats = CompactionStats {
            dry_run: self.config.dry_run,
            ..Default::default()
        };

        info!(
            strategy = ?self.config.strategy,
            dry_run = self.config.dry_run,
            "Starting segment compaction"
        );

        // Get all segments from index
        let all_segments = self.segment_index.get_all_segments().await?;

        // Group segments by topic-partition
        let mut segments_by_tp: std::collections::HashMap<TopicPartition, Vec<SegmentMetadata>> =
            std::collections::HashMap::new();

        for segment in all_segments {
            let tp = TopicPartition::new(segment.topic.clone(), segment.partition);
            segments_by_tp.entry(tp).or_insert_with(Vec::new).push(segment);
        }

        info!(
            topic_partitions = segments_by_tp.len(),
            "Grouped segments by topic-partition"
        );

        // Compact each topic-partition
        for (tp, mut segments) in segments_by_tp {
            // Sort segments by min_offset (oldest first)
            segments.sort_by_key(|s| s.min_offset);

            match self.compact_partition(&tp, segments, &mut stats).await {
                Ok(_) => {}
                Err(e) => {
                    warn!(
                        topic = %tp.topic,
                        partition = tp.partition,
                        error = %e,
                        "Failed to compact partition"
                    );
                    stats.errors += 1;
                }
            }

            // Check safety limit
            if stats.segments_deleted >= self.config.max_deletions_per_run {
                warn!(
                    deleted = stats.segments_deleted,
                    limit = self.config.max_deletions_per_run,
                    "Reached deletion limit, stopping compaction"
                );
                break;
            }
        }

        stats.duration_ms = start.elapsed().unwrap().as_millis() as u64;

        info!(
            identified = stats.segments_identified,
            deleted = stats.segments_deleted,
            bytes_freed = stats.bytes_freed,
            errors = stats.errors,
            duration_ms = stats.duration_ms,
            dry_run = stats.dry_run,
            "Compaction completed"
        );

        Ok(stats)
    }

    /// Compact a single topic-partition
    #[instrument(skip(self, segments, stats))]
    async fn compact_partition(
        &self,
        tp: &TopicPartition,
        segments: Vec<SegmentMetadata>,
        stats: &mut CompactionStats,
    ) -> Result<()> {
        if segments.is_empty() {
            return Ok(());
        }

        // Identify segments to delete based on strategy
        let to_delete = self.identify_segments_to_delete(tp, &segments).await?;

        if to_delete.is_empty() {
            debug!(
                topic = %tp.topic,
                partition = tp.partition,
                "No segments to compact"
            );
            return Ok(());
        }

        info!(
            topic = %tp.topic,
            partition = tp.partition,
            segments_to_delete = to_delete.len(),
            total_segments = segments.len(),
            "Identified segments for deletion"
        );

        stats.segments_identified += to_delete.len();

        // Delete segments
        for segment in to_delete {
            if stats.segments_deleted >= self.config.max_deletions_per_run {
                break;
            }

            match self.delete_segment(&segment).await {
                Ok(_) => {
                    stats.segments_deleted += 1;
                    stats.bytes_freed += segment.size_bytes;
                }
                Err(e) => {
                    warn!(
                        segment_id = %segment.segment_id,
                        error = %e,
                        "Failed to delete segment"
                    );
                    stats.errors += 1;
                }
            }
        }

        Ok(())
    }

    /// Identify segments to delete based on configured strategy
    #[instrument(skip(self, segments))]
    async fn identify_segments_to_delete(
        &self,
        tp: &TopicPartition,
        segments: &[SegmentMetadata],
    ) -> Result<Vec<SegmentMetadata>> {
        let mut to_delete = Vec::new();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        match &self.config.strategy {
            CompactionStrategy::TimeBasedRetention { max_age_secs } => {
                let cutoff = now - max_age_secs;
                for segment in segments {
                    if segment.created_at < cutoff {
                        to_delete.push(segment.clone());
                    }
                }
            }

            CompactionStrategy::SizeBasedRetention { max_size_bytes } => {
                // Keep segments until total size exceeds limit
                // Delete oldest segments first (segments are already sorted by min_offset)
                let mut total_size: u64 = 0;
                let mut keep_from_index = 0;

                // Scan from newest to oldest to find where to start deleting
                for (i, segment) in segments.iter().enumerate().rev() {
                    total_size += segment.size_bytes;
                    if total_size > *max_size_bytes {
                        keep_from_index = i + 1;
                        break;
                    }
                }

                // Delete segments older than keep_from_index
                to_delete = segments[..keep_from_index].to_vec();
            }

            CompactionStrategy::OffsetBasedRetention { keep_offsets } => {
                // Get high watermark (max offset in latest segment)
                if let Some(latest) = segments.last() {
                    let high_watermark = latest.max_offset;
                    let cutoff_offset = high_watermark - keep_offsets;

                    for segment in segments {
                        // Delete segments entirely below cutoff
                        if segment.max_offset < cutoff_offset {
                            to_delete.push(segment.clone());
                        }
                    }
                }
            }

            CompactionStrategy::Hybrid {
                max_age_secs,
                max_size_bytes,
                keep_offsets,
            } => {
                // Apply all configured constraints
                let mut candidates = Vec::new();

                // Time-based filter
                if let Some(max_age) = max_age_secs {
                    let cutoff = now - max_age;
                    for segment in segments {
                        if segment.created_at < cutoff {
                            candidates.push(segment.clone());
                        }
                    }
                }

                // Offset-based filter (add to candidates)
                if let Some(keep_off) = keep_offsets {
                    if let Some(latest) = segments.last() {
                        let high_watermark = latest.max_offset;
                        let cutoff_offset = high_watermark - keep_off;

                        for segment in segments {
                            if segment.max_offset < cutoff_offset
                                && !candidates.iter().any(|s| s.segment_id == segment.segment_id)
                            {
                                candidates.push(segment.clone());
                            }
                        }
                    }
                }

                // Size-based filter (further prune if needed)
                if let Some(max_size) = max_size_bytes {
                    // Sort candidates by min_offset (oldest first)
                    candidates.sort_by_key(|s| s.min_offset);

                    // Calculate current total size
                    let total_size: u64 = segments.iter().map(|s| s.size_bytes).sum();

                    if total_size > *max_size {
                        let mut size_to_free = total_size - max_size;
                        for segment in &candidates {
                            if size_to_free == 0 {
                                break;
                            }
                            to_delete.push(segment.clone());
                            size_to_free = size_to_free.saturating_sub(segment.size_bytes);
                        }
                    } else {
                        // Use time/offset filters only
                        to_delete = candidates;
                    }
                } else {
                    to_delete = candidates;
                }
            }
        }

        debug!(
            topic = %tp.topic,
            partition = tp.partition,
            identified = to_delete.len(),
            "Identified segments for deletion"
        );

        Ok(to_delete)
    }

    /// Delete a single segment
    #[instrument(skip(self))]
    async fn delete_segment(&self, segment: &SegmentMetadata) -> Result<()> {
        if self.config.dry_run {
            info!(
                segment_id = %segment.segment_id,
                path = %segment.object_store_path,
                size_bytes = segment.size_bytes,
                "DRY RUN: Would delete segment"
            );
            return Ok(());
        }

        // Delete from object store
        if self.config.delete_from_object_store {
            match self.object_store.delete(&segment.object_store_path).await {
                Ok(_) => {
                    info!(
                        segment_id = %segment.segment_id,
                        path = %segment.object_store_path,
                        "Deleted segment from object store"
                    );
                }
                Err(e) => {
                    return Err(Error::Internal(format!(
                        "Failed to delete segment {} from object store: {}",
                        segment.segment_id, e
                    )));
                }
            }
        }

        // Remove from index
        if self.config.remove_from_index {
            self.segment_index
                .remove_segment(&segment.topic, segment.partition, &segment.segment_id)
                .await?;
        }

        Ok(())
    }

    /// Get compaction statistics without actually deleting anything
    pub async fn analyze(&self) -> Result<CompactionStats> {
        let mut config = self.config.clone();
        config.dry_run = true;

        let compactor = SegmentCompactor::new(config, self.segment_index.clone(), self.object_store.clone());
        compactor.compact_all().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_store::{ObjectStoreConfig, ObjectStoreFactory};

    #[tokio::test]
    async fn test_time_based_retention() {
        let index = Arc::new(SegmentIndex::new());
        let store = Arc::new(ObjectStoreFactory::create(ObjectStoreConfig::Local {
            root: "/tmp/test_compaction".into(),
        }).unwrap());

        let config = CompactionConfig {
            strategy: CompactionStrategy::TimeBasedRetention {
                max_age_secs: 3600, // 1 hour
            },
            dry_run: true,
            ..Default::default()
        };

        let compactor = SegmentCompactor::new(config, index.clone(), store);

        // Add test segments with different ages
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let old_segment = SegmentMetadata {
            segment_id: "old-segment".to_string(),
            topic: "test-topic".to_string(),
            partition: 0,
            min_offset: 0,
            max_offset: 1000,
            record_count: 1000,
            min_timestamp: now - 7200,
            max_timestamp: now - 7200,
            object_store_path: "test/old-segment.tar.gz".to_string(),
            size_bytes: 1024,
            created_at: now - 7200, // 2 hours ago (older than retention)
            compression: "gzip".to_string(),
        };

        let new_segment = SegmentMetadata {
            segment_id: "new-segment".to_string(),
            topic: "test-topic".to_string(),
            partition: 0,
            min_offset: 1001,
            max_offset: 2000,
            record_count: 1000,
            min_timestamp: now - 1800,
            max_timestamp: now - 1800,
            object_store_path: "test/new-segment.tar.gz".to_string(),
            size_bytes: 1024,
            created_at: now - 1800, // 30 minutes ago (within retention)
            compression: "gzip".to_string(),
        };

        index.add_segment(old_segment).await.unwrap();
        index.add_segment(new_segment).await.unwrap();

        // Run compaction analysis
        let stats = compactor.analyze().await.unwrap();

        // Should identify 1 old segment for deletion
        assert_eq!(stats.segments_identified, 1);
        assert!(stats.dry_run);
    }
}
