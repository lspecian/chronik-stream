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
//!
//! v2.2.9 Performance Fix: Event-driven notifications for topic creation
//! Replaces polling-based retry loop (50ms intervals) with instant wake-up
//! when Raft entries are applied. Provides 10-40x faster topic creation.

use std::sync::Arc;
use async_trait::async_trait;
use dashmap::DashMap;
use tokio::sync::Notify;
use chronik_common::metadata::{
    MetadataStore, MetadataError, Result,
    TopicConfig, TopicMetadata, BrokerMetadata, BrokerStatus,
    PartitionAssignment, ConsumerGroupMetadata, ConsumerOffset,
    SegmentMetadata,
};
use crate::raft_cluster::RaftCluster;
use crate::raft_metadata::MetadataCommand;
use crate::metadata_wal::MetadataWal;
use crate::metadata_wal_replication::MetadataWalReplicator;

/// Raft-backed metadata store implementation
///
/// Works for both single-node and multi-node deployments:
/// - Single-node: Zero overhead (synchronous apply)
/// - Multi-node: Full Raft consensus (replicated)
///
/// v2.2.9: Event-driven notifications for fast topic creation
/// v2.3.0 Phase 2: WAL-based metadata writes (bypasses Raft consensus for 4-5x throughput)
pub struct RaftMetadataStore {
    raft: Arc<RaftCluster>,
    /// Pending topic creation notifications (v2.2.9 performance fix)
    /// Maps topic name → notification channel
    /// When Raft applies CreateTopic command, notifies all waiting threads
    pending_topics: Arc<DashMap<String, Arc<Notify>>>,
    /// Pending broker registration notifications (v2.2.9 performance fix)
    pending_brokers: Arc<DashMap<i32, Arc<Notify>>>,

    /// Phase 2: Metadata WAL for fast local writes (1-2ms vs 10-50ms Raft)
    /// Only used on leader nodes. Followers continue to forward writes to leader.
    metadata_wal: Option<Arc<MetadataWal>>,

    /// Phase 2: Metadata WAL replicator (async fire-and-forget to followers)
    /// Reuses existing WalReplicationManager - no new ports or protocols!
    metadata_wal_replicator: Option<Arc<MetadataWalReplicator>>,

    /// Phase 3: Leader lease manager for fast follower reads
    /// Followers track leader heartbeats to determine if they can safely read
    /// from local replicated state without forwarding to leader
    lease_manager: Option<Arc<crate::leader_lease::LeaseManager>>,
}

impl RaftMetadataStore {
    /// Create a new RaftMetadataStore (v2.2.9 event-driven notifications)
    ///
    /// # Arguments
    /// - `raft`: The RaftCluster that provides metadata state machine
    ///
    /// The notification maps are shared with RaftCluster so that when Raft
    /// applies entries, it can fire notifications to wake up waiting threads.
    ///
    /// Phase 2: This constructor creates a store WITHOUT metadata WAL.
    /// Use `new_with_wal()` for Phase 2 WAL-based writes.
    pub fn new(raft: Arc<RaftCluster>) -> Self {
        Self {
            pending_topics: raft.get_pending_topics_notifications(),
            pending_brokers: raft.get_pending_brokers_notifications(),
            raft,
            metadata_wal: None,
            metadata_wal_replicator: None,
            lease_manager: None,
        }
    }

    /// Create a new RaftMetadataStore with metadata WAL (Phase 2)
    ///
    /// # Arguments
    /// - `raft`: The RaftCluster that provides metadata state machine
    /// - `metadata_wal`: Metadata WAL for fast local writes
    /// - `replicator`: Metadata WAL replicator for async replication to followers
    ///
    /// Phase 2: This enables fast local WAL writes (1-2ms) instead of Raft consensus (10-50ms).
    /// Expected improvement: 4-5x throughput (1,600 → 6,000-8,000 msg/s).
    pub fn new_with_wal(
        raft: Arc<RaftCluster>,
        metadata_wal: Arc<MetadataWal>,
        replicator: Arc<MetadataWalReplicator>,
    ) -> Self {
        Self {
            pending_topics: raft.get_pending_topics_notifications(),
            pending_brokers: raft.get_pending_brokers_notifications(),
            raft,
            metadata_wal: Some(metadata_wal),
            metadata_wal_replicator: Some(replicator),
            lease_manager: None,
        }
    }

