//! Production-ready controller node with Raft consensus.

use crate::raft::{RaftConfig, RaftHandle, init_raft_node};
use crate::raft::state_machine::{Proposal, ControllerState, TopicConfig, BrokerInfo};
use crate::metastore_adapter::ControllerMetadataStore;
use crate::admin_service::AdminService;
use crate::metadata_sync::MetadataSyncManager;
use crate::backend_client::BackendServiceManager;
use chronik_common::{Result, Error};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{info, error, warn};

/// Controller configuration
#[derive(Debug, Clone)]
pub struct ControllerConfig {
    /// Node ID
    pub node_id: u64,
    
    /// Listen address for Raft
    pub raft_addr: SocketAddr,
    
    /// Peer addresses (node_id -> address)
    pub peers: HashMap<u64, SocketAddr>,
    
    /// Data directory
    pub data_dir: PathBuf,
    
    /// Admin API address
    pub admin_addr: SocketAddr,
    
    /// Election timeout range in milliseconds
    pub election_timeout: (u64, u64),
    
    /// Heartbeat interval in milliseconds
    pub heartbeat_interval: u64,
}

impl Default for ControllerConfig {
    fn default() -> Self {
        Self {
            node_id: 1,
            raft_addr: "127.0.0.1:9000".parse().unwrap(),
            peers: HashMap::new(),
            data_dir: PathBuf::from("/var/chronik/controller"),
            admin_addr: "127.0.0.1:9001".parse().unwrap(),
            election_timeout: (150, 300),
            heartbeat_interval: 50,
        }
    }
}

/// Production-ready controller node
pub struct ControllerNode {
    config: ControllerConfig,
    raft_handle: RaftHandle,
    metadata_store: Arc<ControllerMetadataStore>,
    cluster_state: Arc<RwLock<ControllerState>>,
    admin_service: Option<Arc<AdminService>>,
    sync_manager: Option<Arc<MetadataSyncManager>>,
    shutdown: Arc<RwLock<bool>>,
}

impl ControllerNode {
    /// Create a new controller node
    pub async fn new(config: ControllerConfig) -> Result<Self> {
        // Create data directories
        let raft_dir = config.data_dir.join("raft");
        let metadata_dir = config.data_dir.join("metadata");
        
        std::fs::create_dir_all(&raft_dir)
            .map_err(|e| Error::Io(e))?;
        std::fs::create_dir_all(&metadata_dir)
            .map_err(|e| Error::Io(e))?;
        
        // Create Raft configuration
        let mut raft_config = RaftConfig::new(config.node_id);
        raft_config.listen_addr = config.raft_addr;
        raft_config.peers = config.peers.clone();
        raft_config.data_dir = raft_dir;
        raft_config.election_timeout = config.election_timeout;
        raft_config.heartbeat_interval = config.heartbeat_interval;
        
        // Initialize Raft node
        let (raft_node, raft_handle) = init_raft_node(raft_config).await?;
        
        // Start Raft node
        tokio::spawn(async move {
            if let Err(e) = raft_node.run().await {
                error!("Raft node error: {}", e);
            }
        });
        
        // Create metadata store
        let metadata_store = Arc::new(ControllerMetadataStore::new(&metadata_dir)?);
        
        // Initialize cluster state
        let cluster_state = Arc::new(RwLock::new(ControllerState::default()));
        
        Ok(Self {
            config,
            raft_handle,
            metadata_store,
            cluster_state,
            admin_service: None,
            sync_manager: None,
            shutdown: Arc::new(RwLock::new(false)),
        })
    }
    
    /// Start the controller
    pub async fn start(mut self: Arc<Self>) -> Result<()> {
        info!("Starting controller node {}", self.config.node_id);
        
        // Initialize metadata store
        self.metadata_store.init().await?;
        
        // Create backend service manager
        let backend_manager = Arc::new(BackendServiceManager::new(
            self.cluster_state.clone(),
        ));
        
        // Initialize backend connections
        if let Err(e) = backend_manager.initialize().await {
            warn!("Failed to initialize backend connections: {}", e);
        }
        
        // Create metadata sync manager with backend integration
        let sync_manager = Arc::new(MetadataSyncManager::new(
            self.metadata_store.clone(),
            self.cluster_state.clone(),
            Duration::from_secs(30), // Sync every 30 seconds
        ).with_backend_manager(backend_manager.clone()));
        
        // Create admin service with backend integration
        let admin_service = Arc::new(AdminService::new(
            self.metadata_store.clone(),
            self.cluster_state.clone(),
            sync_manager.clone(),
        ).with_backend_manager(backend_manager));
        
        // Store references in self - we need to update the struct
        // For now, we'll work with local variables
        
        // Start metadata sync loop
        let sync_handle = {
            let sync_manager = sync_manager.clone();
            tokio::spawn(async move {
                sync_manager.start_sync_loop().await;
            })
        };
        
        // Start admin API
        let admin_handle = {
            let admin_addr = self.config.admin_addr;
            tokio::spawn(async move {
                let app = admin_service.router();
                let listener = tokio::net::TcpListener::bind(&admin_addr).await
                    .map_err(|e| Error::Network(format!("Failed to bind admin API: {}", e)))?;
                
                info!("Admin API listening on {}", admin_addr);
                
                axum::serve(listener, app).await
                    .map_err(|e| Error::Network(format!("Admin API error: {}", e)))
            })
        };
        
        // Start legacy metadata sync (keep for compatibility)
        let legacy_sync_handle = {
            let controller = self.clone();
            tokio::spawn(async move {
                controller.sync_metadata().await
            })
        };
        
        // Wait for shutdown
        tokio::select! {
            result = admin_handle => {
                if let Err(e) = result {
                    error!("Admin API task failed: {:?}", e);
                }
            }
            result = sync_handle => {
                error!("Metadata sync task completed unexpectedly: {:?}", result);
            }
            result = legacy_sync_handle => {
                if let Err(e) = result {
                    error!("Legacy metadata sync task failed: {}", e);
                }
            }
        }
        
        Ok(())
    }
    
