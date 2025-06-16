use bytes::Bytes;
use chronik_common::types::{SegmentId, SegmentMetadata, TopicPartition};
use chronik_storage::{Segment, SegmentBuilder};

#[test]
fn test_segment_serialization_roundtrip() {
    // Create test metadata
    let metadata = SegmentMetadata {
        id: SegmentId::new(),
        topic_partition: TopicPartition {
            topic: "test-topic".to_string(),
            partition: 0,
        },
        base_offset: 1000,
        last_offset: 1999,
        timestamp_min: 1700000000000,
        timestamp_max: 1700001000000,
        size_bytes: 1024 * 1024,
        record_count: 1000,
        object_key: "test-key".to_string(),
        created_at: chrono::Utc::now(),
    };
    
    // Build segment
    let mut builder = SegmentBuilder::new()
        .with_metadata(metadata.clone());
    
    // Add some test data
    builder.add_kafka_data(b"test kafka data");
    builder.add_index_data(b"test index data");
    
    let segment = builder.build().expect("Failed to build segment");
    
    // Serialize
    let serialized = segment.serialize().expect("Failed to serialize segment");
    
    // Deserialize
    let deserialized = Segment::deserialize(serialized).expect("Failed to deserialize segment");
    
    // Verify
    assert_eq!(deserialized.metadata.id, metadata.id);
    assert_eq!(deserialized.metadata.topic_partition, metadata.topic_partition);
    assert_eq!(deserialized.metadata.base_offset, metadata.base_offset);
    assert_eq!(deserialized.metadata.last_offset, metadata.last_offset);
    assert_eq!(deserialized.kafka_data, Bytes::from("test kafka data"));
    assert_eq!(deserialized.index_data, Bytes::from("test index data"));
}

#[test]
fn test_segment_storage_key() {
    let metadata = SegmentMetadata {
        id: SegmentId::new(),
        topic_partition: TopicPartition {
            topic: "my-topic".to_string(),
            partition: 42,
        },
        base_offset: 1000,
        last_offset: 1999,
        timestamp_min: 0,
        timestamp_max: 0,
        size_bytes: 0,
        record_count: 0,
        object_key: "".to_string(),
        created_at: chrono::Utc::now(),
    };
    
    let segment = SegmentBuilder::new()
        .with_metadata(metadata)
        .build()
        .unwrap();
    
    let key = segment.storage_key();
    assert_eq!(key, "segments/my-topic/partition-00042/segment-0000000000001000-0000000000001999.chrn");
}