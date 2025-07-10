//! Tests for the Kafka protocol implementation.

use bytes::{Buf, Bytes, BytesMut, BufMut};
use chronik_protocol::{
    compression::{CompressionHandler, CompressionType, MessageBatch},
    frame::KafkaFrameCodec,
    records::{RecordBatch, RecordHeader, RecordAttributes, TimestampType},
    kafka_protocol::{ApiKey, ErrorCode, ProtocolCodec,
        RequestHeader, get_supported_apis,
        TopicMetadata, PartitionMetadata},
};
use tokio_util::codec::{Decoder, Encoder};

/// Test frame codec encoding and decoding
#[test]
fn test_frame_codec() {
    let mut codec = KafkaFrameCodec::new();
    let mut buf = BytesMut::new();
    
    // Create a test message (minimum 14 bytes)
    let mut message = BytesMut::new();
    message.put_i16(3); // API key
    message.put_i16(12); // API version
    message.put_i32(123); // Correlation ID
    message.put_i16(5); // Client ID length
    message.put_slice(b"test1"); // Client ID
    let message = message.freeze();
    
    // Encode
    codec.encode(message.clone(), &mut buf).unwrap();
    assert_eq!(buf.len(), message.len() + 4); // 4 bytes for length prefix
    
    // Decode
    let decoded = codec.decode(&mut buf).unwrap().unwrap();
    assert_eq!(decoded, message);
    assert_eq!(buf.len(), 0); // All consumed
}

/// Test message batching without compression
#[test]
fn test_message_batch_no_compression() {
    let mut batch = MessageBatch::new(CompressionType::None);
    
    // Add messages
    for i in 0..5 {
        let msg = Bytes::from(format!("message-{}", i));
        assert!(batch.add(msg).unwrap());
    }
    
    assert_eq!(batch.len(), 5);
    assert!(!batch.is_empty());
    
    // Build batch
    let result = batch.build().unwrap();
    assert_eq!(result.len(), 45); // Sum of "message-0" through "message-4"
}

/// Test message batching with compression
#[test]
fn test_message_batch_with_compression() {
    let mut batch = MessageBatch::new(CompressionType::Gzip);
    
    // Add compressible messages
    for i in 0..20 {
        let msg = Bytes::from(format!("This is a repetitive message number {} with padding", i));
        assert!(batch.add(msg).unwrap());
    }
    
    let compressed = batch.build().unwrap();
    
    // Build uncompressed version for comparison
    let mut uncompressed_batch = MessageBatch::new(CompressionType::None);
    for i in 0..20 {
        let msg = Bytes::from(format!("This is a repetitive message number {} with padding", i));
        uncompressed_batch.add(msg).unwrap();
    }
    let uncompressed = uncompressed_batch.build().unwrap();
    
    // Compressed should be smaller
    assert!(compressed.len() < uncompressed.len());
}

/// Test compression handler
#[test]
fn test_compression_handler() {
    let data = b"This is test data that should be compressed. ".repeat(10);
    
    // Test gzip
    let compressed = CompressionHandler::compress(&data, CompressionType::Gzip).unwrap();
    assert!(compressed.len() < data.len());
    
    let decompressed = CompressionHandler::decompress(&compressed, CompressionType::Gzip).unwrap();
    assert_eq!(&decompressed[..], &data[..]);
    
    // Test no compression
    let uncompressed = CompressionHandler::compress(&data, CompressionType::None).unwrap();
    assert_eq!(&uncompressed[..], &data[..]);
}

/// Test record batch creation and encoding
#[test]
fn test_record_batch() {
    let mut batch = RecordBatch::new(100, 12345, 1);
    
    // Add records
    batch.add_record(
        Some(Bytes::from("key1")),
        Some(Bytes::from("value1")),
        vec![],
    );
    
    batch.add_record(
        Some(Bytes::from("key2")),
        Some(Bytes::from("value2")),
        vec![RecordHeader {
            key: "header-key".to_string(),
            value: Some(Bytes::from("header-value")),
        }],
    );
    
    // Add a tombstone (null value)
    batch.add_record(
        Some(Bytes::from("key3")),
        None,
        vec![],
    );
    
    assert_eq!(batch.records.len(), 3);
    assert_eq!(batch.last_offset_delta, 2);
    
    // Encode
    let encoded = batch.encode().unwrap();
    assert!(!encoded.is_empty());
    
    // Basic structure validation - encoded batch has complex format
    // First 4 bytes is batch length, then 8 bytes base_offset
    assert!(encoded.len() > 61); // Minimum batch header size
}

