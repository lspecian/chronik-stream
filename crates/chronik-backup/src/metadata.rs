//! Backup metadata structures

use crate::CompressionType;
use chrono::{DateTime, Utc, Duration};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Backup metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupMetadata {
    /// Unique backup identifier
    pub backup_id: String,
    /// Timestamp when backup was created
    pub timestamp: DateTime<Utc>,
    /// Type of backup
    pub backup_type: BackupType,
    /// Chronik version that created this backup
    pub version: String,
    /// Compression type used
    pub compression: CompressionType,
    /// Whether encryption is enabled
    pub encryption: bool,
    /// List of backed up topics
    pub topics: Vec<TopicBackupInfo>,
    /// Total size of backup in bytes
    pub total_size: u64,
    /// Total number of segments
    pub total_segments: u64,
    /// Checksum of the backup
    pub checksum: String,
    /// Time taken to create backup
    pub duration: Duration,
    /// Parent backup ID for incremental backups
    pub parent_backup_id: Option<String>,
}

/// Type of backup
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BackupType {
    /// Full backup containing all data
    Full,
    /// Incremental backup containing only changes
    Incremental,
    /// Differential backup containing changes since last full backup
    Differential,
}

/// Topic backup information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopicBackupInfo {
    /// Topic name
    pub name: String,
    /// Number of partitions
    pub partition_count: u32,
    /// Replication factor
    pub replication_factor: u16,
    /// Number of segments backed up
    pub segment_count: u64,
    /// Total size in bytes
    pub total_size: u64,
    /// Segment references
    pub segments: Vec<SegmentBackupRef>,
}

/// Reference to a backed up segment
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SegmentBackupRef {
    /// Topic name
    pub topic: String,
    /// Partition ID
    pub partition: u32,
    /// Segment ID
    pub segment_id: String,
    /// Start offset
    pub start_offset: i64,
    /// End offset
    pub end_offset: i64,
    /// Segment size in bytes
    pub size: u64,
    /// Path in backup storage
    pub backup_path: String,
    /// Checksum of segment data
    pub checksum: String,
    /// Timestamp when segment was created
    pub timestamp: DateTime<Utc>,
}

/// Metadata backup structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetadataBackup {
    /// Version of the backup format
    pub version: String,
    /// Timestamp of the backup
    pub timestamp: DateTime<Utc>,
    /// Topic configurations
    pub topics: Vec<TopicConfig>,
    /// Consumer group states
    pub consumer_groups: Vec<ConsumerGroupState>,
    /// ACL entries
    pub acls: Vec<AclEntry>,
    /// Configuration overrides
    pub configs: HashMap<String, String>,
}

/// Topic configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopicConfig {
    /// Topic name
    pub name: String,
    /// Number of partitions
    pub partitions: u32,
    /// Replication factor
    pub replication_factor: u16,
    /// Topic configuration
    pub config: HashMap<String, String>,
    /// Creation timestamp
    pub created_at: DateTime<Utc>,
}

/// Consumer group state
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsumerGroupState {
    /// Group ID
    pub group_id: String,
    /// Group state
    pub state: String,
    /// Protocol type
    pub protocol_type: String,
    /// Protocol
    pub protocol: Option<String>,
    /// Members
    pub members: Vec<ConsumerGroupMember>,
    /// Committed offsets
    pub offsets: HashMap<TopicPartition, CommittedOffset>,
}

/// Consumer group member
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsumerGroupMember {
    /// Member ID
    pub member_id: String,
    /// Client ID
    pub client_id: String,
    /// Client host
    pub client_host: String,
    /// Session timeout
    pub session_timeout_ms: i32,
    /// Rebalance timeout
    pub rebalance_timeout_ms: i32,
    /// Assigned partitions
    pub assignment: Vec<TopicPartition>,
}

/// Topic partition identifier
#[derive(Debug, Clone, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopicPartition {
    /// Topic name
    pub topic: String,
    /// Partition ID
    pub partition: i32,
}

/// Committed offset information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommittedOffset {
    /// Offset value
    pub offset: i64,
    /// Metadata
    pub metadata: Option<String>,
    /// Commit timestamp
    pub timestamp: DateTime<Utc>,
}

/// ACL entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AclEntry {
    /// Principal
    pub principal: String,
    /// Host
    pub host: String,
    /// Operation
    pub operation: String,
    /// Permission type
    pub permission_type: String,
    /// Resource type
    pub resource_type: String,
    /// Resource name
    pub resource_name: String,
    /// Resource pattern type
    pub resource_pattern_type: String,
}

/// Backup statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupStats {
    /// Number of topics
    pub topic_count: u32,
    /// Number of partitions
    pub partition_count: u32,
    /// Total messages
    pub message_count: u64,
    /// Total size in bytes
    pub total_size: u64,
    /// Compression ratio
    pub compression_ratio: f64,
    /// Backup duration
    pub duration: Duration,
    /// Throughput in MB/s
    pub throughput_mb_per_sec: f64,
}

impl BackupStats {
    /// Calculate stats from backup metadata
    pub fn from_metadata(metadata: &BackupMetadata) -> Self {
        let topic_count = metadata.topics.len() as u32;
        let partition_count: u32 = metadata.topics.iter()
            .map(|t| t.partition_count)
            .sum();
        
        let duration_secs = metadata.duration.num_seconds() as f64;
        let throughput_mb_per_sec = if duration_secs > 0.0 {
            (metadata.total_size as f64 / 1_048_576.0) / duration_secs
        } else {
            0.0
        };
        
        Self {
            topic_count,
            partition_count,
            message_count: 0, // Would need to count from segments
            total_size: metadata.total_size,
            compression_ratio: 1.0, // Would need uncompressed size
            duration: metadata.duration,
            throughput_mb_per_sec,
        }
    }
}

/// Backup summary for quick listing
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupSummary {
    /// Backup ID
    pub backup_id: String,
    /// Backup type
    pub backup_type: BackupType,
    /// Timestamp
    pub timestamp: DateTime<Utc>,
    /// Total size
    pub total_size: u64,
    /// Topic count
    pub topic_count: u32,
    /// Parent backup ID for incremental
    pub parent_backup_id: Option<String>,
}