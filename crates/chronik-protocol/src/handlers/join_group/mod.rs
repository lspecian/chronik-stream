//! JoinGroup handler module
//!
//! Extracted from `encode_join_group_response()` to reduce complexity from 199 to <25 per function.
//! Handles Kafka JoinGroup API (versions 0-9+) with version-specific encoding.
//!
//! **Module Structure:**
//! - `response` - Response encoding with flexible/compact protocol support
//!
//! **Supported Versions:**
//! - v0-v1: Basic join group
//! - v2+: Added throttle_time_ms
//! - v5+: Added group_instance_id
//! - v7+: Added protocol_type
//! - v9+: Flexible/compact protocol, added skip_assignment
//!
//! **Refactoring Goal:**
//! Extract 143-line monolithic `encode_join_group_response()` into focused, testable modules.

pub mod response;

// Re-export key types for convenience
pub use response::JoinGroupResponseEncoder;
