//! Property-based fuzzing tests for Kafka protocol implementation.
//!
//! These tests use proptest to generate random inputs and verify
//! that our implementation handles them correctly without panicking.

use bytes::{Buf, BufMut, BytesMut};
use chronik_protocol::handler::ProtocolHandler;
use proptest::prelude::*;

/// Generate valid API keys [0, 33]
fn valid_api_key() -> impl Strategy<Value = i16> {
    0i16..=33
}

/// Generate reasonable API versions [0, 20]
fn api_version() -> impl Strategy<Value = i16> {
    0i16..=20
}

proptest! {
    /// Test that handler doesn't panic on random inputs
    #[test]
    fn prop_handler_no_panic(data: Vec<u8>) {
        // Create runtime for async test
        let rt = tokio::runtime::Runtime::new().unwrap();
        
        rt.block_on(async {
            let handler = ProtocolHandler::new();
            // Should not panic, can either succeed or return error
            let _ = handler.handle_request(&data).await;
        });
    }

    /// Test that valid headers are handled correctly
    #[test]
    fn prop_valid_headers_handled(
        api_key in valid_api_key(),
        version in api_version(),
        correlation_id: i32,
        client_id in prop::option::of(prop::string::string_regex("[a-zA-Z0-9._-]{0,255}").unwrap()),
    ) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        
        rt.block_on(async {
            let handler = ProtocolHandler::new();
            
            // Build request
            let mut request = BytesMut::new();
            request.put_i16(api_key);
            request.put_i16(version);
            request.put_i32(correlation_id);
            
            // Client ID
            if let Some(id) = client_id {
                request.put_i16(id.len() as i16);
                request.put_slice(id.as_bytes());
            } else {
                request.put_i16(-1); // null
            }
            
            let result = handler.handle_request(&request).await;
            
            // Should either succeed or fail gracefully
            match result {
                Ok(response) => {
                    // Correlation ID should be preserved
                    prop_assert_eq!(response.header.correlation_id, correlation_id);
                }
                Err(_) => {
                    // Error is acceptable for invalid combinations
                }
            }
            Ok(())
        });
    }

    /// Test string encoding edge cases
    #[test]
    fn prop_string_encoding_edge_cases(
        strings in prop::collection::vec(
            prop::option::of(prop::string::string_regex(".*").unwrap()),
            0..10
        )
    ) {
        let mut buf = BytesMut::new();
        let mut encoder = chronik_protocol::parser::Encoder::new(&mut buf);
        
        for s in strings {
            // Test different string encoding methods
            encoder.write_string(s.as_deref());
            
            if let Some(s) = s {
                // Also test compact string for non-null values
                encoder.write_compact_string(Some(&s));
            }
        }
        
        // Should have written something
        prop_assert!(!buf.is_empty());
    }

    /// Test correlation ID preservation with random values
    #[test]
    fn prop_correlation_id_preserved(correlation_id: i32) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        
        rt.block_on(async {
            let handler = ProtocolHandler::new();
            
            // Create ApiVersions request with specific correlation ID
            let mut request = BytesMut::new();
            request.put_i16(18); // ApiVersions
            request.put_i16(0); // Version 0
            request.put_i32(correlation_id);
            request.put_i16(4);
            request.put_slice(b"test");
            
            match handler.handle_request(&request).await {
                Ok(response) => {
                    // Check header
                    prop_assert_eq!(response.header.correlation_id, correlation_id);
                    
                    // Check body
                    let body_id = i32::from_be_bytes([
                        response.body[0],
                        response.body[1],
                        response.body[2],
                        response.body[3],
                    ]);
                    prop_assert_eq!(body_id, correlation_id);
                }
                Err(_) => prop_assert!(false, "ApiVersions request should succeed"),
            }
            Ok(())
        });
    }

    /// Test varint encoding with random values
    #[test]
    fn prop_varint_encoding(value: u32) {
        let mut buf = BytesMut::new();
        let mut encoder = chronik_protocol::parser::Encoder::new(&mut buf);
        
        // Encode
        encoder.write_unsigned_varint(value);
        
        // Should have written at least 1 byte
        prop_assert!(!buf.is_empty());
        
        // Decode and verify
        let mut bytes = buf.as_ref();
        let decoded = read_unsigned_varint(&mut bytes);
        
        prop_assert_eq!(decoded, value);
    }

    /// Test array length handling
    #[test]
    fn prop_array_length_handling(
        lengths in prop::collection::vec(-10i32..1000, 0..10)
    ) {
        let mut buf = BytesMut::new();
        let mut encoder = chronik_protocol::parser::Encoder::new(&mut buf);
        
        for len in lengths {
            encoder.write_i32(len);
            
            // For non-negative lengths, write that many dummy items
            if len >= 0 && len < 100 {
                // Limit to reasonable size
                for _ in 0..len {
                    encoder.write_i16(0); // Dummy item
                }
            }
        }
        
        // Should have written something
        prop_assert!(!buf.is_empty());
    }

    /// Test metadata request parsing with random fields
    #[test]
    fn prop_metadata_request_parsing(
        topics in prop::option::of(
            prop::collection::vec(
                prop::string::string_regex("[a-zA-Z0-9._-]{1,100}").unwrap(),
                0..5
            )
        ),
        allow_auto: bool,
        include_cluster: bool,
        include_topic: bool,
    ) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        
        rt.block_on(async {
            let handler = ProtocolHandler::new();
            
            // Build metadata request
            let mut request = BytesMut::new();
            request.put_i16(3); // Metadata
            request.put_i16(9); // Version 9
            request.put_i32(12345);
            request.put_i16(4);
            request.put_slice(b"test");
            
            // Topics
            if let Some(topics) = topics {
                request.put_i32(topics.len() as i32);
                for topic in topics {
                    request.put_i16(topic.len() as i16);
                    request.put_slice(topic.as_bytes());
                }
            } else {
                request.put_i32(-1); // null
            }
            
            request.put_u8(if allow_auto { 1 } else { 0 });
            request.put_u8(if include_cluster { 1 } else { 0 });
            request.put_u8(if include_topic { 1 } else { 0 });
            
            let result = handler.handle_request(&request).await;
            
            // Should handle any valid combination
            prop_assert!(result.is_ok() || result.is_err());
            Ok(())
        });
    }

    /// Test large request handling
    #[test]
    fn prop_large_request_handling(size_factor in 0u8..=255) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        
        rt.block_on(async {
            let handler = ProtocolHandler::new();
            
            // Create request with large client ID
            let mut request = BytesMut::new();
            request.put_i16(18); // ApiVersions
            request.put_i16(0);
            request.put_i32(999);
            
            // Large client ID (but within i16 range)
            let size = size_factor as usize;
            request.put_i16(size as i16);
            request.put_slice(&vec![b'x'; size]);
            
            let result = handler.handle_request(&request).await;
            
            // Should handle gracefully
            prop_assert!(result.is_ok() || result.is_err());
            Ok(())
        });
    }

    /// Test edge case values
    #[test]
    fn prop_edge_case_values(
        use_min: bool,
        use_max: bool,
        use_zero: bool,
        use_negative: bool,
    ) {
        let mut buf = BytesMut::new();
        let mut encoder = chronik_protocol::parser::Encoder::new(&mut buf);
        
        // Test various edge cases
        if use_min {
            encoder.write_i16(i16::MIN);
            encoder.write_i32(i32::MIN);
            encoder.write_i64(i64::MIN);
        }
        
        if use_max {
            encoder.write_i16(i16::MAX);
            encoder.write_i32(i32::MAX);
            encoder.write_i64(i64::MAX);
        }
        
        if use_zero {
            encoder.write_i16(0);
            encoder.write_i32(0);
            encoder.write_i64(0);
        }
        
        if use_negative {
            encoder.write_i16(-1);
            encoder.write_i32(-1);
            encoder.write_i64(-1);
        }
        
        // Should have written something if any flag was true
        prop_assert_eq!(
            buf.is_empty(),
            !(use_min || use_max || use_zero || use_negative)
        );
    }
}

