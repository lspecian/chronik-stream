//! Protocol conformance tests to ensure compatibility with official Kafka clients.

use bytes::{Bytes, BytesMut, BufMut, Buf};
use chronik_protocol::{
    kafka_protocol::{ApiKey, RequestHeader, ProtocolCodec, TopicMetadata, PartitionMetadata, get_supported_apis},
    compression::{CompressionType, MessageBatch},
    records::{RecordBatch, RecordHeader},
};

/// Test basic protocol parsing
#[test]
fn test_protocol_parser_api_versions() {
    // Create an ApiVersions request header
    let mut data = BytesMut::new();
    
    // Encode request header manually
    data.put_i16(18); // API Key: ApiVersions
    data.put_i16(3);  // API Version: 3
    data.put_i32(123); // Correlation ID: 123
    
    // For flexible version (v3), use compact string
    data.put_u8(12); // Compact string length + 1 = 12 for "test-client"
    data.put_slice(b"test-client"); // Client ID
    
    // Tagged fields
    data.put_u8(0); // Empty tagged fields
    
    // Decode the header
    let mut buf = data.freeze();
    let header = RequestHeader::decode(&mut buf, true).unwrap();
    assert_eq!(header.api_key, ApiKey::ApiVersions);
    assert_eq!(header.api_version, 3);
    assert_eq!(header.correlation_id, 123);
    assert_eq!(header.client_id, Some("test-client".to_string()));
}

/// Test metadata request/response
#[test]
fn test_metadata_request_response() {
    let mut buf = BytesMut::new();
    
    // Create topic metadata
    let topics = vec![
        TopicMetadata {
            name: "test-topic".to_string(),
            is_internal: false,
            partitions: vec![
                PartitionMetadata {
                    id: 0,
                    leader: 1,
                    replicas: vec![1, 2, 3],
                    isr: vec![1, 2, 3],
                    offline_replicas: vec![],
                },
            ],
        },
    ];
    
    // Encode metadata response
    ProtocolCodec::encode_metadata_response(
        &mut buf,
        456, // correlation_id
        "test-cluster",
        1, // controller_id
        &[(1, "localhost".to_string(), 9092)], // brokers
        &topics,
        12, // version
    ).unwrap();
    
    assert!(!buf.is_empty());
    
    // The response should have a proper header
    let mut response = buf.freeze();
    let correlation_id = response.get_i32();
    assert_eq!(correlation_id, 456);
}

/// Test message batching
#[test]
fn test_message_batching() {
    let mut batch = MessageBatch::new(CompressionType::None);
    
    // Add messages
    for i in 0..10 {
        let msg = Bytes::from(format!("message-{}", i));
        assert!(batch.add(msg).unwrap());
    }
    
    assert_eq!(batch.len(), 10);
    assert!(!batch.is_empty());
    
    // Build batch
    let result = batch.build().unwrap();
    assert!(!result.is_empty());
}

/// Test message compression
#[test]
fn test_message_compression() {
    use chronik_protocol::CompressionHandler;
    
    let data = b"This is a test message that will be compressed. ".repeat(10);
    
    // Test gzip compression
    let compressed = CompressionHandler::compress(&data, CompressionType::Gzip).unwrap();
    assert!(compressed.len() < data.len()); // Should be smaller
    
    let decompressed = CompressionHandler::decompress(&compressed, CompressionType::Gzip).unwrap();
    assert_eq!(&decompressed[..], &data[..]);
}

/// Test record batch encoding/decoding
#[test]
fn test_record_batch() {
    let mut batch = RecordBatch::new(1000, 12345, 1);
    
    // Add records with various combinations
    batch.add_record(
        Some(Bytes::from("key1")),
        Some(Bytes::from("value1")),
        vec![],
    );
    
    batch.add_record(
        None, // No key
        Some(Bytes::from("value2")),
        vec![RecordHeader {
            key: "trace-id".to_string(),
            value: Some(Bytes::from("abc123")),
        }],
    );
    
    batch.add_record(
        Some(Bytes::from("key3")),
        None, // No value (tombstone)
        vec![],
    );
    
    // Encode
    let encoded = batch.encode().unwrap();
    assert!(!encoded.is_empty());

    // encode() removes its internal 4-byte scratch padding before returning
    // (`buf.split_off(4)`), so the bytes already start directly with base_offset,
    // exactly as a Kafka RecordBatch on the wire. Decode them as-is.
    let decoded = RecordBatch::decode(encoded).unwrap();
    assert_eq!(decoded.records.len(), 3);
    assert_eq!(decoded.base_offset, 1000);
    assert_eq!(decoded.producer_id, 12345);
    
    // Verify records
    assert_eq!(decoded.records[0].key.as_ref().unwrap(), &Bytes::from("key1"));
    assert_eq!(decoded.records[0].value.as_ref().unwrap(), &Bytes::from("value1"));
    
    assert!(decoded.records[1].key.is_none());
    assert_eq!(decoded.records[1].headers.len(), 1);
    assert_eq!(decoded.records[1].headers[0].key, "trace-id");
    
    assert!(decoded.records[2].value.is_none()); // Tombstone
}

/// Test API version negotiation
#[test]
fn test_version_negotiation() {
    // Get all supported APIs
    let versions = get_supported_apis();
    assert!(!versions.is_empty());
    
    // Verify key APIs are present
    let produce = versions.iter().find(|v| v.api_key == ApiKey::Produce as i16);
    assert!(produce.is_some());
    let produce = produce.unwrap();
    assert_eq!(produce.min_version, 0);
    assert_eq!(produce.max_version, 9);
    
    let fetch = versions.iter().find(|v| v.api_key == ApiKey::Fetch as i16);
    assert!(fetch.is_some());
    let fetch = fetch.unwrap();
    assert_eq!(fetch.min_version, 0);
    assert_eq!(fetch.max_version, 13);
    
    // Test encoding an API versions response
    let mut buf = BytesMut::new();
    ProtocolCodec::encode_api_versions_response(&mut buf, 789, &versions, 3).unwrap();
    assert!(!buf.is_empty());
}

/// Test batch compression
#[test]
fn test_batch_compression() {
    let mut batch = MessageBatch::new(CompressionType::Gzip);
    
    // Add compressible messages
    for i in 0..100 {
        let msg = Bytes::from(format!("This is message number {} with some repetitive content", i));
        assert!(batch.add(msg).unwrap());
    }
    
    let compressed = batch.build().unwrap();
    
    // Build uncompressed version for comparison
    let mut uncompressed_batch = MessageBatch::new(CompressionType::None);
    for i in 0..100 {
        let msg = Bytes::from(format!("This is message number {} with some repetitive content", i));
        uncompressed_batch.add(msg).unwrap();
    }
    let uncompressed = uncompressed_batch.build().unwrap();
    
    // Compressed should be significantly smaller
    assert!(compressed.len() < uncompressed.len() / 2);
}