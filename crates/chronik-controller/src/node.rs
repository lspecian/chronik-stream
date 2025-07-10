//! Controller node with Raft consensus.

use crate::raft_simple::{RaftNode, StateMachine, NodeId, Proposal, ControllerState};
use crate::metastore_adapter::ControllerMetadataStore;
use chronik_common::{Result, Error};
use std::sync::{Arc, Mutex};
use tokio::time::{interval, Duration};

/// Controller node configuration
#[derive(Debug, Clone)]
pub struct ControllerConfig {
    /// Node ID
    pub node_id: NodeId,
    /// Peer node IDs
    pub peers: Vec<NodeId>,
    /// Tick interval for Raft
    pub tick_interval: Duration,
    /// Election timeout range
    pub election_timeout: (u64, u64),
    /// Metadata store path
    pub metadata_path: String,
}

impl Default for ControllerConfig {
    fn default() -> Self {
        Self {
            node_id: 1,
            peers: vec![],
            tick_interval: Duration::from_millis(100),
            election_timeout: (150, 300),
            metadata_path: "/var/chronik/metadata".to_string(),
        }
    }
}

/// Controller node managing cluster state.
pub struct ControllerNode {
    /// Configuration
    config: ControllerConfig,
    /// Raft node
    raft_node: Arc<RaftNode>,
    /// State machine
    state_machine: Arc<StateMachine>,
    /// Metadata store
    metadata_store: Arc<ControllerMetadataStore>,
    /// Running flag
    running: Arc<Mutex<bool>>,
}

impl ControllerNode {
    /// Create a new controller node.
    pub fn new(config: ControllerConfig) -> Result<Self> {
        let raft_node = if config.peers.is_empty() {
            Arc::new(RaftNode::new_single_node(config.node_id)?)
        } else {
            Arc::new(RaftNode::new(config.node_id, config.peers.clone())?)
        };
        
        let state_machine = Arc::new(StateMachine::new());
        
        // Create metadata store with TiKV
        let pd_endpoints = std::env::var("TIKV_PD_ENDPOINTS")
            .unwrap_or_else(|_| "localhost:2379".to_string());
        
        let store = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                use chronik_common::metadata::TiKVMetadataStore;
                let endpoints = pd_endpoints
                    .split(',')
                    .map(|s| s.to_string())
                    .collect::<Vec<String>>();
                TiKVMetadataStore::new(endpoints).await
            })
        }).map_err(|e| Error::Internal(format!("Failed to create TiKV metadata store: {:?}", e)))?;
        
        let metadata_store = Arc::new(ControllerMetadataStore::with_backend(Arc::new(store)));
        
        Ok(Self {
            config,
            raft_node,
            state_machine,
            metadata_store,
            running: Arc::new(Mutex::new(false)),
        })
    }
    
    /// Start the controller node
    pub async fn start(&self) -> Result<()> {
        {
            let mut running = self.running.lock().unwrap();
            if *running {
                return Err(Error::Internal("Controller already running".into()));
            }
            *running = true;
        }
        
        // Initialize metadata store
        self.metadata_store.init().await?;
        
        // Start tick loop
        let raft_node = self.raft_node.clone();
        let tick_interval = self.config.tick_interval;
        let running = self.running.clone();
        
        tokio::spawn(async move {
            let mut ticker = interval(tick_interval);
            
            loop {
                ticker.tick().await;
                
                if !*running.lock().unwrap() {
                    break;
                }
                
                // Tick the raft node
                raft_node.tick();
            }
        });
        
        // Start state machine apply loop
        let raft_node = self.raft_node.clone();
        let state_machine = self.state_machine.clone();
        let running = self.running.clone();
        
        tokio::spawn(async move {
            loop {
                if !*running.lock().unwrap() {
                    break;
                }
                
                match raft_node.process_ready() {
                    Ok(proposals) => {
                        for proposal in proposals {
                            if let Err(e) = state_machine.apply(proposal) {
                                tracing::error!("Failed to apply proposal: {}", e);
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("Error processing proposals: {}", e);
                    }
                }
                
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });
        
        Ok(())
    }
    
    /// Stop the controller node
    pub async fn stop(&self) -> Result<()> {
        let mut running = self.running.lock().unwrap();
        *running = false;
        Ok(())
    }
    
    /// Check if this node is the leader
    pub fn is_leader(&self) -> bool {
        self.raft_node.is_leader()
    }
    
    /// Get the current leader ID
    pub fn leader_id(&self) -> Option<NodeId> {
        self.raft_node.leader_id()
    }
    
    /// Propose a state change (only works if leader)
    pub fn propose(&self, proposal: Proposal) -> Result<()> {
        if !self.is_leader() {
            return Err(Error::Internal("Not the leader".into()));
        }
        
        self.raft_node.propose(proposal)
    }
    
    /// Get current controller state
    pub fn get_state(&self) -> ControllerState {
        self.state_machine.get_state()
    }
    
    /// Campaign to become leader
    pub fn campaign(&self) -> Result<()> {
        self.raft_node.campaign()
    }
}