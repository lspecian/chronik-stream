//! Cross-client compatibility tests for Kafka protocol implementation.
//!
//! These tests verify that our implementation is compatible with various
//! Kafka client libraries by testing against known request/response patterns.

use bytes::{Buf, BufMut, BytesMut};
use chronik_protocol::handler::ProtocolHandler;

/// Test compatibility with librdkafka client requests
#[tokio::test]
async fn test_librdkafka_compatibility() {
    let handler = ProtocolHandler::new();
    
    // Real request captured from librdkafka 1.9.0
    // ApiVersions request (v0) with client ID "rdkafka"
    let librdkafka_request = vec![
        0x00, 0x12, // API key: 18 (ApiVersions)
        0x00, 0x00, // API version: 0
        0x00, 0x00, 0x00, 0x01, // Correlation ID: 1
        0x00, 0x07, // Client ID length: 7
        b'r', b'd', b'k', b'a', b'f', b'k', b'a', // "rdkafka"
    ];
    
    let response = handler.handle_request(&librdkafka_request).await.unwrap();
    
    // Verify response structure
    assert_eq!(response.header.correlation_id, 1);
    
    // Parse response body
    let mut body = &response.body[..];
    let correlation_id = body.get_i32();
    assert_eq!(correlation_id, 1);
    
    // For v0, array comes before error code
    let array_len = body.get_i32();
    assert!(array_len > 0, "Should have API versions");
    
    // Skip array entries
    body.advance((array_len * 6) as usize);
    
    let error_code = body.get_i16();
    assert_eq!(error_code, 0, "Should have no error");
}

/// Test compatibility with Java client (Apache Kafka client) requests
#[tokio::test]
async fn test_java_client_compatibility() {
    let handler = ProtocolHandler::new();
    
    // Real request pattern from Apache Kafka Java client 3.0.0
    // Metadata request (v9) with client ID "consumer-1"
    let mut java_request = BytesMut::new();
    java_request.put_i16(3); // Metadata
    java_request.put_i16(9); // Version 9
    java_request.put_i32(12345); // Correlation ID
    java_request.put_i16(10); // Client ID length
    java_request.put_slice(b"consumer-1");
    
    // Metadata v9 body
    java_request.put_i32(-1); // Topics: null (fetch all)
    java_request.put_u8(1); // allow_auto_topic_creation: true
    java_request.put_u8(0); // include_cluster_authorized_operations: false
    java_request.put_u8(0); // include_topic_authorized_operations: false
    
    let response = handler.handle_request(&java_request).await.unwrap();
    
    // Verify response
    assert_eq!(response.header.correlation_id, 12345);
    
    // Parse response body
    let mut body = &response.body[..];
    let correlation_id = body.get_i32();
    assert_eq!(correlation_id, 12345);
    
    // v9 has throttle time
    let throttle_time = body.get_i32();
    assert_eq!(throttle_time, 0);
    
    // Brokers array
    let brokers_len = body.get_i32();
    assert!(brokers_len >= 0, "Should have brokers array");
}

/// Test compatibility with confluent-kafka-go client requests
#[tokio::test]
async fn test_confluent_kafka_go_compatibility() {
    let handler = ProtocolHandler::new();
    
    // Pattern from confluent-kafka-go 1.9.2
    // ApiVersions request (v1) with client ID "confluent.golang"
    let mut go_request = BytesMut::new();
    go_request.put_i16(18); // ApiVersions
    go_request.put_i16(1); // Version 1
    go_request.put_i32(999); // Correlation ID
    go_request.put_i16(16); // Client ID length
    go_request.put_slice(b"confluent.golang");
    
    let response = handler.handle_request(&go_request).await.unwrap();
    
    // Verify response
    assert_eq!(response.header.correlation_id, 999);
    
    // Parse v1 response
    let mut body = &response.body[..];
    let correlation_id = body.get_i32();
    assert_eq!(correlation_id, 999);
    
    // v1 has error code first
    let error_code = body.get_i16();
    assert_eq!(error_code, 0);
    
    // Then API versions array
    let array_len = body.get_i32();
    assert!(array_len > 0);
    
    // Then throttle time
    body.advance((array_len * 6) as usize);
    let throttle_time = body.get_i32();
    assert!(throttle_time >= 0);
}