    /// Create a new RaftMetadataStore with Phase 2 + Phase 3 enabled
    ///
    /// # Arguments
    /// - `raft`: The RaftCluster that provides metadata state machine
    /// - `metadata_wal`: Metadata WAL for fast local writes (Phase 2)
    /// - `replicator`: Metadata WAL replicator for async replication (Phase 2)
    /// - `lease_manager`: Lease manager for fast follower reads (Phase 3)
    ///
    /// Phase 3: This enables fast follower reads (1-2ms) by checking lease validity.
    /// If lease is valid, read from local state. If expired, forward to leader.
    pub fn new_with_wal_and_lease(
        raft: Arc<RaftCluster>,
        metadata_wal: Arc<MetadataWal>,
        replicator: Arc<MetadataWalReplicator>,
        lease_manager: Arc<crate::leader_lease::LeaseManager>,
    ) -> Self {
        Self {
            pending_topics: raft.get_pending_topics_notifications(),
            pending_brokers: raft.get_pending_brokers_notifications(),
            raft,
            metadata_wal: Some(metadata_wal),
            metadata_wal_replicator: Some(replicator),
            lease_manager: Some(lease_manager),
        }
    }

    /// Get read-only access to state machine
    fn state(&self) -> std::sync::RwLockReadGuard<crate::raft_metadata::MetadataStateMachine> {
        self.raft.get_state_machine()
    }

    /// Notify waiting threads that a topic was created (v2.2.9 performance fix)
    ///
    /// Called by RaftCluster when it applies a CreateTopic command to state machine.
    /// Wakes up all threads waiting for this topic to be created.
    pub fn notify_topic_created(&self, topic_name: &str) {
        if let Some((_, notify)) = self.pending_topics.remove(topic_name) {
            notify.notify_waiters(); // Wake up ALL waiting threads
            tracing::debug!("✓ Notified waiting threads for topic '{}'", topic_name);
        }
    }

    /// Notify waiting threads that a broker was registered (v2.2.9 performance fix)
    pub fn notify_broker_registered(&self, broker_id: i32) {
        if let Some((_, notify)) = self.pending_brokers.remove(&broker_id) {
            notify.notify_waiters();
            tracing::debug!("✓ Notified waiting threads for broker {}", broker_id);
        }
    }
}

#[async_trait]
impl MetadataStore for RaftMetadataStore {
    // ========== Topic operations ==========

    async fn create_topic(&self, name: &str, config: TopicConfig) -> Result<TopicMetadata> {
        // Phase 2: Check if we have metadata WAL enabled (leader fast path)
        if let (Some(metadata_wal), Some(replicator)) = (&self.metadata_wal, &self.metadata_wal_replicator) {
            // PHASE 2 FAST PATH: WAL-based metadata write (1-2ms vs 10-50ms Raft)

            // Check if we're the leader
            if self.raft.am_i_leader() {
                tracing::info!("Phase 2: Leader creating topic '{}' via metadata WAL (fast path)", name);

                // Create metadata command
                let cmd = MetadataCommand::CreateTopic {
                    name: name.to_string(),
                    partition_count: config.partition_count,
                    replication_factor: config.replication_factor,
                    config: config.config.clone(),
                };

                // 1. Write to metadata WAL (durable, 1-2ms)
                let offset = metadata_wal.append(&cmd).await
                    .map_err(|e| MetadataError::StorageError(format!("Metadata WAL write failed: {}", e)))?;

                tracing::info!(
                    "Wrote CreateTopic('{}') to metadata WAL at offset {} (fast!)",
                    name,
                    offset
                );

                // 2. Apply to local state machine immediately
                self.raft.apply_metadata_command_direct(cmd.clone())
                    .map_err(|e| MetadataError::StorageError(format!("Failed to apply command: {}", e)))?;

                tracing::info!("Applied CreateTopic('{}') to state machine", name);

                // 3. Fire notification for any waiting threads
                if let Some((_, notify)) = self.pending_topics.remove(name) {
                    notify.notify_waiters();
                    tracing::debug!("Notified waiting threads for topic '{}'", name);
                }

                // 4. Async replicate to followers (fire-and-forget)
                let replicator_clone = Arc::clone(replicator);
                let cmd_clone = cmd.clone();
                tokio::spawn(async move {
                    if let Err(e) = replicator_clone.replicate(&cmd_clone, offset).await {
                        tracing::warn!("Metadata replication failed for CreateTopic: {}", e);
                        // Don't fail the write - replication is eventual consistency
                    }
                });

                // 5. Return immediately (no waiting for followers!)
                let state = self.state();
                return state.topics.get(name).cloned()
                    .ok_or_else(|| MetadataError::NotFound(format!(
                        "Topic {} not found after creation",
                        name
                    )));
            }
        }

        // FALLBACK PATH: Use Raft consensus (Phase 1 behavior)
        // This path is used by:
        // 1. Followers (always forward to leader via Phase 1 RPC)
        // 2. Leaders without metadata WAL (backward compatibility)
        // 3. Single-node deployments without WAL

        tracing::debug!("Phase 1 fallback: Creating topic '{}' via Raft consensus", name);

        // Register notification channel BEFORE proposing to Raft
        let notify = Arc::new(Notify::new());
        self.pending_topics.insert(name.to_string(), Arc::clone(&notify));

        // Propose to Raft (handles single-node vs multi-node internally)
        self.raft.propose(MetadataCommand::CreateTopic {
            name: name.to_string(),
            partition_count: config.partition_count,
            replication_factor: config.replication_factor,
            config: config.config.clone(),
        }).await.map_err(|e| MetadataError::StorageError(e.to_string()))?;

        // Wait for notification with timeout
        let timeout_duration = tokio::time::Duration::from_millis(2000);

        match tokio::time::timeout(timeout_duration, notify.notified()).await {
            Ok(_) => {
                let state = self.state();
                state.topics.get(name).cloned()
                    .ok_or_else(|| MetadataError::NotFound(format!(
                        "Topic {} notified but not found in state machine",
                        name
                    )))
            }
            Err(_) => {
                tracing::warn!(
                    "Topic '{}' creation timed out after {}ms",
                    name,
                    timeout_duration.as_millis()
                );

                let state = self.state();
                state.topics.get(name).cloned()
                    .ok_or_else(|| MetadataError::NotFound(format!(
                        "Topic {} not found after creation (timeout)",
                        name
                    )))
            }
        }
    }