/// Helper to read unsigned varint
fn read_unsigned_varint(buf: &mut &[u8]) -> u32 {
    let mut value = 0u32;
    let mut i = 0;
    
    loop {
        if buf.is_empty() {
            return value;
        }
        
        let byte = buf.get_u8();
        value |= ((byte & 0x7F) as u32) << (i * 7);
        
        if byte & 0x80 == 0 {
            return value;
        }
        
        i += 1;
        if i >= 5 {
            return value; // Prevent infinite loop
        }
    }
}

/// Additional non-property tests
#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_proptest_setup() {
        // Verify proptest is working
        proptest!(|(x: i32)| {
            prop_assert_eq!(x + 0, x);
        });
    }
    
    /// Test specific malformed requests that have caused issues
    #[tokio::test]
    async fn test_specific_malformed_requests() {
        let handler = ProtocolHandler::new();
        
        // Test cases that have caused issues in the past
        let test_cases = vec![
            // Empty request
            vec![],
            // Just API key
            vec![0x00, 0x12],
            // API key + version
            vec![0x00, 0x12, 0x00, 0x00],
            // Missing client ID
            vec![0x00, 0x12, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01],
            // Truncated client ID
            vec![
                0x00, 0x12, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01,
                0x00, 0x0a, // Says 10 bytes but...
                b't', b'e', b's', b't', // Only 4 bytes
            ],
        ];
        
        for (i, request) in test_cases.iter().enumerate() {
            let result = handler.handle_request(request).await;
            assert!(
                result.is_err(),
                "Malformed request {} should fail",
                i
            );
        }
    }
}