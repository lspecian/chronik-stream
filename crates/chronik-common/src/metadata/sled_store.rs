//! Sled-based implementation of MetadataStore.

use super::traits::*;
use crate::Uuid;
use async_trait::async_trait;
use serde::{Serialize, Deserialize};
use sled::{Db, Tree};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

/// Sled-based metadata store
pub struct SledMetadataStore {
    db: Arc<Db>,
    topics_tree: Arc<Tree>,
    segments_tree: Arc<Tree>,
    brokers_tree: Arc<Tree>,
    assignments_tree: Arc<Tree>,
    groups_tree: Arc<Tree>,
    offsets_tree: Arc<Tree>,
}

impl SledMetadataStore {
    /// Create a new Sled-based metadata store
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self> {
        let db = sled::open(path)
            .map_err(|e| MetadataError::StorageError(format!("Failed to open sled database: {}", e)))?;
        
        let db = Arc::new(db);
        
        // Open trees for different metadata types
        let topics_tree = Arc::new(
            db.open_tree("topics")
                .map_err(|e| MetadataError::StorageError(format!("Failed to open topics tree: {}", e)))?
        );
        
        let segments_tree = Arc::new(
            db.open_tree("segments")
                .map_err(|e| MetadataError::StorageError(format!("Failed to open segments tree: {}", e)))?
        );
        
        let brokers_tree = Arc::new(
            db.open_tree("brokers")
                .map_err(|e| MetadataError::StorageError(format!("Failed to open brokers tree: {}", e)))?
        );
        
        let assignments_tree = Arc::new(
            db.open_tree("assignments")
                .map_err(|e| MetadataError::StorageError(format!("Failed to open assignments tree: {}", e)))?
        );
        
        let groups_tree = Arc::new(
            db.open_tree("groups")
                .map_err(|e| MetadataError::StorageError(format!("Failed to open groups tree: {}", e)))?
        );
        
        let offsets_tree = Arc::new(
            db.open_tree("offsets")
                .map_err(|e| MetadataError::StorageError(format!("Failed to open offsets tree: {}", e)))?
        );
        
        Ok(Self {
            db,
            topics_tree,
            segments_tree,
            brokers_tree,
            assignments_tree,
            groups_tree,
            offsets_tree,
        })
    }
    
    /// Generate a topic key
    fn topic_key(name: &str) -> Vec<u8> {
        format!("topic/{}", name).into_bytes()
    }
    
    /// Generate a segment key
    fn segment_key(topic: &str, segment_id: &str) -> Vec<u8> {
        format!("segment/{}/{}", topic, segment_id).into_bytes()
    }
    
    /// Generate a broker key
    fn broker_key(broker_id: i32) -> Vec<u8> {
        format!("broker/{}", broker_id).into_bytes()
    }
    
    /// Generate a partition assignment key
    fn assignment_key(topic: &str, partition: u32) -> Vec<u8> {
        format!("assignment/{}/{}", topic, partition).into_bytes()
    }
    
    /// Generate a consumer group key
    fn group_key(group_id: &str) -> Vec<u8> {
        format!("group/{}", group_id).into_bytes()
    }
    
    /// Generate a consumer offset key
    fn offset_key(group_id: &str, topic: &str, partition: u32) -> Vec<u8> {
        format!("offset/{}/{}/{}", group_id, topic, partition).into_bytes()
    }
    
    /// Serialize value to bytes
    fn serialize<T: Serialize>(value: &T) -> Result<Vec<u8>> {
        bincode::serialize(value)
            .map_err(|e| MetadataError::SerializationError(format!("Failed to serialize: {}", e)))
    }
    
    /// Deserialize value from bytes
    fn deserialize<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T> {
        bincode::deserialize(bytes)
            .map_err(|e| MetadataError::SerializationError(format!("Failed to deserialize: {}", e)))
    }
}

#[async_trait]
impl MetadataStore for SledMetadataStore {
    async fn create_topic(&self, name: &str, config: TopicConfig) -> Result<TopicMetadata> {
        let key = Self::topic_key(name);
        
        // Check if topic already exists
        if self.topics_tree.contains_key(&key)
            .map_err(|e| MetadataError::StorageError(e.to_string()))? {
            return Err(MetadataError::AlreadyExists(format!("Topic {} already exists", name)));
        }
        
        let now = chrono::Utc::now();
        let metadata = TopicMetadata {
            id: Uuid::new_v4(),
            name: name.to_string(),
            config,
            created_at: now,
            updated_at: now,
        };
        
        let value = Self::serialize(&metadata)?;
        self.topics_tree.insert(key, value)
            .map_err(|e| MetadataError::StorageError(e.to_string()))?;
        
        Ok(metadata)
    }
    
