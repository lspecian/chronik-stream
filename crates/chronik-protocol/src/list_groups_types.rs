//! ListGroups API types

use serde::{Deserialize, Serialize};

/// ListGroups request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListGroupsRequest {
    // Empty for v0-3
}

/// ListGroups response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListGroupsResponse {
    /// Throttle time in milliseconds (v1+)
    pub throttle_time_ms: i32,
    /// Error code
    pub error_code: i16,
    /// List of groups
    pub groups: Vec<ListGroupsResponseGroup>,
}

/// Group info in ListGroups response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListGroupsResponseGroup {
    /// Group ID
    pub group_id: String,
    /// Protocol type
    pub protocol_type: String,
}

/// Error codes for ListGroups
pub mod error_codes {
    pub const NONE: i16 = 0;
    pub const COORDINATOR_NOT_AVAILABLE: i16 = 15;
    pub const GROUP_AUTHORIZATION_FAILED: i16 = 30;
}