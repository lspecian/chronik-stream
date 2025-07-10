//! Tests for metadata store functionality.

#[cfg(test)]
mod tests {
    use super::super::*;
    use tempfile::TempDir;
    use std::collections::HashMap;
    
    async fn create_test_store() -> TiKVMetadataStore {
        // For tests, use a test TiKV instance or mock
        // In a real test environment, you'd spin up a test TiKV cluster
        let endpoints = vec!["localhost:2379".to_string()];
        TiKVMetadataStore::new(endpoints).await.unwrap()
    }
    
    #[tokio::test]
    async fn test_init_system_state() {
        let store = create_test_store().await;
        
        // Initialize system state
        store.init_system_state().await.unwrap();
        
        // Verify internal topics were created
        let topics = store.list_topics().await.unwrap();
        assert_eq!(topics.len(), 2);
        
        // Check __consumer_offsets
        let consumer_offsets = store.get_topic("__consumer_offsets").await.unwrap().unwrap();
        assert_eq!(consumer_offsets.config.partition_count, 50);
        assert_eq!(consumer_offsets.config.replication_factor, 3);
        
        // Check __transaction_state
        let transaction_state = store.get_topic("__transaction_state").await.unwrap().unwrap();
        assert_eq!(transaction_state.config.partition_count, 50);
        assert_eq!(transaction_state.config.replication_factor, 3);
    }
    
    #[tokio::test]
    async fn test_topic_crud_operations() {
        let store = create_test_store().await;
        
        // Create topic
        let config = TopicConfig {
            partition_count: 6,
            replication_factor: 2,
            retention_ms: Some(3600000), // 1 hour
            segment_bytes: 1024 * 1024,
            config: HashMap::new(),
        };
        
        let topic = store.create_topic("test-topic", config.clone()).await.unwrap();
        assert_eq!(topic.name, "test-topic");
        assert_eq!(topic.config.partition_count, 6);
        
        // Get topic
        let retrieved = store.get_topic("test-topic").await.unwrap().unwrap();
        assert_eq!(retrieved.id, topic.id);
        assert_eq!(retrieved.name, "test-topic");
        
        // Update topic
        let mut new_config = config.clone();
        new_config.partition_count = 12;
        new_config.config.insert("compression.type".to_string(), "snappy".to_string());
        
        let updated = store.update_topic("test-topic", new_config).await.unwrap();
        assert_eq!(updated.config.partition_count, 12);
        assert_eq!(updated.config.config.get("compression.type").unwrap(), "snappy");
        
        // List topics
        let topics = store.list_topics().await.unwrap();
        assert_eq!(topics.len(), 1);
        assert_eq!(topics[0].name, "test-topic");
        
        // Delete topic
        store.delete_topic("test-topic").await.unwrap();
        assert!(store.get_topic("test-topic").await.unwrap().is_none());
        assert_eq!(store.list_topics().await.unwrap().len(), 0);
    }
    
    #[tokio::test]
    async fn test_topic_already_exists() {
        let store = create_test_store().await;
        
        let config = TopicConfig::default();
        store.create_topic("duplicate", config.clone()).await.unwrap();
        
        // Try to create duplicate
        let result = store.create_topic("duplicate", config).await;
        assert!(result.is_err());
        match result {
            Err(MetadataError::AlreadyExists(msg)) => {
                assert!(msg.contains("duplicate"));
            }
            _ => panic!("Expected AlreadyExists error"),
        }
    }
    
