//! Raft-backed metadata store implementation (v2.2.7 Phase 3)
//!
//! This module provides a unified metadata store that works for both single-node
//! and multi-node deployments by wrapping RaftCluster.
//!
//! Key features:
//! - **Single-node**: Zero overhead (synchronous apply, <100μs)
//! - **Multi-node**: Full Raft consensus (replicated, 10-50ms)
//! - **Seamless scaling**: Same interface for 1-N nodes
//! - **Single source of truth**: Raft state machine

use std::sync::Arc;
use async_trait::async_trait;
use chronik_common::metadata::{
    MetadataStore, MetadataError, Result,
    TopicConfig, TopicMetadata, BrokerMetadata, BrokerStatus,
    PartitionAssignment, ConsumerGroupMetadata, ConsumerOffset,
    SegmentMetadata,
};
use crate::raft_cluster::RaftCluster;
use crate::raft_metadata::MetadataCommand;

/// Raft-backed metadata store implementation
///
/// Works for both single-node and multi-node deployments:
/// - Single-node: Zero overhead (synchronous apply)
/// - Multi-node: Full Raft consensus (replicated)
pub struct RaftMetadataStore {
    raft: Arc<RaftCluster>,
}

impl RaftMetadataStore {
    /// Create a new RaftMetadataStore
    pub fn new(raft: Arc<RaftCluster>) -> Self {
        Self { raft }
    }

    /// Get read-only access to state machine
    fn state(&self) -> std::sync::RwLockReadGuard<crate::raft_metadata::MetadataStateMachine> {
        self.raft.get_state_machine()
    }
}

#[async_trait]
impl MetadataStore for RaftMetadataStore {
    // ========== Topic operations ==========

    async fn create_topic(&self, name: &str, config: TopicConfig) -> Result<TopicMetadata> {
        // Propose to Raft (handles single-node vs multi-node internally)
        self.raft.propose(MetadataCommand::CreateTopic {
            name: name.to_string(),
            partition_count: config.partition_count,
            replication_factor: config.replication_factor,
            config: config.config.clone(),
        }).await.map_err(|e| MetadataError::StorageError(e.to_string()))?;

        // Wait for Raft entry to be applied with retry logic (v2.2.7 fix)
        // In multi-node clusters, the Raft entry needs time to be committed and applied
        let max_attempts = 20; // 20 attempts * 50ms = 1 second max wait
        let retry_interval = tokio::time::Duration::from_millis(50);

        for attempt in 1..=max_attempts {
            // Check if topic exists in state machine (scope to release lock immediately)
            let found_topic = {
                let state = self.state();
                state.topics.get(name).cloned()
            }; // Lock dropped here

            if let Some(topic) = found_topic {
                tracing::debug!(
                    "Topic '{}' found in state machine after {} attempts ({} ms)",
                    name,
                    attempt,
                    attempt * 50
                );
                return Ok(topic);
            }

            // If not found and not last attempt, wait before retry
            if attempt < max_attempts {
                tokio::time::sleep(retry_interval).await;
            }
        }

        // After all retries, topic still not found
        Err(MetadataError::NotFound(format!(
            "Topic {} not found after creation (waited {} ms)",
            name,
            max_attempts * 50
        )))
    }

    async fn get_topic(&self, name: &str) -> Result<Option<TopicMetadata>> {
        let state = self.state();
        Ok(state.topics.get(name).cloned())
    }

    async fn list_topics(&self) -> Result<Vec<TopicMetadata>> {
        let state = self.state();
        Ok(state.topics.values().cloned().collect())
    }

    async fn update_topic(&self, name: &str, config: TopicConfig) -> Result<TopicMetadata> {
        // For now, we don't have an UpdateTopic command, so return error
        // TODO: Add UpdateTopic command in future
        Err(MetadataError::StorageError(
            format!("Topic update not yet implemented for {}", name)
        ))
    }

    async fn delete_topic(&self, name: &str) -> Result<()> {
        self.raft.propose(MetadataCommand::DeleteTopic {
            name: name.to_string(),
        }).await.map_err(|e| MetadataError::StorageError(e.to_string()))?;

        Ok(())
    }

