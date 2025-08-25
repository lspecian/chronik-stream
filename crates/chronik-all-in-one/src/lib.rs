//! Chronik Stream All-in-One Server Library
//!
//! This library provides the integrated Kafka-compatible server implementation
//! with full production features.

pub mod integrated_server;
pub mod error_handler;
pub mod storage;
pub mod kafka_server;

// Re-export main types for external use and tests
pub use integrated_server::{IntegratedKafkaServer, IntegratedServerConfig};
pub use error_handler::{ErrorHandler, ErrorCode, ServerError, ErrorRecovery};
pub use storage::EmbeddedStorage;