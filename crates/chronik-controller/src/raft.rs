//! Raft consensus implementation for controller nodes.

use chronik_common::{Result, Error};
use raft::prelude::*;
use raft::{Config, RawNode, StateRole};
use raft::storage::MemStorage;
use serde::{Deserialize, Serialize};
use slog::{Logger, o, Drain};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Node ID type
pub type NodeId = u64;

/// Controller state machine data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControllerState {
    /// Topic configurations
    pub topics: HashMap<String, TopicConfig>,
    /// Partition leader assignments
    pub partition_leaders: HashMap<TopicPartition, BrokerId>,
    /// In-sync replica lists
    pub isr_lists: HashMap<TopicPartition, Vec<BrokerId>>,
    /// Consumer group metadata
    pub consumer_groups: HashMap<String, ConsumerGroup>,
    /// Broker metadata
    pub brokers: HashMap<BrokerId, BrokerInfo>,
}

/// Topic configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopicConfig {
    pub name: String,
    pub partition_count: i32,
    pub replication_factor: i32,
    pub configs: HashMap<String, String>,
}

/// Topic partition identifier
#[derive(Debug, Clone, Hash, Eq, PartialEq, Serialize, Deserialize)]
pub struct TopicPartition {
    pub topic: String,
    pub partition: i32,
}

/// Broker ID
pub type BrokerId = i32;

/// Broker information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrokerInfo {
    pub id: BrokerId,
    pub host: String,
    pub port: u16,
    pub rack: Option<String>,
}

/// Consumer group metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsumerGroup {
    pub id: String,
    pub state: GroupState,
    pub protocol_type: String,
    pub generation_id: i32,
    pub leader: Option<String>,
    pub members: HashMap<String, MemberMetadata>,
}

/// Consumer group state
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum GroupState {
    Empty,
    Stable,
    PreparingRebalance,
    CompletingRebalance,
    Dead,
}

/// Group member metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemberMetadata {
    pub member_id: String,
    pub client_id: String,
    pub client_host: String,
    pub session_timeout: i32,
    pub rebalance_timeout: i32,
}

/// Raft proposal types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Proposal {
    CreateTopic(TopicConfig),
    DeleteTopic(String),
    UpdatePartitionLeader {
        topic_partition: TopicPartition,
        leader: BrokerId,
    },
    UpdateIsr {
        topic_partition: TopicPartition,
        isr: Vec<BrokerId>,
    },
    RegisterBroker(BrokerInfo),
    UnregisterBroker(BrokerId),
    CreateConsumerGroup(ConsumerGroup),
    UpdateConsumerGroup(ConsumerGroup),
    DeleteConsumerGroup(String),
}

/// Create a default slog logger
fn create_logger() -> Logger {
    let decorator = slog_term::TermDecorator::new().build();
    let drain = slog_term::FullFormat::new(decorator).build().fuse();
    let drain = slog_async::Async::new(drain).build().fuse();
    Logger::root(drain, o!())
}

/// Raft node wrapper
pub struct RaftNode {
    /// Node ID
    pub id: NodeId,
    /// Raw raft node
    raw_node: Arc<Mutex<RawNode<MemStorage>>>,
    /// Logger
    logger: Logger,
}

impl RaftNode {
    /// Create a new Raft node
    pub fn new(id: NodeId, peers: Vec<NodeId>) -> Result<Self> {
        let cfg = Config {
            id,
            election_tick: 10,
            heartbeat_tick: 3,
            max_size_per_msg: 1024 * 1024,
            max_inflight_msgs: 256,
            ..Default::default()
        };
        
        let mut storage = MemStorage::new();
        let logger = create_logger();
        
        // Bootstrap the cluster
        let mut voters = vec![id];
        voters.extend(peers.iter().cloned());
        storage.initialize_with_conf_state((voters, vec![]));
        
        let raw_node = RawNode::new(&cfg, storage, &logger)
            .map_err(|e| Error::Internal(format!("Failed to create raft node: {}", e)))?;
        
        Ok(Self {
            id,
            raw_node: Arc::new(Mutex::new(raw_node)),
            logger,
        })
    }
    
    /// Bootstrap a single node cluster
    pub fn bootstrap_single_node(id: NodeId) -> Result<Self> {
        let cfg = Config {
            id,
            election_tick: 10,
            heartbeat_tick: 3,
            max_size_per_msg: 1024 * 1024,
            max_inflight_msgs: 256,
            ..Default::default()
        };
        
        let mut storage = MemStorage::new();
        let logger = create_logger();
        
        // Bootstrap with single voter
        storage.initialize_with_conf_state((vec![id], vec![]));
        
        let mut raw_node = RawNode::new(&cfg, storage, &logger)
            .map_err(|e| Error::Internal(format!("Failed to create raft node: {}", e)))?;
        
        // Bootstrap the node
        raw_node.bootstrap(vec![Peer { id, context: None }])
            .map_err(|e| Error::Internal(format!("Failed to bootstrap: {}", e)))?;
        
        Ok(Self {
            id,
            raw_node: Arc::new(Mutex::new(raw_node)),
            logger,
        })
    }
    
    /// Propose a state change
    pub fn propose(&self, proposal: Proposal) -> Result<()> {
        let data = serde_json::to_vec(&proposal)?;
        
        let mut raw_node = self.raw_node.lock().unwrap();
        raw_node.propose(vec![], data)
            .map_err(|e| Error::Internal(format!("Failed to propose: {}", e)))?;
        
        Ok(())
    }
    
