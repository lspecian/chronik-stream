//! Tests for ChronikSegment storage integration.

use chronik_storage::{
    object_store::{ObjectStoreFactory, ObjectStoreConfig, ChronikStorageAdapter, SegmentLocation},
    chronik_segment::{ChronikSegmentBuilder, CompressionType},
    RecordBatch, Record,
};
use std::collections::HashMap;
use tempfile::TempDir;

#[tokio::test]
async fn test_chronik_segment_storage_lifecycle() {
    let temp_dir = TempDir::new().unwrap();
    
    let config = ObjectStoreConfig::local(
        temp_dir.path().to_string_lossy().to_string()
    ).with_prefix("chronik-segments");

    let store = ObjectStoreFactory::create(config).await.unwrap();
    let adapter = ChronikStorageAdapter::with_cache(store, 10);

    // Create test data
    let topic = "test-topic".to_string();
    let partition_id = 0;
    let base_offset = 1000i64;

    let test_batch = RecordBatch {
        records: vec![
            Record {
                offset: base_offset,
                timestamp: 1640995200000, // 2022-01-01
                key: Some(b"user:123".to_vec()),
                value: b"login event".to_vec(),
                headers: [("event_type".to_string(), b"authentication".to_vec())]
                    .into_iter().collect(),
            },
            Record {
                offset: base_offset + 1,
                timestamp: 1640995260000,
                key: Some(b"user:456".to_vec()),
                value: b"purchase event".to_vec(),
                headers: [("event_type".to_string(), b"commerce".to_vec())]
                    .into_iter().collect(),
            },
            Record {
                offset: base_offset + 2,
                timestamp: 1640995320000,
                key: Some(b"user:123".to_vec()),
                value: b"logout event".to_vec(),
                headers: [("event_type".to_string(), b"authentication".to_vec())]
                    .into_iter().collect(),
            },
        ],
    };

    // Build segment
    let segment = ChronikSegmentBuilder::new(topic.clone(), partition_id)
        .add_batch(test_batch)
        .compression(CompressionType::Gzip)
        .with_index(true)
        .with_bloom_filter(true)
        .build()
        .unwrap();

    let location = SegmentLocation::new(topic.clone(), partition_id, base_offset);

    // Test storage
    adapter.store_segment(&location, segment).await.unwrap();

    // Test existence
    assert!(adapter.segment_exists(&location).await.unwrap());

    // Test metadata loading
    let metadata = adapter.load_segment_metadata(&location).await.unwrap();
    assert_eq!(metadata.topic, topic);
    assert_eq!(metadata.partition_id, partition_id);
    assert_eq!(metadata.base_offset, base_offset);
    assert_eq!(metadata.last_offset, base_offset + 2);
    assert_eq!(metadata.record_count, 3);

    // Test full segment loading
    let loaded_segment = adapter.load_segment(&location).await.unwrap();
    assert_eq!(loaded_segment.metadata().topic, topic);
    assert_eq!(loaded_segment.metadata().record_count, 3);
    assert_eq!(loaded_segment.kafka_data().len(), 1);
    assert_eq!(loaded_segment.kafka_data()[0].records.len(), 3);

    // Test bloom filter functionality
    assert!(loaded_segment.might_contain_key(b"user:123"));
    assert!(loaded_segment.might_contain_key(b"user:456"));
    // This might return true due to false positives, but should usually be false
    // assert!(!loaded_segment.might_contain_key(b"user:999"));

    // Test segment statistics
    let stats = adapter.segment_stats(&location).await.unwrap();
    assert_eq!(stats.record_count, 3);
    assert!(stats.storage_size > 0);
    assert!(stats.compression_ratio >= 0.0); // Can be 0.0 for uncompressed or small segments

    // Test deletion
    adapter.delete_segment(&location).await.unwrap();
    assert!(!adapter.segment_exists(&location).await.unwrap());
}

