//! Simplified Raft implementation for controller nodes.

use chronik_common::{Result, Error};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Node ID type
pub type NodeId = u64;

/// Controller state machine data
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
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

/// Simplified Raft node for single-node testing
pub struct RaftNode {
    pub id: NodeId,
    state: Arc<Mutex<ControllerState>>,
    is_leader: Arc<Mutex<bool>>,
    proposals: Arc<Mutex<Vec<Proposal>>>,
}

impl RaftNode {
    /// Create a new single-node Raft instance
    pub fn new_single_node(id: NodeId) -> Result<Self> {
        Ok(Self {
            id,
            state: Arc::new(Mutex::new(ControllerState::default())),
            is_leader: Arc::new(Mutex::new(false)),
            proposals: Arc::new(Mutex::new(Vec::new())),
        })
    }
    
    /// Create a new multi-node Raft instance (simplified)
    pub fn new(id: NodeId, _peers: Vec<NodeId>) -> Result<Self> {
        // For now, just create a single node
        Self::new_single_node(id)
    }
    
    /// Campaign to become leader
    pub fn campaign(&self) -> Result<()> {
        let mut is_leader = self.is_leader.lock().unwrap();
        *is_leader = true;
        Ok(())
    }
    
    /// Check if this node is the leader
    pub fn is_leader(&self) -> bool {
        *self.is_leader.lock().unwrap()
    }
    
    /// Get the current leader ID
    pub fn leader_id(&self) -> Option<NodeId> {
        if self.is_leader() {
            Some(self.id)
        } else {
            None
        }
    }
    
    /// Propose a state change
    pub fn propose(&self, proposal: Proposal) -> Result<()> {
        if !self.is_leader() {
            return Err(Error::Internal("Not the leader".into()));
        }
        
        let mut proposals = self.proposals.lock().unwrap();
        proposals.push(proposal);
        Ok(())
    }
    
    /// Process ready events (simplified - just applies proposals immediately)
    pub fn process_ready(&self) -> Result<Vec<Proposal>> {
        let mut proposals = self.proposals.lock().unwrap();
        let committed = proposals.drain(..).collect::<Vec<_>>();
        
        // Apply to state
        if !committed.is_empty() {
            let mut state = self.state.lock().unwrap();
            for proposal in &committed {
                match proposal {
                    Proposal::CreateTopic(config) => {
                        state.topics.insert(config.name.clone(), config.clone());
                    }
                    Proposal::DeleteTopic(name) => {
                        state.topics.remove(name);
                        state.partition_leaders.retain(|tp, _| &tp.topic != name);
                        state.isr_lists.retain(|tp, _| &tp.topic != name);
                    }
                    Proposal::UpdatePartitionLeader { topic_partition, leader } => {
                        state.partition_leaders.insert(topic_partition.clone(), *leader);
                    }
                    Proposal::UpdateIsr { topic_partition, isr } => {
                        state.isr_lists.insert(topic_partition.clone(), isr.clone());
                    }
                    Proposal::RegisterBroker(broker) => {
                        state.brokers.insert(broker.id, broker.clone());
                    }
                    Proposal::UnregisterBroker(id) => {
                        state.brokers.remove(id);
                    }
                    Proposal::CreateConsumerGroup(group) => {
                        state.consumer_groups.insert(group.id.clone(), group.clone());
                    }
                    Proposal::UpdateConsumerGroup(group) => {
                        state.consumer_groups.insert(group.id.clone(), group.clone());
                    }
                    Proposal::DeleteConsumerGroup(id) => {
                        state.consumer_groups.remove(id);
                    }
                }
            }
        }
        
        Ok(committed)
    }
    
    /// Tick the raft node (no-op for simplified version)
    pub fn tick(&self) {
        // No-op
    }
    
    /// Get current state
    pub fn get_state(&self) -> ControllerState {
        self.state.lock().unwrap().clone()
    }
}

/// State machine that applies committed proposals
pub struct StateMachine {
    state: Arc<Mutex<ControllerState>>,
}

impl StateMachine {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(ControllerState::default())),
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