    async fn get_topic(&self, name: &str) -> Result<Option<TopicMetadata>> {
        let key = Self::topic_key(name);
        
        match self.topics_tree.get(&key)
            .map_err(|e| MetadataError::StorageError(e.to_string()))? {
            Some(bytes) => {
                let metadata = Self::deserialize(&bytes)?;
                Ok(Some(metadata))
            }
            None => Ok(None),
        }
    }
    
    async fn list_topics(&self) -> Result<Vec<TopicMetadata>> {
        let mut topics = Vec::new();
        
        for result in self.topics_tree.iter() {
            let (_, value) = result
                .map_err(|e| MetadataError::StorageError(e.to_string()))?;
            let metadata: TopicMetadata = Self::deserialize(&value)?;
            topics.push(metadata);
        }
        
        Ok(topics)
    }
    
    async fn update_topic(&self, name: &str, config: TopicConfig) -> Result<TopicMetadata> {
        let key = Self::topic_key(name);
        
        let existing = self.topics_tree.get(&key)
            .map_err(|e| MetadataError::StorageError(e.to_string()))?
            .ok_or_else(|| MetadataError::NotFound(format!("Topic {} not found", name)))?;
        
        let mut metadata: TopicMetadata = Self::deserialize(&existing)?;
        metadata.config = config;
        metadata.updated_at = chrono::Utc::now();
        
        let value = Self::serialize(&metadata)?;
        self.topics_tree.insert(key, value)
            .map_err(|e| MetadataError::StorageError(e.to_string()))?;
        
        Ok(metadata)
    }
    
    async fn delete_topic(&self, name: &str) -> Result<()> {
        let key = Self::topic_key(name);
        
        self.topics_tree.remove(&key)
            .map_err(|e| MetadataError::StorageError(e.to_string()))?
            .ok_or_else(|| MetadataError::NotFound(format!("Topic {} not found", name)))?;
        
        // Also delete all segments for this topic
        let prefix = format!("segment/{}/", name);
        let prefix_bytes = prefix.as_bytes();
        
        let mut keys_to_delete = Vec::new();
        for result in self.segments_tree.scan_prefix(prefix_bytes) {
            let (key, _) = result
                .map_err(|e| MetadataError::StorageError(e.to_string()))?;
            keys_to_delete.push(key);
        }
        
        for key in keys_to_delete {
            self.segments_tree.remove(key)
                .map_err(|e| MetadataError::StorageError(e.to_string()))?;
        }
        
        Ok(())
    }
    
    async fn persist_segment_metadata(&self, metadata: SegmentMetadata) -> Result<()> {
        let key = Self::segment_key(&metadata.topic, &metadata.segment_id);
        let value = Self::serialize(&metadata)?;
        
        self.segments_tree.insert(key, value)
            .map_err(|e| MetadataError::StorageError(e.to_string()))?;
        
        Ok(())
    }
    
    async fn get_segment_metadata(&self, topic: &str, segment_id: &str) -> Result<Option<SegmentMetadata>> {
        let key = Self::segment_key(topic, segment_id);
        
        match self.segments_tree.get(&key)
            .map_err(|e| MetadataError::StorageError(e.to_string()))? {
            Some(bytes) => {
                let metadata = Self::deserialize(&bytes)?;
                Ok(Some(metadata))
            }
            None => Ok(None),
        }
    }
    
    async fn list_segments(&self, topic: &str, partition: Option<u32>) -> Result<Vec<SegmentMetadata>> {
        let prefix = format!("segment/{}/", topic);
        let prefix_bytes = prefix.as_bytes();
        
        let mut segments = Vec::new();
        
        for result in self.segments_tree.scan_prefix(prefix_bytes) {
            let (_, value) = result
                .map_err(|e| MetadataError::StorageError(e.to_string()))?;
            let metadata: SegmentMetadata = Self::deserialize(&value)?;
            
            // Filter by partition if specified
            if let Some(p) = partition {
                if metadata.partition == p {
                    segments.push(metadata);
                }
            } else {
                segments.push(metadata);
            }
        }
        
        Ok(segments)
    }
    
    async fn delete_segment(&self, topic: &str, segment_id: &str) -> Result<()> {
        let key = Self::segment_key(topic, segment_id);
        
        self.segments_tree.remove(&key)
            .map_err(|e| MetadataError::StorageError(e.to_string()))?
            .ok_or_else(|| MetadataError::NotFound(format!("Segment {} not found", segment_id)))?;
        
        Ok(())
    }
    
