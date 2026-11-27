//! Comprehensive error handling for the integrated Kafka server.
//!
//! This module provides robust error handling, recovery, and logging
//! to ensure operational excellence and reliability.

use std::io;
use tracing::{error, warn, info, debug};
use thiserror::Error;

/// Stable marker identifying a pre-authentication rejection as it travels
/// through `chronik_common::Error` (which erases the type) back to
/// [`ErrorHandler::from_anyhow`]. Changing this string decouples the gate from
/// its wire error code, so keep it in sync with `connection.rs`.
pub const AUTH_REQUIRED_MARKER: &str = "SASL authentication required";

/// Marker for an ACL denial on a consumer group, carried the same way.
///
/// Only usable for APIs whose error response can actually express an error code
/// (JoinGroup, SyncGroup, Heartbeat, LeaveGroup and the generic fallback).
/// Produce, Fetch, Metadata and CreateTopics drop the code in
/// [`ErrorHandler::build_error_response`] and emit an empty *success* body, so
/// those handlers build their own denial from the parsed request instead.
pub const GROUP_AUTH_DENIED_MARKER: &str = "group authorization failed";

/// Comprehensive error type for the integrated server
#[derive(Error, Debug)]
pub enum ServerError {
    #[error("IO error: {0}")]
    Io(#[from] io::Error),
    
    #[error("Topic not found: {topic}")]
    TopicNotFound { topic: String },
    
    #[error("Controller not available")]
    ControllerNotAvailable,
    
    /// A request arrived on a connection that has not completed SASL
    /// authentication, on a server where authentication is required.
    #[error("{}: {0}", AUTH_REQUIRED_MARKER)]
    AuthenticationRequired(String),

    /// An ACL denied the principal access to a consumer group.
    #[error("{}: {0}", GROUP_AUTH_DENIED_MARKER)]
    GroupAuthorizationFailed(String),

    #[error("Internal server error: {0}")]
    Internal(String),
}

/// Kafka protocol error codes
#[derive(Debug, Clone, Copy)]
#[repr(i16)]
pub enum ErrorCode {
    UnknownTopicOrPartition = 3,
    CoordinatorNotAvailable = 15,
    GroupAuthorizationFailed = 30,
    IllegalSaslState = 34,
    KafkaStorageError = 56,
}

impl ErrorCode {
    /// Convert to i16 for wire protocol
    pub fn to_i16(self) -> i16 {
        self as i16
    }
    
}

/// Error handler with recovery strategies
#[derive(Default)]
pub struct ErrorHandler;

impl ErrorHandler {
    /// Create a new error handler
    pub fn new() -> Self {
        Self
    }
    
    /// Handle a server error with appropriate logging and recovery
    pub async fn handle_error(&self, error: ServerError, context: &str) -> ErrorRecovery {
        // Log the error with appropriate level
        match &error {
            ServerError::Io(e) if e.kind() == io::ErrorKind::WouldBlock => {
                debug!("Non-blocking IO would block in {}: {}", context, e);
                ErrorRecovery::Retry
            }
            ServerError::Io(e) if e.kind() == io::ErrorKind::BrokenPipe => {
                info!("Client disconnected in {}: {}", context, e);
                ErrorRecovery::CloseConnection
            }
            ServerError::TopicNotFound { topic } => {
                warn!("Topic not found in {}: {}", context, topic);
                ErrorRecovery::ReturnError(ErrorCode::UnknownTopicOrPartition)
            }
            ServerError::ControllerNotAvailable => {
                warn!("Controller not available in {}", context);
                ErrorRecovery::ReturnError(ErrorCode::CoordinatorNotAvailable)
            }
            ServerError::AuthenticationRequired(msg) => {
                warn!("Rejected unauthenticated request in {}: {}", context, msg);
                ErrorRecovery::ReturnError(ErrorCode::IllegalSaslState)
            }
            ServerError::GroupAuthorizationFailed(msg) => {
                warn!("Rejected unauthorized group access in {}: {}", context, msg);
                ErrorRecovery::ReturnError(ErrorCode::GroupAuthorizationFailed)
            }
            ServerError::Internal(msg) => {
                error!("Internal server error in {}: {}", context, msg);
                ErrorRecovery::ReturnError(ErrorCode::KafkaStorageError)
            }
            _ => {
                error!("Unhandled error in {}: {}", context, error);
                ErrorRecovery::ReturnError(ErrorCode::KafkaStorageError)
            }
        }
    }
    
