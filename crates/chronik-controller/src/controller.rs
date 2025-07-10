//! Production controller implementation with tikv/raft-rs.

use crate::raft_node::{RaftNode, RaftHandle, NodeId};
use crate::tikv_raft_storage::TiKVRaftStorage;
use crate::raft_transport::{RaftTransport, TransportConfig};
use crate::raft_simple::{Proposal, ControllerState};
use crate::metastore_adapter::ControllerMetadataStore;
use chronik_common::{Result, Error};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use tracing::{info, error};

/// Production controller configuration
#[derive(Debug, Clone)]
pub struct ControllerConfig {
    /// Node ID
    pub node_id: NodeId,
    /// Listen address for Raft
    pub raft_addr: SocketAddr,
    /// Peer addresses
    pub peers: HashMap<NodeId, SocketAddr>,
    /// Data directory
    pub data_dir: PathBuf,
    /// Admin API address
    pub admin_addr: SocketAddr,
}

/// Production controller node
pub struct Controller {
    config: ControllerConfig,
    raft_handle: RaftHandle,
    metadata_store: Arc<ControllerMetadataStore>,
    shutdown: Arc<RwLock<bool>>,
}

impl Controller {
    /// Create a new controller
    pub async fn new(config: ControllerConfig) -> Result<Self> {
        // Create data directories
        let raft_dir = config.data_dir.join("raft");
        let metadata_dir = config.data_dir.join("metadata");
        
        std::fs::create_dir_all(&raft_dir)
            .map_err(|e| Error::Io(e))?;
        std::fs::create_dir_all(&metadata_dir)
            .map_err(|e| Error::Io(e))?;
        
        // Get TiKV PD endpoints
        let pd_endpoints = std::env::var("TIKV_PD_ENDPOINTS")
            .unwrap_or_else(|_| "localhost:2379".to_string())
            .split(',')
            .map(|s| s.to_string())
            .collect::<Vec<String>>();
        
        // Create Raft storage using TiKV
        let _storage = Arc::new(TiKVRaftStorage::new(pd_endpoints.clone()).await?);
        
        // Create metadata store using TiKV
        use chronik_common::metadata::TiKVMetadataStore;
        let tikv_metadata = Arc::new(TiKVMetadataStore::new(pd_endpoints).await?);
        let metadata_store = Arc::new(ControllerMetadataStore::with_backend(tikv_metadata));
        
        // Create message channels
        let (msg_tx, _msg_rx) = mpsc::unbounded_channel();
        let (cmd_tx, cmd_rx) = mpsc::channel(100);
        
        // Create transport
        let transport_config = TransportConfig {
            node_id: config.node_id,
            listen_addr: config.raft_addr,
            peers: config.peers.clone(),
            connection_timeout: std::time::Duration::from_secs(5),
        };
        let transport = Arc::new(RaftTransport::new(transport_config, msg_tx.clone()));
        
        // Create Raft node
        let peer_ids: Vec<NodeId> = config.peers.keys().cloned().collect();
        let raft_node = RaftNode::new(
            config.node_id,
            peer_ids,
            msg_tx,
            cmd_rx,
        )?;
        
        // Create Raft handle
        let raft_handle = RaftHandle::new(cmd_tx);
        
        // Start transport
        tokio::spawn({
            let transport = transport.clone();
            async move {
                if let Err(e) = transport.start().await {
                    error!("Transport error: {}", e);
                }
            }
        });
        
        // Start Raft node
        tokio::spawn(async move {
            if let Err(e) = raft_node.run().await {
                error!("Raft node error: {}", e);
            }
        });
        
        Ok(Self {
            config,
            raft_handle,
            metadata_store,
            shutdown: Arc::new(RwLock::new(false)),
        })
    }
    
    /// Start the controller
    pub async fn start(self: Arc<Self>) -> Result<()> {
        info!("Starting controller node {}", self.config.node_id);
        
        // Start admin API
        let admin_handle = {
            let controller = self.clone();
            tokio::spawn(async move {
                controller.run_admin_api().await
            })
        };
        
        // Start metadata sync
        let sync_handle = {
            let controller = self.clone();
            tokio::spawn(async move {
                controller.sync_metadata().await
            })
        };
        
        // Wait for shutdown
        tokio::select! {
            result = admin_handle => {
                if let Err(e) = result {
                    error!("Admin API task failed: {}", e);
                }
            }
            result = sync_handle => {
                if let Err(e) = result {
                    error!("Metadata sync task failed: {}", e);
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
    
    /// Run admin API
    async fn run_admin_api(&self) -> Result<()> {
        // TODO: Implement admin API using axum
        info!("Admin API listening on {}", self.config.admin_addr);
        
        loop {
            if *self.shutdown.read().await {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        
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
            
            // Get state from Raft
            match self.raft_handle.get_state().await {
                Ok(state) => {
                    // Sync topics
                    for (topic_name, topic_config) in &state.topics {
                        if let Err(e) = self.sync_topic(topic_name, topic_config).await {
                            error!("Failed to sync topic {}: {}", topic_name, e);
                        }
                    }
                    
                    // TODO: Sync other metadata (brokers, consumer groups, etc.)
                }
                Err(e) => {
                    error!("Failed to get Raft state: {}", e);
                }
            }
        }
        
        Ok(())
    }
    
    /// Sync a topic to metadata store
    async fn sync_topic(&self, name: &str, config: &crate::raft_simple::TopicConfig) -> Result<()> {
        // Check if topic exists in metadata store
        if let Ok(Some(_)) = self.metadata_store.store().get_topic(name).await {
            // Topic exists, update if needed
            // TODO: Compare and update
        } else {
            // Create topic in metadata store
            let metadata_config = chronik_common::metadata::traits::TopicConfig {
                partition_count: config.partition_count as u32,
                replication_factor: config.replication_factor as u32,
                retention_ms: config.configs.get("retention.ms")
                    .and_then(|s| s.parse().ok()),
                segment_bytes: config.configs.get("segment.bytes")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(1024 * 1024 * 1024),
                config: config.configs.clone(),
            };
            
            self.metadata_store.store().create_topic(name, metadata_config).await
                .map_err(|e| Error::Internal(format!("Failed to create topic: {:?}", e)))?;
        }
        
        Ok(())
    }
    
    /// Propose a change through Raft
    pub async fn propose(&self, proposal: Proposal) -> Result<()> {
        self.raft_handle.propose(proposal).await
    }
    
    /// Get current leader
    pub async fn get_leader(&self) -> Result<Option<NodeId>> {
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
}