    // ========== Segment operations (local, not in Raft) ==========

    async fn persist_segment_metadata(&self, _metadata: SegmentMetadata) -> Result<()> {
        // Segment metadata is local to each node, not replicated via Raft
        // This is handled by local storage layer
        Ok(())
    }

    async fn get_segment_metadata(&self, _topic: &str, _segment_id: &str) -> Result<Option<SegmentMetadata>> {
        // Segment metadata is local, not in Raft
        Ok(None)
    }

    async fn list_segments(&self, _topic: &str, _partition: Option<u32>) -> Result<Vec<SegmentMetadata>> {
        // Segment metadata is local, not in Raft
        Ok(vec![])
    }


    // ========== Consumer offset operations ==========

    async fn commit_offset(&self, offset: ConsumerOffset) -> Result<()> {
        self.raft.propose(MetadataCommand::CommitOffset {
            group_id: offset.group_id.clone(),
            topic: offset.topic.clone(),
            partition: offset.partition,
            offset: offset.offset,
            metadata: offset.metadata.clone(),
        }).await.map_err(|e| MetadataError::StorageError(e.to_string()))?;

        Ok(())
    }

    async fn get_consumer_offset(&self, group_id: &str, topic: &str, partition: u32) -> Result<Option<ConsumerOffset>> {
        let state = self.state();
        let key = (group_id.to_string(), topic.to_string(), partition);

        if let Some(&offset) = state.consumer_offsets.get(&key) {
            Ok(Some(ConsumerOffset {
                group_id: group_id.to_string(),
                topic: topic.to_string(),
                partition,
                offset,
                metadata: None,
                commit_timestamp: chrono::Utc::now(),
            }))
        } else {
            Ok(None)
        }
    }


    // ========== Broker operations ==========

    async fn register_broker(&self, metadata: BrokerMetadata) -> Result<()> {
        self.raft.propose(MetadataCommand::RegisterBroker {
            broker_id: metadata.broker_id,
            host: metadata.host.clone(),
            port: metadata.port,
            rack: metadata.rack.clone(),
        }).await.map_err(|e| MetadataError::StorageError(e.to_string()))?;

        // Wait for Raft entry to be applied with retry logic (v2.2.7+ fix)
        // In multi-node clusters, the Raft entry needs time to be committed and applied
        // ALSO wait for leader election - followers may take 2-3 seconds to connect
        let max_attempts = 80; // 80 attempts * 50ms = 4 seconds max wait (covers leader election)
        let retry_interval = tokio::time::Duration::from_millis(50);

        for attempt in 1..=max_attempts {
            // Check if broker exists in state machine (scope to release lock immediately)
            let found_broker = {
                let state = self.state();
                state.brokers.get(&metadata.broker_id).cloned()
            }; // Lock dropped here

            if found_broker.is_some() {
                tracing::debug!(
                    "Broker {} registered in state machine after {} attempts ({} ms)",
                    metadata.broker_id,
                    attempt,
                    attempt * 50
                );
                return Ok(());
            }

            // If not found and not last attempt, wait before retry
            if attempt < max_attempts {
                tokio::time::sleep(retry_interval).await;
            }
        }

        // After all retries, broker still not registered
        Err(MetadataError::NotFound(format!(
            "Broker {} not found after registration (waited {} ms)",
            metadata.broker_id,
            max_attempts * 50
        )))
    }

    async fn get_broker(&self, broker_id: i32) -> Result<Option<BrokerMetadata>> {
        let state = self.state();
        if let Some(broker_info) = state.brokers.get(&broker_id) {
            Ok(Some(BrokerMetadata {
                broker_id,
                host: broker_info.host.clone(),
                port: broker_info.port,
                rack: broker_info.rack.clone(),
                status: BrokerStatus::Online,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            }))
        } else {
            Ok(None)
        }
    }

    async fn list_brokers(&self) -> Result<Vec<BrokerMetadata>> {
        let state = self.state();
        let brokers = state.brokers.iter().map(|(&broker_id, broker_info)| {
            BrokerMetadata {
                broker_id,
                host: broker_info.host.clone(),
                port: broker_info.port,
                rack: broker_info.rack.clone(),
                status: BrokerStatus::Online,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            }
        }).collect();

        Ok(brokers)
    }

