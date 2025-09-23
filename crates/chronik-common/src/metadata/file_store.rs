//! Simple file-based metadata store for persistence without complex dependencies
//! 
//! This stores all metadata in a single JSON file that is loaded on startup
//! and persisted on every change. Simple, reliable, and no external dependencies.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::fs;
use tracing::{debug, error, info, warn};

use super::traits::*;

/// Complete metadata state that can be serialized to disk
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct MetadataState {
    topics: HashMap<String, TopicMetadata>,
    brokers: HashMap<i32, BrokerMetadata>,
    consumer_groups: HashMap<String, ConsumerGroupMetadata>,
    consumer_offsets: HashMap<String, ConsumerOffset>, // Serialized key: "group:topic:partition"
    partition_assignments: HashMap<String, PartitionAssignment>, // Serialized key: "topic:partition"
    segments: HashMap<String, SegmentMetadata>, // Serialized key: "topic:segment_id"
    partition_offsets: HashMap<String, (i64, i64)>, // Serialized key: "topic:partition"
    #[serde(default)]
    version: u32,
}

/// File-based implementation of MetadataStore
pub struct FileMetadataStore {
    /// In-memory state
    state: Arc<RwLock<MetadataState>>,
    /// Path to metadata file
    metadata_path: PathBuf,
}

impl FileMetadataStore {
    /// Create a new file-based metadata store
    pub async fn new(data_dir: impl AsRef<Path>) -> Result<Self> {
        let metadata_path = data_dir.as_ref().join("metadata.json");
        
        // Create data directory if it doesn't exist
        if let Some(parent) = metadata_path.parent() {
            fs::create_dir_all(parent).await
                .map_err(|e| MetadataError::StorageError(format!("Failed to create data directory: {}", e)))?;
        }
        
        // Load existing metadata or create new
        let state = if metadata_path.exists() {
            match fs::read_to_string(&metadata_path).await {
                Ok(content) => {
                    match serde_json::from_str::<MetadataState>(&content) {
                        Ok(mut state) => {
                            state.version += 1;
                            info!("Loaded metadata from {} (version {})", metadata_path.display(), state.version);
                            info!("  Topics: {}", state.topics.len());
                            info!("  Brokers: {}", state.brokers.len());
                            info!("  Consumer groups: {}", state.consumer_groups.len());
                            info!("  Segments: {}", state.segments.len());
                            state
                        }
                        Err(e) => {
                            error!("Failed to parse metadata file: {}", e);
                            warn!("Starting with fresh metadata");
                            MetadataState::default()
                        }
                    }
                }
                Err(e) => {
                    error!("Failed to read metadata file: {}", e);
                    warn!("Starting with fresh metadata");
                    MetadataState::default()
                }
            }
        } else {
            info!("No existing metadata found at {}, starting fresh", metadata_path.display());
            MetadataState::default()
        };
        
        let store = Self {
            state: Arc::new(RwLock::new(state)),
            metadata_path,
        };
        
        // Persist initial state
        store.persist().await?;
        
        Ok(store)
    }
    
    /// Persist current state to disk
    async fn persist(&self) -> Result<()> {
        let state = self.state.read().await;
        
        let json = serde_json::to_string_pretty(&*state)
            .map_err(|e| MetadataError::SerializationError(format!("Failed to serialize metadata: {}", e)))?;
        
        // Write to temporary file first, then atomic rename
        let temp_path = self.metadata_path.with_extension("tmp");
        fs::write(&temp_path, json).await
            .map_err(|e| MetadataError::StorageError(format!("Failed to write metadata: {}", e)))?;
        
        fs::rename(&temp_path, &self.metadata_path).await
            .map_err(|e| MetadataError::StorageError(format!("Failed to rename metadata file: {}", e)))?;
        
        debug!("Persisted metadata to {}", self.metadata_path.display());
        Ok(())
    }
}

