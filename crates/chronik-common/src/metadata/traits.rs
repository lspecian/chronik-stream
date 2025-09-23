//! Metadata store abstraction for Chronik Stream.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use thiserror::Error;
use uuid::Uuid;

/// Metadata store errors
#[derive(Error, Debug)]
pub enum MetadataError {
    #[error("Item not found: {0}")]
    NotFound(String),
    
    #[error("Item already exists: {0}")]
    AlreadyExists(String),
    
    #[error("Storage error: {0}")]
    StorageError(String),
    
    #[error("Serialization error: {0}")]
    SerializationError(String),
}

pub type Result<T> = std::result::Result<T, MetadataError>;

/// Topic configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopicConfig {
    pub partition_count: u32,
    pub replication_factor: u32,
    pub retention_ms: Option<i64>,
    pub segment_bytes: i64,
    pub config: HashMap<String, String>,
}

impl Default for TopicConfig {
    fn default() -> Self {
        Self {
            partition_count: 1,
            replication_factor: 1,
            retention_ms: Some(7 * 24 * 60 * 60 * 1000), // 7 days
            segment_bytes: 1024 * 1024 * 1024, // 1GB
            config: HashMap::new(),
        }
    }
}

/// Topic metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopicMetadata {
    pub id: Uuid,
    pub name: String,
    pub config: TopicConfig,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// Segment metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SegmentMetadata {
    pub segment_id: String,
    pub topic: String,
    pub partition: u32,
    pub start_offset: i64,
    pub end_offset: i64,
    pub size: i64,
    pub record_count: i64,
    pub path: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Broker metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrokerMetadata {
    pub broker_id: i32,
    pub host: String,
    pub port: i32,
    pub rack: Option<String>,
    pub status: BrokerStatus,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum BrokerStatus {
    Online,
    Offline,
    Maintenance,
}

/// Partition assignment
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartitionAssignment {
    pub topic: String,
    pub partition: u32,
    pub broker_id: i32,
    pub is_leader: bool,
}

/// Group member information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupMember {
    pub member_id: String,
    pub client_id: String,
    pub client_host: String,
    pub metadata: Vec<u8>,
    pub assignment: Vec<u8>,
}

/// Consumer group metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsumerGroupMetadata {
    pub group_id: String,
    pub state: String,
    pub protocol: String,
    pub protocol_type: String,
    pub generation_id: i32,
    pub leader_id: Option<String>,
    pub leader: String,  // Leader member ID
    pub members: Vec<GroupMember>,  // Group members
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// Consumer offset
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsumerOffset {
    pub group_id: String,
    pub topic: String,
    pub partition: u32,
    pub offset: i64,
    pub metadata: Option<String>,
    pub commit_timestamp: chrono::DateTime<chrono::Utc>,
}

/// Metadata store trait
#[async_trait]
pub trait MetadataStore: Send + Sync {
    // Topic operations
    async fn create_topic(&self, name: &str, config: TopicConfig) -> Result<TopicMetadata>;
    async fn get_topic(&self, name: &str) -> Result<Option<TopicMetadata>>;
    async fn list_topics(&self) -> Result<Vec<TopicMetadata>>;
    async fn update_topic(&self, name: &str, config: TopicConfig) -> Result<TopicMetadata>;
    async fn delete_topic(&self, name: &str) -> Result<()>;
    
    // Segment operations
    async fn persist_segment_metadata(&self, metadata: SegmentMetadata) -> Result<()>;
    async fn get_segment_metadata(&self, topic: &str, segment_id: &str) -> Result<Option<SegmentMetadata>>;
    async fn list_segments(&self, topic: &str, partition: Option<u32>) -> Result<Vec<SegmentMetadata>>;
    async fn delete_segment(&self, topic: &str, segment_id: &str) -> Result<()>;
    
    // Broker operations
    async fn register_broker(&self, metadata: BrokerMetadata) -> Result<()>;
    async fn get_broker(&self, broker_id: i32) -> Result<Option<BrokerMetadata>>;
    async fn list_brokers(&self) -> Result<Vec<BrokerMetadata>>;
    async fn update_broker_status(&self, broker_id: i32, status: BrokerStatus) -> Result<()>;
    
    // Partition assignment operations
    async fn assign_partition(&self, assignment: PartitionAssignment) -> Result<()>;
    async fn get_partition_assignments(&self, topic: &str) -> Result<Vec<PartitionAssignment>>;
    
    // Consumer group operations
    async fn create_consumer_group(&self, metadata: ConsumerGroupMetadata) -> Result<()>;
    async fn get_consumer_group(&self, group_id: &str) -> Result<Option<ConsumerGroupMetadata>>;
    async fn update_consumer_group(&self, metadata: ConsumerGroupMetadata) -> Result<()>;
    async fn commit_offset(&self, offset: ConsumerOffset) -> Result<()>;
    async fn get_consumer_offset(&self, group_id: &str, topic: &str, partition: u32) -> Result<Option<ConsumerOffset>>;
    async fn commit_transactional_offsets(
        &self,
        transactional_id: String,
        producer_id: i64,
        producer_epoch: i16,
        group_id: String,
        offsets: Vec<(String, u32, i64, Option<String>)>, // (topic, partition, offset, metadata)
    ) -> Result<()>;

    // Transaction lifecycle operations
    async fn begin_transaction(
        &self,
        transactional_id: String,
        producer_id: i64,
        producer_epoch: i16,
        timeout_ms: i32,
    ) -> Result<()>;

    async fn add_partitions_to_transaction(
        &self,
        transactional_id: String,
        producer_id: i64,
        producer_epoch: i16,
        partitions: Vec<(String, u32)>, // (topic, partition)
    ) -> Result<()>;

    async fn add_offsets_to_transaction(
        &self,
        transactional_id: String,
        producer_id: i64,
        producer_epoch: i16,
        group_id: String,
    ) -> Result<()>;

    async fn prepare_commit_transaction(
        &self,
        transactional_id: String,
        producer_id: i64,
        producer_epoch: i16,
    ) -> Result<()>;

    async fn commit_transaction(
        &self,
        transactional_id: String,
        producer_id: i64,
        producer_epoch: i16,
    ) -> Result<()>;

    async fn abort_transaction(
        &self,
        transactional_id: String,
        producer_id: i64,
        producer_epoch: i16,
    ) -> Result<()>;

    async fn fence_producer(
        &self,
        transactional_id: String,
        old_producer_id: i64,
        old_producer_epoch: i16,
        new_producer_id: i64,
        new_producer_epoch: i16,
    ) -> Result<()>;
    
    // Partition offset operations
    async fn update_partition_offset(&self, topic: &str, partition: u32, high_watermark: i64, log_start_offset: i64) -> Result<()>;
    async fn get_partition_offset(&self, topic: &str, partition: u32) -> Result<Option<(i64, i64)>>; // Returns (high_watermark, log_start_offset)
    
    // System initialization
    async fn init_system_state(&self) -> Result<()>;
    
    // Batch/transactional operations
    async fn create_topic_with_assignments(&self, 
        topic_name: &str, 
        config: TopicConfig,
        assignments: Vec<PartitionAssignment>,
        offsets: Vec<(u32, i64, i64)> // (partition, high_watermark, log_start_offset)
    ) -> Result<TopicMetadata>;
}