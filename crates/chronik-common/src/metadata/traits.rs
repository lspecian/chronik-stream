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
            partition_count: 3,  // Changed from 1 to 3 for proper consumer group distribution
            replication_factor: 1,
            retention_ms: Some(7 * 24 * 60 * 60 * 1000), // 7 days
            segment_bytes: 1024 * 1024 * 1024, // 1GB
            config: HashMap::new(),
        }
    }
}

impl TopicConfig {
    /// Check if topic should be indexed for full-text search.
    ///
    /// Precedence (highest to lowest):
    /// 1. Explicit topic config: `config["searchable"] = "true"/"false"`
    /// 2. Environment default: `CHRONIK_DEFAULT_SEARCHABLE=true/false`
    /// 3. Hardcoded default: `false` (maximum performance)
    ///
    /// # Examples
    ///
    /// ```
    /// use chronik_common::metadata::TopicConfig;
    /// use std::collections::HashMap;
    ///
    /// // Default: not searchable
    /// let config = TopicConfig::default();
    /// assert!(!config.is_searchable());
    ///
    /// // Explicit searchable
    /// let mut config = TopicConfig::default();
    /// config.config.insert("searchable".to_string(), "true".to_string());
    /// assert!(config.is_searchable());
    /// ```
    pub fn is_searchable(&self) -> bool {
        // 1. Explicit config wins (case-insensitive)
        if let Some(val) = self.config.get("searchable") {
            return val.eq_ignore_ascii_case("true");
        }
        // 2. Fall back to environment default
        std::env::var("CHRONIK_DEFAULT_SEARCHABLE")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)  // 3. Hardcoded default: false (max performance)
    }

    /// Create a searchable topic config
    pub fn with_searchable(mut self, searchable: bool) -> Self {
        self.config.insert("searchable".to_string(), searchable.to_string());
        self
    }

    /// Check if topic should store data in columnar (Parquet) format.
    ///
    /// Precedence (highest to lowest):
    /// 1. Explicit topic config: `config["columnar.enabled"] = "true"/"false"`
    /// 2. Environment default: `CHRONIK_DEFAULT_COLUMNAR=true/false`
    /// 3. Hardcoded default: `false` (maximum performance)
    ///
    /// # Examples
    ///
    /// ```
    /// use chronik_common::metadata::TopicConfig;
    /// use std::collections::HashMap;
    ///
    /// // Default: not columnar
    /// let config = TopicConfig::default();
    /// assert!(!config.is_columnar_enabled());
    ///
    /// // Explicit columnar
    /// let mut config = TopicConfig::default();
    /// config.config.insert("columnar.enabled".to_string(), "true".to_string());
    /// assert!(config.is_columnar_enabled());
    /// ```
    pub fn is_columnar_enabled(&self) -> bool {
        // 1. Explicit config wins (case-insensitive)
        if let Some(val) = self.config.get("columnar.enabled") {
            return val.eq_ignore_ascii_case("true");
        }
        // 2. Fall back to environment default
        std::env::var("CHRONIK_DEFAULT_COLUMNAR")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)  // 3. Hardcoded default: false (max performance)
    }

    /// Get the columnar format (parquet or arrow).
    /// Defaults to "parquet".
    pub fn columnar_format(&self) -> &str {
        self.config.get("columnar.format").map(|s| s.as_str()).unwrap_or("parquet")
    }

    /// Get the columnar compression codec.
    /// Defaults to "zstd".
    pub fn columnar_compression(&self) -> &str {
        self.config.get("columnar.compression").map(|s| s.as_str()).unwrap_or("zstd")
    }

    /// Get the columnar row group size.
    /// Defaults to 65536.
    pub fn columnar_row_group_size(&self) -> usize {
        self.config.get("columnar.row_group_size")
            .and_then(|s| s.parse().ok())
            .unwrap_or(65536)
    }

    /// Get the columnar partitioning strategy (none, hourly, daily).
    /// Defaults to "hourly".
    pub fn columnar_partitioning(&self) -> &str {
        self.config.get("columnar.partitioning").map(|s| s.as_str()).unwrap_or("hourly")
    }

    /// Create a columnar-enabled topic config
    pub fn with_columnar(mut self, enabled: bool) -> Self {
        self.config.insert("columnar.enabled".to_string(), enabled.to_string());
        self
    }

    /// Check if topic has vector search enabled.
    ///
    /// Precedence (highest to lowest):
    /// 1. Explicit topic config: `config["vector.enabled"] = "true"/"false"`
    /// 2. Environment default: `CHRONIK_DEFAULT_VECTOR_ENABLED=true/false`
    /// 3. Hardcoded default: `false`
    pub fn is_vector_enabled(&self) -> bool {
        // 1. Explicit config wins (case-insensitive)
        if let Some(val) = self.config.get("vector.enabled") {
            return val.eq_ignore_ascii_case("true");
        }
        // 2. Fall back to environment default
        std::env::var("CHRONIK_DEFAULT_VECTOR_ENABLED")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(false) // 3. Hardcoded default: false
    }

    /// Get the vector embedding provider (openai, external, local).
    pub fn vector_embedding_provider(&self) -> Option<&str> {
        self.config.get("vector.embedding.provider").map(|s| s.as_str())
    }

    /// Get the vector embedding model.
    pub fn vector_embedding_model(&self) -> Option<&str> {
        self.config.get("vector.embedding.model").map(|s| s.as_str())
    }

    /// Get the vector embedding dimensions.
    pub fn vector_embedding_dimensions(&self) -> Option<usize> {
        self.config.get("vector.embedding.dimensions")
            .and_then(|s| s.parse().ok())
    }

    /// Create a vector-enabled topic config
    pub fn with_vector(mut self, provider: &str, model: &str, dimensions: usize) -> Self {
        self.config.insert("vector.enabled".to_string(), "true".to_string());
        self.config.insert("vector.embedding.provider".to_string(), provider.to_string());
        self.config.insert("vector.embedding.model".to_string(), model.to_string());
        self.config.insert("vector.embedding.dimensions".to_string(), dimensions.to_string());
        self
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

/// Segment metadata (Tantivy indexes)
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

/// Parquet segment metadata (columnar storage for SQL queries)
///
/// Used by the WalIndexer to track Parquet files for DataFusion SQL queries.
/// Each Parquet segment represents a range of Kafka messages converted to
/// columnar format for efficient analytics.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ParquetSegmentMetadata {
    /// Unique segment ID (format: topic-partition-min_offset-max_offset-timestamp)
    pub segment_id: String,
    /// Topic name
    pub topic: String,
    /// Partition number
    pub partition: i32,
    /// Minimum offset in this segment (inclusive)
    pub min_offset: i64,
    /// Maximum offset in this segment (inclusive)
    pub max_offset: i64,
    /// Number of records (rows) in this segment
    pub record_count: usize,
    /// Number of row groups in this Parquet file
    pub row_group_count: usize,
    /// Minimum timestamp in this segment (milliseconds)
    pub min_timestamp: i64,
    /// Maximum timestamp in this segment (milliseconds)
    pub max_timestamp: i64,
    /// Object store path (e.g., "parquet/topic/partition=0/2024/01/01/segment.parquet")
    pub object_store_path: String,
    /// Size in bytes (Parquet file size)
    pub size_bytes: u64,
    /// Creation timestamp (when segment was created)
    pub created_at: i64,
    /// Compression codec used (zstd, snappy, gzip, etc.)
    pub compression: String,
    /// Time partition key if time-partitioned (e.g., "2024-01-01-12" for hourly)
    pub time_partition_key: Option<String>,
    /// Parquet file schema fingerprint (for schema evolution tracking)
    pub schema_fingerprint: Option<String>,
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
    pub broker_id: i32,  // Deprecated: use leader_id instead
    pub is_leader: bool,  // Deprecated: leader determined by leader_id field
    pub replicas: Vec<u64>,  // All replica node IDs (leader is first)
    pub leader_id: u64,  // Leader node ID
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
    
    // Segment operations (Tantivy indexes)
    async fn persist_segment_metadata(&self, metadata: SegmentMetadata) -> Result<()>;
    async fn get_segment_metadata(&self, topic: &str, segment_id: &str) -> Result<Option<SegmentMetadata>>;
    async fn list_segments(&self, topic: &str, partition: Option<u32>) -> Result<Vec<SegmentMetadata>>;
    async fn delete_segment(&self, topic: &str, segment_id: &str) -> Result<()>;

    // Parquet segment operations (columnar storage for SQL queries)
    async fn persist_parquet_segment(&self, metadata: ParquetSegmentMetadata) -> Result<()>;
    async fn get_parquet_segment(&self, topic: &str, partition: i32, segment_id: &str) -> Result<Option<ParquetSegmentMetadata>>;
    async fn list_parquet_segments(&self, topic: &str, partition: Option<i32>) -> Result<Vec<ParquetSegmentMetadata>>;
    async fn get_parquet_paths(&self, topic: &str) -> Result<Vec<String>>;
    async fn delete_parquet_segment(&self, topic: &str, partition: i32, segment_id: &str) -> Result<()>;

    // Broker operations
    async fn register_broker(&self, metadata: BrokerMetadata) -> Result<()>;
    async fn get_broker(&self, broker_id: i32) -> Result<Option<BrokerMetadata>>;
    async fn list_brokers(&self) -> Result<Vec<BrokerMetadata>>;
    async fn update_broker_status(&self, broker_id: i32, status: BrokerStatus) -> Result<()>;
    
    // Partition assignment operations
    async fn assign_partition(&self, assignment: PartitionAssignment) -> Result<()>;
    async fn get_partition_assignments(&self, topic: &str) -> Result<Vec<PartitionAssignment>>;

    // Partition leader query operations (for Kafka Metadata API)
    async fn get_partition_leader(&self, topic: &str, partition: u32) -> Result<Option<i32>>;
    async fn get_partition_replicas(&self, topic: &str, partition: u32) -> Result<Option<Vec<i32>>>;
    
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

    // Replicated event application (for followers receiving from leader)
    // v2.2.9 Phase 7: Apply replicated metadata events WITHOUT writing to WAL
    async fn apply_replicated_event(&self, event: super::events::MetadataEvent) -> Result<()>;

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