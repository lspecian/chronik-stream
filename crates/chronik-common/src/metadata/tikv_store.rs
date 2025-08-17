//! TiKV-based metadata store implementation.

use super::traits::*;
use async_trait::async_trait;
use tikv_client::RawClient;
use std::sync::Arc;
use uuid::Uuid;
use chrono::Utc;

/// TiKV-based metadata store
pub struct TiKVMetadataStore {
    client: Arc<RawClient>,
}

impl TiKVMetadataStore {
    /// Create a new TiKV metadata store
    pub async fn new(pd_endpoints: Vec<String>) -> Result<Self> {
        let client = RawClient::new(pd_endpoints).await
            .map_err(|e| MetadataError::StorageError(format!("Failed to connect to TiKV: {}", e)))?;
        
        Ok(Self {
            client: Arc::new(client),
        })
    }
    
    /// Create key prefix for different metadata types
    fn topic_key(name: &str) -> Vec<u8> {
        format!("topic:{}", name).into_bytes()
    }
    
    fn segment_key(topic: &str, segment_id: &str) -> Vec<u8> {
        format!("segment:{}:{}", topic, segment_id).into_bytes()
    }
    
    fn broker_key(broker_id: i32) -> Vec<u8> {
        format!("broker:{}", broker_id).into_bytes()
    }
    
    fn partition_assignment_key(topic: &str, partition: u32) -> Vec<u8> {
        format!("partition:{}:{}", topic, partition).into_bytes()
    }
    
    fn consumer_group_key(group_id: &str) -> Vec<u8> {
        format!("group:{}", group_id).into_bytes()
    }
    
    fn consumer_offset_key(group_id: &str, topic: &str, partition: u32) -> Vec<u8> {
        format!("offset:{}:{}:{}", group_id, topic, partition).into_bytes()
    }
    
    /// Serialize value to JSON bytes
    fn serialize<T: serde::Serialize>(value: &T) -> Result<Vec<u8>> {
        serde_json::to_vec(value)
            .map_err(|e| MetadataError::SerializationError(e.to_string()))
    }
    
    /// Deserialize value from JSON bytes
    fn deserialize<T: serde::de::DeserializeOwned>(data: &[u8]) -> Result<T> {
        serde_json::from_slice(data)
            .map_err(|e| MetadataError::SerializationError(e.to_string()))
    }
}

#[async_trait]
impl MetadataStore for TiKVMetadataStore {
    async fn create_topic(&self, name: &str, config: TopicConfig) -> Result<TopicMetadata> {
        let key = Self::topic_key(name);
        
        // Check if topic already exists
        if let Some(_) = self.client.get(key.clone()).await
            .map_err(|e| MetadataError::StorageError(e.to_string()))? {
            return Err(MetadataError::AlreadyExists(format!("Topic {} already exists", name)));
        }
        
        let now = Utc::now();
        let metadata = TopicMetadata {
            id: Uuid::new_v4(),
            name: name.to_string(),
            config,
            created_at: now,
            updated_at: now,
        };
        
        let value = Self::serialize(&metadata)?;
        self.client.put(key, value).await
            .map_err(|e| MetadataError::StorageError(e.to_string()))?;
        
        Ok(metadata)
    }
    
    async fn get_topic(&self, name: &str) -> Result<Option<TopicMetadata>> {
        let key = Self::topic_key(name);
        let value = self.client.get(key).await
            .map_err(|e| MetadataError::StorageError(e.to_string()))?;
        
        match value {
            Some(data) => Ok(Some(Self::deserialize(&data)?)),
            None => Ok(None),
        }
    }
    
    async fn list_topics(&self) -> Result<Vec<TopicMetadata>> {
        let prefix = b"topic:";
        let end = b"topic:\x7f";
        let pairs = self.client.scan(prefix.to_vec()..end.to_vec(), 10000).await
            .map_err(|e| MetadataError::StorageError(e.to_string()))?;
        
        let mut topics = Vec::new();
        for pair in pairs {
            topics.push(Self::deserialize(&pair.1)?);
        }
        
        Ok(topics)
    }
    
    async fn update_topic(&self, name: &str, config: TopicConfig) -> Result<TopicMetadata> {
        let key = Self::topic_key(name);
        let value = self.client.get(key.clone()).await
            .map_err(|e| MetadataError::StorageError(e.to_string()))?
            .ok_or_else(|| MetadataError::NotFound(format!("Topic {} not found", name)))?;
        
        let mut metadata: TopicMetadata = Self::deserialize(&value)?;
        metadata.config = config;
        metadata.updated_at = Utc::now();
        
        let new_value = Self::serialize(&metadata)?;
        self.client.put(key, new_value).await
            .map_err(|e| MetadataError::StorageError(e.to_string()))?;
        
        Ok(metadata)
    }
    
    async fn delete_topic(&self, name: &str) -> Result<()> {
        let key = Self::topic_key(name);
        self.client.delete(key).await
            .map_err(|e| MetadataError::StorageError(e.to_string()))?;
        Ok(())
    }
    
