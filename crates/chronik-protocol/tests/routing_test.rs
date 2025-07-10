//! Tests for protocol handler routing verification.

use bytes::{Buf, BytesMut, BufMut};
use chronik_protocol::handler::ProtocolHandler;

/// Test that all supported APIs in supported_api_versions are routable
#[tokio::test]
async fn test_all_supported_apis_are_routable() {
    let handler = ProtocolHandler::new();
    
    // List of all APIs that should be routable (not return generic unsupported error)
    let routable_apis = vec![
        (0, "Produce"),
        (1, "Fetch"),
        (3, "Metadata"),
        (8, "OffsetCommit"),
        (9, "OffsetFetch"),
        (10, "FindCoordinator"),
        (11, "JoinGroup"),
        (12, "Heartbeat"),
        (13, "LeaveGroup"),
        (14, "SyncGroup"),
        (15, "DescribeGroups"),
        (16, "ListGroups"),
        (17, "SaslHandshake"),
        (18, "ApiVersions"),
        (19, "CreateTopics"),
        (20, "DeleteTopics"),
        (32, "DescribeConfigs"),
        (33, "AlterConfigs"),
        (2, "ListOffsets"),
    ];
    
    for (api_key, api_name) in routable_apis {
        // Create minimal valid request
        let mut request_buf = BytesMut::new();
        request_buf.put_i16(api_key);
        request_buf.put_i16(0); // Version 0
        request_buf.put_i32(999); // Correlation ID
        request_buf.put_i16(4); // Client ID length
        request_buf.put_slice(b"test");
        
        // Add minimal body to avoid parsing errors
        match api_key {
            0 => { // Produce needs acks, timeout, topics array
                request_buf.put_i16(-1); // acks
                request_buf.put_i32(30000); // timeout_ms
                request_buf.put_i32(0); // topics array (empty)
            }
            1 => { // Fetch needs replica_id, max_wait_ms, min_bytes, topics array
                request_buf.put_i32(-1); // replica_id
                request_buf.put_i32(500); // max_wait_ms
                request_buf.put_i32(1); // min_bytes
                request_buf.put_i32(0); // topics array (empty)
            }
            3 => { // Metadata v0 has no body
                // Empty body
            }
            17 => { // SaslHandshake needs mechanism
                request_buf.put_i16(4); // Mechanism length
                request_buf.put_slice(b"NONE");
            }
            18 => { // ApiVersions v0 has no body
                // Empty body
            }
            32 => { // DescribeConfigs needs resources array
                request_buf.put_i32(0); // resources array (empty)
            }
            _ => { // For unimplemented APIs, they won't parse the body
                // Empty body is fine
            }
        }
        
        let result = handler.handle_request(&request_buf).await;
        
        // The request should be handled (not fail to parse the API key)
        assert!(
            result.is_ok(),
            "API {} ({}) should be routable but failed: {:?}",
            api_name,
            api_key,
            result.err()
        );
        
        // Check that the correlation ID is preserved
        if let Ok(response) = result {
            assert_eq!(
                response.header.correlation_id, 999,
                "API {} ({}) should preserve correlation ID",
                api_name,
                api_key
            );
        }
    }
}

/// Test that truly unsupported APIs are handled properly
#[tokio::test]
async fn test_unsupported_apis_handled_gracefully() {
    let handler = ProtocolHandler::new();
    
    // Test some API keys that don't exist
    let unsupported_apis = vec![
        (99, "NonExistent99"),
        (255, "NonExistent255"),
        (1000, "NonExistent1000"),
    ];
    
    for (api_key, api_name) in unsupported_apis {
        // Create request with unsupported API key
        let mut request_buf = BytesMut::new();
        request_buf.put_i16(api_key);
        request_buf.put_i16(0); // Version 0
        request_buf.put_i32(888); // Correlation ID
        request_buf.put_i16(4); // Client ID length
        request_buf.put_slice(b"test");
        
        let result = handler.handle_request(&request_buf).await;
        
        // Should either fail to parse or return an error response
        if let Ok(response) = result {
            // If it didn't fail to parse, it should still preserve correlation ID
            assert_eq!(
                response.header.correlation_id, 888,
                "Unsupported API {} should preserve correlation ID",
                api_name
            );
        }
    }
}

