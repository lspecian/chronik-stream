//! Fetch Response Encoding Module
//!
//! Extracted from `encode_fetch_response()` to reduce complexity from ~287 lines to <25 per function.
//! Handles Kafka Fetch API response encoding for versions 0-13+.
//!
//! ## Architecture
//!
//! - `version_fields.rs` - Version-specific header fields (throttle, error, session)
//! - `topic_encoder.rs` - Topic array encoding with flexible/compact support
//! - `partition_encoder.rs` - Partition encoding with version-specific metadata
//! - `records_encoder.rs` - Records and aborted transactions encoding
//! - `tagged_fields.rs` - Tagged fields for flexible versions (v12+)
//!
//! ## Version Support
//!
//! - v0: Basic fetch response
//! - v1+: Throttle time
//! - v4+: Last stable offset, aborted transactions
//! - v5+: Log start offset
//! - v7+: Error code and session ID
//! - v11+: Preferred read replica
//! - v12+: Flexible/compact encoding with tagged fields

pub mod version_fields;
pub mod topic_encoder;
pub mod partition_encoder;
pub mod records_encoder;
pub mod tagged_fields;

pub use version_fields::VersionFieldsEncoder;
pub use topic_encoder::TopicEncoder;
pub use partition_encoder::PartitionEncoder;
pub use records_encoder::RecordsEncoder;
pub use tagged_fields::TaggedFieldsEncoder;
