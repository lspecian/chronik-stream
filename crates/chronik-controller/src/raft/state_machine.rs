//! Controller state machine for Raft consensus.

use chronik_common::{Result, Error};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::SystemTime;
use std::net::SocketAddr;

/// Broker ID type
pub type BrokerId = u32;

/// Controller state managed by Raft
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
    
    /// Cluster metadata
    pub cluster_id: Option<String>,
    
    /// Controller epoch
    pub controller_epoch: u64,
    
    /// Metadata version for change tracking
    pub metadata_version: u64,
    
    /// Controller ID (if this node is the controller)
    pub controller_id: Option<u32>,
    
    /// Leader epoch for controller leader election
    pub leader_epoch: u64,
    
    /// Raft cluster status
    pub raft_status: Option<String>,
}

/// Topic configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopicConfig {
    pub name: String,
    pub partition_count: u32,
    pub replication_factor: u16,
    pub config: HashMap<String, String>,
    pub created_at: SystemTime,
    pub version: u64,
}

/// Topic partition identifier
#[derive(Debug, Clone, Hash, Eq, PartialEq, Serialize, Deserialize)]
pub struct TopicPartition {
    pub topic: String,
    pub partition: i32,
}

impl TopicPartition {
    pub fn new(topic: String, partition: i32) -> Self {
        Self { topic, partition }
    }
}

/// Broker information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrokerInfo {
    pub id: BrokerId,
    pub address: SocketAddr,
    pub rack: Option<String>,
    pub status: Option<String>,
    pub version: Option<String>,
    pub metadata_version: Option<u64>,
}

/// Consumer group metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsumerGroup {
    pub group_id: String,
    pub state: GroupState,
    pub protocol_type: String,
    pub generation_id: i32,
    pub leader: Option<String>,
    pub members: Vec<ConsumerMember>,
    pub topics: Vec<String>,
}

/// Consumer group state
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
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
    pub subscriptions: Vec<String>,
}

/// Consumer group member
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsumerMember {
    pub member_id: String,
    pub client_id: String,
    pub host: String,
    pub session_timeout_ms: u32,
}

/// Proposals that can be made to the Raft cluster
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Proposal {
    /// Create a new topic
    CreateTopic(TopicConfig),
    
    /// Delete an existing topic
    DeleteTopic(String),
    
    /// Update topic configuration
    UpdateTopicConfig {
        topic: String,
        configs: HashMap<String, String>,
    },
    
    /// Update partition leader
    UpdatePartitionLeader {
        topic_partition: TopicPartition,
        leader: BrokerId,
        leader_epoch: u32,
    },
    
    /// Update in-sync replica list
    UpdateIsr {
        topic_partition: TopicPartition,
        isr: Vec<BrokerId>,
        leader_epoch: u32,
    },
    
    /// Register a broker
    RegisterBroker(BrokerInfo),
    
    /// Unregister a broker
    UnregisterBroker(BrokerId),
    
    /// Create consumer group
    CreateConsumerGroup(ConsumerGroup),
    
    /// Update consumer group
    UpdateConsumerGroup(ConsumerGroup),
    
    /// Delete consumer group
    DeleteConsumerGroup(String),
    
    /// Update cluster ID
    UpdateClusterId(String),
}

/// Controller state machine that applies Raft proposals
pub struct ControllerStateMachine {
    state: Arc<RwLock<ControllerState>>,
}

impl ControllerStateMachine {
    /// Create a new state machine
    pub fn new() -> Self {
        Self {
            state: Arc::new(RwLock::new(ControllerState::default())),
        }
    }
    
    /// Create from existing state
    pub fn from_state(state: ControllerState) -> Self {
        Self {
            state: Arc::new(RwLock::new(state)),
        }
    }
    