/// Test that broker-to-broker APIs are rejected
#[tokio::test]
async fn test_broker_to_broker_apis_rejected() {
    let handler = ProtocolHandler::new();
    
    let broker_apis = vec![
        (4, "LeaderAndIsr"),
        (5, "StopReplica"),
        (6, "UpdateMetadata"),
        (7, "ControlledShutdown"),
    ];
    
    for (api_key, api_name) in broker_apis {
        let mut request_buf = BytesMut::new();
        request_buf.put_i16(api_key);
        request_buf.put_i16(0); // Version 0
        request_buf.put_i32(777); // Correlation ID
        request_buf.put_i16(4); // Client ID length
        request_buf.put_slice(b"test");
        
        let result = handler.handle_request(&request_buf).await;
        
        // Should handle the request (not crash)
        assert!(
            result.is_ok(),
            "Broker API {} ({}) should be handled gracefully",
            api_name,
            api_key
        );
        
        if let Ok(response) = result {
            assert_eq!(
                response.header.correlation_id, 777,
                "Broker API {} should preserve correlation ID",
                api_name
            );
        }
    }
}

/// Test that the ApiVersions response includes all expected APIs
#[tokio::test]
async fn test_api_versions_response_completeness() {
    let handler = ProtocolHandler::new();
    
    // Create ApiVersions request
    let mut request_buf = BytesMut::new();
    request_buf.put_i16(18); // ApiVersions
    request_buf.put_i16(0); // Version 0
    request_buf.put_i32(123); // Correlation ID
    request_buf.put_i16(4); // Client ID length
    request_buf.put_slice(b"test");
    
    let response = handler.handle_request(&request_buf).await.unwrap();
    
    // Parse the response to check which APIs are advertised
    let mut body = response.body.clone();
    
    // Skip correlation ID
    let _corr_id = body.get(0..4);
    body.advance(4);
    
    // For v0, array comes first
    let array_len = i32::from_be_bytes([body[0], body[1], body[2], body[3]]);
    body.advance(4);
    
    let mut advertised_apis = std::collections::HashSet::new();
    
    for _ in 0..array_len {
        let api_key = i16::from_be_bytes([body[0], body[1]]);
        body.advance(2);
        let _min_version = i16::from_be_bytes([body[0], body[1]]);
        body.advance(2);
        let _max_version = i16::from_be_bytes([body[0], body[1]]);
        body.advance(2);
        
        advertised_apis.insert(api_key);
    }
    
    // Check that key APIs are advertised
    let required_apis = vec![
        0,  // Produce
        1,  // Fetch
        3,  // Metadata
        18, // ApiVersions
        32, // DescribeConfigs
    ];
    
    for api_key in required_apis {
        assert!(
            advertised_apis.contains(&api_key),
            "API key {} should be advertised in ApiVersions response",
            api_key
        );
    }
}

/// Test specific API routing behavior
#[tokio::test]
async fn test_specific_api_behaviors() {
    let handler = ProtocolHandler::new();
    
    // Test SASL handshake returns authentication failed
    let mut request_buf = BytesMut::new();
    request_buf.put_i16(17); // SaslHandshake
    request_buf.put_i16(0); // Version 0
    request_buf.put_i32(456); // Correlation ID
    request_buf.put_i16(4); // Client ID length
    request_buf.put_slice(b"test");
    request_buf.put_i16(5); // Mechanism length
    request_buf.put_slice(b"PLAIN");
    
    let response = handler.handle_request(&request_buf).await.unwrap();
    assert_eq!(response.header.correlation_id, 456);
    
    // Parse SASL response
    let mut body = response.body.clone();
    body.advance(4); // Skip correlation ID
    let error_code = i16::from_be_bytes([body[0], body[1]]);
    assert_eq!(error_code, 33, "SASL should return authentication failed");
}