//! Unit tests for offset storage module

#[cfg(test)]
mod tests {
    use super::super::*;
    use chronik_common::metadata::{
        traits::{MetadataStore, MetadataError}, 
        ConsumerOffset
    };
    use std::sync::Arc;
    use std::time::Duration;
    use chrono::Utc;
    
    // Mock metadata store for testing
    struct MockMetadataStore {
        offsets: Arc<RwLock<HashMap<String, ConsumerOffset>>>,
        fail_commits: Arc<RwLock<bool>>,
    }
    
    impl MockMetadataStore {
        fn new() -> Self {
            Self {
                offsets: Arc::new(RwLock::new(HashMap::new())),
                fail_commits: Arc::new(RwLock::new(false)),
            }
        }
        
        fn set_fail_commits(&self, fail: bool) {
            let mut flag = self.fail_commits.blocking_write();
            *flag = fail;
        }
        
        fn get_offset_key(group_id: &str, topic: &str, partition: u32) -> String {
            format!("{}:{}:{}", group_id, topic, partition)
        }
    }
    
    #[async_trait::async_trait]
    impl MetadataStore for MockMetadataStore {
        async fn commit_offset(&self, offset: ConsumerOffset) -> chronik_common::metadata::traits::Result<()> {
            if *self.fail_commits.read().await {
                return Err(MetadataError::StorageError("Mock storage failure".to_string()));
            }
            
            let key = Self::get_offset_key(&offset.group_id, &offset.topic, offset.partition);
            let mut offsets = self.offsets.write().await;
            offsets.insert(key, offset);
            Ok(())
        }
        
        async fn get_consumer_offset(&self, group_id: &str, topic: &str, partition: u32) -> chronik_common::metadata::traits::Result<Option<ConsumerOffset>> {
            let key = Self::get_offset_key(group_id, topic, partition);
            let offsets = self.offsets.read().await;
            Ok(offsets.get(&key).cloned())
        }
        
        // Implement other required trait methods as no-ops
        async fn create_topic(&self, _name: &str, _config: chronik_common::metadata::TopicConfig) -> chronik_common::metadata::traits::Result<chronik_common::metadata::TopicMetadata> {
            unimplemented!()
        }
        
        async fn create_topic_with_assignments(
            &self,
            _name: &str,
            _config: chronik_common::metadata::TopicConfig,
            _assignments: Vec<chronik_common::metadata::PartitionAssignment>,
            _segment_boundaries: Vec<(u32, i64, i64)>,
        ) -> chronik_common::metadata::traits::Result<chronik_common::metadata::TopicMetadata> {
            unimplemented!()
        }
        
        async fn get_topic(&self, _name: &str) -> chronik_common::metadata::traits::Result<Option<chronik_common::metadata::TopicMetadata>> {
            unimplemented!()
        }
        
        async fn list_topics(&self) -> chronik_common::metadata::traits::Result<Vec<chronik_common::metadata::TopicMetadata>> {
            unimplemented!()
        }
        
        async fn update_topic(&self, _name: &str, _config: chronik_common::metadata::TopicConfig) -> chronik_common::metadata::traits::Result<chronik_common::metadata::TopicMetadata> {
            unimplemented!()
        }
        
        async fn delete_topic(&self, _name: &str) -> chronik_common::metadata::traits::Result<()> {
            unimplemented!()
        }
        
        async fn persist_segment_metadata(&self, _metadata: chronik_common::metadata::SegmentMetadata) -> chronik_common::metadata::traits::Result<()> {
            unimplemented!()
        }
        
        async fn get_segment_metadata(&self, _topic: &str, _segment_id: &str) -> chronik_common::metadata::traits::Result<Option<chronik_common::metadata::SegmentMetadata>> {
            unimplemented!()
        }
        
        async fn list_segments(&self, _topic: &str, _partition: Option<u32>) -> chronik_common::metadata::traits::Result<Vec<chronik_common::metadata::SegmentMetadata>> {
            unimplemented!()
        }
        
        async fn delete_segment(&self, _topic: &str, _segment_id: &str) -> chronik_common::metadata::traits::Result<()> {
            unimplemented!()
        }
        