    /// Check if this node is the leader
    pub fn is_leader(&self) -> bool {
        let raw_node = self.raw_node.lock().unwrap();
        raw_node.raft.state == StateRole::Leader
    }
    
    /// Get the current leader ID
    pub fn leader_id(&self) -> Option<NodeId> {
        let raw_node = self.raw_node.lock().unwrap();
        Some(raw_node.raft.leader_id)
    }
    
    /// Process ready events
    pub fn process_ready(&self) -> Result<Vec<Proposal>> {
        let mut raw_node = self.raw_node.lock().unwrap();
        
        if !raw_node.has_ready() {
            return Ok(Vec::new());
        }
        
        let mut ready = raw_node.ready();
        let mut committed_proposals = Vec::new();
        
        // Handle committed entries
        let committed_entries = ready.take_committed_entries();
        if !committed_entries.is_empty() {
            for entry in committed_entries {
                if entry.data.is_empty() {
                    // Empty entry, skip
                    continue;
                }
                
                // Deserialize proposal
                match serde_json::from_slice::<Proposal>(&entry.data) {
                    Ok(proposal) => {
                        committed_proposals.push(proposal);
                    }
                    Err(e) => {
                        tracing::error!("Failed to deserialize proposal: {}", e);
                    }
                }
            }
        }
        
        // Apply to storage
        if !ready.entries().is_empty() {
            let storage = raw_node.raft.raft_log.store.clone();
            storage.wl().append(ready.entries()).unwrap();
        }
        
        if let Some(hs) = ready.hs() {
            let storage = raw_node.raft.raft_log.store.clone();
            storage.wl().set_hardstate(hs.clone());
        }
        
        // Send messages
        for msg in ready.take_messages() {
            // In a real implementation, send these messages to other nodes
            tracing::debug!("Would send message: {:?}", msg);
        }
        
        // Apply snapshot
        if *ready.snapshot() != Snapshot::default() {
            let storage = raw_node.raft.raft_log.store.clone();
            storage.wl().apply_snapshot(ready.snapshot().clone()).unwrap();
        }
        
        // Advance the node
        let mut light_rd = raw_node.advance(ready);
        
        // Update commit index
        if let Some(commit) = light_rd.commit_index() {
            let storage = raw_node.raft.raft_log.store.clone();
            storage.wl().mut_hard_state().set_commit(commit);
        }
        
        // Send messages
        for msg in light_rd.take_messages() {
            // In a real implementation, send these messages to other nodes
            tracing::debug!("Would send message: {:?}", msg);
        }
        
        raw_node.advance_apply();
        
        Ok(committed_proposals)
    }
    
    /// Campaign to become leader
    pub fn campaign(&self) -> Result<()> {
        let mut raw_node = self.raw_node.lock().unwrap();
        raw_node.campaign()
            .map_err(|e| Error::Internal(format!("Failed to campaign: {}", e)))?;
        Ok(())
    }
    
    /// Step a message
    pub fn step(&self, msg: Message) -> Result<()> {
        let mut raw_node = self.raw_node.lock().unwrap();
        raw_node.step(msg)
            .map_err(|e| Error::Internal(format!("Failed to step message: {}", e)))?;
        Ok(())
    }
    
    /// Tick the raft node
    pub fn tick(&self) {
        let mut raw_node = self.raw_node.lock().unwrap();
        raw_node.tick();
    }
}

/// State machine that applies committed proposals
pub struct StateMachine {
    state: Arc<Mutex<ControllerState>>,
}

impl StateMachine {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(ControllerState {
                topics: HashMap::new(),
                partition_leaders: HashMap::new(),
                isr_lists: HashMap::new(),
                consumer_groups: HashMap::new(),
                brokers: HashMap::new(),
            })),
        }
    }
    
    /// Apply a committed proposal
    pub fn apply(&self, proposal: Proposal) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        
        match proposal {
            Proposal::CreateTopic(config) => {
                state.topics.insert(config.name.clone(), config);
            }
            Proposal::DeleteTopic(name) => {
                state.topics.remove(&name);
                // Also remove partition leaders and ISRs
                state.partition_leaders.retain(|tp, _| tp.topic != name);
                state.isr_lists.retain(|tp, _| tp.topic != name);
            }
            Proposal::UpdatePartitionLeader { topic_partition, leader } => {
                state.partition_leaders.insert(topic_partition, leader);
            }
            Proposal::UpdateIsr { topic_partition, isr } => {
                state.isr_lists.insert(topic_partition, isr);
            }
            Proposal::RegisterBroker(broker) => {
                state.brokers.insert(broker.id, broker);
            }
            Proposal::UnregisterBroker(id) => {
                state.brokers.remove(&id);
            }
            Proposal::CreateConsumerGroup(group) => {
                state.consumer_groups.insert(group.id.clone(), group);
            }
            Proposal::UpdateConsumerGroup(group) => {
                state.consumer_groups.insert(group.id.clone(), group);
            }
            Proposal::DeleteConsumerGroup(id) => {
                state.consumer_groups.remove(&id);
            }
        }
        
        Ok(())
    }
    
    /// Get current state
    pub fn get_state(&self) -> ControllerState {
        self.state.lock().unwrap().clone()
    }
}