    async fn get_topic(&self, name: &str) -> Result<Option<TopicMetadata>> {
        // Phase 3: Check if we can read from local state using lease
        if !self.raft.am_i_leader() {
            // We're a follower - check if we have a valid lease
            if let Some(lease_manager) = &self.lease_manager {
                if lease_manager.has_valid_lease().await {
                    // Fast path: Read from local replicated state (1-2ms)
                    tracing::trace!("Phase 3: Fast follower read for get_topic('{}')", name);
                    let state = self.state();
                    return Ok(state.topics.get(name).cloned());
                } else {
                    tracing::trace!("Phase 3: Lease expired, forwarding get_topic('{}') to leader", name);
                }
            }

            // Phase 1.2: No valid lease - forward to leader (safe fallback)
            tracing::debug!("Forwarding get_topic('{}') to leader", name);
            let query = crate::metadata_rpc::MetadataQuery::GetTopic {
                name: name.to_string(),
            };
            let response = self.raft.query_leader(query).await
                .map_err(|e| MetadataError::StorageError(e.to_string()))?;

            match response {
                crate::metadata_rpc::MetadataQueryResponse::Topic(topic) => Ok(topic),
                _ => Err(MetadataError::StorageError("Unexpected response type".to_string())),
            }
        } else {
            // We're the leader, read from local state
            let state = self.state();
            Ok(state.topics.get(name).cloned())
        }
    }

