//! Common types used throughout Chronik Stream.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Topic and partition identifier.
#[derive(Debug, Clone, Hash, Eq, PartialEq, Serialize, Deserialize)]
pub struct TopicPartition {
    pub topic: String,
    pub partition: i32,
}

/// Offset within a partition.
pub type Offset = i64;

/// Timestamp in milliseconds since epoch.
pub type Timestamp = i64;

/// Node identifier in the cluster.
pub type NodeId = u32;

/// Broker identifier.
pub type BrokerId = i32;

/// Segment identifier.
#[derive(Debug, Clone, Copy, Hash, Eq, PartialEq, Serialize, Deserialize)]
pub struct SegmentId(pub Uuid);

impl SegmentId {
    /// Create a new random segment ID.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for SegmentId {
    fn default() -> Self {
        Self::new()
    }
}

/// Segment metadata stored in the metastore.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SegmentMetadata {
    pub id: SegmentId,
    pub topic_partition: TopicPartition,
    pub base_offset: Offset,
    pub last_offset: Offset,
    pub timestamp_min: Timestamp,
    pub timestamp_max: Timestamp,
    pub size_bytes: u64,
    pub record_count: u64,
    pub object_key: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Topic configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopicConfig {
    pub name: String,
    pub partition_count: i32,
    pub replication_factor: i32,
    pub retention_ms: Option<i64>,
    pub retention_bytes: Option<i64>,
    pub segment_bytes: i64,
    pub segment_ms: i64,
    pub cleanup_policy: CleanupPolicy,
}

/// Cleanup policy for topics.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CleanupPolicy {
    Delete,
    Compact,
    CompactAndDelete,
}

impl Default for CleanupPolicy {
    fn default() -> Self {
        Self::Delete
    }
}