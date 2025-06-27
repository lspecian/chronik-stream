//! Production-ready Raft consensus implementation using tikv/raft-rs.

pub mod config;
pub mod node;
pub mod state_machine;
pub mod storage;
pub mod transport;

pub use config::RaftConfig;
pub use node::{RaftNode, RaftHandle};
pub use state_machine::{ControllerStateMachine, Proposal};
pub use storage::{RaftStorage, ArcRaftStorage};
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
    
    // Create storage
    let storage = RaftStorage::new(&config.data_dir)?;
    
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