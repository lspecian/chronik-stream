//! Heartbeat API types

use serde::{Deserialize, Serialize};

/// Heartbeat request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatRequest {
    /// The group ID
    pub group_id: String,
    /// The generation ID
    pub generation_id: i32,
    /// The member ID
    pub member_id: String,
    /// Group instance ID (v3+)
    pub group_instance_id: Option<String>,
}

/// Heartbeat response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatResponse {
    /// Throttle time in milliseconds (v1+)
    pub throttle_time_ms: i32,
    /// Error code
    pub error_code: i16,
}

/// Error codes for Heartbeat
pub mod error_codes {
    pub const NONE: i16 = 0;
    pub const COORDINATOR_NOT_AVAILABLE: i16 = 15;
    pub const UNKNOWN_MEMBER_ID: i16 = 25;
    pub const ILLEGAL_GENERATION: i16 = 22;
    pub const REBALANCE_IN_PROGRESS: i16 = 27;
    pub const GROUP_AUTHORIZATION_FAILED: i16 = 30;
}