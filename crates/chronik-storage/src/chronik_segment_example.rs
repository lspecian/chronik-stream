//! Example usage of the Chronik Segment Format implementation
//!
//! This module demonstrates how to use the ChronikSegment and ChronikSegmentBuilder
//! for creating, writing, and reading segments.

#[cfg(test)]
mod examples {
    use super::super::{RecordBatch, Record, chronik_segment::*};
    use std::io::Cursor;
    use std::collections::HashMap;

    #[test]
    fn example_basic_usage() {
        // Create some sample records
        let mut headers = HashMap::new();
        headers.insert("content-type".to_string(), b"application/json".to_vec());

        let records = vec![
            Record {
                offset: 100,
                timestamp: 1640995200000, // 2022-01-01T00:00:00Z
                key: Some(b"user:1".to_vec()),
                value: br#"{"user_id": 1, "action": "login"}"#.to_vec(),
                headers: headers.clone(),
            },
            Record {
                offset: 101,
                timestamp: 1640995260000, // 2022-01-01T00:01:00Z
                key: Some(b"user:1".to_vec()),
                value: br#"{"user_id": 1, "action": "view_page", "page": "/dashboard"}"#.to_vec(),
                headers: headers.clone(),
            },
            Record {
                offset: 102,
                timestamp: 1640995320000, // 2022-01-01T00:02:00Z
                key: Some(b"user:2".to_vec()),
                value: br#"{"user_id": 2, "action": "login"}"#.to_vec(),
                headers,
            },
        ];

        let batch = RecordBatch { records };

        // Create a segment using the builder pattern
        let segment = ChronikSegmentBuilder::new("user-events".to_string(), 0)
            .add_batch(batch)
            .compression(CompressionType::Gzip)
            .with_index(true)
            .with_bloom_filter(true)
            .build()
            .expect("Failed to build segment");

        println!("Created segment with {} records", segment.metadata().record_count);

        // Test bloom filter
        assert!(segment.might_contain_key(b"user:1"));
        assert!(segment.might_contain_key(b"user:2"));
    }

    #[test]
    fn example_serialization_and_reading() {
        // Create a segment with test data
        let batch = RecordBatch {
            records: vec![
                Record {
                    offset: 0,
                    timestamp: 1000,
                    key: Some(b"key1".to_vec()),
                    value: b"value1".to_vec(),
                    headers: HashMap::new(),
                },
                Record {
                    offset: 1,
                    timestamp: 2000,
                    key: Some(b"key2".to_vec()),
                    value: b"value2".to_vec(),
                    headers: HashMap::new(),
                },
            ],
        };

        let mut segment = ChronikSegment::new(
            "test-topic".to_string(),
            0,
            vec![batch],
        ).expect("Failed to create segment");

        // Serialize the segment to a buffer
        let mut buffer = Cursor::new(Vec::new());
        segment.write_to(&mut buffer).expect("Failed to write segment");

        println!("Serialized segment size: {} bytes", buffer.get_ref().len());
        println!("Compression ratio: {:.1}%", segment.metadata().compression_ratio * 100.0);

        // Read the segment back
        buffer.set_position(0);
        let loaded_segment = ChronikSegment::read_from(&mut buffer)
            .expect("Failed to read segment");

        assert_eq!(loaded_segment.metadata().topic, "test-topic");
        assert_eq!(loaded_segment.metadata().record_count, 2);
        assert_eq!(loaded_segment.kafka_data().len(), 1);
    }

    #[test]
    fn example_offset_range_reading() {
        // Create a segment with multiple records
        let records: Vec<Record> = (0..10).map(|i| Record {
            offset: i,
            timestamp: 1000 + i * 100,
            key: Some(format!("key{}", i).into_bytes()),
            value: format!("value{}", i).into_bytes(),
            headers: HashMap::new(),
        }).collect();

        let batch = RecordBatch { records };
        let mut segment = ChronikSegment::new(
            "range-test".to_string(),
            0,
            vec![batch],
        ).expect("Failed to create segment");

        // Serialize the segment
        let mut buffer = Cursor::new(Vec::new());
        segment.write_to(&mut buffer).expect("Failed to write segment");

        // Read specific offset ranges
        buffer.set_position(0);
        let range_3_to_6 = ChronikSegment::read_offset_range(&mut buffer, 3, 6)
            .expect("Failed to read range");

        assert_eq!(range_3_to_6.len(), 4); // offsets 3, 4, 5, 6
        assert_eq!(range_3_to_6[0].offset, 3);
        assert_eq!(range_3_to_6[3].offset, 6);

        // Read only metadata (efficient for segment discovery)
        buffer.set_position(0);
        let (header, metadata) = ChronikSegment::read_metadata(&mut buffer)
            .expect("Failed to read metadata");

        println!("Segment spans offsets {} to {}", 
                 metadata.base_offset, metadata.last_offset);
        println!("Total size: {} bytes compressed, {} bytes uncompressed",
                 header.kafka_size, header.kafka_uncompressed_size);
    }

    #[test]
    fn example_compression_comparison() {
        // Create test data with repetitive content (good for compression)
        let records: Vec<Record> = (0..100).map(|i| Record {
            offset: i,
            timestamp: 1640995200000 + i * 1000,
            key: Some(b"session:abc123".to_vec()),
            value: format!(r#"{{"event": "page_view", "session": "abc123", "timestamp": {}, "page": "/home"}}"#, 
                          1640995200000 + i * 1000).into_bytes(),
            headers: HashMap::new(),
        }).collect();

        let batch = RecordBatch { records };

        // Test with compression
        let mut compressed_segment = ChronikSegment::new_with_compression(
            "compression-test".to_string(),
            0,
            vec![batch.clone()],
            CompressionType::Gzip,
        ).expect("Failed to create compressed segment");

        // Test without compression
        let mut uncompressed_segment = ChronikSegment::new_with_compression(
            "compression-test".to_string(),
            0,
            vec![batch],
            CompressionType::None,
        ).expect("Failed to create uncompressed segment");

        // Serialize both
        let mut compressed_buffer = Cursor::new(Vec::new());
        compressed_segment.write_to(&mut compressed_buffer)
            .expect("Failed to write compressed segment");

        let mut uncompressed_buffer = Cursor::new(Vec::new());
        uncompressed_segment.write_to(&mut uncompressed_buffer)
            .expect("Failed to write uncompressed segment");

        let compressed_size = compressed_buffer.get_ref().len();
        let uncompressed_size = uncompressed_buffer.get_ref().len();
        let compression_ratio = 1.0 - (compressed_size as f64 / uncompressed_size as f64);

        println!("Compressed size: {} bytes", compressed_size);
        println!("Uncompressed size: {} bytes", uncompressed_size);
        println!("Compression ratio: {:.1}%", compression_ratio * 100.0);

        assert!(compressed_size < uncompressed_size);
    }
}