//! Raft metadata state machine (v2.5.0 Phase 2)
//!
//! This module implements a MINIMAL Raft state machine for cluster metadata coordination.
//! It does NOT handle data replication - that's done by WAL streaming.
//!
//! Managed by Raft:
//! - Cluster membership (which nodes are alive)
//! - Partition assignments (partition-0 → [node1, node2, node3])
//! - Partition leaders (partition-0 leader = node1)
//! - ISR tracking (in-sync replicas per partition)
//!
//! NOT managed by Raft:
//! - Data replication (WAL streaming handles this)
//! - Message writes (ProduceHandler handles this)

use serde::{Serialize, Deserialize};
use std::collections::HashMap;
use anyhow::Result;
use chronik_common::metadata::{TopicMetadata, TopicConfig, ConsumerGroupMetadata, ConsumerOffset};
use chrono::Utc;
use uuid::Uuid;

/// Partition identifier
pub type PartitionKey = (String, i32);  // (topic, partition)

/// Consumer offset key: (group_id, topic, partition)
pub type ConsumerOffsetKey = (String, String, u32);

/// Commands that modify metadata state
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MetadataCommand {
    /// Add a node to the cluster
    AddNode {
        node_id: u64,
        address: String,
    },

    /// Remove a node from the cluster
    RemoveNode {
        node_id: u64,
    },

    /// Register a broker in the cluster (for Kafka client discovery)
    RegisterBroker {
        broker_id: i32,
        host: String,
        port: i32,
        rack: Option<String>,
    },

    /// Update broker status (online/offline/maintenance)
    UpdateBrokerStatus {
        broker_id: i32,
        status: String,  // "online", "offline", "maintenance"
    },

    /// Remove a broker from the cluster
    RemoveBroker {
        broker_id: i32,
    },

    /// Assign partition replicas
    AssignPartition {
        topic: String,
        partition: i32,
        replicas: Vec<u64>,  // Node IDs that should replicate this partition
    },

    /// Set partition leader
    SetPartitionLeader {
        topic: String,
        partition: i32,
        leader: u64,  // Node ID of the leader
    },

    /// Update ISR (in-sync replicas) for a partition
    UpdateISR {
        topic: String,
        partition: i32,
        isr: Vec<u64>,  // Node IDs that are in-sync
    },

    // NEW v2.2.7: Topic metadata commands
    /// Create a new topic
    CreateTopic {
        name: String,
        partition_count: u32,
        replication_factor: u32,
        config: HashMap<String, String>,
    },

    /// Delete a topic
    DeleteTopic {
        name: String,
    },

    // NEW v2.2.7: Consumer offset commands
    /// Commit a consumer offset
    CommitOffset {
        group_id: String,
        topic: String,
        partition: u32,
        offset: i64,
        metadata: Option<String>,
    },

    /// Commit multiple consumer offsets in a batch
    CommitOffsetBatch {
        group_id: String,
        offsets: Vec<(String, u32, i64, Option<String>)>,  // (topic, partition, offset, metadata)
    },

    /// Update partition offset (high watermark and log start offset)
    UpdatePartitionOffset {
        topic: String,
        partition: u32,
        high_watermark: i64,
        log_start_offset: i64,
    },

    // NEW v2.2.7: Consumer group commands
    /// Create a consumer group
    CreateConsumerGroup {
        group_id: String,
        protocol_type: String,
        protocol: String,
    },

    /// Update consumer group metadata
    UpdateConsumerGroup {
        group_id: String,
        state: String,
        generation_id: i32,
        leader: String,
    },

    /// Delete a consumer group
    DeleteConsumerGroup {
        group_id: String,
    },
}

/// Broker information stored in Raft state machine
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrokerInfo {
    pub broker_id: i32,
    pub host: String,
    pub port: i32,
    pub rack: Option<String>,
    pub status: String,  // "online", "offline", "maintenance"
}

