#[cfg(test)]
mod tests {
    use crate::fetch_handler::FetchHandler;
    use chronik_common::metadata::TiKVMetadataStore;
    use chronik_storage::{
        object_store::backends::local::LocalBackend,
        SegmentReader,
    };
    use std::sync::Arc;
    use std::collections::HashMap;
    use tempfile::TempDir;
    use chronik_protocol::types::{FetchRequest, FetchRequestTopic, FetchRequestPartition};
    
    async fn create_test_handler() -> (FetchHandler, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let storage_path = temp_dir.path().join("storage");
        std::fs::create_dir_all(&storage_path).unwrap();
        
        // Create metadata store
        let metadata_store = Arc::new(
            TiKVMetadataStore::new(vec!["127.0.0.1:2379".to_string()])
                .await
                .unwrap()
        );
        
        // Create storage
        let storage_config = chronik_storage::object_store::ObjectStoreConfig {
            backend: chronik_storage::object_store::StorageBackend::Local {
                path: storage_path.to_string_lossy().to_string(),
            },
            retry: Default::default(),
        };
        let storage: Arc<dyn chronik_storage::object_store::ObjectStore> = Arc::new(
            LocalBackend::new(storage_config).await.unwrap()
        );
        
        // Create segment reader
        let segment_reader = Arc::new(SegmentReader::new(
            Default::default(),
            storage,
        ));
        
        let handler = FetchHandler::new(segment_reader, metadata_store);
        
        (handler, temp_dir)
    }
    
    #[tokio::test]
    async fn test_fetch_empty_topic() {
        let (handler, _temp_dir) = create_test_handler().await;
        
        let request = FetchRequest {
            replica_id: -1,
            max_wait_ms: 0,
            min_bytes: 0,
            topics: vec![FetchRequestTopic {
                name: "non-existent".to_string(),
                partitions: vec![FetchRequestPartition {
                    partition: 0,
                    fetch_offset: 0,
                    partition_max_bytes: 1024 * 1024,
                }],
            }],
        };
        
        let response = handler.handle_fetch(request, 123).await.unwrap();
        
        assert_eq!(response.header.correlation_id, 123);
        assert_eq!(response.topics.len(), 1);
        assert_eq!(response.topics[0].partitions.len(), 1);
        assert_eq!(response.topics[0].partitions[0].error_code, 3); // UNKNOWN_TOPIC_OR_PARTITION
    }
    
    #[tokio::test]
    async fn test_update_buffer() {
        let (handler, _temp_dir) = create_test_handler().await;
        
        // Add some records to buffer
        let records = vec![
            chronik_storage::Record {
                offset: 0,
                timestamp: 1000,
                key: Some(b"key1".to_vec()),
                value: b"value1".to_vec(),
                headers: HashMap::new(),
            },
            chronik_storage::Record {
                offset: 1,
                timestamp: 1001,
                key: Some(b"key2".to_vec()),
                value: b"value2".to_vec(),
                headers: HashMap::new(),
            },
        ];
        
        handler.update_buffer("test-topic", 0, records, 2).await.unwrap();
        
        // Verify buffer state
        {
            let state = handler.state.read().await;
            let buffer = state.buffers.get(&("test-topic".to_string(), 0)).unwrap();
            assert_eq!(buffer.records.len(), 2);
            assert_eq!(buffer.base_offset, 0);
            assert_eq!(buffer.high_watermark, 2);
        }
    }
    
    #[tokio::test]
    async fn test_buffer_trimming() {
        let (handler, _temp_dir) = create_test_handler().await;
        
        // Add more than 1000 records
        let mut records = Vec::new();
        for i in 0..1500 {
            records.push(chronik_storage::Record {
                offset: i,
                timestamp: 1000 + i,
                key: Some(format!("key{}", i).into_bytes()),
                value: format!("value{}", i).into_bytes(),
                headers: HashMap::new(),
            });
        }
        
        handler.update_buffer("test-topic", 0, records, 1500).await.unwrap();
        
        // Verify buffer was trimmed
        {
            let state = handler.state.read().await;
            let buffer = state.buffers.get(&("test-topic".to_string(), 0)).unwrap();
            assert_eq!(buffer.records.len(), 1000); // Should be trimmed
            assert_eq!(buffer.base_offset, 500); // First 500 removed
            assert_eq!(buffer.high_watermark, 1500);
        }
    }
    
    #[tokio::test]
    async fn test_clear_topic_buffers() {
        let (handler, _temp_dir) = create_test_handler().await;
        
        // Add records to multiple topics
        let records = vec![chronik_storage::Record {
            offset: 0,
            timestamp: 1000,
            key: None,
            value: b"test".to_vec(),
            headers: HashMap::new(),
        }];
        
        handler.update_buffer("topic1", 0, records.clone(), 1).await.unwrap();
        handler.update_buffer("topic2", 0, records, 1).await.unwrap();
        
        // Clear topic1
        handler.clear_topic_buffers("topic1").await.unwrap();
        
        // Verify only topic1 was cleared
        {
            let state = handler.state.read().await;
            assert!(!state.buffers.contains_key(&("topic1".to_string(), 0)));
            assert!(state.buffers.contains_key(&("topic2".to_string(), 0)));
        }
    }
    
    #[tokio::test]
    async fn test_timeout_behavior() {
        let (handler, _temp_dir) = create_test_handler().await;
        
        let request = FetchRequest {
            replica_id: -1,
            max_wait_ms: 100, // 100ms timeout
            min_bytes: 1000,  // Request 1KB minimum
            topics: vec![FetchRequestTopic {
                name: "test-topic".to_string(),
                partitions: vec![FetchRequestPartition {
                    partition: 0,
                    fetch_offset: 0,
                    partition_max_bytes: 1024 * 1024,
                }],
            }],
        };
        
        let start = std::time::Instant::now();
        let response = handler.handle_fetch(request, 456).await.unwrap();
        let elapsed = start.elapsed();
        
        // Should timeout after ~100ms
        assert!(elapsed.as_millis() >= 90 && elapsed.as_millis() <= 150);
        assert_eq!(response.header.correlation_id, 456);
    }
    
    #[tokio::test]
    async fn test_partial_response() {
        let (handler, _temp_dir) = create_test_handler().await;
        
        // Add some small records
        let records = vec![
            chronik_storage::Record {
                offset: 0,
                timestamp: 1000,
                key: Some(b"k".to_vec()),
                value: b"v".to_vec(),
                headers: HashMap::new(),
            },
        ];
        
        handler.update_buffer("test-topic", 0, records, 1).await.unwrap();
        
        let request = FetchRequest {
            replica_id: -1,
            max_wait_ms: 100,
            min_bytes: 1000, // Request more than available
            topics: vec![FetchRequestTopic {
                name: "test-topic".to_string(),
                partitions: vec![FetchRequestPartition {
                    partition: 0,
                    fetch_offset: 0,
                    partition_max_bytes: 1024 * 1024,
                }],
            }],
        };
        
        let response = handler.handle_fetch(request, 789).await.unwrap();
        
        // Should return partial data even though min_bytes not met
        assert_eq!(response.topics[0].partitions[0].error_code, 0);
        assert!(response.topics[0].partitions[0].records.len() > 0);
    }
}