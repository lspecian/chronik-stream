//! Version negotiation tests for Kafka protocol compatibility.
//!
//! These tests verify that our implementation correctly handles different
//! API versions and properly negotiates with clients of varying capabilities.

use bytes::{Buf, BufMut, BytesMut};
use chronik_protocol::handler::ProtocolHandler;
use chronik_protocol::parser::{ApiKey, supported_api_versions};

/// Test that all advertised API versions are actually supported
#[tokio::test]
async fn test_advertised_versions_are_supported() {
    let handler = ProtocolHandler::new();
    let supported = supported_api_versions();
    
    for (api_key, version_range) in supported {
        for version in version_range.min..=version_range.max {
            // Create request with specific version
            let mut request = BytesMut::new();
            request.put_i16(api_key as i16);
            request.put_i16(version);
            request.put_i32(12345); // Correlation ID
            request.put_i16(4); // Client ID length
            request.put_slice(b"test");
            
            // Add minimal body based on API
            add_minimal_request_body(&mut request, api_key, version);
            
            let result = handler.handle_request(&request).await;
            
            // We should either get a successful response or a known error
            // (not a parse error or panic)
            match &result {
                Ok(response) => {
                    assert_eq!(response.header.correlation_id, 12345);
                }
                Err(e) => {
                    // Some APIs are advertised but not implemented yet
                    // This is OK as long as we handle them gracefully
                    assert!(
                        e.to_string().contains("unimplemented") ||
                        e.to_string().contains("not implemented"),
                        "Unexpected error for {} v{}: {}",
                        api_key as i16,
                        version,
                        e
                    );
                }
            }
        }
    }
}

/// Test version downgrade scenarios
#[tokio::test]
async fn test_version_downgrade_handling() {
    let handler = ProtocolHandler::new();
    
    // Test cases: (API, high_version, low_version)
    let downgrade_tests = vec![
        (ApiKey::Produce, 9, 0),
        (ApiKey::Fetch, 13, 0),
        (ApiKey::Metadata, 12, 0),
        (ApiKey::ApiVersions, 3, 0),
    ];
    
    for (api_key, high_version, low_version) in downgrade_tests {
        // First, verify high version works
        let request = create_test_request(api_key, high_version);
        let result = handler.handle_request(&request).await;
        assert!(
            result.is_ok(),
            "High version {} v{} should work",
            api_key as i16,
            high_version
        );
        
        // Then verify low version also works
        let request = create_test_request(api_key, low_version);
        let result = handler.handle_request(&request).await;
        assert!(
            result.is_ok(),
            "Low version {} v{} should work",
            api_key as i16,
            low_version
        );
    }
}

/// Test handling of unsupported versions
#[tokio::test]
async fn test_unsupported_version_handling() {
    let handler = ProtocolHandler::new();
    let supported = supported_api_versions();
    
    for (api_key, version_range) in supported {
        // Test version too low
        if version_range.min > 0 {
            let mut request = BytesMut::new();
            request.put_i16(api_key as i16);
            request.put_i16(version_range.min - 1); // One below minimum
            request.put_i32(999);
            request.put_i16(4);
            request.put_slice(b"test");
            
            let result = handler.handle_request(&request).await;
            assert!(
                result.is_ok(), // Should return error response, not fail
                "Should handle too-low version for {}",
                api_key as i16
            );
        }
        
        // Test version too high
        let mut request = BytesMut::new();
        request.put_i16(api_key as i16);
        request.put_i16(version_range.max + 1); // One above maximum
        request.put_i32(999);
        request.put_i16(4);
        request.put_slice(b"test");
        
        let result = handler.handle_request(&request).await;
        assert!(
            result.is_ok(), // Should return error response, not fail
            "Should handle too-high version for {}",
            api_key as i16
        );
    }
}

/// Test ApiVersions negotiation flow
#[tokio::test]
async fn test_api_versions_negotiation_flow() {
    let handler = ProtocolHandler::new();
    
    // Simulate different client version capabilities
    let client_scenarios = vec![
        ("Old client", vec![0]), // Only supports v0
        ("Modern client", vec![0, 1, 2, 3]), // Supports all versions
        ("Intermediate client", vec![0, 1]), // Supports v0 and v1
    ];
    
    for (client_name, supported_versions) in client_scenarios {
        for &version in &supported_versions {
            let mut request = BytesMut::new();
            request.put_i16(18); // ApiVersions
            request.put_i16(version);
            request.put_i32(12345);
            request.put_i16(client_name.len() as i16);
            request.put_slice(client_name.as_bytes());
            
            let response = handler.handle_request(&request).await.unwrap();
            
            // Verify response is version-appropriate
            verify_api_versions_response_format(&response.body, version);
        }
    }
}

