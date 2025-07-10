//! Error handling tests for Kafka protocol implementation.
//!
//! These tests verify that our implementation correctly handles various
//! error conditions and returns appropriate error codes.

use bytes::{BufMut, BytesMut};
use chronik_protocol::handler::ProtocolHandler;

/// Kafka error codes
#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(i16)]
enum ErrorCode {
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
}

/// Test handling of malformed requests
#[tokio::test]
async fn test_malformed_request_handling() {
    let handler = ProtocolHandler::new();
    
    let malformed_requests = vec![
        // Empty request
        vec![],
        // Too short for header
        vec![0x00, 0x00],
        // Invalid API key
        vec![0xFF, 0xFF, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0xFF, 0xFF],
        // Truncated in the middle of header
        vec![0x00, 0x00, 0x00, 0x00, 0x00],
    ];
    
    for (i, request) in malformed_requests.iter().enumerate() {
        let result = handler.handle_request(request).await;
        
        // Should either fail to parse or return error response
        match result {
            Ok(response) => {
                // If it somehow succeeded, verify it's an error response
                let error_code = extract_error_code(&response.body);
                assert_ne!(
                    error_code,
                    Some(0),
                    "Malformed request {} should not return success",
                    i
                );
            }
            Err(e) => {
                // Expected - malformed request failed to parse
                assert!(
                    e.to_string().contains("Protocol") ||
                    e.to_string().contains("bytes"),
                    "Unexpected error for malformed request {}: {}",
                    i,
                    e
                );
            }
        }
    }
}

/// Test handling of partial requests
#[tokio::test]
async fn test_partial_request_handling() {
    let handler = ProtocolHandler::new();
    
    // Create a complete valid request
    let mut complete = BytesMut::new();
    complete.put_i16(18); // ApiVersions
    complete.put_i16(0); // Version
    complete.put_i32(12345); // Correlation ID
    complete.put_i16(4); // Client ID length
    complete.put_slice(b"test");
    
    // Test each prefix length
    for len in 1..complete.len() {
        let partial = &complete[..len];
        let result = handler.handle_request(partial).await;
        
        // Should fail with incomplete request error
        assert!(
            result.is_err(),
            "Partial request of length {} should fail",
            len
        );
        
        if let Err(err) = result {
            assert!(
                err.to_string().contains("bytes") ||
                err.to_string().contains("enough") ||
                err.to_string().contains("Protocol"),
                "Unexpected error for partial request: {}",
                err
            );
        }
    }
}

/// Test unsupported version error responses
#[tokio::test]
async fn test_unsupported_version_errors() {
    let handler = ProtocolHandler::new();
    
    // Create request with unsupported version
    let mut request = BytesMut::new();
    request.put_i16(3); // Metadata
    request.put_i16(99); // Version way too high
    request.put_i32(999);
    request.put_i16(4);
    request.put_slice(b"test");
    
    let response = handler.handle_request(&request).await.unwrap();
    
    // Should return UNSUPPORTED_VERSION error
    let error_code = extract_error_code(&response.body);
    assert_eq!(
        error_code,
        Some(ErrorCode::UnsupportedVersion as i16),
        "Should return UNSUPPORTED_VERSION error"
    );
}

/// Test SASL authentication failure
#[tokio::test]
async fn test_sasl_authentication_failure() {
    let handler = ProtocolHandler::new();
    
    // Create SASL handshake request
    let mut request = BytesMut::new();
    request.put_i16(17); // SaslHandshake
    request.put_i16(0); // Version
    request.put_i32(456);
    request.put_i16(4);
    request.put_slice(b"test");
    request.put_i16(5); // Mechanism length
    request.put_slice(b"PLAIN");
    
    let response = handler.handle_request(&request).await.unwrap();
    
    // Parse response
    let mut body = response.body.clone();
    body.advance(4); // Skip correlation ID
    
    let error_code = body.get_i16();
    assert_eq!(
        error_code,
        ErrorCode::UnsupportedSaslMechanism as i16,
        "SASL should return authentication failure"
    );
}