/// Metadata state machine (applied from Raft log)
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MetadataStateMachine {
    /// Cluster nodes: node_id → address (for Raft consensus)
    pub nodes: HashMap<u64, String>,

    /// Brokers: broker_id → broker info (for Kafka client discovery)
    /// This is separate from nodes because broker_id (i32) != node_id (u64)
    /// and brokers need additional metadata (host, port, rack)
    pub brokers: HashMap<i32, BrokerInfo>,

    /// Partition assignments: (topic, partition) → replica node IDs
    pub partition_assignments: HashMap<PartitionKey, Vec<u64>>,

    /// Partition leaders: (topic, partition) → leader node ID
    pub partition_leaders: HashMap<PartitionKey, u64>,

    /// ISR sets: (topic, partition) → in-sync replica node IDs
    pub isr_sets: HashMap<PartitionKey, Vec<u64>>,

    // NEW v2.2.7: ALL metadata now in Raft state machine
    /// Topics: topic name → topic metadata
    pub topics: HashMap<String, TopicMetadata>,

    /// Consumer groups: group_id → group metadata
    pub consumer_groups: HashMap<String, ConsumerGroupMetadata>,

    /// Consumer offsets: (group_id, topic, partition) → offset
    pub consumer_offsets: HashMap<ConsumerOffsetKey, i64>,

    /// Partition high watermarks: (topic, partition) → high watermark
    pub partition_high_watermarks: HashMap<PartitionKey, i64>,

    /// Partition log start offsets: (topic, partition) → log start offset
    pub partition_log_start_offsets: HashMap<PartitionKey, i64>,
}

impl MetadataStateMachine {
    /// Create a new empty state machine
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply a command to the state machine (called by Raft)
    pub fn apply(&mut self, cmd: MetadataCommand) -> Result<Vec<u8>> {
        match cmd {
            MetadataCommand::AddNode { node_id, address } => {
                self.nodes.insert(node_id, address);
                Ok(vec![])
            }

            MetadataCommand::RemoveNode { node_id } => {
                self.nodes.remove(&node_id);
                Ok(vec![])
            }

            MetadataCommand::RegisterBroker { broker_id, host, port, rack } => {
                let broker_info = BrokerInfo {
                    broker_id,
                    host,
                    port,
                    rack,
                    status: "online".to_string(),
                };
                self.brokers.insert(broker_id, broker_info);
                Ok(vec![])
            }

            MetadataCommand::UpdateBrokerStatus { broker_id, status } => {
                if let Some(broker) = self.brokers.get_mut(&broker_id) {
                    broker.status = status;
                }
                Ok(vec![])
            }

            MetadataCommand::RemoveBroker { broker_id } => {
                self.brokers.remove(&broker_id);
                Ok(vec![])
            }

            MetadataCommand::AssignPartition { topic, partition, replicas } => {
                self.partition_assignments.insert((topic, partition), replicas);
                Ok(vec![])
            }

            MetadataCommand::SetPartitionLeader { topic, partition, leader } => {
                self.partition_leaders.insert((topic, partition), leader);
                Ok(vec![])
            }

            MetadataCommand::UpdateISR { topic, partition, isr } => {
                self.isr_sets.insert((topic, partition), isr);
                Ok(vec![])
            }

            // NEW v2.2.7: Topic metadata commands
            MetadataCommand::CreateTopic { name, partition_count, replication_factor, config } => {
                let metadata = TopicMetadata {
                    id: Uuid::new_v4(),
                    name: name.clone(),
                    config: TopicConfig {
                        partition_count: partition_count.clone(),
                        replication_factor: replication_factor.clone(),
                        retention_ms: None,
                        segment_bytes: 100 * 1024 * 1024,  // 100MB default
                        config: config.clone(),
                    },
                    created_at: Utc::now(),
                    updated_at: Utc::now(),
                };
                self.topics.insert(name.clone(), metadata);
                Ok(vec![])
            }

            MetadataCommand::DeleteTopic { name } => {
                self.topics.remove(name.as_str());
                Ok(vec![])
            }

            // NEW v2.2.7: Consumer offset commands
            MetadataCommand::CommitOffset { group_id, topic, partition, offset, metadata: _ } => {
                let key = (group_id.clone(), topic.clone(), partition.clone());
                self.consumer_offsets.insert(key, offset.clone());
                Ok(vec![])
            }

            MetadataCommand::CommitOffsetBatch { group_id, offsets } => {
                for (topic, partition, offset, _metadata) in offsets {
                    let key = (group_id.clone(), topic.clone(), partition.clone());
                    self.consumer_offsets.insert(key, offset.clone());
                }
                Ok(vec![])
            }

            MetadataCommand::UpdatePartitionOffset { topic, partition, high_watermark, log_start_offset } => {
                let key = (topic.clone(), partition.clone() as i32);
                self.partition_high_watermarks.insert(key.clone(), high_watermark.clone());
                self.partition_log_start_offsets.insert(key, log_start_offset.clone());
                Ok(vec![])
            }

            // NEW v2.2.7: Consumer group commands
            MetadataCommand::CreateConsumerGroup { group_id, protocol_type, protocol } => {
                let group_metadata = ConsumerGroupMetadata {
                    group_id: group_id.clone(),
                    state: "Empty".to_string(),
                    protocol: protocol.clone(),
                    protocol_type: protocol_type.clone(),
                    generation_id: 0,
                    leader_id: None,
                    leader: String::new(),
                    members: vec![],
                    created_at: Utc::now(),
                    updated_at: Utc::now(),
                };
                self.consumer_groups.insert(group_id.clone(), group_metadata);
                Ok(vec![])
            }

            MetadataCommand::UpdateConsumerGroup { group_id, state, generation_id, leader } => {
                if let Some(group) = self.consumer_groups.get_mut(group_id.as_str()) {
                    group.state = state.clone();
                    group.generation_id = generation_id.clone();
                    group.leader = leader.clone();
                    group.updated_at = Utc::now();
                }
                Ok(vec![])
            }

            MetadataCommand::DeleteConsumerGroup { group_id } => {
                self.consumer_groups.remove(group_id.as_str());
                Ok(vec![])
            }
        }
    }