    async fn persist_segment_metadata(&self, metadata: SegmentMetadata) -> Result<()> {
        let key = Self::segment_key(&metadata.topic, &metadata.segment_id);
        let value = Self::serialize(&metadata)?;
        self.client.put(key, value).await
            .map_err(|e| MetadataError::StorageError(e.to_string()))?;
        Ok(())
    }
    
    async fn get_segment_metadata(&self, topic: &str, segment_id: &str) -> Result<Option<SegmentMetadata>> {
        let key = Self::segment_key(topic, segment_id);
        let value = self.client.get(key).await
            .map_err(|e| MetadataError::StorageError(e.to_string()))?;
        
        match value {
            Some(data) => Ok(Some(Self::deserialize(&data)?)),
            None => Ok(None),
        }
    }
    
    async fn list_segments(&self, topic: &str, partition: Option<u32>) -> Result<Vec<SegmentMetadata>> {
        let prefix = format!("segment:{}:", topic).into_bytes();
        let end = format!("segment:{}:\u{ffff}", topic).into_bytes();
        let pairs = self.client.scan(prefix..end, 10000).await
            .map_err(|e| MetadataError::StorageError(e.to_string()))?;
        
        let mut segments = Vec::new();
        for pair in pairs {
            let segment: SegmentMetadata = Self::deserialize(&pair.1)?;
            if partition.is_none() || segment.partition == partition.unwrap() {
                segments.push(segment);
            }
        }
        
        Ok(segments)
    }
    
    async fn delete_segment(&self, topic: &str, segment_id: &str) -> Result<()> {
        let key = Self::segment_key(topic, segment_id);
        self.client.delete(key).await
            .map_err(|e| MetadataError::StorageError(e.to_string()))?;
        Ok(())
    }
    
    async fn register_broker(&self, metadata: BrokerMetadata) -> Result<()> {
        let key = Self::broker_key(metadata.broker_id);
        let value = Self::serialize(&metadata)?;
        self.client.put(key, value).await
            .map_err(|e| MetadataError::StorageError(e.to_string()))?;
        Ok(())
    }
    
    async fn get_broker(&self, broker_id: i32) -> Result<Option<BrokerMetadata>> {
        let key = Self::broker_key(broker_id);
        let value = self.client.get(key).await
            .map_err(|e| MetadataError::StorageError(e.to_string()))?;
        
        match value {
            Some(data) => Ok(Some(Self::deserialize(&data)?)),
            None => Ok(None),
        }
    }
    
    async fn list_brokers(&self) -> Result<Vec<BrokerMetadata>> {
        let prefix = b"broker:";
        let end = b"broker:\x7f";
        let pairs = self.client.scan(prefix.to_vec()..end.to_vec(), 10000).await
            .map_err(|e| MetadataError::StorageError(e.to_string()))?;
        
        let mut brokers = Vec::new();
        for pair in pairs {
            brokers.push(Self::deserialize(&pair.1)?);
        }
        
        Ok(brokers)
    }
    
    async fn update_broker_status(&self, broker_id: i32, status: BrokerStatus) -> Result<()> {
        let key = Self::broker_key(broker_id);
        let value = self.client.get(key.clone()).await
            .map_err(|e| MetadataError::StorageError(e.to_string()))?
            .ok_or_else(|| MetadataError::NotFound(format!("Broker {} not found", broker_id)))?;
        
        let mut metadata: BrokerMetadata = Self::deserialize(&value)?;
        metadata.status = status;
        metadata.updated_at = Utc::now();
        
        let new_value = Self::serialize(&metadata)?;
        self.client.put(key, new_value).await
            .map_err(|e| MetadataError::StorageError(e.to_string()))?;
        
        Ok(())
    }
    
    async fn assign_partition(&self, assignment: PartitionAssignment) -> Result<()> {
        let key = Self::partition_assignment_key(&assignment.topic, assignment.partition);
        let value = Self::serialize(&assignment)?;
        self.client.put(key, value).await
            .map_err(|e| MetadataError::StorageError(e.to_string()))?;
        Ok(())
    }
    
    async fn get_partition_assignments(&self, topic: &str) -> Result<Vec<PartitionAssignment>> {
        let prefix = format!("partition:{}:", topic).into_bytes();
        let end = format!("partition:{}:\u{ffff}", topic).into_bytes();
        let pairs = self.client.scan(prefix..end, 10000).await
            .map_err(|e| MetadataError::StorageError(e.to_string()))?;
        
        let mut assignments = Vec::new();
        for pair in pairs {
            assignments.push(Self::deserialize(&pair.1)?);
        }
        
        Ok(assignments)
    }
    
    async fn create_consumer_group(&self, metadata: ConsumerGroupMetadata) -> Result<()> {
        let key = Self::consumer_group_key(&metadata.group_id);
        let value = Self::serialize(&metadata)?;
        self.client.put(key, value).await
            .map_err(|e| MetadataError::StorageError(e.to_string()))?;
        Ok(())
    }
    