    #[tokio::test]
    async fn test_segment_operations() {
        let store = create_test_store().await;
        
        // Create topic first
        store.create_topic("segments-topic", TopicConfig::default()).await.unwrap();
        
        // Create segments
        let segments = vec![
            SegmentMetadata {
                segment_id: "seg-001".to_string(),
                topic: "segments-topic".to_string(),
                partition: 0,
                start_offset: 0,
                end_offset: 999,
                size: 1024 * 1024,
                record_count: 1000,
                path: "/data/segments-topic/0/seg-001".to_string(),
                created_at: chrono::Utc::now(),
            },
            SegmentMetadata {
                segment_id: "seg-002".to_string(),
                topic: "segments-topic".to_string(),
                partition: 0,
                start_offset: 1000,
                end_offset: 1999,
                size: 1024 * 1024,
                record_count: 1000,
                path: "/data/segments-topic/0/seg-002".to_string(),
                created_at: chrono::Utc::now(),
            },
            SegmentMetadata {
                segment_id: "seg-003".to_string(),
                topic: "segments-topic".to_string(),
                partition: 1,
                start_offset: 0,
                end_offset: 999,
                size: 1024 * 1024,
                record_count: 1000,
                path: "/data/segments-topic/1/seg-003".to_string(),
                created_at: chrono::Utc::now(),
            },
        ];
        
        // Persist segments
        for segment in &segments {
            store.persist_segment_metadata(segment.clone()).await.unwrap();
        }
        
        // Get segment
        let retrieved = store.get_segment_metadata("segments-topic", "seg-001").await.unwrap().unwrap();
        assert_eq!(retrieved.segment_id, "seg-001");
        assert_eq!(retrieved.start_offset, 0);
        assert_eq!(retrieved.end_offset, 999);
        
        // List all segments for topic
        let all_segments = store.list_segments("segments-topic", None).await.unwrap();
        assert_eq!(all_segments.len(), 3);
        
        // List segments for partition 0
        let partition_0_segments = store.list_segments("segments-topic", Some(0)).await.unwrap();
        assert_eq!(partition_0_segments.len(), 2);
        assert!(partition_0_segments.iter().all(|s| s.partition == 0));
        
        // List segments for partition 1
        let partition_1_segments = store.list_segments("segments-topic", Some(1)).await.unwrap();
        assert_eq!(partition_1_segments.len(), 1);
        assert_eq!(partition_1_segments[0].segment_id, "seg-003");
        
        // Delete segment
        store.delete_segment("segments-topic", "seg-001").await.unwrap();
        assert!(store.get_segment_metadata("segments-topic", "seg-001").await.unwrap().is_none());
        assert_eq!(store.list_segments("segments-topic", None).await.unwrap().len(), 2);
    }
    
    #[tokio::test]
    async fn test_broker_operations() {
        let store = create_test_store().await;
        
        // Register brokers
        let brokers = vec![
            BrokerMetadata {
                broker_id: 1,
                host: "broker1.example.com".to_string(),
                port: 9092,
                rack: Some("rack-1".to_string()),
                status: BrokerStatus::Online,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            },
            BrokerMetadata {
                broker_id: 2,
                host: "broker2.example.com".to_string(),
                port: 9092,
                rack: Some("rack-2".to_string()),
                status: BrokerStatus::Online,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            },
        ];
        
        for broker in &brokers {
            store.register_broker(broker.clone()).await.unwrap();
        }
        
        // Get broker
        let broker1 = store.get_broker(1).await.unwrap().unwrap();
        assert_eq!(broker1.host, "broker1.example.com");
        assert_eq!(broker1.rack.as_ref().unwrap(), "rack-1");
        
        // List brokers
        let all_brokers = store.list_brokers().await.unwrap();
        assert_eq!(all_brokers.len(), 2);
        
        // Update broker status
        store.update_broker_status(2, BrokerStatus::Maintenance).await.unwrap();
        let broker2 = store.get_broker(2).await.unwrap().unwrap();
        assert!(matches!(broker2.status, BrokerStatus::Maintenance));
    }
    
    #[tokio::test]
    async fn test_partition_assignments() {
        let store = create_test_store().await;
        
        // Create topic and register brokers
        store.create_topic("assigned-topic", TopicConfig {
            partition_count: 3,
            replication_factor: 2,
            ..Default::default()
        }).await.unwrap();
        
        // Register brokers
        for i in 1..=3 {
            store.register_broker(BrokerMetadata {
                broker_id: i,
                host: format!("broker{}.example.com", i),
                port: 9092,
                rack: None,
                status: BrokerStatus::Online,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            }).await.unwrap();
        }
        
        // Assign partitions
        let assignments = vec![
            PartitionAssignment { topic: "assigned-topic".to_string(), partition: 0, broker_id: 1, is_leader: true },
            PartitionAssignment { topic: "assigned-topic".to_string(), partition: 0, broker_id: 2, is_leader: false },
            PartitionAssignment { topic: "assigned-topic".to_string(), partition: 1, broker_id: 2, is_leader: true },
            PartitionAssignment { topic: "assigned-topic".to_string(), partition: 1, broker_id: 3, is_leader: false },
            PartitionAssignment { topic: "assigned-topic".to_string(), partition: 2, broker_id: 3, is_leader: true },
            PartitionAssignment { topic: "assigned-topic".to_string(), partition: 2, broker_id: 1, is_leader: false },
        ];
        
        for assignment in &assignments {
            store.assign_partition(assignment.clone()).await.unwrap();
        }
        
        // Get assignments
        let retrieved = store.get_partition_assignments("assigned-topic").await.unwrap();
        assert_eq!(retrieved.len(), 6);
        
        // Verify leaders
        let leaders: Vec<_> = retrieved.iter().filter(|a| a.is_leader).collect();
        assert_eq!(leaders.len(), 3);
        assert!(leaders.iter().any(|a| a.partition == 0 && a.broker_id == 1));
        assert!(leaders.iter().any(|a| a.partition == 1 && a.broker_id == 2));
        assert!(leaders.iter().any(|a| a.partition == 2 && a.broker_id == 3));
    }
    