        async fn register_broker(&self, _metadata: chronik_common::metadata::BrokerMetadata) -> chronik_common::metadata::traits::Result<()> {
            unimplemented!()
        }
        
        async fn get_broker(&self, _broker_id: i32) -> chronik_common::metadata::traits::Result<Option<chronik_common::metadata::BrokerMetadata>> {
            unimplemented!()
        }
        
        async fn list_brokers(&self) -> chronik_common::metadata::traits::Result<Vec<chronik_common::metadata::BrokerMetadata>> {
            unimplemented!()
        }
        
        async fn update_broker_status(&self, _broker_id: i32, _status: chronik_common::metadata::BrokerStatus) -> chronik_common::metadata::traits::Result<()> {
            unimplemented!()
        }
        
        async fn assign_partition(&self, _assignment: chronik_common::metadata::PartitionAssignment) -> chronik_common::metadata::traits::Result<()> {
            unimplemented!()
        }
        
        async fn get_partition_assignments(&self, _topic: &str) -> chronik_common::metadata::traits::Result<Vec<chronik_common::metadata::PartitionAssignment>> {
            unimplemented!()
        }
        
        async fn create_consumer_group(&self, _metadata: chronik_common::metadata::ConsumerGroupMetadata) -> chronik_common::metadata::traits::Result<()> {
            unimplemented!()
        }
        
        async fn get_consumer_group(&self, _group_id: &str) -> chronik_common::metadata::traits::Result<Option<chronik_common::metadata::ConsumerGroupMetadata>> {
            unimplemented!()
        }
        
        async fn update_consumer_group(&self, _metadata: chronik_common::metadata::ConsumerGroupMetadata) -> chronik_common::metadata::traits::Result<()> {
            unimplemented!()
        }
        
        async fn update_partition_offset(&self, _topic: &str, _partition: u32, _high_watermark: i64, _log_start_offset: i64) -> chronik_common::metadata::traits::Result<()> {
            unimplemented!()
        }
        
        async fn get_partition_offset(&self, _topic: &str, _partition: u32) -> chronik_common::metadata::traits::Result<Option<(i64, i64)>> {
            unimplemented!()
        }
        
        async fn init_system_state(&self) -> chronik_common::metadata::traits::Result<()> {
            unimplemented!()
        }
    }
    
    fn create_test_storage(metadata_store: Arc<dyn MetadataStore>) -> OffsetStorage {
        let config = OffsetStorageConfig {
            retention_duration: Duration::from_secs(60), // 1 minute for testing
            cleanup_interval: Duration::from_secs(5),
            enable_cleanup: false, // Disable auto-cleanup in tests
        };
        
        OffsetStorage::new(metadata_store, config)
    }
    
