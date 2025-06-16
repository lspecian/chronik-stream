//! Tests for the Kafka protocol implementation.

use bytes::{Bytes, BytesMut, BufMut};
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