//! Wire format verification tests for Kafka protocol compatibility.
//!
//! These tests verify that our protocol implementation produces byte-for-byte
//! compatible messages with the official Kafka protocol specification.

use bytes::{Buf, BufMut, BytesMut};
use chronik_protocol::handler::ProtocolHandler;
use chronik_protocol::parser::{ApiKey, Encoder};

/// Test that request headers are serialized correctly
#[test]
fn test_request_header_wire_format() {
    let test_cases = vec![
        (
            // API key, version, correlation ID, client ID
            (ApiKey::Produce as i16, 0i16, 12345i32, Some("test-client")),
            // Expected bytes (without length prefix)
            vec![
                0x00, 0x00, // API key: 0 (Produce)
                0x00, 0x00, // API version: 0
                0x00, 0x00, 0x30, 0x39, // Correlation ID: 12345
                0x00, 0x0b, // Client ID length: 11
                b't', b'e', b's', b't', b'-', b'c', b'l', b'i', b'e', b'n', b't', // "test-client"
            ],
        ),
        (
            (ApiKey::Metadata as i16, 12i16, 999i32, Some("my-app")),
            vec![
                0x00, 0x03, // API key: 3 (Metadata)
                0x00, 0x0c, // API version: 12
                0x00, 0x00, 0x03, 0xe7, // Correlation ID: 999
                0x00, 0x06, // Client ID length: 6
                b'm', b'y', b'-', b'a', b'p', b'p', // "my-app"
            ],
        ),
        (
            // Test with null client ID
            (ApiKey::ApiVersions as i16, 3i16, 456i32, None),
            vec![
                0x00, 0x12, // API key: 18 (ApiVersions)
                0x00, 0x03, // API version: 3
                0x00, 0x00, 0x01, 0xc8, // Correlation ID: 456
                0xff, 0xff, // Client ID length: -1 (null)
            ],
        ),
    ];

    for ((api_key, version, correlation_id, client_id), expected) in test_cases {
        let mut buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut buf);
        
        // Encode header
        encoder.write_i16(api_key);
        encoder.write_i16(version);
        encoder.write_i32(correlation_id);
        encoder.write_string(client_id);
        
        assert_eq!(
            buf.to_vec(),
            expected,
            "Header encoding mismatch for API key {}, version {}",
            api_key,
            version
        );
    }
}

/// Test ApiVersions response wire format for v0
#[test]
fn test_api_versions_v0_wire_format() {
    // This is the critical test for kafkactl compatibility
    let mut buf = BytesMut::new();
    let mut encoder = Encoder::new(&mut buf);
    
    // Correlation ID
    encoder.write_i32(12345);
    
    // For v0: array comes BEFORE error_code
    // API versions array
    encoder.write_i32(3); // Array length
    
    // API version entries
    encoder.write_i16(0); // Produce
    encoder.write_i16(0); // Min version
    encoder.write_i16(9); // Max version
    
    encoder.write_i16(1); // Fetch
    encoder.write_i16(0); // Min version
    encoder.write_i16(13); // Max version
    
    encoder.write_i16(3); // Metadata
    encoder.write_i16(0); // Min version
    encoder.write_i16(12); // Max version
    
    // Error code comes AFTER array for v0
    encoder.write_i16(0); // Error code: NONE
    
    let expected = vec![
        // Correlation ID
        0x00, 0x00, 0x30, 0x39,
        // Array length
        0x00, 0x00, 0x00, 0x03,
        // Produce API
        0x00, 0x00, 0x00, 0x00, 0x00, 0x09,
        // Fetch API
        0x00, 0x01, 0x00, 0x00, 0x00, 0x0d,
        // Metadata API
        0x00, 0x03, 0x00, 0x00, 0x00, 0x0c,
        // Error code
        0x00, 0x00,
    ];
    
    assert_eq!(buf.to_vec(), expected, "ApiVersions v0 wire format mismatch");
}