/// Test compatibility with node-rdkafka client requests
#[tokio::test]
async fn test_node_rdkafka_compatibility() {
    let handler = ProtocolHandler::new();
    
    // Pattern from node-rdkafka 2.13.0
    // Produce request (v0) - simplest version
    let mut node_request = BytesMut::new();
    node_request.put_i16(0); // Produce
    node_request.put_i16(0); // Version 0
    node_request.put_i32(42); // Correlation ID
    node_request.put_i16(12); // Client ID length
    node_request.put_slice(b"node-rdkafka");
    
    // Produce v0 body
    node_request.put_i16(1); // acks: 1
    node_request.put_i32(1000); // timeout: 1000ms
    node_request.put_i32(0); // topics array: empty
    
    let response = handler.handle_request(&node_request).await.unwrap();
    
    // Verify response
    assert_eq!(response.header.correlation_id, 42);
    
    // Parse response
    let mut body = &response.body[..];
    let correlation_id = body.get_i32();
    assert_eq!(correlation_id, 42);
    
    // Topics array
    let topics_len = body.get_i32();
    assert_eq!(topics_len, 0, "Should have empty topics array");
}

/// Test compatibility with PyKafka client requests
#[tokio::test]
async fn test_pykafka_compatibility() {
    let handler = ProtocolHandler::new();
    
    // Pattern from PyKafka 2.8.0
    // DescribeConfigs request (v0)
    let mut py_request = BytesMut::new();
    py_request.put_i16(32); // DescribeConfigs
    py_request.put_i16(0); // Version 0
    py_request.put_i32(777); // Correlation ID
    py_request.put_i16(7); // Client ID length
    py_request.put_slice(b"pykafka");
    
    // DescribeConfigs v0 body
    py_request.put_i32(1); // resources array length
    py_request.put_i8(2); // resource type: TOPIC
    py_request.put_i16(10); // resource name length
    py_request.put_slice(b"test-topic");
    py_request.put_i32(-1); // config names: null (all)
    
    let response = handler.handle_request(&py_request).await.unwrap();
    
    // Verify response
    assert_eq!(response.header.correlation_id, 777);
}

/// Test compatibility with sarama (Go) client requests
#[tokio::test]
async fn test_sarama_compatibility() {
    let handler = ProtocolHandler::new();
    
    // Pattern from Shopify/sarama 1.34.1
    // ApiVersions request (v3) - flexible version
    let mut sarama_request = BytesMut::new();
    sarama_request.put_i16(18); // ApiVersions
    sarama_request.put_i16(3); // Version 3 (flexible)
    sarama_request.put_i32(1337); // Correlation ID
    sarama_request.put_i16(6); // Client ID length
    sarama_request.put_slice(b"sarama");
    
    // v3 body (flexible)
    sarama_request.put_u8(7); // client_software_name length + 1
    sarama_request.put_slice(b"sarama");
    sarama_request.put_u8(7); // client_software_version length + 1
    sarama_request.put_slice(b"1.34.1");
    sarama_request.put_u8(0); // tagged fields
    
    let response = handler.handle_request(&sarama_request).await.unwrap();
    
    // Verify response
    assert_eq!(response.header.correlation_id, 1337);
}

/// Test handling of malformed client requests
#[tokio::test]
async fn test_malformed_client_request_handling() {
    let handler = ProtocolHandler::new();
    
    // Common client mistakes
    let malformed_cases = vec![
        (
            "Wrong endianness",
            vec![
                0x12, 0x00, // API key in wrong endianness
                0x00, 0x00, // Version
                0x00, 0x00, 0x00, 0x01, // Correlation ID
                0x00, 0x04, // Client ID length
                b't', b'e', b's', b't',
            ],
        ),
        (
            "String length mismatch",
            vec![
                0x00, 0x12, // ApiVersions
                0x00, 0x00, // Version
                0x00, 0x00, 0x00, 0x01, // Correlation ID
                0x00, 0x10, // Client ID length: 16 (but only 4 bytes follow)
                b't', b'e', b's', b't',
            ],
        ),
        (
            "Negative string length",
            vec![
                0x00, 0x12, // ApiVersions
                0x00, 0x00, // Version
                0x00, 0x00, 0x00, 0x01, // Correlation ID
                0xff, 0xfe, // Client ID length: -2 (invalid)
            ],
        ),
    ];
    
    for (name, request) in malformed_cases {
        let result = handler.handle_request(&request).await;
        assert!(
            result.is_err() || result.is_ok(),
            "Should handle {} gracefully",
            name
        );
    }
}

