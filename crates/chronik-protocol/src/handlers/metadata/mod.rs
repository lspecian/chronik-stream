//! Metadata handler module
//!
//! Extracted from `handle_metadata()` to reduce complexity from 244 to <25 per function.
//! Handles Kafka Metadata API (versions 0-13) with version-specific parsing and encoding.
//!
//! **Module Structure:**
//! - `parser` - Request parsing with flexible/compact protocol support
//! - `broker` - Broker retrieval, validation, and filtering
//! - `response` - Response building and version-specific encoding
//!
//! **Supported Versions:**
//! - v0: Basic metadata
//! - v1-v3: Added controller_id, rack, throttle_time
//! - v4: Added allow_auto_topic_creation
//! - v5: Added offline_replicas
//! - v7: Added leader_epoch
//! - v8-v10: Added authorized_operations
//! - v9+: Flexible/compact protocol
//! - v10+: Added topic_id (UUID)
//! - v11+: Removed cluster_authorized_operations
//!
//! **Refactoring Goal:**
//! Extract 287-line monolithic `handle_metadata()` into focused, testable modules.

pub mod parser;
pub mod broker;
pub mod response;

// Re-export key types for convenience
pub use parser::MetadataRequestParser;
pub use broker::{BrokerRetriever, BrokerValidator};
pub use response::MetadataResponseBuilder;