    async fn get_consumer_group(&self, group_id: &str) -> Result<Option<ConsumerGroupMetadata>> {
        let key = Self::consumer_group_key(group_id);
        let value = self.client.get(key).await
            .map_err(|e| MetadataError::StorageError(e.to_string()))?;
        
        match value {
            Some(data) => Ok(Some(Self::deserialize(&data)?)),
            None => Ok(None),
        }
    }
    
    async fn update_consumer_group(&self, metadata: ConsumerGroupMetadata) -> Result<()> {
        let key = Self::consumer_group_key(&metadata.group_id);
        let value = Self::serialize(&metadata)?;
        self.client.put(key, value).await
            .map_err(|e| MetadataError::StorageError(e.to_string()))?;
        Ok(())
    }
    
    async fn commit_offset(&self, offset: ConsumerOffset) -> Result<()> {
        let key = Self::consumer_offset_key(&offset.group_id, &offset.topic, offset.partition);
        let value = Self::serialize(&offset)?;
        self.client.put(key, value).await
            .map_err(|e| MetadataError::StorageError(e.to_string()))?;
        Ok(())
    }
    
    async fn get_consumer_offset(&self, group_id: &str, topic: &str, partition: u32) -> Result<Option<ConsumerOffset>> {
        let key = Self::consumer_offset_key(group_id, topic, partition);
        let value = self.client.get(key).await
            .map_err(|e| MetadataError::StorageError(e.to_string()))?;
        
        match value {
            Some(data) => Ok(Some(Self::deserialize(&data)?)),
            None => Ok(None),
        }
    }
    
    async fn update_partition_offset(&self, topic: &str, partition: u32, high_watermark: i64, log_start_offset: i64) -> Result<()> {
        let key = format!("partition:{}:{}:offset", topic, partition).into_bytes();
        let value = bincode::serialize(&(high_watermark, log_start_offset))
            .map_err(|e| MetadataError::SerializationError(e.to_string()))?;
        self.client.put(key, value).await
            .map_err(|e| MetadataError::StorageError(e.to_string()))?;
        Ok(())
    }
    
    async fn get_partition_offset(&self, topic: &str, partition: u32) -> Result<Option<(i64, i64)>> {
        let key = format!("partition:{}:{}:offset", topic, partition).into_bytes();
        let value = self.client.get(key).await
            .map_err(|e| MetadataError::StorageError(e.to_string()))?;
        
        match value {
            Some(data) => {
                let offsets = bincode::deserialize(&data)
                    .map_err(|e| MetadataError::SerializationError(e.to_string()))?;
                Ok(Some(offsets))
            }
            None => Ok(None),
        }
    }
    
    async fn init_system_state(&self) -> Result<()> {
        // Create default system topics if they don't exist
        let system_topics = vec![
            ("__consumer_offsets", 50, 3),
            ("__transaction_state", 50, 3),
        ];
        
        for (name, partitions, replication_factor) in system_topics {
            if self.get_topic(name).await?.is_none() {
                let config = TopicConfig {
                    partition_count: partitions,
                    replication_factor,
                    retention_ms: Some(7 * 24 * 60 * 60 * 1000), // 7 days
                    segment_bytes: 100 * 1024 * 1024, // 100MB
                    config: std::collections::HashMap::new(),
                };
                self.create_topic(name, config).await?;
            }
        }
        
        Ok(())
    }
    
    async fn create_topic_with_assignments(&self, 
        topic_name: &str, 
        config: TopicConfig,
        assignments: Vec<PartitionAssignment>,
        offsets: Vec<(u32, i64, i64)>
    ) -> Result<TopicMetadata> {
        // For now, implement this as a sequence of operations
        // In a production system, we would use TiKV transactions for true atomicity
        
        let now = Utc::now();
        let metadata = TopicMetadata {
            id: Uuid::new_v4(),
            name: topic_name.to_string(),
            config,
            created_at: now,
            updated_at: now,
        };
        
        // Create topic metadata
        let key = Self::topic_key(topic_name);
        if let Some(_) = self.client.get(key.clone()).await
            .map_err(|e| MetadataError::StorageError(e.to_string()))? {
            return Err(MetadataError::AlreadyExists(format!("Topic {} already exists", topic_name)));
        }
        
        let value = Self::serialize(&metadata)?;
        self.client.put(key, value).await
            .map_err(|e| MetadataError::StorageError(e.to_string()))?;
        
        // Create partition assignments
        for assignment in &assignments {
            let assignment_key = Self::partition_assignment_key(&assignment.topic, assignment.partition);
            let assignment_value = Self::serialize(assignment)?;
            self.client.put(assignment_key, assignment_value).await
                .map_err(|e| MetadataError::StorageError(e.to_string()))?;
        }
        
        // Initialize partition offsets
        for (partition, high_watermark, log_start_offset) in offsets {
            let offset_key = format!("partition:{}:{}:offset", topic_name, partition).into_bytes();
            let offset_value = bincode::serialize(&(high_watermark, log_start_offset))
                .map_err(|e| MetadataError::SerializationError(e.to_string()))?;
            self.client.put(offset_key, offset_value).await
                .map_err(|e| MetadataError::StorageError(e.to_string()))?;
        }
        
        Ok(metadata)
    }
}