    #[tokio::test]
    async fn test_commit_and_fetch_single_offset() {
        let mock_store = Arc::new(MockMetadataStore::new());
        let storage = create_test_storage(mock_store);
        
        // Commit a single offset
        let results = storage.commit_offsets(
            "test-group",
            1,
            vec![("test-topic".to_string(), 0, 100, Some("metadata".to_string()))],
        ).await.unwrap();
        
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].error_code, 0);
        assert_eq!(results[0].topic, "test-topic");
        assert_eq!(results[0].partition, 0);
        
        // Fetch the offset back
        let fetched = storage.fetch_offsets(
            "test-group",
            vec![("test-topic".to_string(), 0)],
        ).await.unwrap();
        
        assert_eq!(fetched.len(), 1);
        assert_eq!(fetched[0].topic, "test-topic");
        assert_eq!(fetched[0].partition, 0);
        assert_eq!(fetched[0].offset, 100);
        assert_eq!(fetched[0].metadata, Some("metadata".to_string()));
        assert_eq!(fetched[0].error_code, 0);
    }
    
    #[tokio::test]
    async fn test_commit_multiple_offsets() {
        let mock_store = Arc::new(MockMetadataStore::new());
        let storage = create_test_storage(mock_store);
        
        // Commit multiple offsets
        let results = storage.commit_offsets(
            "test-group",
            1,
            vec![
                ("topic1".to_string(), 0, 100, None),
                ("topic1".to_string(), 1, 200, Some("meta1".to_string())),
                ("topic2".to_string(), 0, 300, Some("meta2".to_string())),
            ],
        ).await.unwrap();
        
        assert_eq!(results.len(), 3);
        for result in &results {
            assert_eq!(result.error_code, 0);
        }
        
        // Fetch all offsets
        let fetched = storage.fetch_offsets(
            "test-group",
            vec![
                ("topic1".to_string(), 0),
                ("topic1".to_string(), 1),
                ("topic2".to_string(), 0),
            ],
        ).await.unwrap();
        
        assert_eq!(fetched.len(), 3);
        
        // Verify offsets
        let offset_map: HashMap<(String, u32), (i64, Option<String>)> = fetched.into_iter()
            .map(|f| ((f.topic, f.partition), (f.offset, f.metadata)))
            .collect();
        
        assert_eq!(offset_map.get(&("topic1".to_string(), 0)), Some(&(100, None)));
        assert_eq!(offset_map.get(&("topic1".to_string(), 1)), Some(&(200, Some("meta1".to_string()))));
        assert_eq!(offset_map.get(&("topic2".to_string(), 0)), Some(&(300, Some("meta2".to_string()))));
    }
    
    #[tokio::test]
    async fn test_fetch_non_existent_offset() {
        let mock_store = Arc::new(MockMetadataStore::new());
        let storage = create_test_storage(mock_store);
        
        // Fetch non-existent offset
        let fetched = storage.fetch_offsets(
            "non-existent-group",
            vec![("test-topic".to_string(), 0)],
        ).await.unwrap();
        
        assert_eq!(fetched.len(), 1);
        assert_eq!(fetched[0].topic, "test-topic");
        assert_eq!(fetched[0].partition, 0);
        assert_eq!(fetched[0].offset, -1); // Not found
        assert_eq!(fetched[0].metadata, None);
        assert_eq!(fetched[0].error_code, 0);
    }
    
    #[tokio::test]
    async fn test_cache_hit() {
        let mock_store = Arc::new(MockMetadataStore::new());
        let storage = create_test_storage(mock_store);
        
        // Commit an offset
        storage.commit_offsets(
            "cache-test-group",
            1,
            vec![("cache-topic".to_string(), 0, 100, None)],
        ).await.unwrap();
        
        // First fetch - from storage
        let fetched1 = storage.fetch_offsets(
            "cache-test-group",
            vec![("cache-topic".to_string(), 0)],
        ).await.unwrap();
        
        assert_eq!(fetched1[0].offset, 100);
        
        // Second fetch - should hit cache
        let fetched2 = storage.fetch_offsets(
            "cache-test-group",
            vec![("cache-topic".to_string(), 0)],
        ).await.unwrap();
        
        assert_eq!(fetched2[0].offset, 100);
        
        // Verify cache stats
        let stats = storage.get_stats().await;
        assert!(stats.cached_entries > 0);
    }
    
    #[tokio::test]
    async fn test_storage_error_handling() {
        let mock_store = Arc::new(MockMetadataStore::new());
        mock_store.set_fail_commits(true);
        let storage = create_test_storage(mock_store);
        
        // Try to commit - should fail
        let results = storage.commit_offsets(
            "test-group",
            1,
            vec![("test-topic".to_string(), 0, 100, None)],
        ).await.unwrap();
        
        assert_eq!(results.len(), 1);
        assert_ne!(results[0].error_code, 0); // Should have error code
    }
    
    #[tokio::test]
    async fn test_overwrite_offset() {
        let mock_store = Arc::new(MockMetadataStore::new());
        let storage = create_test_storage(mock_store);
        
        // Commit initial offset
        storage.commit_offsets(
            "test-group",
            1,
            vec![("test-topic".to_string(), 0, 100, Some("v1".to_string()))],
        ).await.unwrap();
        
        // Overwrite with new offset
        storage.commit_offsets(
            "test-group",
            2,
            vec![("test-topic".to_string(), 0, 200, Some("v2".to_string()))],
        ).await.unwrap();
        
        // Fetch should return latest
        let fetched = storage.fetch_offsets(
            "test-group",
            vec![("test-topic".to_string(), 0)],
        ).await.unwrap();
        
        assert_eq!(fetched[0].offset, 200);
        assert_eq!(fetched[0].metadata, Some("v2".to_string()));
    }
}