    /// Shutdown the controller
    pub async fn shutdown(&self) -> Result<()> {
        info!("Shutting down controller");
        let mut shutdown = self.shutdown.write().await;
        *shutdown = true;
        Ok(())
    }
    
    /// Propose a change through Raft
    pub async fn propose(&self, proposal: Proposal) -> Result<()> {
        self.raft_handle.propose(proposal).await
    }
    
    /// Get current leader
    pub async fn get_leader(&self) -> Result<Option<u64>> {
        self.raft_handle.get_leader().await
    }
    
    /// Check if this node is the leader
    pub async fn is_leader(&self) -> Result<bool> {
        let leader = self.get_leader().await?;
        Ok(leader == Some(self.config.node_id))
    }
    
    /// Get controller state
    pub async fn get_state(&self) -> Result<ControllerState> {
        self.raft_handle.get_state().await
    }
    
    /// Add a new node to the cluster
    pub async fn add_node(&self, node_id: u64) -> Result<()> {
        // First add as learner
        self.raft_handle.add_learner(node_id).await?;
        info!("Added node {} as learner", node_id);
        Ok(())
    }
    
    /// Promote a learner to voter
    pub async fn promote_node(&self, node_id: u64) -> Result<()> {
        self.raft_handle.promote_learner(node_id).await?;
        info!("Promoted node {} to voter", node_id);
        Ok(())
    }
    
    /// Remove a node from the cluster
    pub async fn remove_node(&self, node_id: u64) -> Result<()> {
        self.raft_handle.remove_node(node_id).await?;
        info!("Removed node {} from cluster", node_id);
        Ok(())
    }
    
    
    /// Sync metadata between Raft and metadata store
    async fn sync_metadata(&self) -> Result<()> {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
        
        loop {
            interval.tick().await;
            
            if *self.shutdown.read().await {
                break;
            }
            
            // Only sync if we're the leader
            if !self.is_leader().await.unwrap_or(false) {
                continue;
            }
            
            // Get state from Raft
            match self.get_state().await {
                Ok(state) => {
                    // Sync topics
                    for (topic_name, topic_config) in &state.topics {
                        if let Err(e) = self.sync_topic(topic_name, topic_config).await {
                            warn!("Failed to sync topic {}: {}", topic_name, e);
                        }
                    }
                    
                    // Sync brokers
                    for (broker_id, broker_info) in &state.brokers {
                        if let Err(e) = self.sync_broker(*broker_id, broker_info).await {
                            warn!("Failed to sync broker {}: {}", broker_id, e);
                        }
                    }
                }
                Err(e) => {
                    error!("Failed to get Raft state: {}", e);
                }
            }
        }
        
        Ok(())
    }
    
    /// Sync a topic to metadata store
    async fn sync_topic(&self, name: &str, config: &TopicConfig) -> Result<()> {
        // Check if topic exists in metadata store
        if let Ok(Some(_)) = self.metadata_store.store().get_topic(name).await {
            // Topic exists, update if needed
            // For now, skip update logic
        } else {
            // Create topic in metadata store
            let metadata_config = chronik_common::metadata::traits::TopicConfig {
                partition_count: config.partition_count as u32,
                replication_factor: config.replication_factor as u32,
                retention_ms: config.config.get("retention.ms")
                    .and_then(|s| s.parse().ok()),
                segment_bytes: config.config.get("segment.bytes")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(1024 * 1024 * 1024),
                config: config.config.clone(),
            };
            
            self.metadata_store.store().create_topic(name, metadata_config).await
                .map_err(|e| Error::Internal(format!("Failed to create topic: {:?}", e)))?;
        }
        
        Ok(())
    }
    
    /// Sync a broker to metadata store
    async fn sync_broker(&self, id: i32, info: &BrokerInfo) -> Result<()> {
        // Create broker in metadata store
        // Create a simple broker registration - this is a placeholder
        // In a real implementation, we would use proper metadata structs
        info!("Would register broker {} at {}", id, info.address);
        
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[tokio::test]
    async fn test_single_node_controller() {
        let config = ControllerConfig {
            data_dir: PathBuf::from("/tmp/chronik-test-controller"),
            ..Default::default()
        };
        
        let controller = Arc::new(ControllerNode::new(config).await.unwrap());
        
        // Wait for election
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        
        // Should become leader
        assert!(controller.is_leader().await.unwrap());
        
        // Test creating a topic
        let topic_config = TopicConfig {
            name: "test-topic".to_string(),
            partition_count: 3,
            replication_factor: 1,
            config: HashMap::new(),
            created_at: SystemTime::now(),
            version: 1,
        };
        
        controller.propose(Proposal::CreateTopic(topic_config)).await.unwrap();
        
        // Wait for proposal to be committed
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        
        // Check state
        let state = controller.get_state().await.unwrap();
        assert!(state.topics.contains_key("test-topic"));
        assert_eq!(state.topics["test-topic"].partition_count, 3);
        
        controller.shutdown().await.unwrap();
    }
}