    /// Convert a generic error to ServerError
    pub fn from_anyhow(error: anyhow::Error) -> ServerError {
        // Try to downcast to known error types
        if let Some(io_error) = error.downcast_ref::<io::Error>() {
            return ServerError::Io(io::Error::new(io_error.kind(), error.to_string()));
        }

        // Check for chronik-specific errors
        let error_string = error.to_string();

        // The pre-authentication gate reports through chronik_common::Error, which
        // reaches here only as a string. Match the marker first so an
        // unauthenticated request yields ILLEGAL_SASL_STATE rather than being
        // swallowed by the generic Internal fallback below.
        if error_string.contains(AUTH_REQUIRED_MARKER) {
            return ServerError::AuthenticationRequired(error_string);
        }
        if error_string.contains(GROUP_AUTH_DENIED_MARKER) {
            return ServerError::GroupAuthorizationFailed(error_string);
        }

        if (error_string.contains("topic") || error_string.contains("Topic"))
            && (error_string.contains("not found") || error_string.contains("does not exist")) {
                return ServerError::TopicNotFound { 
                    topic: extract_topic_name(&error_string).unwrap_or_default() 
                };
            }
        
        if error_string.contains("controller") || error_string.contains("Controller") {
            return ServerError::ControllerNotAvailable;
        }
        
        // Default to internal error
        ServerError::Internal(error.to_string())
    }
    
