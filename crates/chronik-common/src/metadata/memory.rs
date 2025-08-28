//! In-memory implementation of MetadataStore for testing

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use super::traits::*;

/// In-memory implementation of MetadataStore for testing
pub struct InMemoryMetadataStore {
    topics: Arc<RwLock<HashMap<String, TopicMetadata>>>,
    brokers: Arc<RwLock<HashMap<i32, BrokerMetadata>>>,
    consumer_groups: Arc<RwLock<HashMap<String, ConsumerGroupMetadata>>>,
    consumer_offsets: Arc<RwLock<HashMap<(String, String, u32), ConsumerOffset>>>,
    partition_assignments: Arc<RwLock<HashMap<(String, u32), PartitionAssignment>>>,
    segments: Arc<RwLock<HashMap<(String, String), SegmentMetadata>>>,
    partition_offsets: Arc<RwLock<HashMap<(String, u32), (i64, i64)>>>,
}

impl InMemoryMetadataStore {
    pub fn new() -> Self {
        Self {
            topics: Arc::new(RwLock::new(HashMap::new())),
            brokers: Arc::new(RwLock::new(HashMap::new())),
            consumer_groups: Arc::new(RwLock::new(HashMap::new())),
            consumer_offsets: Arc::new(RwLock::new(HashMap::new())),
            partition_assignments: Arc::new(RwLock::new(HashMap::new())),
            segments: Arc::new(RwLock::new(HashMap::new())),
            partition_offsets: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

#[async_trait]
impl MetadataStore for InMemoryMetadataStore {
    async fn create_topic(&self, name: &str, config: TopicConfig) -> Result<TopicMetadata> {
        let mut topics = self.topics.write().await;
        
        if topics.contains_key(name) {
            return Err(MetadataError::AlreadyExists(format!("Topic {} already exists", name)));
        }
        
        let metadata = TopicMetadata {
            name: name.to_string(),
            id: uuid::Uuid::new_v4(),
            config: config.clone(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        
        topics.insert(name.to_string(), metadata.clone());
        drop(topics); // Release the lock
        
        // Create partition assignments - for now, make the current broker (node 1) the leader for all partitions
        let mut partition_assignments = self.partition_assignments.write().await;
        for partition in 0..config.partition_count {
            let assignment = PartitionAssignment {
                topic: name.to_string(),
                partition,
                broker_id: 1, // Use broker ID 1 (the current node)
                is_leader: true,
            };
            let key = (name.to_string(), partition);
            partition_assignments.insert(key, assignment);
        }
        drop(partition_assignments);
        
        // Initialize partition offsets
        let mut partition_offsets = self.partition_offsets.write().await;
        for partition in 0..config.partition_count {
            let key = (name.to_string(), partition);
            partition_offsets.insert(key, (0, 0)); // Start with offset 0
        }
        
        Ok(metadata)
    }
    
    async fn get_topic(&self, name: &str) -> Result<Option<TopicMetadata>> {
        let topics = self.topics.read().await;
        Ok(topics.get(name).cloned())
    }
    
    async fn list_topics(&self) -> Result<Vec<TopicMetadata>> {
        let topics = self.topics.read().await;
        Ok(topics.values().cloned().collect())
    }
    
    async fn update_topic(&self, name: &str, config: TopicConfig) -> Result<TopicMetadata> {
        let mut topics = self.topics.write().await;
        
        if let Some(metadata) = topics.get_mut(name) {
            metadata.config = config;
            metadata.updated_at = chrono::Utc::now();
            Ok(metadata.clone())
        } else {
            Err(MetadataError::NotFound(format!("Topic {} not found", name)))
        }
    }
    
    async fn delete_topic(&self, name: &str) -> Result<()> {
        let mut topics = self.topics.write().await;
        topics.remove(name)
            .ok_or_else(|| MetadataError::NotFound(format!("Topic {} not found", name)))?;
        Ok(())
    }
    
    async fn persist_segment_metadata(&self, metadata: SegmentMetadata) -> Result<()> {
        let mut segments = self.segments.write().await;
        let key = (metadata.topic.clone(), metadata.segment_id.clone());
        segments.insert(key, metadata);
        Ok(())
    }
    
    async fn get_segment_metadata(&self, topic: &str, segment_id: &str) -> Result<Option<SegmentMetadata>> {
        let segments = self.segments.read().await;
        let key = (topic.to_string(), segment_id.to_string());
        Ok(segments.get(&key).cloned())
    }
    
    async fn list_segments(&self, topic: &str, partition: Option<u32>) -> Result<Vec<SegmentMetadata>> {
        let segments = self.segments.read().await;
        Ok(segments.values()
            .filter(|s| s.topic == topic && (partition.is_none() || s.partition == partition.unwrap()))
            .cloned()
            .collect())
    }
    
    async fn delete_segment(&self, topic: &str, segment_id: &str) -> Result<()> {
        let mut segments = self.segments.write().await;
        let key = (topic.to_string(), segment_id.to_string());
        segments.remove(&key)
            .ok_or_else(|| MetadataError::NotFound(format!("Segment {} not found", segment_id)))?;
        Ok(())
    }
    
    async fn register_broker(&self, broker: BrokerMetadata) -> Result<()> {
        let mut brokers = self.brokers.write().await;
        brokers.insert(broker.broker_id, broker);
        Ok(())
    }
    
    async fn get_broker(&self, id: i32) -> Result<Option<BrokerMetadata>> {
        let brokers = self.brokers.read().await;
        Ok(brokers.get(&id).cloned())
    }
    
    async fn list_brokers(&self) -> Result<Vec<BrokerMetadata>> {
        let brokers = self.brokers.read().await;
        Ok(brokers.values().cloned().collect())
    }
    
    async fn update_broker_status(&self, broker_id: i32, status: BrokerStatus) -> Result<()> {
        let mut brokers = self.brokers.write().await;
        if let Some(broker) = brokers.get_mut(&broker_id) {
            broker.status = status;
            broker.updated_at = chrono::Utc::now();
            Ok(())
        } else {
            Err(MetadataError::NotFound(format!("Broker {} not found", broker_id)))
        }
    }
    
    async fn assign_partition(&self, assignment: PartitionAssignment) -> Result<()> {
        let mut assignments = self.partition_assignments.write().await;
        let key = (assignment.topic.clone(), assignment.partition);
        assignments.insert(key, assignment);
        Ok(())
    }
    
    async fn get_partition_assignments(&self, topic: &str) -> Result<Vec<PartitionAssignment>> {
        let assignments = self.partition_assignments.read().await;
        Ok(assignments.values()
            .filter(|a| a.topic == topic)
            .cloned()
            .collect())
    }
    
    async fn create_consumer_group(&self, group: ConsumerGroupMetadata) -> Result<()> {
        let mut groups = self.consumer_groups.write().await;
        
        if groups.contains_key(&group.group_id) {
            return Err(MetadataError::AlreadyExists(format!("Consumer group {} already exists", group.group_id)));
        }
        
        groups.insert(group.group_id.clone(), group);
        Ok(())
    }
    
    async fn get_consumer_group(&self, group_id: &str) -> Result<Option<ConsumerGroupMetadata>> {
        let groups = self.consumer_groups.read().await;
        Ok(groups.get(group_id).cloned())
    }
    
    async fn update_consumer_group(&self, group: ConsumerGroupMetadata) -> Result<()> {
        let mut groups = self.consumer_groups.write().await;
        groups.insert(group.group_id.clone(), group);
        Ok(())
    }
    
    async fn commit_offset(&self, offset: ConsumerOffset) -> Result<()> {
        let mut offsets = self.consumer_offsets.write().await;
        let key = (offset.group_id.clone(), offset.topic.clone(), offset.partition);
        offsets.insert(key, offset);
        Ok(())
    }
    
    async fn get_consumer_offset(&self, group_id: &str, topic: &str, partition: u32) -> Result<Option<ConsumerOffset>> {
        let offsets = self.consumer_offsets.read().await;
        let key = (group_id.to_string(), topic.to_string(), partition);
        Ok(offsets.get(&key).cloned())
    }
    
    async fn update_partition_offset(&self, topic: &str, partition: u32, high_watermark: i64, log_start_offset: i64) -> Result<()> {
        let mut offsets = self.partition_offsets.write().await;
        let key = (topic.to_string(), partition);
        offsets.insert(key, (high_watermark, log_start_offset));
        Ok(())
    }
    
    async fn get_partition_offset(&self, topic: &str, partition: u32) -> Result<Option<(i64, i64)>> {
        let offsets = self.partition_offsets.read().await;
        let key = (topic.to_string(), partition);
        Ok(offsets.get(&key).cloned())
    }
    
    async fn init_system_state(&self) -> Result<()> {
        // No-op for in-memory store
        Ok(())
    }
    
    async fn create_topic_with_assignments(&self, 
        topic_name: &str, 
        config: TopicConfig,
        assignments: Vec<PartitionAssignment>,
        offsets: Vec<(u32, i64, i64)>
    ) -> Result<TopicMetadata> {
        // For in-memory store, all operations are naturally atomic within each async block
        
        // Check if topic already exists
        {
            let topics = self.topics.read().await;
            if topics.contains_key(topic_name) {
                return Err(MetadataError::AlreadyExists(format!("Topic {} already exists", topic_name)));
            }
        }
        
        let metadata = TopicMetadata {
            name: topic_name.to_string(),
            id: uuid::Uuid::new_v4(),
            config,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        
        // Create topic metadata
        {
            let mut topics = self.topics.write().await;
            topics.insert(topic_name.to_string(), metadata.clone());
        }
        
        // Create partition assignments
        {
            let mut partition_assignments = self.partition_assignments.write().await;
            for assignment in assignments {
                let key = (assignment.topic.clone(), assignment.partition);
                partition_assignments.insert(key, assignment);
            }
        }
        
        // Initialize partition offsets
        {
            let mut partition_offsets = self.partition_offsets.write().await;
            for (partition, high_watermark, log_start_offset) in offsets {
                let key = (topic_name.to_string(), partition);
                partition_offsets.insert(key, (high_watermark, log_start_offset));
            }
        }
        
        Ok(metadata)
    }
}