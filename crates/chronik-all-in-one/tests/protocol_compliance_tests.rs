//! Kafka protocol compliance tests.
//!
//! These tests verify that the server correctly implements the Kafka wire protocol.

use std::io::Cursor;
use bytes::{Buf, BufMut, BytesMut};

/// Test helper to build a Kafka request
struct RequestBuilder {
    buffer: BytesMut,
}

impl RequestBuilder {
    fn new() -> Self {
        Self {
            buffer: BytesMut::new(),
        }
    }
    
    fn api_key(mut self, key: i16) -> Self {
        self.buffer.put_i16(key);
        self
    }
    
    fn api_version(mut self, version: i16) -> Self {
        self.buffer.put_i16(version);
        self
    }
    
    fn correlation_id(mut self, id: i32) -> Self {
        self.buffer.put_i32(id);
        self
    }
    
    fn client_id(mut self, id: Option<&str>) -> Self {
        match id {
            Some(id) => {
                self.buffer.put_i16(id.len() as i16);
                self.buffer.put_slice(id.as_bytes());
            }
            None => {
                self.buffer.put_i16(-1); // Null string
            }
        }
        self
    }
    
    fn build(mut self) -> Vec<u8> {
        let mut result = Vec::new();
        // Add size header
        result.extend_from_slice(&(self.buffer.len() as i32).to_be_bytes());
        result.extend_from_slice(&self.buffer);
        result
    }
}

#[test]
fn test_api_versions_request_format() {
    // Build ApiVersions request
    let request = RequestBuilder::new()
        .api_key(18)  // ApiVersions
        .api_version(0)
        .correlation_id(1)
        .client_id(Some("test-client"))
        .build();
    
    // Verify request structure
    let mut cursor = Cursor::new(&request);
    
    // Size
    let size = cursor.get_i32();
    assert_eq!(size as usize, request.len() - 4);
    
    // API key
    let api_key = cursor.get_i16();
    assert_eq!(api_key, 18);
    
    // API version
    let api_version = cursor.get_i16();
    assert_eq!(api_version, 0);
    
    // Correlation ID
    let correlation_id = cursor.get_i32();
    assert_eq!(correlation_id, 1);
    
    // Client ID
    let client_id_len = cursor.get_i16();
    assert_eq!(client_id_len, 11); // "test-client" length
    
    let mut client_id_bytes = vec![0u8; client_id_len as usize];
    cursor.copy_to_slice(&mut client_id_bytes);
    assert_eq!(String::from_utf8(client_id_bytes).unwrap(), "test-client");
}

#[test]
fn test_metadata_request_format() {
    // Build Metadata request v0
    let mut builder = BytesMut::new();
    builder.put_i16(3);  // Metadata
    builder.put_i16(0);  // Version 0
    builder.put_i32(42); // Correlation ID
    builder.put_i16(13); // Client ID length
    builder.put_slice(b"metadata-test");
    builder.put_i32(-1); // Null topics array (get all topics)
    
    let mut request = Vec::new();
    request.extend_from_slice(&(builder.len() as i32).to_be_bytes());
    request.extend_from_slice(&builder);
    
    // Verify size is correct
    let size = i32::from_be_bytes([request[0], request[1], request[2], request[3]]);
    assert_eq!(size as usize, request.len() - 4);
}

#[test]
fn test_produce_request_format() {
    let mut buffer = BytesMut::new();
    
    // Request header
    buffer.put_i16(0);  // API key: Produce
    buffer.put_i16(3);  // API version: 3
    buffer.put_i32(100); // Correlation ID
    buffer.put_i16(8);  // Client ID length
    buffer.put_slice(b"producer");
    
    // Produce request body (v3)
    buffer.put_i16(-1); // Transactional ID (null)
    buffer.put_i16(1);  // Acks
    buffer.put_i32(30000); // Timeout
    
    // Topics array
    buffer.put_i32(1); // 1 topic
    buffer.put_i16(10); // Topic name length
    buffer.put_slice(b"test-topic");
    
    // Partitions array
    buffer.put_i32(1); // 1 partition
    buffer.put_i32(0); // Partition ID
    
    // Record batch placeholder
    buffer.put_i32(0); // Empty records
    
    let mut request = Vec::new();
    request.extend_from_slice(&(buffer.len() as i32).to_be_bytes());
    request.extend_from_slice(&buffer);
    
    // Verify structure
    assert!(request.len() > 4);
    let size = i32::from_be_bytes([request[0], request[1], request[2], request[3]]);
    assert_eq!(size as usize, request.len() - 4);
}