    async fn update_broker_status(&self, broker_id: i32, status: BrokerStatus) -> Result<()> {
        let status_str = match status {
            BrokerStatus::Online => "online",
            BrokerStatus::Offline => "offline",
            BrokerStatus::Maintenance => "maintenance",
        };

        self.raft.propose(MetadataCommand::UpdateBrokerStatus {
            broker_id,
            status: status_str.to_string(),
        }).await.map_err(|e| MetadataError::StorageError(e.to_string()))?;

        Ok(())
    }


    // ========== Partition assignment operations ==========

    async fn assign_partition(&self, assignment: PartitionAssignment) -> Result<()> {
        // For now, we use the existing AssignPartition command which uses node IDs
        // Convert broker_id to node_id (they should be the same in our case)
        let node_id = assignment.broker_id as u64;

        self.raft.propose(MetadataCommand::AssignPartition {
            topic: assignment.topic.clone(),
            partition: assignment.partition as i32,
            replicas: vec![node_id],
        }).await.map_err(|e| MetadataError::StorageError(e.to_string()))?;

        if assignment.is_leader {
            self.raft.propose(MetadataCommand::SetPartitionLeader {
                topic: assignment.topic.clone(),
                partition: assignment.partition as i32,
                leader: node_id,
            }).await.map_err(|e| MetadataError::StorageError(e.to_string()))?;
        }

        Ok(())
    }

    async fn get_partition_leader(&self, topic: &str, partition: u32) -> Result<Option<i32>> {
        let state = self.state();
        Ok(state.partition_leaders
            .get(&(topic.to_string(), partition as i32))
            .map(|&node_id| node_id as i32))
    }



    // ========== Consumer group operations ==========

    async fn get_consumer_group(&self, group_id: &str) -> Result<Option<ConsumerGroupMetadata>> {
        let state = self.state();
        Ok(state.consumer_groups.get(group_id).cloned())
    }

    async fn update_consumer_group(&self, metadata: ConsumerGroupMetadata) -> Result<()> {
        self.raft.propose(MetadataCommand::UpdateConsumerGroup {
            group_id: metadata.group_id.clone(),
            state: metadata.state.clone(),
            generation_id: metadata.generation_id,
            leader: metadata.leader.clone(),
        }).await.map_err(|e| MetadataError::StorageError(e.to_string()))?;

        Ok(())
    }

    // ========== Missing trait methods (stubs for now) ==========

    async fn delete_segment(&self, _topic: &str, _segment_id: &str) -> Result<()> {
        Ok(())
    }

    async fn get_partition_assignments(&self, topic: &str) -> Result<Vec<PartitionAssignment>> {
        let state = self.state();
        let mut assignments = Vec::new();

        // Get partition count for this topic
        let partition_count = state.topics.get(topic)
            .map(|t| t.config.partition_count)
            .unwrap_or(0);

        for partition in 0..partition_count {
            let key = (topic.to_string(), partition as i32);

            if let Some(replicas) = state.partition_assignments.get(&key) {
                let leader = state.partition_leaders.get(&key).copied();

                for &replica_node_id in replicas {
                    assignments.push(PartitionAssignment {
                        topic: topic.to_string(),
                        partition,
                        broker_id: replica_node_id as i32,
                        is_leader: Some(replica_node_id) == leader,
                    });
                }
            }
        }

        Ok(assignments)
    }

    async fn get_partition_replicas(&self, topic: &str, partition: u32) -> Result<Option<Vec<i32>>> {
        let state = self.state();
        Ok(state.partition_assignments
            .get(&(topic.to_string(), partition as i32))
            .map(|replicas| replicas.iter().map(|&node_id| node_id as i32).collect()))
    }

    async fn update_partition_offset(&self, topic: &str, partition: u32, high_watermark: i64, log_start_offset: i64) -> Result<()> {
        self.raft.propose(MetadataCommand::UpdatePartitionOffset {
            topic: topic.to_string(),
            partition,
            high_watermark,
            log_start_offset,
        }).await.map_err(|e| MetadataError::StorageError(e.to_string()))?;

        Ok(())
    }