    async fn list_topics(&self) -> Result<Vec<TopicMetadata>> {
        // Phase 3: Check if we can read from local state using lease
        if !self.raft.am_i_leader() {
            // We're a follower - check if we have a valid lease
            if let Some(lease_manager) = &self.lease_manager {
                if lease_manager.has_valid_lease().await {
                    // Fast path: Read from local replicated state (1-2ms)
                    tracing::trace!("Phase 3: Fast follower read for list_topics()");
                    let state = self.state();
                    return Ok(state.topics.values().cloned().collect());
                } else {
                    tracing::trace!("Phase 3: Lease expired, forwarding list_topics() to leader");
                }
            }

            // Phase 1.2: No valid lease - forward to leader (safe fallback)
            tracing::debug!("Forwarding list_topics() to leader");
            let query = crate::metadata_rpc::MetadataQuery::ListTopics;
            let response = self.raft.query_leader(query).await
                .map_err(|e| MetadataError::StorageError(e.to_string()))?;

            match response {
                crate::metadata_rpc::MetadataQueryResponse::TopicList(topics) => Ok(topics),
                _ => Err(MetadataError::StorageError("Unexpected response type".to_string())),
            }
        } else {
            // We're the leader, read from local state
            let state = self.state();
            Ok(state.topics.values().cloned().collect())
        }
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
        // Phase 2: Check if we have metadata WAL enabled (leader fast path)
        if let (Some(metadata_wal), Some(replicator)) = (&self.metadata_wal, &self.metadata_wal_replicator) {
            // PHASE 2 FAST PATH: WAL-based metadata write (1-2ms vs 10-50ms Raft)

            if self.raft.am_i_leader() {
                tracing::info!("Phase 2: Leader registering broker {} via metadata WAL (fast path)", metadata.broker_id);

                // Create metadata command
                let cmd = MetadataCommand::RegisterBroker {
                    broker_id: metadata.broker_id,
                    host: metadata.host.clone(),
                    port: metadata.port,
                    rack: metadata.rack.clone(),
                };

                // 1. Write to metadata WAL (durable, 1-2ms)
                let offset = metadata_wal.append(&cmd).await
                    .map_err(|e| MetadataError::StorageError(format!("Metadata WAL write failed: {}", e)))?;

                tracing::info!(
                    "Wrote RegisterBroker({}) to metadata WAL at offset {} (fast!)",
                    metadata.broker_id,
                    offset
                );

                // 2. Apply to local state machine immediately
                self.raft.apply_metadata_command_direct(cmd.clone())
                    .map_err(|e| MetadataError::StorageError(format!("Failed to apply command: {}", e)))?;

                tracing::info!("Applied RegisterBroker({}) to state machine", metadata.broker_id);

                // 3. Fire notification for any waiting threads
                if let Some((_, notify)) = self.pending_brokers.remove(&metadata.broker_id) {
                    notify.notify_waiters();
                    tracing::debug!("Notified waiting threads for broker {}", metadata.broker_id);
                }

                // 4. Async replicate to followers (fire-and-forget)
                let replicator_clone = Arc::clone(replicator);
                let cmd_clone = cmd.clone();
                tokio::spawn(async move {
                    if let Err(e) = replicator_clone.replicate(&cmd_clone, offset).await {
                        tracing::warn!("Metadata replication failed for RegisterBroker: {}", e);
                    }
                });

                // 5. Return immediately
                let state = self.state();
                if state.brokers.get(&metadata.broker_id).is_some() {
                    return Ok(());
                } else {
                    return Err(MetadataError::NotFound(format!(
                        "Broker {} not found after registration",
                        metadata.broker_id
                    )));
                }
            }
        }

        // FALLBACK PATH: Use Raft consensus (Phase 1 behavior)
        tracing::debug!("Phase 1 fallback: Registering broker {} via Raft consensus", metadata.broker_id);

        let notify = Arc::new(Notify::new());
        self.pending_brokers.insert(metadata.broker_id, Arc::clone(&notify));

        self.raft.propose(MetadataCommand::RegisterBroker {
            broker_id: metadata.broker_id,
            host: metadata.host.clone(),
            port: metadata.port,
            rack: metadata.rack.clone(),
        }).await.map_err(|e| MetadataError::StorageError(e.to_string()))?;

        let timeout_duration = tokio::time::Duration::from_millis(2000);

        match tokio::time::timeout(timeout_duration, notify.notified()).await {
            Ok(_) => {
                let state = self.state();
                if state.brokers.get(&metadata.broker_id).is_some() {
                    Ok(())
                } else {
                    Err(MetadataError::NotFound(format!(
                        "Broker {} notified but not found in state machine",
                        metadata.broker_id
                    )))
                }
            }
            Err(_) => {
                tracing::warn!(
                    "Broker {} registration timed out after {}ms",
                    metadata.broker_id,
                    timeout_duration.as_millis()
                );

                let state = self.state();
                if state.brokers.get(&metadata.broker_id).is_some() {
                    Ok(())
                } else {
                    Err(MetadataError::NotFound(format!(
                        "Broker {} not found after registration (timeout)",
                        metadata.broker_id
                    )))
                }
            }
        }
    }

    async fn get_broker(&self, broker_id: i32) -> Result<Option<BrokerMetadata>> {
        // Phase 1.2: Forward to leader if we're a follower
        if !self.raft.am_i_leader() {
            tracing::debug!("Forwarding get_broker({}) to leader", broker_id);
            let query = crate::metadata_rpc::MetadataQuery::GetBroker { broker_id };
            let response = self.raft.query_leader(query).await
                .map_err(|e| MetadataError::StorageError(e.to_string()))?;

            match response {
                crate::metadata_rpc::MetadataQueryResponse::Broker(broker) => Ok(broker),
                _ => Err(MetadataError::StorageError("Unexpected response type".to_string())),
            }
        } else {
            // We're the leader, read from local state
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
    }

    async fn list_brokers(&self) -> Result<Vec<BrokerMetadata>> {
        // Phase 1.2: Forward to leader if we're a follower
        if !self.raft.am_i_leader() {
            tracing::debug!("Forwarding list_brokers() to leader");
            let query = crate::metadata_rpc::MetadataQuery::ListBrokers;
            let response = self.raft.query_leader(query).await
                .map_err(|e| MetadataError::StorageError(e.to_string()))?;

            match response {
                crate::metadata_rpc::MetadataQueryResponse::BrokerList(brokers) => Ok(brokers),
                _ => Err(MetadataError::StorageError("Unexpected response type".to_string())),
            }
        } else {
            // We're the leader, read from local state
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
