//! CreateTopics API Handler Modules
//!
//! Refactored from monolithic `handle_create_topics()` (365 lines, ~240 complexity)
//! into focused modules with <25 complexity per function.
//!
//! ## Module Structure
//!
//! - **request_parser**: Parse CreateTopics request (topics array, timeout, validate_only)
//! - **topic_parser**: Parse individual topic from request body
//! - **topic_validator**: Validate topic parameters (name, partitions, replication, configs)
//! - **topic_creator**: Create topic in metadata store and build response
//! - **response_encoder**: Encode CreateTopics response (v0-v7+)
//!
//! ## Supported Versions
//!
//! - **v0**: Basic CreateTopics
//! - **v1**: Add validate_only and error_message
//! - **v2**: Add throttle_time_ms
//! - **v5**: Flexible/compact encoding, detailed topic info (num_partitions, replication_factor, configs)
//! - **v7**: Add topic UUID field
//!
//! ## Refactoring Goals (Phase 2.4 Part 2)
//!
//! - Reduce complexity from ~240 → <25 per function
//! - Extract 5 focused modules with single responsibility
//! - Add comprehensive unit tests
//! - Maintain Kafka protocol compatibility

pub mod request_parser;
pub mod topic_parser;
pub mod topic_validator;
pub mod topic_creator;
pub mod response_encoder;

// Re-export key types for easy access
pub use request_parser::RequestParser;
pub use topic_parser::TopicParser;
pub use topic_validator::TopicValidator;
pub use topic_creator::TopicCreator;
pub use response_encoder::ResponseEncoder;
