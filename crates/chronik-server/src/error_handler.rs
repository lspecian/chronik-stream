//! Comprehensive error handling for the integrated Kafka server.
//!
//! This module provides robust error handling, recovery, and logging
//! to ensure operational excellence and reliability.

use std::fmt;
use std::io;
use tracing::{error, warn, info, debug};
use thiserror::Error;

/// Comprehensive error type for the integrated server
#[derive(Error, Debug)]
pub enum ServerError {
    #[error("IO error: {0}")]
    Io(#[from] io::Error),
    
    #[error("Protocol error: {message}")]
    Protocol { message: String, correlation_id: Option<i32> },
    
    #[error("Storage error: {0}")]
    Storage(String),
    
    #[error("Metadata error: {0}")]
    Metadata(String),
    
    #[error("Topic not found: {topic}")]
    TopicNotFound { topic: String },
    
    #[error("Partition not found: topic={topic}, partition={partition}")]
    PartitionNotFound { topic: String, partition: i32 },
    
    #[error("Controller not available")]
    ControllerNotAvailable,
    
    #[error("Invalid request: {0}")]
    InvalidRequest(String),
    
    #[error("Authorization failed: {0}")]
    AuthorizationFailed(String),
    
    #[error("Rate limit exceeded")]
    RateLimitExceeded,
    
    #[error("Request timeout after {duration_ms}ms")]
    RequestTimeout { duration_ms: u64 },
    
    #[error("Internal server error: {0}")]
    Internal(String),
}

/// Kafka protocol error codes
#[derive(Debug, Clone, Copy)]
#[repr(i16)]
pub enum ErrorCode {
    None = 0,
    OffsetOutOfRange = 1,
    CorruptMessage = 2,
    UnknownTopicOrPartition = 3,
    InvalidFetchSize = 4,
    LeaderNotAvailable = 5,
    NotLeaderForPartition = 6,
    RequestTimedOut = 7,
    BrokerNotAvailable = 8,
    ReplicaNotAvailable = 9,
    MessageTooLarge = 10,
    StaleControllerEpoch = 11,
    OffsetMetadataTooLarge = 12,
    NetworkException = 13,
    CoordinatorLoadInProgress = 14,
    CoordinatorNotAvailable = 15,
    NotCoordinator = 16,
    InvalidTopicException = 17,
    RecordListTooLarge = 18,
    NotEnoughReplicas = 19,
    NotEnoughReplicasAfterAppend = 20,
    InvalidRequiredAcks = 21,
    IllegalGeneration = 22,
    InconsistentGroupProtocol = 23,
    InvalidGroupId = 24,
    UnknownMemberId = 25,
    InvalidSessionTimeout = 26,
    RebalanceInProgress = 27,
    InvalidCommitOffsetSize = 28,
    TopicAuthorizationFailed = 29,
    GroupAuthorizationFailed = 30,
    ClusterAuthorizationFailed = 31,
    InvalidTimestamp = 32,
    UnsupportedSaslMechanism = 33,
    IllegalSaslState = 34,
    UnsupportedVersion = 35,
    TopicAlreadyExists = 36,
    InvalidPartitions = 37,
    InvalidReplicationFactor = 38,
    InvalidReplicaAssignment = 39,
    InvalidConfig = 40,
    NotController = 41,
    InvalidRequest = 42,
    UnsupportedForMessageFormat = 43,
    PolicyViolation = 44,
    OutOfOrderSequenceNumber = 45,
    DuplicateSequenceNumber = 46,
    InvalidProducerEpoch = 47,
    InvalidTxnState = 48,
    InvalidProducerIdMapping = 49,
    InvalidTransactionTimeout = 50,
    ConcurrentTransactions = 51,
    TransactionCoordinatorFenced = 52,
    TransactionalIdAuthorizationFailed = 53,
    SecurityDisabled = 54,
    OperationNotAttempted = 55,
    KafkaStorageError = 56,
    LogDirNotFound = 57,
    SaslAuthenticationFailed = 58,
    UnknownProducerId = 59,
    ReassignmentInProgress = 60,
    DelegationTokenAuthDisabled = 61,
    DelegationTokenNotFound = 62,
    DelegationTokenOwnerMismatch = 63,
    DelegationTokenRequestNotAllowed = 64,
    DelegationTokenAuthorizationFailed = 65,
    DelegationTokenExpired = 66,
    InvalidPrincipalType = 67,
    NonEmptyGroup = 68,
    GroupIdNotFound = 69,
    FetchSessionIdNotFound = 70,
    InvalidFetchSessionEpoch = 71,
    ListenerNotFound = 72,
    TopicDeletionDisabled = 73,
    FencedLeaderEpoch = 74,
    UnknownLeaderEpoch = 75,
    UnsupportedCompressionType = 76,
    StaleBrokerEpoch = 77,
    OffsetNotAvailable = 78,
    MemberIdRequired = 79,
    PreferredLeaderNotAvailable = 80,
    GroupMaxSizeReached = 81,
    FencedInstanceId = 82,
    EligibleLeadersNotAvailable = 83,
    ElectionNotNeeded = 84,
    NoReassignmentInProgress = 85,
    GroupSubscribedToTopic = 86,
    InvalidRecord = 87,
    UnstableOffsetCommit = 88,
}

impl ErrorCode {
    /// Convert to i16 for wire protocol
    pub fn to_i16(self) -> i16 {
        self as i16
    }
    
