//! CreateTopics API types

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// CreateTopics request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateTopicsRequest {
    /// Topics to create
    pub topics: Vec<CreateTopicRequest>,
    /// Timeout in milliseconds
    pub timeout_ms: i32,
    /// Whether to validate only (v1+)
    pub validate_only: bool,
}

/// Individual topic creation request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateTopicRequest {
    /// Topic name
    pub name: String,
    /// Number of partitions
    pub num_partitions: i32,
    /// Replication factor
    pub replication_factor: i16,
    /// Replica assignments per partition (optional)
    pub replica_assignments: Vec<ReplicaAssignment>,
    /// Topic configs
    pub configs: HashMap<String, String>,
}

/// Replica assignment for a partition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplicaAssignment {
    /// Partition index
    pub partition_index: i32,
    /// Broker IDs for replicas
    pub broker_ids: Vec<i32>,
}

/// CreateTopics response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateTopicsResponse {
    /// Throttle time in milliseconds
    pub throttle_time_ms: i32,
    /// Topic creation results
    pub topics: Vec<CreateTopicResponse>,
}

/// Individual topic creation response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateTopicResponse {
    /// Topic name
    pub name: String,
    /// Error code (0 = success)
    pub error_code: i16,
    /// Error message (optional)
    pub error_message: Option<String>,
    /// Number of partitions (v5+)
    pub num_partitions: i32,
    /// Replication factor (v5+)
    pub replication_factor: i16,
    /// Config entries (v5+)
    pub configs: Vec<CreateTopicConfigEntry>,
}

/// Config entry in response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateTopicConfigEntry {
    /// Config name
    pub name: String,
    /// Config value
    pub value: Option<String>,
    /// Read only
    pub read_only: bool,
    /// Config source
    pub config_source: i8,
    /// Is sensitive
    pub is_sensitive: bool,
}

/// Error codes for CreateTopics
pub mod error_codes {
    pub const NONE: i16 = 0;
    pub const REQUEST_TIMED_OUT: i16 = 7;
    pub const INVALID_TOPIC_EXCEPTION: i16 = 17;
    pub const TOPIC_ALREADY_EXISTS: i16 = 36;
    pub const INVALID_PARTITIONS: i16 = 37;
    pub const INVALID_REPLICATION_FACTOR: i16 = 38;
    pub const INVALID_REPLICA_ASSIGNMENT: i16 = 39;
    pub const INVALID_CONFIG: i16 = 40;
    pub const NOT_CONTROLLER: i16 = 41;
    pub const INVALID_REQUEST: i16 = 42;
}