    async fn register_broker(&self, metadata: BrokerMetadata) -> Result<()> {
        let key = Self::broker_key(metadata.broker_id);
        let value = Self::serialize(&metadata)?;
        
        self.brokers_tree.insert(key, value)
            .map_err(|e| MetadataError::StorageError(e.to_string()))?;
        
        Ok(())
    }
    
    async fn get_broker(&self, broker_id: i32) -> Result<Option<BrokerMetadata>> {
        let key = Self::broker_key(broker_id);
        
        match self.brokers_tree.get(&key)
            .map_err(|e| MetadataError::StorageError(e.to_string()))? {
            Some(bytes) => {
                let metadata = Self::deserialize(&bytes)?;
                Ok(Some(metadata))
            }
            None => Ok(None),
        }
    }
    
    async fn list_brokers(&self) -> Result<Vec<BrokerMetadata>> {
        let mut brokers = Vec::new();
        
        for result in self.brokers_tree.iter() {
            let (_, value) = result
                .map_err(|e| MetadataError::StorageError(e.to_string()))?;
            let metadata: BrokerMetadata = Self::deserialize(&value)?;
            brokers.push(metadata);
        }
        
        Ok(brokers)
    }
    
    async fn update_broker_status(&self, broker_id: i32, status: BrokerStatus) -> Result<()> {
        let key = Self::broker_key(broker_id);
        
        let existing = self.brokers_tree.get(&key)
            .map_err(|e| MetadataError::StorageError(e.to_string()))?
            .ok_or_else(|| MetadataError::NotFound(format!("Broker {} not found", broker_id)))?;
        
        let mut metadata: BrokerMetadata = Self::deserialize(&existing)?;
        metadata.status = status;
        metadata.updated_at = chrono::Utc::now();
        
        let value = Self::serialize(&metadata)?;
        self.brokers_tree.insert(key, value)
            .map_err(|e| MetadataError::StorageError(e.to_string()))?;
        
        Ok(())
    }
    
    async fn assign_partition(&self, assignment: PartitionAssignment) -> Result<()> {
        let key = Self::assignment_key(&assignment.topic, assignment.partition);
        let value = Self::serialize(&assignment)?;
        
        self.assignments_tree.insert(key, value)
            .map_err(|e| MetadataError::StorageError(e.to_string()))?;
        
        Ok(())
    }
    
    async fn get_partition_assignments(&self, topic: &str) -> Result<Vec<PartitionAssignment>> {
        let prefix = format!("assignment/{}/", topic);
        let prefix_bytes = prefix.as_bytes();
        
        let mut assignments = Vec::new();
        
        for result in self.assignments_tree.scan_prefix(prefix_bytes) {
            let (_, value) = result
                .map_err(|e| MetadataError::StorageError(e.to_string()))?;
            let assignment: PartitionAssignment = Self::deserialize(&value)?;
            assignments.push(assignment);
        }
        
        Ok(assignments)
    }
    
    async fn create_consumer_group(&self, metadata: ConsumerGroupMetadata) -> Result<()> {
        let key = Self::group_key(&metadata.group_id);
        let value = Self::serialize(&metadata)?;
        
        self.groups_tree.insert(key, value)
            .map_err(|e| MetadataError::StorageError(e.to_string()))?;
        
        Ok(())
    }
    
    async fn get_consumer_group(&self, group_id: &str) -> Result<Option<ConsumerGroupMetadata>> {
        let key = Self::group_key(group_id);
        
        match self.groups_tree.get(&key)
            .map_err(|e| MetadataError::StorageError(e.to_string()))? {
            Some(bytes) => {
                let metadata = Self::deserialize(&bytes)?;
                Ok(Some(metadata))
            }
            None => Ok(None),
        }
    }
    
    async fn update_consumer_group(&self, metadata: ConsumerGroupMetadata) -> Result<()> {
        let key = Self::group_key(&metadata.group_id);
        let value = Self::serialize(&metadata)?;
        
        self.groups_tree.insert(key, value)
            .map_err(|e| MetadataError::StorageError(e.to_string()))?;
        
        Ok(())
    }
    
    async fn commit_offset(&self, offset: ConsumerOffset) -> Result<()> {
        let key = Self::offset_key(&offset.group_id, &offset.topic, offset.partition);
        let value = Self::serialize(&offset)?;
        
        self.offsets_tree.insert(key, value)
            .map_err(|e| MetadataError::StorageError(e.to_string()))?;
        
        Ok(())
    }
    