/// Test record attributes
#[test]
fn test_record_attributes() {
    // Test encoding
    let attrs = RecordAttributes {
        compression: CompressionType::Gzip,
        timestamp_type: TimestampType::LogAppendTime,
        is_transactional: true,
        is_control_batch: false,
    };
    
    let byte = attrs.to_byte();
    assert_eq!(byte & 0x07, 1); // Gzip = 1
    assert_ne!(byte & 0x08, 0); // LogAppendTime flag set
    assert_ne!(byte & 0x10, 0); // Transactional flag set
    assert_eq!(byte & 0x20, 0); // Control batch flag not set
    
    // Test decoding
    let decoded = RecordAttributes::from_byte(byte);
    assert_eq!(decoded.compression.id(), CompressionType::Gzip.id());
    assert_eq!(decoded.timestamp_type, TimestampType::LogAppendTime);
    assert!(decoded.is_transactional);
    assert!(!decoded.is_control_batch);
}

/// Test API versions response encoding
#[test]
fn test_api_versions_response() {
    let mut buf = BytesMut::new();
    let apis = get_supported_apis();
    
    // Encode with flexible version (v3)
    ProtocolCodec::encode_api_versions_response(&mut buf, 456, &apis, 3).unwrap();
    
    // Verify basic structure
    assert!(!buf.is_empty());
    assert!(buf.len() > 4); // At least header
    
    // Check correlation ID
    let correlation_id = i32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
    assert_eq!(correlation_id, 456);
}

/// Test metadata response encoding
#[test]
fn test_metadata_response() {
    let mut buf = BytesMut::new();
    
    let brokers = vec![
        (1, "broker1.example.com".to_string(), 9092),
        (2, "broker2.example.com".to_string(), 9092),
    ];
    
    let topics = vec![
        TopicMetadata {
            name: "test-topic".to_string(),
            is_internal: false,
            partitions: vec![
                PartitionMetadata {
                    id: 0,
                    leader: 1,
                    replicas: vec![1, 2],
                    isr: vec![1, 2],
                    offline_replicas: vec![],
                },
                PartitionMetadata {
                    id: 1,
                    leader: 2,
                    replicas: vec![1, 2],
                    isr: vec![1, 2],
                    offline_replicas: vec![],
                },
            ],
        },
    ];
    
    // Encode metadata response (v9 - flexible)
    ProtocolCodec::encode_metadata_response(
        &mut buf,
        789,
        "test-cluster",
        1,
        &brokers,
        &topics,
        9,
    ).unwrap();
    
    // Verify basic structure
    assert!(!buf.is_empty());
    
    // Check correlation ID
    let correlation_id = i32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
    assert_eq!(correlation_id, 789);
}

/// Test request header encoding/decoding
#[test]
fn test_request_header() {
    let mut buf = BytesMut::new();
    
    let header = RequestHeader {
        api_key: ApiKey::Metadata,
        api_version: 12,
        correlation_id: 999,
        client_id: Some("my-client".to_string()),
    };
    
    // Encode with flexible version
    header.encode(&mut buf, true);
    
    // Decode
    let mut data = buf.freeze();
    let decoded = RequestHeader::decode(&mut data, true).unwrap();
    
    assert_eq!(decoded.api_key as i16, ApiKey::Metadata as i16);
    assert_eq!(decoded.api_version, 12);
    assert_eq!(decoded.correlation_id, 999);
    assert_eq!(decoded.client_id, Some("my-client".to_string()));
}

/// Test large frame handling
#[test]
fn test_large_frame() {
    let mut codec = KafkaFrameCodec::new();
    let mut buf = BytesMut::new();
    
    // Create a large message (1MB)
    let large_message = Bytes::from(vec![0xAB; 1024 * 1024]);
    
    // Encode
    codec.encode(large_message.clone(), &mut buf).unwrap();
    assert_eq!(buf.len(), large_message.len() + 4);
    
    // Decode
    let decoded = codec.decode(&mut buf).unwrap().unwrap();
    assert_eq!(decoded.len(), large_message.len());
}

/// Test error codes
#[test]
fn test_error_codes() {
    assert_eq!(ErrorCode::None.code(), 0);
    assert_eq!(ErrorCode::UnknownTopicOrPartition.code(), 3);
    assert_eq!(ErrorCode::UnsupportedVersion.code(), 35);
    assert_eq!(ErrorCode::TopicAlreadyExists.code(), 36);
}

/// Test API key mapping
#[test]
fn test_api_key_mapping() {
    assert_eq!(ApiKey::from_i16(0), Some(ApiKey::Produce));
    assert_eq!(ApiKey::from_i16(1), Some(ApiKey::Fetch));
    assert_eq!(ApiKey::from_i16(3), Some(ApiKey::Metadata));
    assert_eq!(ApiKey::from_i16(18), Some(ApiKey::ApiVersions));
    assert_eq!(ApiKey::from_i16(999), None); // Invalid
}

