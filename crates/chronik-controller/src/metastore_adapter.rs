//! Adapter to integrate metadata store with the controller.

use chronik_common::metadata::{MetadataStore, TiKVMetadataStore, TopicConfig as MetaTopicConfig, BrokerMetadata, BrokerStatus, PartitionAssignment};
use crate::raft_simple::{TopicConfig, BrokerInfo, BrokerId};
use chronik_common::{Result, Error};
use std::sync::Arc;

/// Controller metadata store adapter
pub struct ControllerMetadataStore {
    store: Arc<dyn MetadataStore>,
}

impl ControllerMetadataStore {
    /// Create a new metadata store with TiKV backend
    pub async fn new(pd_endpoints: Vec<String>) -> Result<Self> {
        let store = TiKVMetadataStore::new(pd_endpoints).await
            .map_err(|e| Error::Internal(format!("Failed to create metadata store: {:?}", e)))?;
        Ok(Self {
            store: Arc::new(store),
        })
    }
    
    /// Create metadata store with custom backend
    pub fn with_backend(store: Arc<dyn MetadataStore>) -> Self {
        Self { store }
    }
    
    /// Get the underlying metadata store
    pub fn store(&self) -> Arc<dyn MetadataStore> {
        self.store.clone()
    }
    
    /// Initialize the metadata store with system state
    pub async fn init(&self) -> Result<()> {
        self.store.init_system_state().await
            .map_err(|e| Error::Internal(format!("Failed to initialize metadata store: {:?}", e)))?;
        Ok(())
    }
    
    /// Convert controller TopicConfig to metadata TopicConfig
    pub fn to_meta_topic_config(config: &TopicConfig) -> MetaTopicConfig {
        // Extract retention and segment settings from configs map if present
        let retention_ms = config.configs.get("retention.ms")
            .and_then(|v| v.parse::<i64>().ok())
            .or(Some(7 * 24 * 60 * 60 * 1000)); // 7 days default
        
        let segment_bytes = config.configs.get("segment.bytes")
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(1024 * 1024 * 1024); // 1GB default
            
        MetaTopicConfig {
            partition_count: config.partition_count as u32,
            replication_factor: config.replication_factor as u32,
            retention_ms,
            segment_bytes: segment_bytes as i64,
            config: config.configs.clone(),
        }
    }
    
    /// Convert metadata TopicConfig to controller TopicConfig  
    pub fn from_meta_topic_config(name: String, config: &MetaTopicConfig) -> TopicConfig {
        let mut configs = config.config.clone();
        
        // Add retention and segment settings to configs
        if let Some(retention) = config.retention_ms {
            configs.insert("retention.ms".to_string(), retention.to_string());
        }
        configs.insert("segment.bytes".to_string(), config.segment_bytes.to_string());
        
        TopicConfig {
            name,
            partition_count: config.partition_count as i32,
            replication_factor: config.replication_factor as i32,
            configs,
        }
    }
    
    /// Register a broker in the metadata store
    pub async fn register_broker(&self, broker_id: BrokerId, info: &BrokerInfo) -> Result<()> {
        let metadata = BrokerMetadata {
            broker_id: broker_id as i32,
            host: info.host.clone(),
            port: info.port as i32,
            rack: info.rack.clone(),
            status: BrokerStatus::Online,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        
        self.store.register_broker(metadata).await
            .map_err(|e| Error::Internal(format!("Failed to register broker: {:?}", e)))?;
        Ok(())
    }
    
    /// Assign a partition to a broker
    pub async fn assign_partition(&self, topic: &str, partition: u32, broker_id: BrokerId, is_leader: bool) -> Result<()> {
        let assignment = PartitionAssignment {
            topic: topic.to_string(),
            partition,
            broker_id: broker_id as i32,
            is_leader,
        };
        
        self.store.assign_partition(assignment).await
            .map_err(|e| Error::Internal(format!("Failed to assign partition: {:?}", e)))?;
        Ok(())
    }
}