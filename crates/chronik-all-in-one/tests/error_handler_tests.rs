//! Unit tests for the error handler module.

use chronik_all_in_one::error_handler::*;

#[test]
fn test_error_code_values() {
    // Verify error codes match Kafka protocol specification
    assert_eq!(ErrorCode::None.to_i16(), 0);
    assert_eq!(ErrorCode::OffsetOutOfRange.to_i16(), 1);
    assert_eq!(ErrorCode::CorruptMessage.to_i16(), 2);
    assert_eq!(ErrorCode::UnknownTopicOrPartition.to_i16(), 3);
    assert_eq!(ErrorCode::InvalidFetchSize.to_i16(), 4);
    assert_eq!(ErrorCode::LeaderNotAvailable.to_i16(), 5);
    assert_eq!(ErrorCode::NotLeaderForPartition.to_i16(), 6);
    assert_eq!(ErrorCode::RequestTimedOut.to_i16(), 7);
    assert_eq!(ErrorCode::BrokerNotAvailable.to_i16(), 8);
    assert_eq!(ErrorCode::ReplicaNotAvailable.to_i16(), 9);
    assert_eq!(ErrorCode::MessageTooLarge.to_i16(), 10);
    assert_eq!(ErrorCode::NotController.to_i16(), 41);
    assert_eq!(ErrorCode::InvalidRequest.to_i16(), 42);
    assert_eq!(ErrorCode::KafkaStorageError.to_i16(), 56);
}

#[test]
fn test_error_code_descriptions() {
    assert_eq!(ErrorCode::None.description(), "No error");
    assert_eq!(ErrorCode::UnknownTopicOrPartition.description(), "Topic or partition does not exist");
    assert_eq!(ErrorCode::NotController.description(), "This broker is not the controller");
    assert_eq!(ErrorCode::TopicAlreadyExists.description(), "Topic already exists");
    assert_eq!(ErrorCode::InvalidPartitions.description(), "Invalid number of partitions");
    assert_eq!(ErrorCode::InvalidReplicationFactor.description(), "Invalid replication factor");
    assert_eq!(ErrorCode::CoordinatorNotAvailable.description(), "Coordinator is not available");
    assert_eq!(ErrorCode::UnsupportedVersion.description(), "Unsupported API version");
    assert_eq!(ErrorCode::InvalidRequest.description(), "Invalid request format");
}

#[tokio::test]
async fn test_error_handler_recovery_strategies() {
    let handler = ErrorHandler::new();
    
    // Test TopicNotFound error
    let error = ServerError::TopicNotFound { 
        topic: "missing-topic".to_string() 
    };
    let recovery = handler.handle_error(error, "test context").await;
    match recovery {
        ErrorRecovery::ReturnError(code) => {
            assert_eq!(code.to_i16(), ErrorCode::UnknownTopicOrPartition.to_i16());
        }
        _ => panic!("Expected ReturnError recovery for TopicNotFound"),
    }
    
    // Test ControllerNotAvailable error
    let error = ServerError::ControllerNotAvailable;
    let recovery = handler.handle_error(error, "test context").await;
    match recovery {
        ErrorRecovery::ReturnError(code) => {
            assert_eq!(code.to_i16(), ErrorCode::CoordinatorNotAvailable.to_i16());
        }
        _ => panic!("Expected ReturnError recovery for ControllerNotAvailable"),
    }
    
    // Test RateLimitExceeded error
    let error = ServerError::RateLimitExceeded;
    let recovery = handler.handle_error(error, "test context").await;
    match recovery {
        ErrorRecovery::Throttle(ms) => {
            assert_eq!(ms, 1000);
        }
        _ => panic!("Expected Throttle recovery for RateLimitExceeded"),
    }
    
    // Test BrokenPipe IO error
    let io_error = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pipe broken");
    let error = ServerError::Io(io_error);
    let recovery = handler.handle_error(error, "test context").await;
    match recovery {
        ErrorRecovery::CloseConnection => {
            // Expected
        }
        _ => panic!("Expected CloseConnection recovery for BrokenPipe"),
    }
}