    #[tokio::test]
    async fn test_consumer_group_operations() {
        let store = create_test_store().await;
        
        // Create consumer group
        let group = ConsumerGroupMetadata {
            group_id: "test-group".to_string(),
            state: "Stable".to_string(),
            protocol: "range".to_string(),
            protocol_type: "consumer".to_string(),
            generation_id: 1,
            leader_id: Some("consumer-1".to_string()),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        
        store.create_consumer_group(group.clone()).await.unwrap();
        
        // Get consumer group
        let retrieved = store.get_consumer_group("test-group").await.unwrap().unwrap();
        assert_eq!(retrieved.group_id, "test-group");
        assert_eq!(retrieved.generation_id, 1);
        assert_eq!(retrieved.leader_id.as_ref().unwrap(), "consumer-1");
        
        // Update consumer group
        let mut updated_group = group.clone();
        updated_group.generation_id = 2;
        updated_group.leader_id = Some("consumer-2".to_string());
        updated_group.updated_at = chrono::Utc::now();
        
        store.update_consumer_group(updated_group).await.unwrap();
        
        let updated = store.get_consumer_group("test-group").await.unwrap().unwrap();
        assert_eq!(updated.generation_id, 2);
        assert_eq!(updated.leader_id.as_ref().unwrap(), "consumer-2");
    }
    
    #[tokio::test]
    async fn test_consumer_offsets() {
        let store = create_test_store().await;
        
        // Create topic
        store.create_topic("offset-topic", TopicConfig {
            partition_count: 2,
            ..Default::default()
        }).await.unwrap();
        
        // Commit offsets
        let offsets = vec![
            ConsumerOffset {
                group_id: "group1".to_string(),
                topic: "offset-topic".to_string(),
                partition: 0,
                offset: 1000,
                metadata: Some("checkpoint-1".to_string()),
                commit_timestamp: chrono::Utc::now(),
            },
            ConsumerOffset {
                group_id: "group1".to_string(),
                topic: "offset-topic".to_string(),
                partition: 1,
                offset: 2000,
                metadata: None,
                commit_timestamp: chrono::Utc::now(),
            },
        ];
        
        for offset in &offsets {
            store.commit_offset(offset.clone()).await.unwrap();
        }
        
        // Get offsets
        let offset0 = store.get_consumer_offset("group1", "offset-topic", 0).await.unwrap().unwrap();
        assert_eq!(offset0.offset, 1000);
        assert_eq!(offset0.metadata.as_ref().unwrap(), "checkpoint-1");
        
        let offset1 = store.get_consumer_offset("group1", "offset-topic", 1).await.unwrap().unwrap();
        assert_eq!(offset1.offset, 2000);
        assert!(offset1.metadata.is_none());
        
        // Update offset
        let new_offset = ConsumerOffset {
            group_id: "group1".to_string(),
            topic: "offset-topic".to_string(),
            partition: 0,
            offset: 1500,
            metadata: Some("checkpoint-2".to_string()),
            commit_timestamp: chrono::Utc::now(),
        };
        
        store.commit_offset(new_offset).await.unwrap();
        
        let updated = store.get_consumer_offset("group1", "offset-topic", 0).await.unwrap().unwrap();
        assert_eq!(updated.offset, 1500);
        assert_eq!(updated.metadata.as_ref().unwrap(), "checkpoint-2");
    }
    
    #[tokio::test]
    async fn test_topic_deletion_cascades() {
        let store = create_test_store().await;
        
        // Create topic with segments
        store.create_topic("cascade-topic", TopicConfig::default()).await.unwrap();
        
        // Add segments
        for i in 0..3 {
            store.persist_segment_metadata(SegmentMetadata {
                segment_id: format!("seg-{:03}", i),
                topic: "cascade-topic".to_string(),
                partition: 0,
                start_offset: i * 1000,
                end_offset: (i + 1) * 1000 - 1,
                size: 1024 * 1024,
                record_count: 1000,
                path: format!("/data/cascade-topic/0/seg-{:03}", i),
                created_at: chrono::Utc::now(),
            }).await.unwrap();
        }
        
        // Verify segments exist
        assert_eq!(store.list_segments("cascade-topic", None).await.unwrap().len(), 3);
        
        // Delete topic
        store.delete_topic("cascade-topic").await.unwrap();
        
        // Verify topic and segments are gone
        assert!(store.get_topic("cascade-topic").await.unwrap().is_none());
        assert_eq!(store.list_segments("cascade-topic", None).await.unwrap().len(), 0);
    }
}