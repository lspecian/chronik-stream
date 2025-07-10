//! JoinGroup API types

use serde::{Deserialize, Serialize};
use bytes::Bytes;

/// JoinGroup request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JoinGroupRequest {
    /// The group ID
    pub group_id: String,
    /// Session timeout in milliseconds
    pub session_timeout_ms: i32,
    /// Rebalance timeout in milliseconds (v1+)
    pub rebalance_timeout_ms: i32,
    /// Member ID assigned by coordinator
    pub member_id: String,
    /// Group instance ID (v5+)
    pub group_instance_id: Option<String>,
    /// Protocol type (e.g., "consumer")
    pub protocol_type: String,
    /// Supported protocols
    pub protocols: Vec<JoinGroupRequestProtocol>,
}

/// Protocol in JoinGroup request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JoinGroupRequestProtocol {
    /// Protocol name
    pub name: String,
    /// Protocol metadata
    pub metadata: Bytes,
}

/// JoinGroup response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JoinGroupResponse {
    /// Throttle time in milliseconds (v2+)
    pub throttle_time_ms: i32,
    /// Error code
    pub error_code: i16,
    /// Generation ID
    pub generation_id: i32,
    /// Protocol type (v7+)
    pub protocol_type: Option<String>,
    /// Selected protocol
    pub protocol_name: Option<String>,
    /// Leader ID
    pub leader: String,
    /// Member ID
    pub member_id: String,
    /// Members (only provided to leader)
    pub members: Vec<JoinGroupResponseMember>,
}

/// Member in JoinGroup response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JoinGroupResponseMember {
    /// Member ID
    pub member_id: String,
    /// Group instance ID (v5+)
    pub group_instance_id: Option<String>,
    /// Member metadata
    pub metadata: Bytes,
}

/// Error codes for JoinGroup
pub mod error_codes {
    pub const NONE: i16 = 0;
    pub const COORDINATOR_NOT_AVAILABLE: i16 = 15;
    pub const INVALID_GROUP_ID: i16 = 24;
    pub const UNKNOWN_MEMBER_ID: i16 = 25;
    pub const INVALID_SESSION_TIMEOUT: i16 = 26;
    pub const REBALANCE_IN_PROGRESS: i16 = 27;
    pub const INCONSISTENT_GROUP_PROTOCOL: i16 = 23;
    pub const GROUP_AUTHORIZATION_FAILED: i16 = 30;
    pub const MEMBER_ID_REQUIRED: i16 = 79;
}

/// Consumer protocol metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsumerProtocolMetadata {
    /// Version
    pub version: i16,
    /// Topics the consumer is interested in
    pub topics: Vec<String>,
    /// User data
    pub user_data: Option<Bytes>,
}

/// Consumer protocol assignment
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsumerProtocolAssignment {
    /// Version
    pub version: i16,
    /// Assigned topic partitions
    pub assignments: Vec<TopicAssignment>,
    /// User data
    pub user_data: Option<Bytes>,
}

/// Topic assignment
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopicAssignment {
    /// Topic name
    pub topic: String,
    /// Assigned partitions
    pub partitions: Vec<i32>,
}