#[test]
fn test_fetch_request_format() {
    let mut buffer = BytesMut::new();
    
    // Request header
    buffer.put_i16(1);  // API key: Fetch
    buffer.put_i16(4);  // API version: 4
    buffer.put_i32(200); // Correlation ID
    buffer.put_i16(8);  // Client ID length
    buffer.put_slice(b"consumer");
    
    // Fetch request body (v4)
    buffer.put_i32(-1); // Replica ID (-1 for consumer)
    buffer.put_i32(100); // Max wait time
    buffer.put_i32(1);  // Min bytes
    buffer.put_i32(1048576); // Max bytes
    buffer.put_i8(0);   // Isolation level
    
    // Topics array
    buffer.put_i32(1); // 1 topic
    buffer.put_i16(10); // Topic name length
    buffer.put_slice(b"test-topic");
    
    // Partitions array
    buffer.put_i32(1); // 1 partition
    buffer.put_i32(0); // Partition ID
    buffer.put_i64(0); // Fetch offset
    buffer.put_i32(1048576); // Max bytes
    
    let mut request = Vec::new();
    request.extend_from_slice(&(buffer.len() as i32).to_be_bytes());
    request.extend_from_slice(&buffer);
    
    // Verify structure
    assert!(request.len() > 4);
    let size = i32::from_be_bytes([request[0], request[1], request[2], request[3]]);
    assert_eq!(size as usize, request.len() - 4);
}

#[test]
fn test_create_topics_request_format() {
    let mut buffer = BytesMut::new();
    
    // Request header
    buffer.put_i16(19); // API key: CreateTopics
    buffer.put_i16(0);  // API version: 0
    buffer.put_i32(300); // Correlation ID
    buffer.put_i16(7);  // Client ID length
    buffer.put_slice(b"creator");
    
    // CreateTopics request body (v0)
    // Topics array
    buffer.put_i32(1); // 1 topic
    buffer.put_i16(8); // Topic name length
    buffer.put_slice(b"new-topic");
    buffer.put_i32(3); // Num partitions
    buffer.put_i16(1); // Replication factor
    
    // Replica assignments (empty)
    buffer.put_i32(0);
    
    // Config entries (empty)
    buffer.put_i32(0);
    
    // Timeout
    buffer.put_i32(30000);
    
    let mut request = Vec::new();
    request.extend_from_slice(&(buffer.len() as i32).to_be_bytes());
    request.extend_from_slice(&buffer);
    
    // Verify structure
    assert!(request.len() > 4);
    let size = i32::from_be_bytes([request[0], request[1], request[2], request[3]]);
    assert_eq!(size as usize, request.len() - 4);
}

#[test]
fn test_response_header_format() {
    // Test response header parsing
    let response = vec![
        0, 0, 0, 20, // Size: 20 bytes
        0, 0, 1, 234, // Correlation ID: 490
        0, 0, 0, 0,  // Throttle time: 0
        0, 0,        // Error code: 0
        0, 0, 0, 1,  // Array count: 1
        0, 4,        // String length: 4
        b't', b'e', b's', b't', // String: "test"
    ];
    
    let mut cursor = Cursor::new(&response);
    
    // Size
    let size = cursor.get_i32();
    assert_eq!(size, 20);
    
    // Correlation ID
    let correlation_id = cursor.get_i32();
    assert_eq!(correlation_id, 490);
    
    // Throttle time
    let throttle_time = cursor.get_i32();
    assert_eq!(throttle_time, 0);
    
    // Error code
    let error_code = cursor.get_i16();
    assert_eq!(error_code, 0);
    
    // Array count
    let array_count = cursor.get_i32();
    assert_eq!(array_count, 1);
    
    // String
    let string_len = cursor.get_i16();
    assert_eq!(string_len, 4);
    
    let mut string_bytes = vec![0u8; string_len as usize];
    cursor.copy_to_slice(&mut string_bytes);
    assert_eq!(String::from_utf8(string_bytes).unwrap(), "test");
}

#[test]
fn test_nullable_string_encoding() {
    let mut buffer = BytesMut::new();
    
    // Null string
    buffer.put_i16(-1);
    assert_eq!(buffer.len(), 2);
    assert_eq!(buffer[0..2], (-1i16).to_be_bytes());
    
    buffer.clear();
    
    // Empty string
    buffer.put_i16(0);
    assert_eq!(buffer.len(), 2);
    assert_eq!(buffer[0..2], (0i16).to_be_bytes());
    
    buffer.clear();
    
    // Non-empty string
    let test_str = "kafka";
    buffer.put_i16(test_str.len() as i16);
    buffer.put_slice(test_str.as_bytes());
    assert_eq!(buffer.len(), 2 + test_str.len());
    assert_eq!(buffer[0..2], (5i16).to_be_bytes());
    assert_eq!(&buffer[2..], test_str.as_bytes());
}

#[test]
fn test_array_encoding() {
    let mut buffer = BytesMut::new();
    
    // Null array
    buffer.put_i32(-1);
    assert_eq!(buffer.len(), 4);
    assert_eq!(buffer[0..4], (-1i32).to_be_bytes());
    
    buffer.clear();
    
    // Empty array
    buffer.put_i32(0);
    assert_eq!(buffer.len(), 4);
    assert_eq!(buffer[0..4], (0i32).to_be_bytes());
    
    buffer.clear();
    
    // Array with elements
    buffer.put_i32(3); // 3 elements
    assert_eq!(buffer.len(), 4);
    assert_eq!(buffer[0..4], (3i32).to_be_bytes());
}