//! Tests for the fetch handler

use chronik_ingest::fetch_handler::FetchHandler;
use chronik_storage::{SegmentReader, Record, object_store::backends::local::LocalBackend, SegmentReaderConfig};
use chronik_common::metadata::memory::MemoryMetadataStore;
use chronik_common::metadata::traits::MetadataStore;
use chronik_protocol::{FetchRequest, FetchRequestTopic, FetchRequestPartition};
use std::sync::Arc;
use std::collections::HashMap;

#[tokio::test]
async fn test_fetch_empty_partition() {
    // Setup
    let temp_dir = tempfile::tempdir().unwrap();
    let object_store = Arc::new(LocalBackend::new(temp_dir.path().to_str().unwrap()).unwrap());
    let segment_reader = Arc::new(SegmentReader::new(SegmentReaderConfig::default(), object_store));
    let metadata_store = Arc::new(MemoryMetadataStore::new());
    
    // Create topic and partition metadata
    metadata_store.create_topic("test-topic", 1, 1, HashMap::new()).await.unwrap();
    metadata_store.update_partition_offset("test-topic", 0, 0, 0).await.unwrap();
    
    let fetch_handler = FetchHandler::new(segment_reader, metadata_store);
    
    // Create fetch request
    let request = FetchRequest {
        replica_id: -1,
        max_wait_ms: 1000,
        min_bytes: 1,
        max_bytes: 1048576,
        isolation_level: 0,
        session_id: 0,
        session_epoch: -1,
        topics: vec![
            FetchRequestTopic {
                name: "test-topic".to_string(),
                partitions: vec![
                    FetchRequestPartition {
                        partition: 0,
                        current_leader_epoch: -1,
                        fetch_offset: 0,
                        log_start_offset: -1,
                        partition_max_bytes: 1048576,
                    }
                ],
            }
        ],
        forgotten_topics_data: vec![],
        rack_id: None,
    };
    
    // Execute fetch
    let response = fetch_handler.handle_fetch(request, 1).await.unwrap();
    
    // Verify response
    assert_eq!(response.topics.len(), 1);
    assert_eq!(response.topics[0].name, "test-topic");
    assert_eq!(response.topics[0].partitions.len(), 1);
    assert_eq!(response.topics[0].partitions[0].error_code, 0);
    assert_eq!(response.topics[0].partitions[0].high_watermark, 0);
    assert!(response.topics[0].partitions[0].records.is_empty());
}

#[tokio::test]
async fn test_fetch_with_buffer() {
    // Setup
    let temp_dir = tempfile::tempdir().unwrap();
    let object_store = Arc::new(LocalBackend::new(temp_dir.path().to_str().unwrap()).unwrap());
    let segment_reader = Arc::new(SegmentReader::new(SegmentReaderConfig::default(), object_store));
    let metadata_store = Arc::new(MemoryMetadataStore::new());
    
    // Create topic and partition metadata
    metadata_store.create_topic("test-topic", 1, 1, HashMap::new()).await.unwrap();
    metadata_store.update_partition_offset("test-topic", 0, 0, 0).await.unwrap();
    
    let fetch_handler = FetchHandler::new(segment_reader, metadata_store);
    
    // Add some records to buffer
    let records = vec![
        Record {
            offset: 0,
            timestamp: 1000,
            key: Some(b"key1".to_vec()),
            value: b"value1".to_vec(),
            headers: HashMap::new(),
        },
        Record {
            offset: 1,
            timestamp: 1001,
            key: Some(b"key2".to_vec()),
            value: b"value2".to_vec(),
            headers: HashMap::new(),
        },
    ];
    
    fetch_handler.update_buffer("test-topic", 0, records.clone(), 2).await.unwrap();
    
    // Create fetch request
    let request = FetchRequest {
        replica_id: -1,
        max_wait_ms: 1000,
        min_bytes: 1,
        max_bytes: 1048576,
        isolation_level: 0,
        session_id: 0,
        session_epoch: -1,
        topics: vec![
            FetchRequestTopic {
                name: "test-topic".to_string(),
                partitions: vec![
                    FetchRequestPartition {
                        partition: 0,
                        current_leader_epoch: -1,
                        fetch_offset: 0,
                        log_start_offset: -1,
                        partition_max_bytes: 1048576,
                    }
                ],
            }
        ],
        forgotten_topics_data: vec![],
        rack_id: None,
    };
    
    // Execute fetch
    let response = fetch_handler.handle_fetch(request, 1).await.unwrap();
    
    // Verify response
    assert_eq!(response.topics.len(), 1);
    assert_eq!(response.topics[0].name, "test-topic");
    assert_eq!(response.topics[0].partitions.len(), 1);
    assert_eq!(response.topics[0].partitions[0].error_code, 0);
    assert_eq!(response.topics[0].partitions[0].high_watermark, 2);
    assert!(!response.topics[0].partitions[0].records.is_empty());
}