    async fn get_consumer_offset(&self, group_id: &str, topic: &str, partition: u32) -> Result<Option<ConsumerOffset>> {
        let key = Self::offset_key(group_id, topic, partition);
        
        match self.offsets_tree.get(&key)
            .map_err(|e| MetadataError::StorageError(e.to_string()))? {
            Some(bytes) => {
                let offset = Self::deserialize(&bytes)?;
                Ok(Some(offset))
            }
            None => Ok(None),
        }
    }
    
    async fn init_system_state(&self) -> Result<()> {
        // Create default internal topics if they don't exist
        let internal_topics = vec![
            ("__consumer_offsets", TopicConfig {
                partition_count: 50,
                replication_factor: 3,
                retention_ms: Some(7 * 24 * 60 * 60 * 1000), // 7 days
                segment_bytes: 100 * 1024 * 1024, // 100MB
                config: HashMap::new(),
            }),
            ("__transaction_state", TopicConfig {
                partition_count: 50,
                replication_factor: 3,
                retention_ms: Some(7 * 24 * 60 * 60 * 1000), // 7 days
                segment_bytes: 100 * 1024 * 1024, // 100MB
                config: HashMap::new(),
            }),
        ];
        
        for (name, config) in internal_topics {
            // Check if topic already exists
            if self.get_topic(name).await?.is_none() {
                self.create_topic(name, config).await?;
            }
        }
        
        // Ensure the database is flushed
        self.db.flush()
            .map_err(|e| MetadataError::StorageError(format!("Failed to flush database: {}", e)))?;
        
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    
    #[tokio::test]
    async fn test_topic_operations() {
        let temp_dir = TempDir::new().unwrap();
        let store = SledMetadataStore::new(temp_dir.path()).unwrap();
        
        // Test create topic
        let config = TopicConfig::default();
        let topic = store.create_topic("test-topic", config.clone()).await.unwrap();
        assert_eq!(topic.name, "test-topic");
        
        // Test get topic
        let retrieved = store.get_topic("test-topic").await.unwrap().unwrap();
        assert_eq!(retrieved.name, "test-topic");
        
        // Test list topics
        let topics = store.list_topics().await.unwrap();
        assert_eq!(topics.len(), 1);
        
        // Test update topic
        let mut new_config = config.clone();
        new_config.partition_count = 10;
        let updated = store.update_topic("test-topic", new_config).await.unwrap();
        assert_eq!(updated.config.partition_count, 10);
        
        // Test delete topic
        store.delete_topic("test-topic").await.unwrap();
        assert!(store.get_topic("test-topic").await.unwrap().is_none());
    }
    
    #[tokio::test]
    async fn test_segment_operations() {
        let temp_dir = TempDir::new().unwrap();
        let store = SledMetadataStore::new(temp_dir.path()).unwrap();
        
        // Create a topic first
        store.create_topic("test-topic", TopicConfig::default()).await.unwrap();
        
        // Test persist segment
        let segment = SegmentMetadata {
            segment_id: "seg-1".to_string(),
            topic: "test-topic".to_string(),
            partition: 0,
            start_offset: 0,
            end_offset: 100,
            size: 1024,
            record_count: 50,
            path: "/data/seg-1".to_string(),
            created_at: chrono::Utc::now(),
        };
        
        store.persist_segment_metadata(segment.clone()).await.unwrap();
        
        // Test get segment
        let retrieved = store.get_segment_metadata("test-topic", "seg-1").await.unwrap().unwrap();
        assert_eq!(retrieved.segment_id, "seg-1");
        
        // Test list segments
        let segments = store.list_segments("test-topic", None).await.unwrap();
        assert_eq!(segments.len(), 1);
        
        // Test delete segment
        store.delete_segment("test-topic", "seg-1").await.unwrap();
        assert!(store.get_segment_metadata("test-topic", "seg-1").await.unwrap().is_none());
    }
    
    #[tokio::test]
    async fn test_init_system_state() {
        let temp_dir = TempDir::new().unwrap();
        let store = SledMetadataStore::new(temp_dir.path()).unwrap();
        
        // Initialize system state
        store.init_system_state().await.unwrap();
        
        // Check internal topics were created
        let topics = store.list_topics().await.unwrap();
        assert!(topics.iter().any(|t| t.name == "__consumer_offsets"));
        assert!(topics.iter().any(|t| t.name == "__transaction_state"));
    }
}