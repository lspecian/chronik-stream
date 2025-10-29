//! Batch metadata for Raft replication coordination
//!
//! This module defines the batch metadata structure that is used to coordinate
//! WAL batch commits with Raft proposals. When a WAL batch is committed locally,
//! the batch metadata is sent to Raft for replication across the cluster.

use serde::{Deserialize, Serialize};

/// Metadata about a committed WAL batch that needs Raft replication
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchMetadata {
    /// Topic name
    pub topic: String,

    /// Partition ID
    pub partition: i32,

    /// First offset in the batch
    pub base_offset: i64,

    /// Last offset in the batch (inclusive)
    pub last_offset: i64,

    /// Number of records in the batch
    pub record_count: usize,

    /// Segment ID for idempotency (prevents duplicate proposals)
    pub segment_id: u64,

    /// Timestamp when batch was created (for metrics)
    pub created_at_ms: u64,
}

impl BatchMetadata {
    /// Create a new batch metadata
    pub fn new(
        topic: String,
        partition: i32,
        base_offset: i64,
        last_offset: i64,
        record_count: usize,
        segment_id: u64,
    ) -> Self {
        Self {
            topic,
            partition,
            base_offset,
            last_offset,
            record_count,
            segment_id,
            created_at_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
        }
    }

    /// Get partition key (topic, partition)
    pub fn partition_key(&self) -> (String, i32) {
        (self.topic.clone(), self.partition)
    }

    /// Get offset range
    pub fn offset_range(&self) -> (i64, i64) {
        (self.base_offset, self.last_offset)
    }
}