#[async_trait]
impl MetadataStore for FileMetadataStore {
    async fn create_topic(&self, name: &str, config: TopicConfig) -> Result<TopicMetadata> {
        let mut state = self.state.write().await;
        
        if state.topics.contains_key(name) {
            return Err(MetadataError::AlreadyExists(format!("Topic {} already exists", name)));
        }
        
        let metadata = TopicMetadata {
            name: name.to_string(),
            id: uuid::Uuid::new_v4(),
            config: config.clone(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        
        state.topics.insert(name.to_string(), metadata.clone());
        
        // Create partition assignments - for now, make the current broker (node 1) the leader for all partitions
        for partition in 0..config.partition_count {
            let assignment = PartitionAssignment {
                topic: name.to_string(),
                partition,
                broker_id: 1, // Use broker ID 1 (the current node)
                is_leader: true,
            };
            // Use topic-partition as string key for file store
            let key = format!("{}-{}", name, partition);
            state.partition_assignments.insert(key, assignment);
        }
        
        // Initialize partition offsets
        for partition in 0..config.partition_count {
            // Use topic-partition as string key for file store
            let key = format!("{}-{}", name, partition);
            state.partition_offsets.insert(key, (0, 0)); // Start with offset 0
        }
        
        drop(state);
        
        self.persist().await?;
        
        info!("Created topic {} with {} partitions", name, metadata.config.partition_count);
        Ok(metadata)
    }
    
    async fn get_topic(&self, name: &str) -> Result<Option<TopicMetadata>> {
        let state = self.state.read().await;
        Ok(state.topics.get(name).cloned())
    }
    
    async fn list_topics(&self) -> Result<Vec<TopicMetadata>> {
        let state = self.state.read().await;
        Ok(state.topics.values().cloned().collect())
    }
    
    async fn update_topic(&self, name: &str, config: TopicConfig) -> Result<TopicMetadata> {
        let mut state = self.state.write().await;
        
        if let Some(metadata) = state.topics.get_mut(name) {
            metadata.config = config;
            metadata.updated_at = chrono::Utc::now();
            let result = metadata.clone();
            drop(state);
            self.persist().await?;
            Ok(result)
        } else {
            Err(MetadataError::NotFound(format!("Topic {} not found", name)))
        }
    }
    
    async fn delete_topic(&self, name: &str) -> Result<()> {
        let mut state = self.state.write().await;
        state.topics.remove(name)
            .ok_or_else(|| MetadataError::NotFound(format!("Topic {} not found", name)))?;
        drop(state);
        self.persist().await?;
        Ok(())
    }
    
    async fn persist_segment_metadata(&self, metadata: SegmentMetadata) -> Result<()> {
        let mut state = self.state.write().await;
        let key = format!("{}:{}", metadata.topic, metadata.segment_id);
        state.segments.insert(key, metadata);
        drop(state);
        self.persist().await?;
        Ok(())
    }
    
    async fn get_segment_metadata(&self, topic: &str, segment_id: &str) -> Result<Option<SegmentMetadata>> {
        let state = self.state.read().await;
        let key = format!("{}:{}", topic, segment_id);
        Ok(state.segments.get(&key).cloned())
    }
    
    async fn list_segments(&self, topic: &str, partition: Option<u32>) -> Result<Vec<SegmentMetadata>> {
        let state = self.state.read().await;
        Ok(state.segments.values()
            .filter(|s| s.topic == topic && (partition.is_none() || s.partition == partition.unwrap()))
            .cloned()
            .collect())
    }
    
    async fn delete_segment(&self, topic: &str, segment_id: &str) -> Result<()> {
        let mut state = self.state.write().await;
        let key = format!("{}:{}", topic, segment_id);
        state.segments.remove(&key)
            .ok_or_else(|| MetadataError::NotFound(format!("Segment {} not found", segment_id)))?;
        drop(state);
        self.persist().await?;
        Ok(())
    }
    
    async fn register_broker(&self, broker: BrokerMetadata) -> Result<()> {
        let mut state = self.state.write().await;
        state.brokers.insert(broker.broker_id, broker);
        drop(state);
        self.persist().await?;
        Ok(())
    }
    
    async fn get_broker(&self, id: i32) -> Result<Option<BrokerMetadata>> {
        let state = self.state.read().await;
        Ok(state.brokers.get(&id).cloned())
    }
    
    async fn list_brokers(&self) -> Result<Vec<BrokerMetadata>> {
        let state = self.state.read().await;
        Ok(state.brokers.values().cloned().collect())
    }
    
    async fn update_broker_status(&self, broker_id: i32, status: BrokerStatus) -> Result<()> {
        let mut state = self.state.write().await;
        if let Some(broker) = state.brokers.get_mut(&broker_id) {
            broker.status = status;
            broker.updated_at = chrono::Utc::now();
            drop(state);
            self.persist().await?;
            Ok(())
        } else {
            Err(MetadataError::NotFound(format!("Broker {} not found", broker_id)))
        }
    }
    
    async fn assign_partition(&self, assignment: PartitionAssignment) -> Result<()> {
        let mut state = self.state.write().await;
        let key = format!("{}:{}", assignment.topic, assignment.partition);
        state.partition_assignments.insert(key, assignment);
        drop(state);
        self.persist().await?;
        Ok(())
    }
    
    async fn get_partition_assignments(&self, topic: &str) -> Result<Vec<PartitionAssignment>> {
        let state = self.state.read().await;
        Ok(state.partition_assignments.values()
            .filter(|a| a.topic == topic)
            .cloned()
            .collect())
    }
    
    async fn create_consumer_group(&self, group: ConsumerGroupMetadata) -> Result<()> {
        let mut state = self.state.write().await;
        
        if state.consumer_groups.contains_key(&group.group_id) {
            return Err(MetadataError::AlreadyExists(format!("Consumer group {} already exists", group.group_id)));
        }
        
        state.consumer_groups.insert(group.group_id.clone(), group);
        drop(state);
        self.persist().await?;
        Ok(())
    }
    
    async fn get_consumer_group(&self, group_id: &str) -> Result<Option<ConsumerGroupMetadata>> {
        let state = self.state.read().await;
        Ok(state.consumer_groups.get(group_id).cloned())
    }
    
    async fn update_consumer_group(&self, group: ConsumerGroupMetadata) -> Result<()> {
        let mut state = self.state.write().await;
        state.consumer_groups.insert(group.group_id.clone(), group);
        drop(state);
        self.persist().await?;
        Ok(())
    }
    
    async fn commit_offset(&self, offset: ConsumerOffset) -> Result<()> {
        let mut state = self.state.write().await;
        let key = format!("{}:{}:{}", offset.group_id, offset.topic, offset.partition);
        state.consumer_offsets.insert(key, offset);
        drop(state);
        self.persist().await?;
        Ok(())
    }
    
    async fn get_consumer_offset(&self, group_id: &str, topic: &str, partition: u32) -> Result<Option<ConsumerOffset>> {
        let state = self.state.read().await;
        let key = format!("{}:{}:{}", group_id, topic, partition);
        Ok(state.consumer_offsets.get(&key).cloned())
    }
    
    async fn update_partition_offset(&self, topic: &str, partition: u32, high_watermark: i64, log_start_offset: i64) -> Result<()> {
        let mut state = self.state.write().await;
        let key = format!("{}:{}", topic, partition);
        state.partition_offsets.insert(key, (high_watermark, log_start_offset));
        drop(state);
        self.persist().await?;
        Ok(())
    }
    
    async fn get_partition_offset(&self, topic: &str, partition: u32) -> Result<Option<(i64, i64)>> {
        let state = self.state.read().await;
        let key = format!("{}:{}", topic, partition);
        Ok(state.partition_offsets.get(&key).copied())
    }
    
    async fn init_system_state(&self) -> Result<()> {
        // Already initialized on load
        Ok(())
    }
    
    async fn create_topic_with_assignments(&self, 
        topic_name: &str, 
        config: TopicConfig,
        assignments: Vec<PartitionAssignment>,
        offsets: Vec<(u32, i64, i64)>
    ) -> Result<TopicMetadata> {
        let metadata = self.create_topic(topic_name, config).await?;
        
        for assignment in assignments {
            self.assign_partition(assignment).await?;
        }
        
        for (partition, hw, lso) in offsets {
            self.update_partition_offset(topic_name, partition, hw, lso).await?;
        }
        
        Ok(metadata)
    }

    async fn commit_transactional_offsets(
        &self,
        _transactional_id: String,
        _producer_id: i64,
        _producer_epoch: i16,
        group_id: String,
        offsets: Vec<(String, u32, i64, Option<String>)>, // (topic, partition, offset, metadata)
    ) -> Result<()> {
        // For file store, we just commit the offsets like regular commits
        // In a real implementation, we'd track transaction state
        let mut state = self.state.write().await;

        for (topic, partition, offset, metadata) in offsets {
            let key = format!("{}:{}:{}", group_id, topic, partition);
            state.consumer_offsets.insert(key, ConsumerOffset {
                group_id: group_id.clone(),
                topic,
                partition,
                offset,
                metadata,
                commit_timestamp: chrono::Utc::now(),
            });
        }

        drop(state);
        self.persist().await?;
        Ok(())
    }

    async fn begin_transaction(
        &self,
        _transactional_id: String,
        _producer_id: i64,
        _producer_epoch: i16,
        _timeout_ms: i32,
    ) -> Result<()> {
        // Stub implementation for file store
        Ok(())
    }

    async fn add_partitions_to_transaction(
        &self,
        _transactional_id: String,
        _producer_id: i64,
        _producer_epoch: i16,
        _partitions: Vec<(String, u32)>,
    ) -> Result<()> {
        // Stub implementation for file store
        Ok(())
    }

    async fn add_offsets_to_transaction(
        &self,
        _transactional_id: String,
        _producer_id: i64,
        _producer_epoch: i16,
        _group_id: String,
    ) -> Result<()> {
        // Stub implementation for file store
        Ok(())
    }

    async fn prepare_commit_transaction(
        &self,
        _transactional_id: String,
        _producer_id: i64,
        _producer_epoch: i16,
    ) -> Result<()> {
        // Stub implementation for file store
        Ok(())
    }

    async fn commit_transaction(
        &self,
        _transactional_id: String,
        _producer_id: i64,
        _producer_epoch: i16,
    ) -> Result<()> {
        // Stub implementation for file store
        Ok(())
    }

    async fn abort_transaction(
        &self,
        _transactional_id: String,
        _producer_id: i64,
        _producer_epoch: i16,
    ) -> Result<()> {
        // Stub implementation for file store
        Ok(())
    }

    async fn fence_producer(
        &self,
        _transactional_id: String,
        _old_producer_id: i64,
        _old_producer_epoch: i16,
        _new_producer_id: i64,
        _new_producer_epoch: i16,
    ) -> Result<()> {
        // Stub implementation for file store
        Ok(())
    }
}