/// Test ApiVersions v0 field ordering
#[test]
fn test_api_versions_v0_field_ordering() {
    let mut buf = BytesMut::new();
    let apis = get_supported_apis();
    
    // Encode with v0 - field order should be: api_versions array, then error_code
    ProtocolCodec::encode_api_versions_response(&mut buf, 123, &apis, 0).unwrap();
    
    // Skip correlation ID (4 bytes)
    let mut data = &buf[4..];
    
    // First should be array length (4 bytes)
    let array_len = i32::from_be_bytes([data[0], data[1], data[2], data[3]]);
    assert!(array_len > 0, "API versions array should not be empty");
    
    // Calculate where error_code should be
    // Each API version entry is 6 bytes (api_key: 2, min_version: 2, max_version: 2)
    let error_code_offset = 4 + (array_len as usize * 6);
    
    // Verify we have enough data
    assert!(data.len() >= error_code_offset + 2, "Response too short");
    
    // Error code should be after the array
    let error_code = i16::from_be_bytes([data[error_code_offset], data[error_code_offset + 1]]);
    assert_eq!(error_code, 0, "Error code should be 0");
}

/// Test ApiVersions v1+ field ordering
#[test]
fn test_api_versions_v1_field_ordering() {
    let mut buf = BytesMut::new();
    let apis = get_supported_apis();
    
    // Encode with v1 - field order should be: error_code, then api_versions array
    ProtocolCodec::encode_api_versions_response(&mut buf, 456, &apis, 1).unwrap();
    
    // Skip correlation ID (4 bytes)
    let mut data = &buf[4..];
    
    // First should be error_code (2 bytes)
    let error_code = i16::from_be_bytes([data[0], data[1]]);
    assert_eq!(error_code, 0, "Error code should be 0");
    
    // Then array length (4 bytes)
    let array_len = i32::from_be_bytes([data[2], data[3], data[4], data[5]]);
    assert!(array_len > 0, "API versions array should not be empty");
    
    // Then throttle_time_ms (4 bytes) for v1+
    let throttle_offset = 6 + (array_len as usize * 6);
    assert!(data.len() >= throttle_offset + 4, "Response too short for throttle_time_ms");
    let throttle_time = i32::from_be_bytes([
        data[throttle_offset], 
        data[throttle_offset + 1], 
        data[throttle_offset + 2], 
        data[throttle_offset + 3]
    ]);
    assert_eq!(throttle_time, 0, "Throttle time should be 0");
}

/// Test DescribeConfigs request/response
#[tokio::test]
async fn test_describe_configs() {
    use chronik_protocol::handler::ProtocolHandler;
    use bytes::BytesMut;
    
    let handler = ProtocolHandler::new();
    
    // Build a DescribeConfigs request (v0)
    let mut request_buf = BytesMut::new();
    
    // Request header
    request_buf.put_i16(32); // ApiKey::DescribeConfigs
    request_buf.put_i16(0);  // Version 0
    request_buf.put_i32(999); // Correlation ID
    request_buf.put_i16(11); // Client ID length (corrected from 10 to 11)
    request_buf.put_slice(b"test-client"); // 11 bytes
    
    // Request body
    request_buf.put_i32(1); // 1 resource
    request_buf.put_i8(2);  // Resource type: TOPIC
    request_buf.put_i16(10); // Resource name length
    request_buf.put_slice(b"test-topic");
    // No configuration keys in v0
    
    // Add length prefix
    let mut final_buf = BytesMut::new();
    final_buf.put_i32(request_buf.len() as i32);
    final_buf.extend_from_slice(&request_buf);
    
    // Handle request - skip the length prefix (first 4 bytes)
    let response = handler.handle_request(&final_buf[4..]).await.unwrap();
    
    // Verify response
    assert_eq!(response.header.correlation_id, 999);
    assert!(!response.body.is_empty());
    
    // Parse response body
    let mut body = response.body.clone();
    
    // Skip correlation ID (already in header)
    let corr_id = body.get_i32();
    assert_eq!(corr_id, 999);
    
    // Throttle time
    let throttle_time = body.get_i32();
    assert_eq!(throttle_time, 0);
    
    // Results array
    let result_count = body.get_i32();
    assert_eq!(result_count, 1);
    
    // First result
    let error_code = body.get_i16();
    assert_eq!(error_code, 0); // No error
    
    // Error message (null)
    let msg_len = body.get_i16();
    assert_eq!(msg_len, -1);
    
    // Resource type
    let resource_type = body.get_i8();
    assert_eq!(resource_type, 2); // TOPIC
    
    // Resource name
    let name_len = body.get_i16() as usize;
    assert_eq!(name_len, 10);
    let mut name_bytes = vec![0u8; name_len];
    body.copy_to_slice(&mut name_bytes);
    assert_eq!(&name_bytes, b"test-topic");
    
    // Configs array
    let config_count = body.get_i32();
    assert!(config_count > 0); // Should have some configs
    
    // Check first config
    let config_name_len = body.get_i16() as usize;
    assert!(config_name_len > 0);
    let mut config_name = vec![0u8; config_name_len];
    body.copy_to_slice(&mut config_name);
    let config_name_str = String::from_utf8(config_name).unwrap();
    
    // Should be one of our default configs
    assert!(["retention.ms", "segment.ms", "segment.bytes", "min.insync.replicas", 
             "compression.type", "cleanup.policy", "max.message.bytes"]
        .contains(&config_name_str.as_str()));
}