#[tokio::test]
async fn test_fetch_unknown_topic() {
    // Setup
    let temp_dir = tempfile::tempdir().unwrap();
    let object_store = Arc::new(LocalBackend::new(temp_dir.path().to_str().unwrap()).unwrap());
    let segment_reader = Arc::new(SegmentReader::new(SegmentReaderConfig::default(), object_store));
    let metadata_store = Arc::new(MemoryMetadataStore::new());
    
    let fetch_handler = FetchHandler::new(segment_reader, metadata_store);
    
    // Create fetch request for unknown topic
    let request = FetchRequest {
        replica_id: -1,
        max_wait_ms: 1000,
        min_bytes: 1,
        max_bytes: 1048576,
        isolation_level: 0,
        session_id: 0,
        session_epoch: -1,
        topics: vec![
            FetchRequestTopic {
                name: "unknown-topic".to_string(),
                partitions: vec![
                    FetchRequestPartition {
                        partition: 0,
                        current_leader_epoch: -1,
                        fetch_offset: 0,
                        log_start_offset: -1,
                        partition_max_bytes: 1048576,
                    }
                ],
            }
        ],
        forgotten_topics_data: vec![],
        rack_id: None,
    };
    
    // Execute fetch
    let response = fetch_handler.handle_fetch(request, 1).await.unwrap();
    
    // Verify response shows unknown topic error
    assert_eq!(response.topics.len(), 1);
    assert_eq!(response.topics[0].name, "unknown-topic");
    assert_eq!(response.topics[0].partitions.len(), 1);
    assert_eq!(response.topics[0].partitions[0].error_code, 3); // UNKNOWN_TOPIC_OR_PARTITION
}

#[tokio::test]
async fn test_fetch_offset_out_of_range() {
    // Setup
    let temp_dir = tempfile::tempdir().unwrap();
    let object_store = Arc::new(LocalBackend::new(temp_dir.path().to_str().unwrap()).unwrap());
    let segment_reader = Arc::new(SegmentReader::new(SegmentReaderConfig::default(), object_store));
    let metadata_store = Arc::new(MemoryMetadataStore::new());
    
    // Create topic with log start offset > 0
    metadata_store.create_topic("test-topic", 1, 1, HashMap::new()).await.unwrap();
    metadata_store.update_partition_offset("test-topic", 0, 10, 5).await.unwrap(); // high_watermark=10, log_start=5
    
    let fetch_handler = FetchHandler::new(segment_reader, metadata_store);
    
    // Create fetch request with offset < log_start_offset
    let request = FetchRequest {
        replica_id: -1,
        max_wait_ms: 1000,
        min_bytes: 1,
        max_bytes: 1048576,
        isolation_level: 0,
        session_id: 0,
        session_epoch: -1,
        topics: vec![
            FetchRequestTopic {
                name: "test-topic".to_string(),
                partitions: vec![
                    FetchRequestPartition {
                        partition: 0,
                        current_leader_epoch: -1,
                        fetch_offset: 2, // Less than log_start_offset (5)
                        log_start_offset: -1,
                        partition_max_bytes: 1048576,
                    }
                ],
            }
        ],
        forgotten_topics_data: vec![],
        rack_id: None,
    };
    
    // Execute fetch
    let response = fetch_handler.handle_fetch(request, 1).await.unwrap();
    
    // Verify response shows offset out of range error
    assert_eq!(response.topics.len(), 1);
    assert_eq!(response.topics[0].partitions.len(), 1);
    assert_eq!(response.topics[0].partitions[0].error_code, 1); // OFFSET_OUT_OF_RANGE
}