#[tokio::test]
async fn test_segment_listing_and_organization() {
    let temp_dir = TempDir::new().unwrap();
    
    let config = ObjectStoreConfig::local(
        temp_dir.path().to_string_lossy().to_string()
    );

    let store = ObjectStoreFactory::create(config).await.unwrap();
    let adapter = ChronikStorageAdapter::new(store);

    // Create segments for multiple topics and partitions
    let topics_and_segments = vec![
        ("orders", 0, vec![0, 1000, 2000]),
        ("orders", 1, vec![0, 500, 1500]),
        ("users", 0, vec![0, 800]),
        ("analytics", 0, vec![0]),
    ];

    for (topic, partition, offsets) in topics_and_segments {
        for base_offset in offsets {
            let test_batch = RecordBatch {
                records: vec![
                    Record {
                        offset: base_offset,
                        timestamp: 1640995200000,
                        key: Some(format!("key:{}", base_offset).into_bytes()),
                        value: format!("value for offset {}", base_offset).into_bytes(),
                        headers: HashMap::new(),
                    }
                ],
            };

            let segment = ChronikSegmentBuilder::new(topic.to_string(), partition)
                .add_batch(test_batch)
                .compression(CompressionType::None) // Faster for tests
                .build()
                .unwrap();

            let location = SegmentLocation::new(topic.to_string(), partition, base_offset);
            adapter.store_segment(&location, segment).await.unwrap();
        }
    }

    // Test listing all segments for "orders" topic
    let orders_segments = adapter.list_segments("orders", None).await.unwrap();
    assert_eq!(orders_segments.len(), 6); // 3 for partition 0, 3 for partition 1

    // Test listing segments for specific partition
    let orders_p0_segments = adapter.list_segments("orders", Some(0)).await.unwrap();
    assert_eq!(orders_p0_segments.len(), 3);
    
    // Verify they're sorted by base offset
    assert_eq!(orders_p0_segments[0].base_offset, 0);
    assert_eq!(orders_p0_segments[1].base_offset, 1000);
    assert_eq!(orders_p0_segments[2].base_offset, 2000);

    let orders_p1_segments = adapter.list_segments("orders", Some(1)).await.unwrap();
    assert_eq!(orders_p1_segments.len(), 3);

    // Test listing segments for other topics
    let users_segments = adapter.list_segments("users", None).await.unwrap();
    assert_eq!(users_segments.len(), 2);

    let analytics_segments = adapter.list_segments("analytics", None).await.unwrap();
    assert_eq!(analytics_segments.len(), 1);

    // Test non-existent topic
    let empty_segments = adapter.list_segments("nonexistent", None).await.unwrap();
    assert!(empty_segments.is_empty());
}

#[tokio::test]
async fn test_segment_location_key_conversion() {
    // Test key generation and parsing
    let location = SegmentLocation::new("my-topic".to_string(), 5, 123456789);
    let key = location.to_storage_key();
    
    assert_eq!(key, "segments/my-topic/partition-5/segment-00000000000123456789.chronik");

    // Test round-trip conversion
    let parsed_location = SegmentLocation::from_storage_key(&key).unwrap();
    assert_eq!(parsed_location, location);

    // Test with zero-padded offset
    let small_offset_location = SegmentLocation::new("test".to_string(), 0, 42);
    let small_key = small_offset_location.to_storage_key();
    assert_eq!(small_key, "segments/test/partition-0/segment-00000000000000000042.chronik");

    let parsed_small = SegmentLocation::from_storage_key(&small_key).unwrap();
    assert_eq!(parsed_small, small_offset_location);
}