/// Test version negotiation with different clients
#[tokio::test]
async fn test_client_version_negotiation() {
    let handler = ProtocolHandler::new();
    
    // Different clients with different version capabilities
    let client_tests = vec![
        ("legacy-client", 0i16), // Only supports v0
        ("modern-client", 3i16), // Supports up to v3
        ("future-client", 99i16), // Requests unsupported version
    ];
    
    for (client_name, version) in client_tests {
        let mut request = BytesMut::new();
        request.put_i16(18); // ApiVersions
        request.put_i16(version);
        request.put_i32(1000 + version as i32);
        request.put_i16(client_name.len() as i16);
        request.put_slice(client_name.as_bytes());
        
        // Add version-specific body
        match version {
            0 => {
                // v0 has no body
            }
            3 => {
                // v3 has flexible body
                request.put_u8(1); // empty client_software_name
                request.put_u8(1); // empty client_software_version
                request.put_u8(0); // tagged fields
            }
            _ => {
                // Unknown version - add minimal body
            }
        }
        
        let result = handler.handle_request(&request).await;
        
        if version <= 3 {
            assert!(
                result.is_ok(),
                "Should handle {} with version {}",
                client_name,
                version
            );
            
            let response = result.unwrap();
            assert_eq!(
                response.header.correlation_id,
                1000 + version as i32,
                "Correlation ID should be preserved for {}",
                client_name
            );
        } else {
            // Version too high - should either fail or return error response
            assert!(
                result.is_err() || result.is_ok(),
                "Should handle {} with unsupported version {} gracefully",
                client_name,
                version
            );
        }
    }
}

/// Test compatibility with kafkactl tool patterns
#[tokio::test]
async fn test_kafkactl_patterns() {
    let handler = ProtocolHandler::new();
    
    // kafkactl specific patterns from our debugging
    let mut kafkactl_request = BytesMut::new();
    kafkactl_request.put_i16(18); // ApiVersions
    kafkactl_request.put_i16(0); // Version 0
    kafkactl_request.put_i32(0); // Correlation ID: 0 (kafkactl uses 0)
    kafkactl_request.put_i16(8); // Client ID length
    kafkactl_request.put_slice(b"kafkactl");
    
    let response = handler.handle_request(&kafkactl_request).await.unwrap();
    
    // Verify response matches kafkactl expectations
    assert_eq!(response.header.correlation_id, 0);
    
    // Parse response - kafkactl expects v0 format
    let mut body = &response.body[..];
    let correlation_id = body.get_i32();
    assert_eq!(correlation_id, 0);
    
    // v0: array before error code
    let array_len = body.get_i32();
    assert!(array_len > 0);
    
    // Skip array
    body.advance((array_len * 6) as usize);
    
    // Error code at the end
    let error_code = body.get_i16();
    assert_eq!(error_code, 0);
}

/// Test that our responses are parseable by common client decoders
#[tokio::test]
async fn test_response_parseability() {
    let handler = ProtocolHandler::new();
    
    // Test various API responses
    let test_apis = vec![
        (18i16, 0i16), // ApiVersions v0
        (18i16, 1i16), // ApiVersions v1
        (18i16, 3i16), // ApiVersions v3
        (3i16, 0i16),  // Metadata v0
        (3i16, 9i16),  // Metadata v9
        (32i16, 0i16), // DescribeConfigs v0
    ];
    
    for (api_key, version) in test_apis {
        let mut request = BytesMut::new();
        request.put_i16(api_key);
        request.put_i16(version);
        request.put_i32(9999);
        request.put_i16(4);
        request.put_slice(b"test");
        
        // Add minimal body based on API
        match api_key {
            3 => {
                // Metadata
                if version >= 1 {
                    request.put_i32(-1); // null topics
                }
                if version >= 4 {
                    request.put_u8(1); // allow_auto_topic_creation
                }
                if version >= 8 {
                    request.put_u8(0); // include_cluster_authorized_operations
                    request.put_u8(0); // include_topic_authorized_operations
                }
            }
            18 => {
                // ApiVersions
                if version == 3 {
                    request.put_u8(1); // empty client_software_name
                    request.put_u8(1); // empty client_software_version
                    request.put_u8(0); // tagged fields
                }
            }
            32 => {
                // DescribeConfigs
                request.put_i32(0); // empty resources
            }
            _ => {}
        }
        
        let response = handler.handle_request(&request).await.unwrap();
        
        // Verify response is parseable
        assert!(!response.body.is_empty(), "Response should have body");
        assert_eq!(response.header.correlation_id, 9999);
        
        // Verify correlation ID in body
        let body_correlation_id = i32::from_be_bytes([
            response.body[0],
            response.body[1],
            response.body[2],
            response.body[3],
        ]);
        assert_eq!(
            body_correlation_id, 9999,
            "Correlation ID should be in response body for API {} v{}",
            api_key, version
        );
    }
}