/// Test error response format consistency
#[tokio::test]
async fn test_error_response_format() {
    let handler = ProtocolHandler::new();
    
    // Test various APIs that should return errors
    let error_apis = vec![
        (99, "NonExistentAPI"),
        (4, "LeaderAndIsr"), // Broker-only API
        (22, "InitProducerId"), // Transaction API
    ];
    
    for (api_key, name) in error_apis {
        let mut request = BytesMut::new();
        request.put_i16(api_key);
        request.put_i16(0);
        request.put_i32(12345);
        request.put_i16(4);
        request.put_slice(b"test");
        
        let response = handler.handle_request(&request).await.unwrap();
        
        // Verify correlation ID is preserved
        assert_eq!(response.header.correlation_id, 12345);
        
        // Verify response has error code
        let error_code = extract_error_code(&response.body);
        assert!(
            error_code.is_some() && error_code != Some(0),
            "API {} ({}) should return error code",
            api_key,
            name
        );
    }
}

/// Test handling of invalid string encodings
#[tokio::test]
async fn test_invalid_string_encoding() {
    let handler = ProtocolHandler::new();
    
    // Create request with invalid UTF-8 in client ID
    let mut request = BytesMut::new();
    request.put_i16(18); // ApiVersions
    request.put_i16(0);
    request.put_i32(999);
    request.put_i16(4); // Client ID length
    request.put(&[0xFF, 0xFE, 0xFD, 0xFC][..]); // Invalid UTF-8
    
    let result = handler.handle_request(&request).await;
    
    // Should handle gracefully (either parse error or continue)
    match result {
        Ok(response) => {
            // If it parsed, correlation ID should still be preserved
            assert_eq!(response.header.correlation_id, 999);
        }
        Err(e) => {
            // Should be UTF-8 error
            assert!(
                e.to_string().contains("UTF-8") ||
                e.to_string().contains("utf8") ||
                e.to_string().contains("string"),
                "Unexpected error: {}",
                e
            );
        }
    }
}

/// Test handling of oversized requests
#[tokio::test]
async fn test_oversized_request_handling() {
    let handler = ProtocolHandler::new();
    
    // Create request with very large client ID
    let mut request = BytesMut::new();
    request.put_i16(18); // ApiVersions
    request.put_i16(0);
    request.put_i32(999);
    request.put_i16(10000); // Client ID length way too big
    
    // Don't actually put 10000 bytes, just a few
    request.put_slice(b"test");
    
    let result = handler.handle_request(&request).await;
    
    // Should fail to parse
    assert!(
        result.is_err(),
        "Oversized request should fail to parse"
    );
}

/// Test sequential error handling (concurrent test removed due to handler not being Clone)
#[tokio::test]
async fn test_sequential_error_handling() {
    let handler = ProtocolHandler::new();
    
    // Test multiple error cases sequentially
    for i in 0..10 {
        let mut request = BytesMut::new();
        request.put_i16(99); // Invalid API
        request.put_i16(0);
        request.put_i32(i);
        request.put_i16(4);
        request.put_slice(b"test");
        
        let result = handler.handle_request(&request).await;
        assert!(
            result.is_ok(),
            "Request {} should not panic",
            i
        );
        
        let response = result.unwrap();
        assert_eq!(
            response.header.correlation_id,
            i as i32,
            "Correlation ID should be preserved"
        );
    }
}

/// Helper to extract error code from response body
fn extract_error_code(body: &[u8]) -> Option<i16> {
    use bytes::Buf;
    
    if body.len() < 6 {
        return None;
    }
    
    let mut buf = body;
    buf.advance(4); // Skip correlation ID
    
    // Next should be error code (for most error responses)
    Some(buf.get_i16())
}

/// Test that all error codes are distinct
#[test]
fn test_error_codes_are_distinct() {
    use std::collections::HashSet;
    
    let error_codes = vec![
        ErrorCode::None,
        ErrorCode::OffsetOutOfRange,
        ErrorCode::CorruptMessage,
        ErrorCode::UnknownTopicOrPartition,
        ErrorCode::InvalidFetchSize,
        ErrorCode::LeaderNotAvailable,
        ErrorCode::UnsupportedVersion,
        ErrorCode::UnsupportedSaslMechanism,
    ];
    
    let mut seen = HashSet::new();
    for code in error_codes {
        assert!(
            seen.insert(code as i16),
            "Duplicate error code: {}",
            code as i16
        );
    }
}

// Import necessary traits
use bytes::Buf;