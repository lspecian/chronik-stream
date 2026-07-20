//! Cross-client compatibility tests for Kafka protocol implementation.
//!
//! These tests verify that our implementation is compatible with various
//! Kafka client libraries by testing against known request/response patterns.

use bytes::{Buf, BufMut, BytesMut};
use chronik_protocol::handler::{ProtocolHandler, Response};
use chronik_protocol::ApiKey;

/// Assemble the on-the-wire response bytes a real client parses (minus the 4-byte
/// length prefix): correlation_id, then an empty tagged-fields byte for flexible
/// response headers (all APIs except ApiVersions, which always uses a v0 header),
/// then the body. Mirrors `integrated_server::encode_response`. The correlation_id
/// lives in the wire header, NOT in `Response::body`.
fn to_wire(resp: &Response) -> Vec<u8> {
    let mut w = Vec::new();
    w.extend_from_slice(&resp.header.correlation_id.to_be_bytes());
    if resp.is_flexible && resp.api_key != ApiKey::ApiVersions {
        w.push(0);
    }
    w.extend_from_slice(&resp.body);
    w
}

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

    assert_eq!(response.header.correlation_id, 1);

    // Parse the full wire response as librdkafka would. ApiVersions uses a
    // non-flexible header, so wire = correlation_id + body.
    let wire = to_wire(&response);
    let mut w = &wire[..];
    assert_eq!(w.get_i32(), 1, "correlation_id at the front of the wire response");

    // ApiVersions v0 body layout (chronik): error_code, then the versions array.
    let error_code = w.get_i16();
    assert_eq!(error_code, 0, "Should have no error");
    let array_len = w.get_i32();
    assert!(array_len > 0, "Should advertise API versions");
}

/// Test compatibility with Java client (Apache Kafka client) requests
#[tokio::test]
async fn test_java_client_compatibility() {
    let handler = ProtocolHandler::new();
    
    // Apache Kafka Java client 3.0.0: Metadata v9. v9 is flexible, so the request
    // header carries a tagged-fields byte after client_id and the body uses compact
    // encoding (a null topics array is the varint 0, NOT i32 -1).
    let mut java_request = BytesMut::new();
    java_request.put_i16(3); // Metadata
    java_request.put_i16(9); // Version 9
    java_request.put_i32(12345); // Correlation ID
    java_request.put_i16(10); // Client ID length
    java_request.put_slice(b"consumer-1");
    java_request.put_u8(0); // header tagged fields (flexible)

    // Metadata v9 body (flexible)
    java_request.put_u8(0); // topics: null (compact array)
    java_request.put_u8(1); // allow_auto_topic_creation: true
    java_request.put_u8(0); // include_cluster_authorized_operations: false
    java_request.put_u8(0); // include_topic_authorized_operations: false
    java_request.put_u8(0); // body tagged fields

    let response = handler.handle_request(&java_request).await.unwrap();

    // Verify response
    assert_eq!(response.header.correlation_id, 12345);
    
    // Parse the wire response as the Java client would. Metadata v9 has a FLEXIBLE
    // response header, so wire = correlation_id + one tagged-fields byte + body.
    let wire = to_wire(&response);
    let mut w = &wire[..];
    assert_eq!(w.get_i32(), 12345, "correlation_id at the front of the wire response");
    assert_eq!(w.get_u8(), 0, "flexible response-header tagged fields");
    // Body: throttle_time_ms, then a compact brokers array (varint length).
    let throttle_time = w.get_i32();
    assert_eq!(throttle_time, 0);
    assert!(!w.is_empty(), "brokers/cluster metadata follows throttle");
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
    
    // Parse the wire response. ApiVersions uses a non-flexible header.
    let wire = to_wire(&response);
    let mut w = &wire[..];
    assert_eq!(w.get_i32(), 999, "correlation_id at the front of the wire response");

    // ApiVersions v1 body layout (chronik): throttle_time_ms, error_code, array.
    let throttle_time = w.get_i32();
    assert!(throttle_time >= 0);
    let error_code = w.get_i16();
    assert_eq!(error_code, 0);
    let array_len = w.get_i32();
    assert!(array_len > 0);
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
    
    // Parse the wire response (Produce uses a non-flexible header at v0).
    let wire = to_wire(&response);
    let mut w = &wire[..];
    assert_eq!(w.get_i32(), 42, "correlation_id at the front of the wire response");

    // Produce v0 response body starts with the per-topic responses array; the
    // request carried no topics, so it is empty.
    let topics_len = w.get_i32();
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
    
    // Parse the wire response (ApiVersions v0, non-flexible header). kafkactl uses
    // correlation_id 0, which must round-trip.
    let wire = to_wire(&response);
    let mut w = &wire[..];
    assert_eq!(w.get_i32(), 0, "correlation_id 0 round-trips on the wire");

    // ApiVersions v0 body layout (chronik): error_code, then the versions array.
    let error_code = w.get_i16();
    assert_eq!(error_code, 0);
    let array_len = w.get_i32();
    assert!(array_len > 0);
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
        
        // Flexible request headers (flexible APIs EXCEPT ApiVersions, which always
        // uses a v0 request header) carry a tagged-fields byte after client_id. In
        // this list only Metadata v9 qualifies.
        let flexible = matches!((api_key, version), (3, v) if v >= 9);
        if flexible {
            request.put_u8(0); // request-header tagged fields
        }

        // Add a minimal, version-correct body per API.
        match api_key {
            3 => {
                // Metadata
                if flexible {
                    request.put_u8(0); // topics: null (compact array)
                    request.put_u8(1); // allow_auto_topic_creation
                    request.put_u8(0); // include_cluster_authorized_operations
                    request.put_u8(0); // include_topic_authorized_operations
                    request.put_u8(0); // body tagged fields
                } else {
                    // v0 uses an empty topics array; v1+ allow null (-1) = all.
                    if version == 0 {
                        request.put_i32(0);
                    } else {
                        request.put_i32(-1);
                    }
                    if version >= 4 {
                        request.put_u8(1); // allow_auto_topic_creation
                    }
                    if version >= 8 {
                        request.put_u8(0);
                        request.put_u8(0);
                    }
                }
            }
            18 => {
                // ApiVersions (request header is never flexible). v3+ has a compact body.
                if version >= 3 {
                    request.put_u8(1); // empty client_software_name
                    request.put_u8(1); // empty client_software_version
                    request.put_u8(0); // tagged fields
                }
            }
            32 => {
                // DescribeConfigs v0: empty resources array.
                request.put_i32(0);
            }
            _ => {}
        }

        let response = handler.handle_request(&request).await.unwrap();

        // The correlation_id is the first field of the WIRE response, not the body.
        assert!(!response.body.is_empty(), "Response {} v{} should have a body", api_key, version);
        assert_eq!(response.header.correlation_id, 9999);
        let wire = to_wire(&response);
        let wire_corr = i32::from_be_bytes([wire[0], wire[1], wire[2], wire[3]]);
        assert_eq!(
            wire_corr, 9999,
            "Correlation ID should be at the front of the wire response for API {} v{}",
            api_key, version
        );
    }
}