/// Test ApiVersions response wire format for v3 (flexible version)
#[test]
fn test_api_versions_v3_wire_format() {
    let mut buf = BytesMut::new();
    let mut encoder = Encoder::new(&mut buf);
    
    // Correlation ID
    encoder.write_i32(789);
    
    // For v3+: error_code comes FIRST
    encoder.write_i16(0); // Error code: NONE
    
    // API versions array with compact encoding
    encoder.write_unsigned_varint(4); // Array length + 1 (compact array)
    
    // API version entries
    encoder.write_i16(0); // Produce
    encoder.write_i16(0); // Min version
    encoder.write_i16(9); // Max version
    encoder.write_unsigned_varint(0); // Tagged fields
    
    encoder.write_i16(1); // Fetch
    encoder.write_i16(0); // Min version
    encoder.write_i16(13); // Max version
    encoder.write_unsigned_varint(0); // Tagged fields
    
    encoder.write_i16(3); // Metadata
    encoder.write_i16(0); // Min version
    encoder.write_i16(12); // Max version
    encoder.write_unsigned_varint(0); // Tagged fields
    
    // Throttle time
    encoder.write_i32(0);
    
    // Tagged fields
    encoder.write_unsigned_varint(0);
    
    // Verify the structure is correct (exact bytes would depend on varint encoding)
    assert!(buf.len() > 20, "ApiVersions v3 response too short");
    
    // Verify correlation ID
    assert_eq!(&buf[0..4], &[0x00, 0x00, 0x03, 0x15]);
    
    // Verify error code position
    assert_eq!(&buf[4..6], &[0x00, 0x00]);
}

/// Test Metadata request wire format
#[test]
fn test_metadata_request_wire_format() {
    let test_cases = vec![
        (
            // Version 0: no request body
            0,
            vec![],
            vec![], // Empty body for v0
        ),
        (
            // Version 1: topics array
            1,
            vec![Some("test-topic".to_string()), Some("another-topic".to_string())],
            vec![
                0x00, 0x00, 0x00, 0x02, // Topics array length: 2
                0x00, 0x0a, // String length: 10
                b't', b'e', b's', b't', b'-', b't', b'o', b'p', b'i', b'c',
                0x00, 0x0d, // String length: 13
                b'a', b'n', b'o', b't', b'h', b'e', b'r', b'-', b't', b'o', b'p', b'i', b'c',
            ],
        ),
        (
            // Version 4: topics array + allow_auto_topic_creation
            4,
            vec![], // Null topics array (fetch all)
            vec![
                0xff, 0xff, 0xff, 0xff, // Topics array: null (-1)
                0x01, // allow_auto_topic_creation: true
            ],
        ),
    ];
    
    for (version, topics, expected_body) in test_cases {
        let mut buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut buf);
        
        match version {
            0 => {
                // v0 has no body
            }
            1..=3 => {
                // Topics array
                if topics.is_empty() {
                    encoder.write_i32(-1); // Null array
                } else {
                    encoder.write_i32(topics.len() as i32);
                    for topic in &topics {
                        encoder.write_string(topic.as_deref());
                    }
                }
            }
            4..=8 => {
                // Topics array
                if topics.is_empty() {
                    encoder.write_i32(-1); // Null array
                } else {
                    encoder.write_i32(topics.len() as i32);
                    for topic in &topics {
                        encoder.write_string(topic.as_deref());
                    }
                }
                // allow_auto_topic_creation
                encoder.write_bool(true);
            }
            _ => panic!("Unsupported metadata version in test"),
        }
        
        if !expected_body.is_empty() {
            assert_eq!(
                buf.to_vec(),
                expected_body,
                "Metadata request v{} body mismatch",
                version
            );
        }
    }
}

/// Test Produce request wire format
#[test]
fn test_produce_request_wire_format() {
    let mut buf = BytesMut::new();
    let mut encoder = Encoder::new(&mut buf);
    
    // Produce v0 request
    encoder.write_i16(-1); // acks: -1 (all)
    encoder.write_i32(30000); // timeout_ms
    encoder.write_i32(1); // topics array length
    
    // Topic
    encoder.write_string(Some("test-topic"));
    encoder.write_i32(1); // partitions array length
    
    // Partition
    encoder.write_i32(0); // partition index
    encoder.write_i32(100); // message set size
    // Note: actual message set bytes would go here
    
    let expected_start = vec![
        0xff, 0xff, // acks: -1
        0x00, 0x00, 0x75, 0x30, // timeout: 30000
        0x00, 0x00, 0x00, 0x01, // topics: 1
        0x00, 0x0a, // topic name length: 10
        b't', b'e', b's', b't', b'-', b't', b'o', b'p', b'i', b'c',
        0x00, 0x00, 0x00, 0x01, // partitions: 1
        0x00, 0x00, 0x00, 0x00, // partition: 0
        0x00, 0x00, 0x00, 0x64, // message set size: 100
    ];
    
    assert_eq!(
        &buf[..expected_start.len()],
        &expected_start[..],
        "Produce request wire format mismatch"
    );
}

