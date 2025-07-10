//! LeaveGroup API types

use serde::{Deserialize, Serialize};

/// LeaveGroup request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaveGroupRequest {
    /// The group ID to leave
    pub group_id: String,
    /// Members to remove (v3+: array, v0-2: single member extracted into array)
    pub members: Vec<MemberIdentity>,
}

/// Member identity for leave group
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemberIdentity {
    /// The member ID to remove
    pub member_id: String,
    /// The group instance ID (v3+)
    pub group_instance_id: Option<String>,
}

/// LeaveGroup response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaveGroupResponse {
    /// Throttle time in milliseconds
    pub throttle_time_ms: i32,
    /// Top-level error code
    pub error_code: i16,
    /// Per-member responses (v3+)
    pub members: Vec<MemberResponse>,
}

/// Per-member response for leave group
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemberResponse {
    /// The member ID
    pub member_id: String,
    /// The group instance ID
    pub group_instance_id: Option<String>,
    /// Error code for this member
    pub error_code: i16,
}

/// Error codes for LeaveGroup
pub mod error_codes {
    pub const NONE: i16 = 0;
    pub const COORDINATOR_NOT_AVAILABLE: i16 = 15;
    pub const NOT_COORDINATOR: i16 = 16;
    pub const INVALID_GROUP_ID: i16 = 24;
    pub const UNKNOWN_MEMBER_ID: i16 = 25;
    pub const GROUP_AUTHORIZATION_FAILED: i16 = 30;
    pub const GROUP_ID_NOT_FOUND: i16 = 69;
}