    /// Get human-readable description
    pub fn description(&self) -> &'static str {
        match self {
            ErrorCode::None => "No error",
            ErrorCode::UnknownTopicOrPartition => "Topic or partition does not exist",
            ErrorCode::NotController => "This broker is not the controller",
            ErrorCode::TopicAlreadyExists => "Topic already exists",
            ErrorCode::InvalidPartitions => "Invalid number of partitions",
            ErrorCode::InvalidReplicationFactor => "Invalid replication factor",
            ErrorCode::CoordinatorNotAvailable => "Coordinator is not available",
            ErrorCode::UnsupportedVersion => "Unsupported API version",
            ErrorCode::InvalidRequest => "Invalid request format",
            _ => "Unknown error",
        }
    }
}

/// Error handler with recovery strategies
pub struct ErrorHandler {
    /// Maximum retry attempts
    max_retries: u32,
    /// Base retry delay in milliseconds
    retry_delay_ms: u64,
    /// Enable detailed error logging
    detailed_logging: bool,
}

impl Default for ErrorHandler {
    fn default() -> Self {
        Self {
            max_retries: 3,
            retry_delay_ms: 100,
            detailed_logging: true,
        }
    }
}

impl ErrorHandler {
    /// Create a new error handler
    pub fn new() -> Self {
        Self::default()
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
            ServerError::Protocol { message, correlation_id } => {
                error!("Protocol error in {} (correlation_id={:?}): {}", 
                       context, correlation_id, message);
                ErrorRecovery::ReturnError(ErrorCode::InvalidRequest)
            }
            ServerError::RequestTimeout { duration_ms } => {
                warn!("Request timeout after {}ms in {}", duration_ms, context);
                ErrorRecovery::ReturnError(ErrorCode::RequestTimedOut)
            }
            ServerError::RateLimitExceeded => {
                warn!("Rate limit exceeded in {}", context);
                ErrorRecovery::Throttle(1000) // Throttle for 1 second
            }
            ServerError::AuthorizationFailed(msg) => {
                warn!("Authorization failed in {}: {}", context, msg);
                ErrorRecovery::ReturnError(ErrorCode::TopicAuthorizationFailed)
            }
            ServerError::Internal(msg) => {
                error!("Internal server error in {}: {}", context, msg);
                ErrorRecovery::ReturnError(ErrorCode::KafkaStorageError)
            }
            _ => {
                error!("Unhandled error in {}: {}", context, error);
                ErrorRecovery::ReturnError(ErrorCode::UnknownServerError)
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
        if error_string.contains("topic") || error_string.contains("Topic") {
            if error_string.contains("not found") || error_string.contains("does not exist") {
                return ServerError::TopicNotFound { 
                    topic: extract_topic_name(&error_string).unwrap_or_default() 
                };
            }
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
    /// Throttle the client for specified milliseconds
    Throttle(u64),
    /// Continue processing
    Continue,
}

/// Additional error code for internal use
impl ErrorCode {
    const UnknownServerError: ErrorCode = ErrorCode::KafkaStorageError;
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
        assert_eq!(ErrorCode::None.to_i16(), 0);
        assert_eq!(ErrorCode::UnknownTopicOrPartition.to_i16(), 3);
        assert_eq!(ErrorCode::NotController.to_i16(), 41);
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