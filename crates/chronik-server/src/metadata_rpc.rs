//! RPC protocol for metadata forwarding between Raft nodes
//!
//! v2.2.7 Phase 1: Leader-Forwarding Pattern
//!
//! This module defines the RPC types used for followers to forward
//! metadata queries and writes to the Raft leader. This prevents
//! split-brain scenarios where followers might return stale data.

use serde::{Serialize, Deserialize};
use chronik_common::metadata::{TopicMetadata, BrokerMetadata, PartitionAssignment};

/// Metadata query types for leader-forwarding
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MetadataQuery {
    /// Query for a specific topic
    GetTopic { name: String },

    /// List all topics
    ListTopics,

    /// Query for a specific broker
    GetBroker { broker_id: i32 },

    /// List all brokers
    ListBrokers,

    /// Get partition assignment (leader, replicas, ISR)
    GetPartitionAssignment {
        topic: String,
        partition: i32
    },

    /// Get high watermark for a partition
    GetHighWatermark {
        topic: String,
        partition: i32
    },

    /// Get partition count for a topic
    GetPartitionCount { topic: String },
}

/// Metadata query response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MetadataQueryResponse {
    /// Single topic result
    Topic(Option<TopicMetadata>),

    /// List of topics
    TopicList(Vec<TopicMetadata>),

    /// Single broker result
    Broker(Option<BrokerMetadata>),

    /// List of brokers
    BrokerList(Vec<BrokerMetadata>),

    /// Partition assignment
    PartitionAssignment(Option<PartitionAssignment>),

    /// High watermark offset
    HighWatermark(i64),

    /// Partition count
    PartitionCount(i32),
}

/// Metadata write commands for leader-forwarding
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MetadataWriteCommand {
    /// Create a new topic
    CreateTopic {
        name: String,
        partition_count: i32,
        replication_factor: i32,
        config: std::collections::HashMap<String, String>,
    },

    /// Register a broker
    RegisterBroker {
        broker_id: i32,
        host: String,
        port: i32,
        rack: Option<String>,
    },

    /// Set partition leader
    SetPartitionLeader {
        topic: String,
        partition: i32,
        leader_id: u64
    },

    /// Update high watermark
    UpdateHighWatermark {
        topic: String,
        partition: i32,
        offset: i64,
    },

    /// Create a consumer group
    CreateConsumerGroup {
        group_id: String,
        protocol_type: String,
        protocol: String,
    },

    /// Delete a topic
    DeleteTopic {
        name: String,
    },

    /// Commit consumer offset
    CommitOffset {
        group_id: String,
        topic: String,
        partition: i32,
        offset: i64,
    },

    /// Assign partition
    AssignPartition {
        topic: String,
        partition: i32,
        replicas: Vec<u64>,
    },

    /// Update broker status
    UpdateBrokerStatus {
        broker_id: i32,
        status: String, // "Online" or "Offline"
    },

    /// Update consumer group
    UpdateConsumerGroup {
        group_id: String,
        state: String,
        generation_id: i32,
        leader: Option<String>,
    },

    /// Update partition offset
    UpdatePartitionOffset {
        topic: String,
        partition: u32,
        high_watermark: i64,
        log_start_offset: i64,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metadata_query_serialization() {
        let query = MetadataQuery::GetTopic { name: "test".to_string() };
        let bytes = bincode::serialize(&query).unwrap();
        let deserialized: MetadataQuery = bincode::deserialize(&bytes).unwrap();

        match deserialized {
            MetadataQuery::GetTopic { name } => assert_eq!(name, "test"),
            _ => panic!("Wrong query type"),
        }
    }

    #[test]
    fn test_metadata_write_command_serialization() {
        let cmd = MetadataWriteCommand::CreateTopic {
            name: "test-topic".to_string(),
            partition_count: 3,
            replication_factor: 3,
            config: std::collections::HashMap::new(),
        };

        let bytes = bincode::serialize(&cmd).unwrap();
        let deserialized: MetadataWriteCommand = bincode::deserialize(&bytes).unwrap();

        match deserialized {
            MetadataWriteCommand::CreateTopic { name, partition_count, replication_factor, .. } => {
                assert_eq!(name, "test-topic");
                assert_eq!(partition_count, 3);
                assert_eq!(replication_factor, 3);
            }
            _ => panic!("Wrong command type"),
        }
    }
}
