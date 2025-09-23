//! Kafka wire protocol implementation for Chronik Stream.
//!
//! This crate provides comprehensive Kafka protocol support including:
//! - Full wire protocol parsing for Kafka 2.x through 4.x
//! - Request/response framing with length prefixes
//! - Message compression (Gzip, Snappy, LZ4, Zstd)
//! - Record batch format v2 support
//! - API version negotiation
//! - All major Kafka API implementations

pub mod compression;
pub mod error_codes;
pub mod frame;
pub mod handler;
pub mod kafka_protocol;
pub mod metadata_fix;
pub mod parser;
pub mod records;
pub mod types;
pub mod sasl_types;
pub mod create_topics_types;
pub mod list_offsets_types;
pub mod find_coordinator_types;
pub mod join_group_types;
pub mod sync_group_types;
pub mod heartbeat_types;
pub mod list_groups_types;
pub mod leave_group_types;
pub mod consumer_group_types;
pub mod delete_topics_types;
pub mod delete_groups_types;
pub mod alter_configs_types;
pub mod describe_configs_types;
pub mod incremental_alter_configs_types;
pub mod create_partitions_types;
pub mod offset_delete_types;
pub mod describe_groups_types;
pub mod end_txn_types;
pub mod init_producer_id_types;
pub mod add_partitions_to_txn_types;
pub mod add_offsets_to_txn_types;
pub mod txn_offset_commit_types;
pub mod metadata_types;
pub mod fetch_types;
pub mod produce_types;
pub mod sasl;

// Temporarily disabled due to kafka-protocol crate compatibility issues
// pub mod handler_v2;
// pub mod protocol_v2;

// Re-export main types
pub use compression::{CompressionHandler, CompressionType, MessageBatch};
pub use frame::{Frame, KafkaFrameCodec};
pub use handler::ProtocolHandler;
pub use kafka_protocol::{
    ApiKey as KafkaApiKey, ApiVersion, ErrorCode, ProtocolCodec,
    RequestHeader as KafkaRequestHeader, ResponseHeader as KafkaResponseHeader,
    TopicMetadata, PartitionMetadata, get_supported_apis,
};
pub use parser::{
    ApiKey, Decoder, Encoder, RequestHeader, ResponseHeader, VersionRange,
    parse_request_header, supported_api_versions, write_response_header,
};
pub use records::{Record, RecordBatch, RecordHeader, RecordAttributes, TimestampType};
pub use types::*;
pub use sasl_types::{SaslHandshakeRequest, SaslHandshakeResponse, SaslAuthenticateRequest, SaslAuthenticateResponse};