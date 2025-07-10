//! Production-ready Raft consensus implementation using tikv/raft-rs.

pub mod config;
pub mod node;
pub mod state_machine;
pub mod transport;

pub use config::RaftConfig;
pub use node::{RaftNode, RaftHandle};
pub use state_machine::{ControllerStateMachine, Proposal};
pub use transport::{RaftTransport, TransportConfig, TransportMessage};

use chronik_common::{Result, Error};

/// Node ID type for Raft cluster
pub type NodeId = u64;

/// Initialize a Raft node with the given configuration
pub async fn init_raft_node(config: RaftConfig) -> Result<(RaftNode, RaftHandle)> {
    // Validate configuration
    if config.node_id == 0 {
        return Err(Error::Internal("Node ID must be greater than 0".into()));
    }
    
    // Create TiKV storage
    use crate::tikv_raft_storage::TiKVRaftStorage;
    let pd_endpoints = std::env::var("TIKV_PD_ENDPOINTS")
        .unwrap_or_else(|_| "localhost:2379".to_string())
        .split(',')
        .map(|s| s.to_string())
        .collect::<Vec<_>>();
    let storage = TiKVRaftStorage::new(pd_endpoints).await?;
    
    // Create transport
    let transport = RaftTransport::new(TransportConfig {
        node_id: config.node_id,
        listen_addr: config.listen_addr,
        peers: config.peers.clone(),
        connection_timeout: std::time::Duration::from_secs(5),
    });
    
    // Create Raft node
    let (node, handle) = RaftNode::new(config, storage, transport)?;
    
    Ok((node, handle))
}