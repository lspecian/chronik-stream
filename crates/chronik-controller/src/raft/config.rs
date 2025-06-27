//! Raft configuration.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;

use super::NodeId;

/// Raft node configuration
#[derive(Debug, Clone)]
pub struct RaftConfig {
    /// Unique node ID in the cluster
    pub node_id: NodeId,
    
    /// Listen address for Raft RPC
    pub listen_addr: SocketAddr,
    
    /// Peer node addresses (node_id -> address)
    pub peers: HashMap<NodeId, SocketAddr>,
    
    /// Data directory for Raft log and snapshots
    pub data_dir: PathBuf,
    
    /// Election timeout range in milliseconds (min, max)
    pub election_timeout: (u64, u64),
    
    /// Heartbeat interval in milliseconds
    pub heartbeat_interval: u64,
    
    /// Maximum size per message in bytes
    pub max_size_per_msg: u64,
    
    /// Maximum number of in-flight messages
    pub max_inflight_msgs: usize,
    
    /// Snapshot interval (number of log entries)
    pub snapshot_interval: u64,
    
    /// Whether to pre-vote before starting election
    pub pre_vote: bool,
}

impl Default for RaftConfig {
    fn default() -> Self {
        Self {
            node_id: 1,
            listen_addr: "127.0.0.1:9000".parse().unwrap(),
            peers: HashMap::new(),
            data_dir: PathBuf::from("/tmp/chronik-raft"),
            election_timeout: (150, 300),
            heartbeat_interval: 50,
            max_size_per_msg: 1024 * 1024, // 1MB
            max_inflight_msgs: 256,
            snapshot_interval: 10000,
            pre_vote: true,
        }
    }
}

impl RaftConfig {
    /// Create a new Raft configuration
    pub fn new(node_id: NodeId) -> Self {
        Self {
            node_id,
            ..Default::default()
        }
    }
    
    /// Add a peer node
    pub fn add_peer(&mut self, node_id: NodeId, addr: SocketAddr) {
        self.peers.insert(node_id, addr);
    }
    
    /// Get all node IDs (including self)
    pub fn all_nodes(&self) -> Vec<NodeId> {
        let mut nodes = vec![self.node_id];
        nodes.extend(self.peers.keys().cloned());
        nodes.sort();
        nodes
    }
    
    /// Convert to tikv/raft-rs Config
    pub fn to_raft_config(&self) -> raft::Config {
        let mut cfg = raft::Config {
            id: self.node_id,
            election_tick: 10,
            heartbeat_tick: 3,
            max_size_per_msg: self.max_size_per_msg,
            max_inflight_msgs: self.max_inflight_msgs,
            applied: 0,
            pre_vote: self.pre_vote,
            ..Default::default()
        };
        
        // Calculate election tick based on heartbeat interval
        let election_tick_min = self.election_timeout.0 / self.heartbeat_interval;
        let election_tick_max = self.election_timeout.1 / self.heartbeat_interval;
        cfg.min_election_tick = election_tick_min as usize;
        cfg.max_election_tick = election_tick_max as usize;
        
        cfg
    }
}