    async fn get_partition_offset(&self, topic: &str, partition: u32) -> Result<Option<(i64, i64)>> {
        let state = self.state();
        let key = (topic.to_string(), partition as i32);

        let hw = state.partition_high_watermarks.get(&key).copied();
        let lso = state.partition_log_start_offsets.get(&key).copied();

        match (hw, lso) {
            (Some(hw), Some(lso)) => Ok(Some((hw, lso))),
            _ => Ok(None),
        }
    }

    async fn init_system_state(&self) -> Result<()> {
        // System topics will be created separately via create_topic
        // No special initialization needed in Raft-backed store
        Ok(())
    }

    async fn create_topic_with_assignments(&self,
        topic_name: &str,
        config: TopicConfig,
        assignments: Vec<PartitionAssignment>,
        _offsets: Vec<(u32, i64, i64)>
    ) -> Result<TopicMetadata> {
        // Create topic first
        let metadata = self.create_topic(topic_name, config).await?;

        // Then assign partitions
        for assignment in assignments {
            self.assign_partition(assignment).await?;
        }

        Ok(metadata)
    }

    async fn create_consumer_group(&self, metadata: ConsumerGroupMetadata) -> Result<()> {
        self.raft.propose(MetadataCommand::CreateConsumerGroup {
            group_id: metadata.group_id.clone(),
            protocol_type: metadata.protocol_type.clone(),
            protocol: metadata.protocol.clone(),
        }).await.map_err(|e| MetadataError::StorageError(e.to_string()))?;

        Ok(())
    }

    // ========== Transaction operations (not implemented yet) ==========

    async fn commit_transactional_offsets(
        &self,
        _transactional_id: String,
        _producer_id: i64,
        _producer_epoch: i16,
        _group_id: String,
        _offsets: Vec<(String, u32, i64, Option<String>)>,
    ) -> Result<()> {
        Err(MetadataError::StorageError("Transactions not yet implemented in RaftMetadataStore".to_string()))
    }

    async fn begin_transaction(
        &self,
        _transactional_id: String,
        _producer_id: i64,
        _producer_epoch: i16,
        _timeout_ms: i32,
    ) -> Result<()> {
        Err(MetadataError::StorageError("Transactions not yet implemented in RaftMetadataStore".to_string()))
    }

    async fn add_partitions_to_transaction(
        &self,
        _transactional_id: String,
        _producer_id: i64,
        _producer_epoch: i16,
        _partitions: Vec<(String, u32)>,
    ) -> Result<()> {
        Err(MetadataError::StorageError("Transactions not yet implemented in RaftMetadataStore".to_string()))
    }

    async fn add_offsets_to_transaction(
        &self,
        _transactional_id: String,
        _producer_id: i64,
        _producer_epoch: i16,
        _group_id: String,
    ) -> Result<()> {
        Err(MetadataError::StorageError("Transactions not yet implemented in RaftMetadataStore".to_string()))
    }

    async fn prepare_commit_transaction(
        &self,
        _transactional_id: String,
        _producer_id: i64,
        _producer_epoch: i16,
    ) -> Result<()> {
        Err(MetadataError::StorageError("Transactions not yet implemented in RaftMetadataStore".to_string()))
    }

    async fn commit_transaction(
        &self,
        _transactional_id: String,
        _producer_id: i64,
        _producer_epoch: i16,
    ) -> Result<()> {
        Err(MetadataError::StorageError("Transactions not yet implemented in RaftMetadataStore".to_string()))
    }

    async fn abort_transaction(
        &self,
        _transactional_id: String,
        _producer_id: i64,
        _producer_epoch: i16,
    ) -> Result<()> {
        Err(MetadataError::StorageError("Transactions not yet implemented in RaftMetadataStore".to_string()))
    }

    async fn fence_producer(
        &self,
        _transactional_id: String,
        _old_producer_id: i64,
        _old_producer_epoch: i16,
        _new_producer_id: i64,
        _new_producer_epoch: i16,
    ) -> Result<()> {
        Err(MetadataError::StorageError("Transactions not yet implemented in RaftMetadataStore".to_string()))
    }
}
