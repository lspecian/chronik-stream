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
pub mod frame;
pub mod handler;
pub mod kafka_protocol;
pub mod parser;
pub mod records;
pub mod types;

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