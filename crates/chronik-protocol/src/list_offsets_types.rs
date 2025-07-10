//! ListOffsets API types

use serde::{Deserialize, Serialize};

/// ListOffsets request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListOffsetsRequest {
    /// Replica ID of the requester (-1 for consumers)
    pub replica_id: i32,
    /// Isolation level (0 = read uncommitted, 1 = read committed)
    pub isolation_level: i8,
    /// Topics to list offsets for
    pub topics: Vec<ListOffsetsRequestTopic>,
}

/// Topic in ListOffsets request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListOffsetsRequestTopic {
    /// Topic name
    pub name: String,
    /// Partitions to list offsets for
    pub partitions: Vec<ListOffsetsRequestPartition>,
}

/// Partition in ListOffsets request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListOffsetsRequestPartition {
    /// Partition index
    pub partition_index: i32,
    /// Current leader epoch (-1 if unknown)
    pub current_leader_epoch: i32,
    /// Timestamp to search for (-1 = latest, -2 = earliest)
    pub timestamp: i64,
}

/// ListOffsets response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListOffsetsResponse {
    /// Throttle time in milliseconds
    pub throttle_time_ms: i32,
    /// Topics with offset info
    pub topics: Vec<ListOffsetsResponseTopic>,
}

/// Topic in ListOffsets response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListOffsetsResponseTopic {
    /// Topic name
    pub name: String,
    /// Partitions with offset info
    pub partitions: Vec<ListOffsetsResponsePartition>,
}

/// Partition in ListOffsets response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListOffsetsResponsePartition {
    /// Partition index
    pub partition_index: i32,
    /// Error code
    pub error_code: i16,
    /// Timestamp of the found offset
    pub timestamp: i64,
    /// The offset found
    pub offset: i64,
    /// Leader epoch of the found offset
    pub leader_epoch: i32,
}

/// Special timestamp values
pub const LATEST_TIMESTAMP: i64 = -1;
pub const EARLIEST_TIMESTAMP: i64 = -2;

/// Error codes for ListOffsets
pub mod error_codes {
    pub const NONE: i16 = 0;
    pub const UNKNOWN_TOPIC_OR_PARTITION: i16 = 3;
    pub const NOT_LEADER_FOR_PARTITION: i16 = 6;
    pub const REPLICA_NOT_AVAILABLE: i16 = 9;
    pub const OFFSET_OUT_OF_RANGE: i16 = 1;
}