    /// Get partition replicas
    pub fn get_partition_replicas(&self, topic: &str, partition: i32) -> Option<Vec<u64>> {
        self.partition_assignments
            .get(&(topic.to_string(), partition))
            .cloned()
    }

    /// Get partition leader
    pub fn get_partition_leader(&self, topic: &str, partition: i32) -> Option<u64> {
        self.partition_leaders
            .get(&(topic.to_string(), partition))
            .copied()
    }

    /// Get ISR for partition
    pub fn get_isr(&self, topic: &str, partition: i32) -> Option<Vec<u64>> {
        self.isr_sets
            .get(&(topic.to_string(), partition))
            .cloned()
    }

    /// Check if a node is in ISR for a partition
    pub fn is_in_sync(&self, topic: &str, partition: i32, node_id: u64) -> bool {
        self.get_isr(topic, partition)
            .map(|isr| isr.contains(&node_id))
            .unwrap_or(false)
    }

    /// Get all partitions where the specified node is the leader
    ///
    /// Returns a list of (topic, partition) tuples where this node is the leader.
    /// Used by WAL replication discovery to know which partitions to replicate.
    pub fn get_partitions_where_leader(&self, node_id: u64) -> Vec<PartitionKey> {
        self.partition_leaders
            .iter()
            .filter(|(_key, &leader)| leader == node_id)
            .map(|(key, _leader)| key.clone())
            .collect()
    }

    /// Get broker information
    pub fn get_broker(&self, broker_id: i32) -> Option<&BrokerInfo> {
        self.brokers.get(&broker_id)
    }

    /// Get all brokers
    pub fn get_all_brokers(&self) -> Vec<&BrokerInfo> {
        self.brokers.values().collect()
    }

    /// Get all broker IDs
    pub fn get_broker_ids(&self) -> Vec<i32> {
        self.brokers.keys().copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metadata_state_machine() {
        let mut sm = MetadataStateMachine::new();

        // Add nodes
        sm.apply(MetadataCommand::AddNode {
            node_id: 1,
            address: "localhost:9092".to_string(),
        }).unwrap();

        sm.apply(MetadataCommand::AddNode {
            node_id: 2,
            address: "localhost:9093".to_string(),
        }).unwrap();

        assert_eq!(sm.nodes.len(), 2);

        // Assign partition
        sm.apply(MetadataCommand::AssignPartition {
            topic: "test".to_string(),
            partition: 0,
            replicas: vec![1, 2],
        }).unwrap();

        assert_eq!(sm.get_partition_replicas("test", 0), Some(vec![1, 2]));

        // Set leader
        sm.apply(MetadataCommand::SetPartitionLeader {
            topic: "test".to_string(),
            partition: 0,
            leader: 1,
        }).unwrap();

        assert_eq!(sm.get_partition_leader("test", 0), Some(1));

        // Update ISR
        sm.apply(MetadataCommand::UpdateISR {
            topic: "test".to_string(),
            partition: 0,
            isr: vec![1, 2],
        }).unwrap();

        assert_eq!(sm.get_isr("test", 0), Some(vec![1, 2]));
        assert!(sm.is_in_sync("test", 0, 1));
        assert!(sm.is_in_sync("test", 0, 2));
        assert!(!sm.is_in_sync("test", 0, 3));
    }
}