    /// Build an error response for Kafka protocol
    pub fn build_error_response(
        &self,
        error_code: ErrorCode,
        correlation_id: i32,
        api_key: i16,
        api_version: i16,
    ) -> Vec<u8> {
        let mut response = Vec::new();
        
        // Size placeholder (4 bytes)
        response.extend_from_slice(&[0, 0, 0, 0]);
        
        // Correlation ID (4 bytes)
        response.extend_from_slice(&correlation_id.to_be_bytes());
        
        // Error handling based on API
        match api_key {
            // Metadata (API key 3)
            3 => {
                // Metadata v9+ format (v12 is flexible)
                // throttle_time_ms (4 bytes) - v9+
                if api_version >= 9 {
                    response.extend_from_slice(&0i32.to_be_bytes());
                }

                // brokers array (4 bytes for empty array)
                response.extend_from_slice(&0i32.to_be_bytes());

                // cluster_id (nullable string - null = 0xffff)
                if api_version >= 2 {
                    response.extend_from_slice(&(-1i16).to_be_bytes()); // null cluster_id
                }

                // controller_id (4 bytes)
                if api_version >= 1 {
                    response.extend_from_slice(&(-1i32).to_be_bytes()); // -1 = no controller
                }

                // topics array (4 bytes for empty array)
                response.extend_from_slice(&0i32.to_be_bytes());

                // cluster_authorized_operations (4 bytes) - v8+
                if api_version >= 8 {
                    response.extend_from_slice(&(-2147483648i32).to_be_bytes()); // Unknown
                }

                // tagged_fields (1 byte = 0x00 for empty) - v9+ flexible
                if api_version >= 9 {
                    response.push(0x00);
                }
            }
            // CreateTopics
            19 => {
                if api_version >= 1 {
                    // Throttle time (4 bytes)
                    response.extend_from_slice(&0i32.to_be_bytes());
                }
                // Topics array count (4 bytes)
                response.extend_from_slice(&0i32.to_be_bytes());
            }
            // Produce
            0 => {
                if api_version >= 1 {
                    // Throttle time (4 bytes)
                    response.extend_from_slice(&0i32.to_be_bytes());
                }
                // Topics array count (4 bytes)
                response.extend_from_slice(&0i32.to_be_bytes());
            }
            // Fetch
            1 => {
                if api_version >= 1 {
                    // Throttle time (4 bytes)
                    response.extend_from_slice(&0i32.to_be_bytes());
                }
                // Topics array count (4 bytes)
                response.extend_from_slice(&0i32.to_be_bytes());
            }
            // OffsetCommit
            8 => {
                if api_version >= 3 {
                    // Throttle time (4 bytes)
                    response.extend_from_slice(&0i32.to_be_bytes());
                }
                // Topics array count (4 bytes) - empty array
                response.extend_from_slice(&0i32.to_be_bytes());
            }
            // JoinGroup (API key 11)
            11 => {
                // ThrottleTimeMs (v2+, required for v5+)
                if api_version >= 2 {
                    response.extend_from_slice(&0i32.to_be_bytes());
                }
                // ErrorCode (v0+)
                response.extend_from_slice(&error_code.to_i16().to_be_bytes());
                // GenerationId (v0+)
                response.extend_from_slice(&(-1i32).to_be_bytes());
                // ProtocolType (v7+, nullable string)
                if api_version >= 7 {
                    response.extend_from_slice(&(-1i16).to_be_bytes()); // null
                }
                // ProtocolName (v0+, nullable string)
                response.extend_from_slice(&(-1i16).to_be_bytes()); // null
                // Leader (v0+, non-nullable string)
                response.extend_from_slice(&0i16.to_be_bytes()); // empty string
                // SkipAssignment (v9+)
                if api_version >= 9 {
                    response.push(0x00); // false
                }
                // MemberId (v0+, non-nullable string)
                response.extend_from_slice(&0i16.to_be_bytes()); // empty string
                // Members array (v0+)
                response.extend_from_slice(&0i32.to_be_bytes()); // empty array
                // Tagged fields (v9+ flexible)
                if api_version >= 9 {
                    response.push(0x00); // no tagged fields
                }
            }
            // SyncGroup (API key 14)
            14 => {
                // SyncGroup response format:
                // v1+: throttle_time_ms (i32)
                // v0+: error_code (i16)
                // v5+: protocol_type (string), protocol_name (string)
                // v0+: assignment (bytes)
                // v4+: tagged_fields
                if api_version >= 1 {
                    response.extend_from_slice(&0i32.to_be_bytes()); // throttle_time_ms
                }
                response.extend_from_slice(&error_code.to_i16().to_be_bytes());
                if api_version >= 5 {
                    if api_version >= 4 {
                        // compact empty strings (length 0 encoded as varint 1)
                        response.push(1); // protocol_type: empty
                        response.push(1); // protocol_name: empty
                    } else {
                        response.extend_from_slice(&0i16.to_be_bytes()); // protocol_type: empty
                        response.extend_from_slice(&0i16.to_be_bytes()); // protocol_name: empty
                    }
                }
                if api_version >= 4 {
                    response.push(1); // compact bytes: empty assignment (length 0 encoded as varint 1)
                } else {
                    response.extend_from_slice(&(-1i32).to_be_bytes()); // standard bytes: null assignment
                }
                if api_version >= 4 {
                    response.push(0x00); // tagged fields: none
                }
            }
            // Heartbeat (API key 12)
            12 => {
                if api_version >= 1 {
                    response.extend_from_slice(&0i32.to_be_bytes()); // throttle_time_ms
                }
                response.extend_from_slice(&error_code.to_i16().to_be_bytes());
                if api_version >= 4 {
                    response.push(0x00); // tagged fields
                }
            }
            // LeaveGroup (API key 13)
            13 => {
                if api_version >= 1 {
                    response.extend_from_slice(&0i32.to_be_bytes()); // throttle_time_ms
                }
                response.extend_from_slice(&error_code.to_i16().to_be_bytes());
                if api_version >= 3 {
                    // members array (empty for error)
                    response.push(1); // compact array: 0 elements (encoded as varint 1)
                }
                if api_version >= 4 {
                    response.push(0x00); // tagged fields
                }
            }
            _ => {
                // Generic error response
                response.extend_from_slice(&error_code.to_i16().to_be_bytes());
            }
        }
        
        // Update size
        let size = (response.len() - 4) as i32;
        response[0..4].copy_from_slice(&size.to_be_bytes());
        
        response
    }
}

/// Error recovery strategy
#[derive(Debug, Clone)]
pub enum ErrorRecovery {
    /// Retry the operation
    Retry,
    /// Return error response to client
    ReturnError(ErrorCode),
    /// Close the connection
    CloseConnection,
}

/// Extract topic name from error message
fn extract_topic_name(error_msg: &str) -> Option<String> {
    // Simple extraction - look for quoted strings or specific patterns
    if let Some(start) = error_msg.find('"') {
        if let Some(end) = error_msg[start + 1..].find('"') {
            return Some(error_msg[start + 1..start + 1 + end].to_string());
        }
    }
    
    if let Some(pos) = error_msg.find("topic ") {
        let topic_part = &error_msg[pos + 6..];
        if let Some(end) = topic_part.find(' ') {
            return Some(topic_part[..end].to_string());
        }
    }
    
    None
}

/// Middleware for connection error handling
pub async fn handle_connection_error(
    error: io::Error,
    addr: std::net::SocketAddr,
) {
    match error.kind() {
        io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe => {
            debug!("Client {} disconnected", addr);
        }
        io::ErrorKind::TimedOut => {
            warn!("Connection timeout for client {}", addr);
        }
        _ => {
            error!("Connection error for client {}: {}", addr, error);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_error_code_conversion() {
        assert_eq!(ErrorCode::UnknownTopicOrPartition.to_i16(), 3);
    }
    
    #[test]
    fn test_extract_topic_name() {
        assert_eq!(
            extract_topic_name("Topic \"test-topic\" not found"),
            Some("test-topic".to_string())
        );
        assert_eq!(
            extract_topic_name("Unknown topic my-topic in request"),
            Some("my-topic".to_string())
        );
    }
    
    #[tokio::test]
    async fn test_error_handler() {
        let handler = ErrorHandler::new();
        
        let error = ServerError::TopicNotFound { 
            topic: "test".to_string() 
        };
        
        let recovery = handler.handle_error(error, "test context").await;
        
        match recovery {
            ErrorRecovery::ReturnError(code) => {
                assert_eq!(code.to_i16(), ErrorCode::UnknownTopicOrPartition.to_i16());
            }
            _ => panic!("Expected ReturnError recovery"),
        }
    }
}