#[test]
fn test_error_response_building() {
    let handler = ErrorHandler::new();
    
    // Test Produce error response (API key 0)
    let response = handler.build_error_response(
        ErrorCode::UnknownTopicOrPartition,
        12345, // correlation_id
        0,     // api_key (Produce)
        3,     // api_version
    );
    
    // Verify response structure
    assert!(response.len() >= 12, "Response should have size, correlation_id, and throttle_time");
    
    // Check size field
    let size = i32::from_be_bytes([response[0], response[1], response[2], response[3]]);
    assert_eq!(size as usize, response.len() - 4, "Size field should match actual size");
    
    // Check correlation ID
    let correlation_id = i32::from_be_bytes([response[4], response[5], response[6], response[7]]);
    assert_eq!(correlation_id, 12345, "Correlation ID should match");
    
    // Check throttle time (for v1+)
    let throttle_time = i32::from_be_bytes([response[8], response[9], response[10], response[11]]);
    assert_eq!(throttle_time, 0, "Throttle time should be 0");
}

#[test]
fn test_from_anyhow_conversion() {
    // Test topic not found conversion
    let error = anyhow::anyhow!("Topic \"test-topic\" not found");
    let server_error = ErrorHandler::from_anyhow(error);
    match server_error {
        ServerError::TopicNotFound { topic } => {
            assert_eq!(topic, "test-topic");
        }
        _ => panic!("Expected TopicNotFound error"),
    }
    
    // Test controller error conversion
    let error = anyhow::anyhow!("Controller is not available");
    let server_error = ErrorHandler::from_anyhow(error);
    match server_error {
        ServerError::ControllerNotAvailable => {
            // Expected
        }
        _ => panic!("Expected ControllerNotAvailable error"),
    }
    
    // Test IO error conversion
    let io_error = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "access denied");
    let error = anyhow::Error::from(io_error);
    let server_error = ErrorHandler::from_anyhow(error);
    match server_error {
        ServerError::Io(e) => {
            assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied);
        }
        _ => panic!("Expected Io error"),
    }
    
    // Test generic error conversion
    let error = anyhow::anyhow!("Some random error");
    let server_error = ErrorHandler::from_anyhow(error);
    match server_error {
        ServerError::Internal(msg) => {
            assert_eq!(msg, "Some random error");
        }
        _ => panic!("Expected Internal error"),
    }
}

#[test]
fn test_extract_topic_name() {
    // Test with quoted topic name
    let msg = "Topic \"my-topic\" not found in broker";
    let result = extract_topic_name(msg);
    assert_eq!(result, Some("my-topic".to_string()));
    
    // Test with topic keyword
    let msg = "Unknown topic test-topic in request";
    let result = extract_topic_name(msg);
    assert_eq!(result, Some("test-topic".to_string()));
    
    // Test with no topic name
    let msg = "Some other error message";
    let result = extract_topic_name(msg);
    assert_eq!(result, None);
}

#[tokio::test]
async fn test_handle_connection_error() {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
    
    // Test connection reset
    let error = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset");
    handle_connection_error(error, addr).await;
    // Should log as debug
    
    // Test broken pipe
    let error = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "broken");
    handle_connection_error(error, addr).await;
    // Should log as debug
    
    // Test timeout
    let error = std::io::Error::new(std::io::ErrorKind::TimedOut, "timeout");
    handle_connection_error(error, addr).await;
    // Should log as warning
    
    // Test other error
    let error = std::io::Error::new(std::io::ErrorKind::Other, "unknown");
    handle_connection_error(error, addr).await;
    // Should log as error
}

// Helper function for tests
fn extract_topic_name(error_msg: &str) -> Option<String> {
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