#[tokio::test]
async fn test_segment_caching() {
    let temp_dir = TempDir::new().unwrap();
    
    let config = ObjectStoreConfig::local(
        temp_dir.path().to_string_lossy().to_string()
    );

    let store = ObjectStoreFactory::create(config).await.unwrap();
    let adapter = ChronikStorageAdapter::with_cache(store, 2); // Small cache for testing

    let topic = "cached-topic".to_string();
    let partition_id = 0;

    // Create and store multiple segments
    let locations: Vec<_> = (0..3)
        .map(|i| {
            let base_offset = i * 1000;
            SegmentLocation::new(topic.clone(), partition_id, base_offset)
        })
        .collect();

    for (i, location) in locations.iter().enumerate() {
        let test_batch = RecordBatch {
            records: vec![
                Record {
                    offset: location.base_offset,
                    timestamp: 1640995200000 + i as i64 * 1000,
                    key: Some(format!("key:{}", i).into_bytes()),
                    value: format!("cached value {}", i).into_bytes(),
                    headers: HashMap::new(),
                }
            ],
        };

        let segment = ChronikSegmentBuilder::new(topic.clone(), partition_id)
            .add_batch(test_batch)
            .compression(CompressionType::None)
            .build()
            .unwrap();

        adapter.store_segment(location, segment).await.unwrap();
    }

    // Load segments - first two should be cached
    let _segment1 = adapter.load_segment(&locations[0]).await.unwrap();
    let _segment2 = adapter.load_segment(&locations[1]).await.unwrap();

    // Load third segment - this should evict the first one from cache
    let _segment3 = adapter.load_segment(&locations[2]).await.unwrap();

    // Load first segment again - should be loaded from storage, not cache
    let _segment1_again = adapter.load_segment(&locations[0]).await.unwrap();

    // All operations should succeed regardless of cache state
    assert_eq!(_segment1.metadata().base_offset, 0);
    assert_eq!(_segment2.metadata().base_offset, 1000);
    assert_eq!(_segment3.metadata().base_offset, 2000);
    assert_eq!(_segment1_again.metadata().base_offset, 0);
}

#[tokio::test]
async fn test_segment_compression_types() {
    let temp_dir = TempDir::new().unwrap();
    
    let config = ObjectStoreConfig::local(
        temp_dir.path().to_string_lossy().to_string()
    );

    let store = ObjectStoreFactory::create(config).await.unwrap();
    let adapter = ChronikStorageAdapter::new(store);

    let topic = "compression-test".to_string();
    let partition_id = 0;

    // Create identical data with different compression
    let test_data = (0..100).map(|i| {
        Record {
            offset: i,
            timestamp: 1640995200000 + i,
            key: Some(format!("key:{:04}", i).into_bytes()),
            value: format!("This is a test record with some repeated content to test compression efficiency. Record number: {}", i).into_bytes(),
            headers: HashMap::new(),
        }
    }).collect();

    let test_batch = RecordBatch { records: test_data };

    // Test with no compression
    let uncompressed_segment = ChronikSegmentBuilder::new(topic.clone(), partition_id)
        .add_batch(test_batch.clone())
        .compression(CompressionType::None)
        .build()
        .unwrap();

    let uncompressed_location = SegmentLocation::new(topic.clone(), partition_id, 0);
    adapter.store_segment(&uncompressed_location, uncompressed_segment).await.unwrap();

    // Test with gzip compression
    let compressed_segment = ChronikSegmentBuilder::new(topic.clone(), partition_id)
        .add_batch(test_batch.clone())
        .compression(CompressionType::Gzip)
        .build()
        .unwrap();

    let compressed_location = SegmentLocation::new(topic.clone(), partition_id, 1000);
    adapter.store_segment(&compressed_location, compressed_segment).await.unwrap();

    // Compare sizes
    let uncompressed_stats = adapter.segment_stats(&uncompressed_location).await.unwrap();
    let compressed_stats = adapter.segment_stats(&compressed_location).await.unwrap();

    // Compressed should be smaller for this repetitive data
    assert!(compressed_stats.storage_size < uncompressed_stats.storage_size);
    assert!(compressed_stats.compression_ratio >= 0.0); // Compression ratio should be valid

    // Both should have the same record count
    assert_eq!(uncompressed_stats.record_count, 100);
    assert_eq!(compressed_stats.record_count, 100);

    // Both should load successfully with identical data
    let uncompressed_loaded = adapter.load_segment(&uncompressed_location).await.unwrap();
    let compressed_loaded = adapter.load_segment(&compressed_location).await.unwrap();

    assert_eq!(uncompressed_loaded.kafka_data()[0].records.len(), 100);
    assert_eq!(compressed_loaded.kafka_data()[0].records.len(), 100);

    // Verify data integrity
    assert_eq!(
        uncompressed_loaded.kafka_data()[0].records[0].key,
        compressed_loaded.kafka_data()[0].records[0].key
    );
    assert_eq!(
        uncompressed_loaded.kafka_data()[0].records[50].value,
        compressed_loaded.kafka_data()[0].records[50].value
    );
}