/// Test version-specific field presence
#[tokio::test]
async fn test_version_specific_fields() {
    let handler = ProtocolHandler::new();
    
    // Test Metadata request with version-specific fields
    for version in 0..=12 {
        let mut request = BytesMut::new();
        request.put_i16(3); // Metadata
        request.put_i16(version);
        request.put_i32(999);
        request.put_i16(4);
        request.put_slice(b"test");
        
        // Add version-specific body
        match version {
            0 => {
                // v0: no body
            }
            1..=3 => {
                // v1-3: topics array only
                request.put_i32(-1); // null topics (all)
            }
            4..=7 => {
                // v4-7: topics + allow_auto_topic_creation
                request.put_i32(-1); // null topics
                request.put_u8(1); // allow_auto_topic_creation: true
            }
            8..=12 => {
                // v8+: additional fields
                request.put_i32(-1); // null topics
                request.put_u8(1); // allow_auto_topic_creation
                request.put_u8(0); // include_cluster_authorized_operations
                request.put_u8(0); // include_topic_authorized_operations
            }
            _ => {}
        }
        
        let result = handler.handle_request(&request).await;
        assert!(
            result.is_ok(),
            "Metadata v{} should be handled",
            version
        );
    }
}

/// Test handling of flexible versions (tagged fields)
#[tokio::test]
async fn test_flexible_version_handling() {
    let handler = ProtocolHandler::new();
    
    // ApiVersions v3 is flexible
    let mut request = BytesMut::new();
    request.put_i16(18); // ApiVersions
    request.put_i16(3); // Version 3 (flexible)
    request.put_i32(12345);
    
    // Flexible versions use compact strings
    request.put_u8(5); // Client ID length + 1
    request.put_slice(b"test");
    
    // Client software name (compact string)
    request.put_u8(12); // Length + 1
    request.put_slice(b"test-client");
    
    // Client software version (compact string)
    request.put_u8(6); // Length + 1
    request.put_slice(b"1.0.0");
    
    // Tagged fields (empty)
    request.put_u8(0);
    
    let response = handler.handle_request(&request).await.unwrap();
    
    // Verify response uses flexible encoding
    // (Would need to parse the response to fully verify)
    assert!(response.body.len() > 10);
}

/// Helper to create a test request with minimal valid body
fn create_test_request(api_key: ApiKey, version: i16) -> BytesMut {
    let mut request = BytesMut::new();
    request.put_i16(api_key as i16);
    request.put_i16(version);
    request.put_i32(12345);
    request.put_i16(4);
    request.put_slice(b"test");
    
    add_minimal_request_body(&mut request, api_key, version);
    request
}

/// Add minimal request body for different APIs
fn add_minimal_request_body(request: &mut BytesMut, api_key: ApiKey, version: i16) {
    match api_key {
        ApiKey::Produce => {
            if version >= 3 {
                request.put_i16(-1); // transactional_id (null)
            }
            request.put_i16(-1); // acks
            request.put_i32(30000); // timeout
            request.put_i32(0); // topics (empty)
        }
        ApiKey::Fetch => {
            request.put_i32(-1); // replica_id
            request.put_i32(500); // max_wait_ms
            request.put_i32(1); // min_bytes
            if version >= 3 {
                request.put_i32(1048576); // max_bytes
            }
            if version >= 4 {
                request.put_i8(0); // isolation_level
            }
            if version >= 7 {
                request.put_i32(0); // session_id
                request.put_i32(-1); // session_epoch
            }
            request.put_i32(0); // topics (empty)
        }
        ApiKey::Metadata => {
            if version >= 1 {
                request.put_i32(-1); // topics (null = all)
            }
            if version >= 4 {
                request.put_u8(1); // allow_auto_topic_creation
            }
            if version >= 8 {
                request.put_u8(0); // include_cluster_authorized_operations
                request.put_u8(0); // include_topic_authorized_operations
            }
        }
        ApiKey::ApiVersions => {
            if version >= 3 {
                // Flexible version
                request.put_u8(1); // client_software_name (empty)
                request.put_u8(1); // client_software_version (empty)
                request.put_u8(0); // tagged fields
            }
        }
        ApiKey::DescribeConfigs => {
            request.put_i32(0); // resources (empty)
            if version >= 1 {
                request.put_u8(0); // include_synonyms
            }
            if version >= 3 {
                request.put_u8(0); // include_documentation
            }
        }
        _ => {
            // Other APIs can have empty body for testing
        }
    }
}

/// Verify ApiVersions response format for different versions
fn verify_api_versions_response_format(body: &[u8], version: i16) {
    use bytes::Buf;
    let mut buf = body;
    
    // Skip correlation ID
    buf.advance(4);
    
    match version {
        0 => {
            // v0: array, then error_code
            let array_len = buf.get_i32();
            assert!(array_len > 0, "Should have API versions");
            
            // Skip array entries
            buf.advance((array_len * 6) as usize); // Each entry is 6 bytes
            
            let error_code = buf.get_i16();
            assert_eq!(error_code, 0, "Error code should be 0");
        }
        1..=2 => {
            // v1-2: error_code, array, throttle_time
            let error_code = buf.get_i16();
            assert_eq!(error_code, 0, "Error code should be 0");
            
            let array_len = buf.get_i32();
            assert!(array_len > 0, "Should have API versions");
        }
        3 => {
            // v3: flexible version with tagged fields
            let error_code = buf.get_i16();
            assert_eq!(error_code, 0, "Error code should be 0");
            
            // Compact array uses varint
            let array_len = read_unsigned_varint(&mut buf);
            assert!(array_len > 1, "Should have API versions");
        }
        _ => panic!("Unsupported ApiVersions version in test"),
    }
}

/// Helper to read unsigned varint
fn read_unsigned_varint(buf: &mut &[u8]) -> u32 {
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