/// Test error response wire format
#[test]
fn test_error_response_wire_format() {
    // Test various error codes are encoded correctly
    let error_codes = vec![
        (0, "NONE"),
        (1, "OFFSET_OUT_OF_RANGE"),
        (2, "CORRUPT_MESSAGE"),
        (3, "UNKNOWN_TOPIC_OR_PARTITION"),
        (35, "UNSUPPORTED_VERSION"),
    ];
    
    for (code, name) in error_codes {
        let mut buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut buf);
        
        // Simple error response format
        encoder.write_i32(999); // Correlation ID
        encoder.write_i16(code); // Error code
        
        assert_eq!(
            buf[4..6],
            code.to_be_bytes(),
            "Error code {} ({}) not encoded correctly",
            code,
            name
        );
    }
}

/// Test string encoding formats
#[test]
fn test_string_encoding_formats() {
    // Test both legacy and compact string encoding
    let test_strings = vec![
        ("hello", false),
        ("", false),
        ("a very long string that exceeds typical lengths", false),
        ("compact", true),
    ];
    
    for (s, use_compact) in test_strings {
        let mut buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut buf);
        
        if use_compact {
            encoder.write_compact_string(Some(s));
            
            // Compact strings use varint length + 1
            let mut bytes = buf.clone();
            let length = read_unsigned_varint(&mut bytes) - 1;
            assert_eq!(length as usize, s.len());
        } else {
            encoder.write_string(Some(s));
            
            // Legacy strings use 2-byte length
            let length = i16::from_be_bytes([buf[0], buf[1]]);
            assert_eq!(length as usize, s.len());
            assert_eq!(&buf[2..2 + s.len()], s.as_bytes());
        }
    }
}

/// Test null value encoding
#[test]
fn test_null_value_encoding() {
    // Null string (legacy)
    {
        let mut buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut buf);
        encoder.write_string(None);
        drop(encoder); // Release the borrow
        assert_eq!(&buf[..2], &[0xff, 0xff]); // -1 in big-endian
    }
    
    // Null bytes
    {
        let mut buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut buf);
        encoder.write_bytes(None);
        drop(encoder); // Release the borrow
        assert_eq!(&buf[..4], &[0xff, 0xff, 0xff, 0xff]); // -1 in big-endian
    }
    
    // Null compact string
    {
        let mut buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut buf);
        encoder.write_compact_string(None);
        drop(encoder); // Release the borrow
        assert_eq!(&buf[..1], &[0x00]); // 0 in varint
    }
}

/// Helper to read unsigned varint
fn read_unsigned_varint(buf: &mut dyn Buf) -> u32 {
    let mut value = 0u32;
    let mut i = 0;
    
    loop {
        let byte = buf.get_u8();
        value |= ((byte & 0x7F) as u32) << (i * 7);
        
        if byte & 0x80 == 0 {
            return value;
        }
        
        i += 1;
        if i >= 5 {
            panic!("Varint too long");
        }
    }
}

/// Test that response correlation IDs are preserved
#[tokio::test]
async fn test_correlation_id_preservation() {
    let handler = ProtocolHandler::new();
    
    let test_ids = vec![0, 1, 12345, i32::MAX, -1, i32::MIN];
    
    for correlation_id in test_ids {
        // Create a minimal ApiVersions request
        let mut request = BytesMut::new();
        request.put_i16(18); // ApiVersions
        request.put_i16(0); // Version 0
        request.put_i32(correlation_id);
        request.put_i16(4); // Client ID length
        request.put_slice(b"test");
        
        let response = handler.handle_request(&request).await.unwrap();
        
        assert_eq!(
            response.header.correlation_id,
            correlation_id,
            "Correlation ID not preserved"
        );
        
        // Also verify it's in the response body
        let body_correlation_id = i32::from_be_bytes([
            response.body[0],
            response.body[1],
            response.body[2],
            response.body[3],
        ]);
        
        assert_eq!(
            body_correlation_id,
            correlation_id,
            "Correlation ID not in response body"
        );
    }
}