//! FindCoordinator API types

use serde::{Deserialize, Serialize};

/// FindCoordinator request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FindCoordinatorRequest {
    /// Coordinator key (group id for consumer groups)
    pub key: String,
    /// Coordinator type (0 = group, 1 = transaction)
    pub key_type: i8,
}

/// Single coordinator entry for v4+ batched response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Coordinator {
    /// Coordinator key
    pub key: String,
    /// Coordinator node ID
    pub node_id: i32,
    /// Coordinator host
    pub host: String,
    /// Coordinator port
    pub port: i32,
    /// Error code
    pub error_code: i16,
    /// Error message
    pub error_message: Option<String>,
}

/// FindCoordinator response (supports both v0-v3 and v4+ formats)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FindCoordinatorResponse {
    /// Throttle time in milliseconds
    pub throttle_time_ms: i32,

    // v0-v3 fields (for backward compatibility)
    /// Error code (v0-v3 only)
    pub error_code: i16,
    /// Error message (v0-v3 only)
    pub error_message: Option<String>,
    /// Coordinator node ID (v0-v3 only)
    pub node_id: i32,
    /// Coordinator host (v0-v3 only)
    pub host: String,
    /// Coordinator port (v0-v3 only)
    pub port: i32,

    // v4+ fields
    /// Coordinators array (v4+ only, for batched requests)
    pub coordinators: Vec<Coordinator>,
}

/// Coordinator types
pub mod coordinator_type {
    pub const GROUP: i8 = 0;
    pub const TRANSACTION: i8 = 1;
}

/// Error codes for FindCoordinator
pub mod error_codes {
    pub const NONE: i16 = 0;
    pub const COORDINATOR_NOT_AVAILABLE: i16 = 15;
    pub const GROUP_AUTHORIZATION_FAILED: i16 = 30;
    pub const INVALID_REQUEST: i16 = 42;
}