    /// Apply a proposal to the state machine
    pub fn apply(&self, proposal: Proposal) -> Result<()> {
        let mut state = self.state.write().unwrap();
        
        match proposal {
            Proposal::CreateTopic(config) => {
                if state.topics.contains_key(&config.name) {
                    return Err(Error::Internal(format!("Topic {} already exists", config.name)));
                }
                state.topics.insert(config.name.clone(), config);
            }
            
            Proposal::DeleteTopic(name) => {
                state.topics.remove(&name);
                // Remove all partition metadata
                state.partition_leaders.retain(|tp, _| tp.topic != name);
                state.isr_lists.retain(|tp, _| tp.topic != name);
            }
            
            Proposal::UpdateTopicConfig { topic, configs } => {
                if let Some(topic_config) = state.topics.get_mut(&topic) {
                    topic_config.config.extend(configs);
                    topic_config.version += 1;
                } else {
                    return Err(Error::NotFound(format!("Topic {} not found", topic)));
                }
            }
            
            Proposal::UpdatePartitionLeader { topic_partition, leader, .. } => {
                state.partition_leaders.insert(topic_partition, leader);
            }
            
            Proposal::UpdateIsr { topic_partition, isr, .. } => {
                state.isr_lists.insert(topic_partition, isr);
            }
            
            Proposal::RegisterBroker(broker) => {
                state.brokers.insert(broker.id, broker);
            }
            
            Proposal::UnregisterBroker(id) => {
                state.brokers.remove(&id);
                // Remove broker from ISR lists
                for isr in state.isr_lists.values_mut() {
                    isr.retain(|&broker_id| broker_id != id);
                }
            }
            
            Proposal::CreateConsumerGroup(group) => {
                state.consumer_groups.insert(group.group_id.clone(), group);
            }
            
            Proposal::UpdateConsumerGroup(group) => {
                state.consumer_groups.insert(group.group_id.clone(), group);
            }
            
            Proposal::DeleteConsumerGroup(id) => {
                state.consumer_groups.remove(&id);
            }
            
            Proposal::UpdateClusterId(id) => {
                state.cluster_id = Some(id);
            }
        }
        
        // Increment controller epoch on state change
        state.controller_epoch += 1;
        state.metadata_version += 1;
        
        Ok(())
    }
    
    /// Get current state
    pub fn get_state(&self) -> ControllerState {
        self.state.read().unwrap().clone()
    }
    
    /// Create a snapshot of the current state
    pub fn snapshot(&self) -> Vec<u8> {
        let state = self.state.read().unwrap();
        bincode::serialize(&*state).unwrap()
    }
    
    /// Restore state from a snapshot
    pub fn restore(&self, data: &[u8]) -> Result<()> {
        let new_state: ControllerState = bincode::deserialize(data)
            .map_err(|e| Error::Internal(format!("Failed to deserialize snapshot: {}", e)))?;
        
        let mut state = self.state.write().unwrap();
        *state = new_state;
        
        Ok(())
    }
}

impl ControllerState {
    /// Check if the cluster is healthy
    pub fn is_healthy(&self) -> bool {
        // Consider cluster healthy if we have online brokers and controller
        let online_brokers = self.get_online_brokers().len();
        online_brokers > 0 && self.controller_id.is_some()
    }
    
    /// Get online brokers
    pub fn get_online_brokers(&self) -> Vec<&BrokerInfo> {
        self.brokers.values()
            .filter(|broker| broker.status.as_deref() == Some("online"))
            .collect()
    }
    
    /// Get total partition count across all topics
    pub fn total_partitions(&self) -> usize {
        self.topics.values()
            .map(|topic| topic.partition_count as usize)
            .sum()
    }
    
    /// Get health issues
    pub fn get_health_issues(&self) -> Vec<String> {
        let mut issues = Vec::new();
        
        // Check for offline brokers
        let offline_brokers = self.brokers.values()
            .filter(|b| b.status.as_deref() != Some("online"))
            .count();
        
        if offline_brokers > 0 {
            issues.push(format!("{} brokers are offline", offline_brokers));
        }
        
        // Check for topics without leaders
        let topics_without_leaders = self.topics.keys()
            .filter(|topic| {
                !self.partition_leaders.keys().any(|tp| &tp.topic == *topic)
            })
            .count();
            
        if topics_without_leaders > 0 {
            issues.push(format!("{} topics have no partition leaders", topics_without_leaders));
        }
        
        // Check if no controller is elected
        if self.controller_id.is_none() {
            issues.push("No controller elected".to_string());
        }
        
        issues
    }
    
    /// Increment metadata version
    pub fn increment_metadata_version(&mut self) {
        self.metadata_version += 1;
    }
}