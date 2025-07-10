//! SyncGroup API types

use serde::{Deserialize, Serialize};
use bytes::Bytes;

/// SyncGroup request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncGroupRequest {
    /// The group ID
    pub group_id: String,
    /// The generation ID
    pub generation_id: i32,
    /// The member ID
    pub member_id: String,
    /// Group instance ID (v3+)
    pub group_instance_id: Option<String>,
    /// Protocol type (v5+)
    pub protocol_type: Option<String>,
    /// Protocol name (v5+)
    pub protocol_name: Option<String>,
    /// Group assignments
    pub assignments: Vec<SyncGroupRequestAssignment>,
}

/// Assignment in SyncGroup request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncGroupRequestAssignment {
    /// Member ID
    pub member_id: String,
    /// Assignment data
    pub assignment: Bytes,
}

/// SyncGroup response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncGroupResponse {
    /// Throttle time in milliseconds (v1+)
    pub throttle_time_ms: i32,
    /// Error code
    pub error_code: i16,
    /// Protocol type (v5+)
    pub protocol_type: Option<String>,
    /// Protocol name (v5+)
    pub protocol_name: Option<String>,
    /// Member assignment
    pub assignment: Bytes,
}

/// Error codes for SyncGroup
pub mod error_codes {
    pub const NONE: i16 = 0;
    pub const COORDINATOR_NOT_AVAILABLE: i16 = 15;
    pub const UNKNOWN_MEMBER_ID: i16 = 25;
    pub const ILLEGAL_GENERATION: i16 = 22;
    pub const REBALANCE_IN_PROGRESS: i16 = 27;
    pub const GROUP_AUTHORIZATION_FAILED: i16 = 30;
}