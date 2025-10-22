//! Raft Group Manager - Manages multiple partition replicas
//!
//! This module provides a centralized manager for all Raft partition replicas
//! in a Chronik node. It handles replica lifecycle, routing, and provides
//! a unified interface for produce/fetch operations.

use crate::{MemoryStateMachine, PartitionReplica, RaftClient, RaftConfig, RaftError, RaftLogStorage, Result, StateMachine, Transport, GrpcTransport};
use chronik_monitoring::RaftMetrics;
use parking_lot::RwLock;
use raft::{prelude::Message, StateRole};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock as TokioRwLock;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, trace, warn};

/// Key for identifying a partition replica (topic, partition_id)
type PartitionKey = (String, i32);

/// Health status for a partition replica
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HealthStatus {
    /// Node is Raft leader
    Leader,
    /// Node is Raft follower
    Follower,
    /// Node is Raft candidate
    Candidate,
    /// Node is down or unreachable
    Down,
}

/// Health information for a Raft group
#[derive(Debug, Clone)]
pub struct GroupHealth {
    /// Topic name
    pub topic: String,
    /// Partition ID
    pub partition: i32,
    /// Health status
    pub status: HealthStatus,
    /// Current term
    pub term: u64,
    /// Current leader ID (0 if unknown)
    pub leader_id: u64,
    /// Commit index
    pub commit_index: u64,
    /// Applied index
    pub applied_index: u64,
}

/// Manages all Raft partition replicas for this node
pub struct RaftGroupManager {
    /// Node ID for this Chronik instance
    node_id: u64,

    /// Raft configuration template
    config: RaftConfig,

    /// Map of (topic, partition) -> PartitionReplica
    replicas: Arc<RwLock<HashMap<PartitionKey, Arc<PartitionReplica>>>>,

    /// Log storage factory for creating new replicas
    log_storage_factory: Arc<dyn Fn() -> Arc<dyn RaftLogStorage> + Send + Sync>,

    /// State machine factory for creating new replicas
    state_machine_factory: Arc<dyn Fn() -> Arc<TokioRwLock<dyn StateMachine>> + Send + Sync>,

    /// Raft transport layer for sending messages to peers (gRPC or in-memory)
    raft_transport: Arc<dyn Transport>,

    /// Background tick task handle
    tick_task: RwLock<Option<JoinHandle<()>>>,

    /// Shutdown signal
    shutdown: Arc<AtomicBool>,

    /// Tick interval (how often to call tick() on all groups)
    tick_interval: Duration,

    /// Raft metrics collector
    metrics: RaftMetrics,
}

impl RaftGroupManager {
    /// Create a new RaftGroupManager
    ///
    /// # Arguments
    /// * `node_id` - This node's ID in the cluster
    /// * `config` - Raft configuration template
    /// * `log_storage_factory` - Factory function to create log storage for new replicas
    ///
    /// Note: Uses MemoryStateMachine by default. For custom state machines, use with_state_machine_factory
    pub fn new<F>(
        node_id: u64,
        config: RaftConfig,
        log_storage_factory: F,
    ) -> Self
    where
        F: Fn() -> Arc<dyn RaftLogStorage> + Send + Sync + 'static,
    {
        // Default state machine factory creates MemoryStateMachine instances
        let state_machine_factory = || -> Arc<TokioRwLock<dyn StateMachine>> {
            Arc::new(TokioRwLock::new(MemoryStateMachine::new()))
        };

        Self::with_factories(
            node_id,
            config,
            log_storage_factory,
            state_machine_factory,
            Duration::from_millis(100)
        )
    }

    /// Create a new RaftGroupManager with custom tick interval
    ///
    /// # Arguments
    /// * `node_id` - This node's ID in the cluster
    /// * `config` - Raft configuration template
    /// * `log_storage_factory` - Factory function to create log storage for new replicas
    /// * `tick_interval` - How often to tick all Raft groups
    pub fn with_tick_interval<F>(
        node_id: u64,
        config: RaftConfig,
        log_storage_factory: F,
        tick_interval: Duration,
    ) -> Self
    where
        F: Fn() -> Arc<dyn RaftLogStorage> + Send + Sync + 'static,
    {
        // Default state machine factory creates MemoryStateMachine instances
        let state_machine_factory = || -> Arc<TokioRwLock<dyn StateMachine>> {
            Arc::new(TokioRwLock::new(MemoryStateMachine::new()))
        };

        Self::with_factories(
            node_id,
            config,
            log_storage_factory,
            state_machine_factory,
            tick_interval
        )
    }

    /// Create a new RaftGroupManager with custom factories and tick interval
    ///
    /// # Arguments
    /// * `node_id` - This node's ID in the cluster
    /// * `config` - Raft configuration template
    /// * `log_storage_factory` - Factory function to create log storage for new replicas
    /// * `state_machine_factory` - Factory function to create state machines for new replicas
    /// * `tick_interval` - How often to tick all Raft groups
    pub fn with_factories<F, S>(
        node_id: u64,
        config: RaftConfig,
        log_storage_factory: F,
        state_machine_factory: S,
        tick_interval: Duration,
    ) -> Self
    where
        F: Fn() -> Arc<dyn RaftLogStorage> + Send + Sync + 'static,
        S: Fn() -> Arc<TokioRwLock<dyn StateMachine>> + Send + Sync + 'static,
    {
        info!(
            "Creating RaftGroupManager for node {} with config: {:?}, tick_interval: {:?}",
            node_id, config, tick_interval
        );

        Self {
            node_id,
            config,
            replicas: Arc::new(RwLock::new(HashMap::new())),
            log_storage_factory: Arc::new(log_storage_factory),
            state_machine_factory: Arc::new(state_machine_factory),
            raft_transport: Arc::new(GrpcTransport::new()),  // Default to gRPC for production
            tick_task: RwLock::new(None),
            shutdown: Arc::new(AtomicBool::new(false)),
            tick_interval,
            metrics: RaftMetrics::new(),
        }
    }

    /// Create a new RaftGroupManager with custom transport (for testing).
    ///
    /// This allows injecting an InMemoryTransport for tests or other transport implementations.
    pub fn with_transport<F, S, T>(
        node_id: u64,
        config: RaftConfig,
        log_storage_factory: F,
        state_machine_factory: S,
        tick_interval: Duration,
        transport: T,
    ) -> Self
    where
        F: Fn() -> Arc<dyn RaftLogStorage> + Send + Sync + 'static,
        S: Fn() -> Arc<TokioRwLock<dyn StateMachine>> + Send + Sync + 'static,
        T: Transport + 'static,
    {
        info!(
            "Creating RaftGroupManager for node {} with custom transport, tick_interval: {:?}",
            node_id, tick_interval
        );

        Self {
            node_id,
            config,
            replicas: Arc::new(RwLock::new(HashMap::new())),
            log_storage_factory: Arc::new(log_storage_factory),
            state_machine_factory: Arc::new(state_machine_factory),
            raft_transport: Arc::new(transport),
            tick_task: RwLock::new(None),
            shutdown: Arc::new(AtomicBool::new(false)),
            tick_interval,
            metrics: RaftMetrics::new(),
        }
    }

    /// Add a peer node for Raft communication
    ///
    /// # Arguments
    /// * `node_id` - Node ID of the peer
    /// * `addr` - Address of the peer (gRPC URL or identifier for in-memory transport)
    pub async fn add_peer(&self, node_id: u64, addr: String) -> Result<()> {
        info!("Adding peer {} at {}", node_id, addr);
        self.raft_transport.add_peer(node_id, addr).await
    }

    /// Get reference to the Raft transport layer
    pub fn raft_transport(&self) -> &Arc<dyn Transport> {
        &self.raft_transport
    }

    /// Get or create a partition replica
    ///
    /// # Arguments
    /// * `topic` - Topic name
    /// * `partition` - Partition ID
    /// * `peers` - List of peer node IDs (empty for single-node)
    ///
    /// # Returns
    /// Arc to the PartitionReplica
    pub fn get_or_create_replica(
        &self,
        topic: &str,
        partition: i32,
        peers: Vec<u64>,
    ) -> Result<Arc<PartitionReplica>> {
        let key = (topic.to_string(), partition);

        // Fast path: replica already exists
        {
            let replicas = self.replicas.read();
            if let Some(replica) = replicas.get(&key) {
                debug!(
                    "Returning existing replica for {}-{}",
                    topic, partition
                );
                return Ok(Arc::clone(replica));
            }
        }

        // Slow path: create new replica
        info!(
            node_id = self.node_id,
            topic = %topic,
            partition = partition,
            peer_count = peers.len(),
            peers = ?peers,
            "Creating new PartitionReplica"
        );

        let log_storage = (self.log_storage_factory)();
        let state_machine = (self.state_machine_factory)();

        let replica = PartitionReplica::new(
            topic.to_string(),
            partition,
            self.config.clone(),
            log_storage,
            state_machine,
            peers,
        )?;

        let replica_arc = Arc::new(replica);

        // Insert into map
        {
            let mut replicas = self.replicas.write();
            replicas.insert(key, Arc::clone(&replica_arc));
        }

        info!(
            node_id = self.node_id,
            topic = %topic,
            partition = partition,
            total_replicas = self.replicas.read().len(),
            "Created PartitionReplica successfully"
        );

        Ok(replica_arc)
    }

    /// Get an existing replica (if it exists)
    ///
    /// # Arguments
    /// * `topic` - Topic name
    /// * `partition` - Partition ID
    ///
    /// # Returns
    /// Some(Arc<PartitionReplica>) if replica exists, None otherwise
    pub fn get_replica(&self, topic: &str, partition: i32) -> Option<Arc<PartitionReplica>> {
        let key = (topic.to_string(), partition);
        let replicas = self.replicas.read();
        replicas.get(&key).map(Arc::clone)
    }

    /// Check if a partition has Raft enabled
    ///
    /// # Arguments
    /// * `topic` - Topic name
    /// * `partition` - Partition ID
    ///
    /// # Returns
    /// true if Raft replica exists for this partition
    pub fn has_replica(&self, topic: &str, partition: i32) -> bool {
        let key = (topic.to_string(), partition);
        let replicas = self.replicas.read();
        replicas.contains_key(&key)
    }

    /// Remove a partition replica
    ///
    /// # Arguments
    /// * `topic` - Topic name
    /// * `partition` - Partition ID
    pub fn remove_replica(&self, topic: &str, partition: i32) -> Option<Arc<PartitionReplica>> {
        let key = (topic.to_string(), partition);
        let mut replicas = self.replicas.write();

        if let Some(replica) = replicas.remove(&key) {
            info!("Removed PartitionReplica for {}-{}", topic, partition);
            Some(replica)
        } else {
            warn!("Attempted to remove non-existent replica for {}-{}", topic, partition);
            None
        }
    }

    /// List all active replicas
    ///
    /// # Returns
    /// Vector of (topic, partition) tuples
    pub fn list_replicas(&self) -> Vec<(String, i32)> {
        let replicas = self.replicas.read();
        replicas.keys().cloned().collect()
    }

    /// Get count of active replicas
    pub fn replica_count(&self) -> usize {
        let replicas = self.replicas.read();
        replicas.len()
    }

    /// Update leader/follower counts metrics
    fn update_role_counts(&self) {
        let replicas = self.replicas.read();

        let mut leader_count = 0;
        let mut follower_count = 0;

        for replica in replicas.values() {
            match replica.role() {
                StateRole::Leader => leader_count += 1,
                StateRole::Follower => follower_count += 1,
                _ => {} // Don't count candidates
            }
        }

        let node_id_str = self.node_id.to_string();
        self.metrics.set_leader_count(&node_id_str, leader_count);
        self.metrics.set_follower_count(&node_id_str, follower_count);
    }

    /// Tick all replicas (should be called periodically)
    ///
    /// This drives Raft state machines forward for election timeouts
    /// and heartbeats.
    pub fn tick_all(&self) -> Result<()> {
        let replicas = self.replicas.read();

        if !replicas.is_empty() {
            debug!(
                node_id = self.node_id,
                active_groups = replicas.len(),
                "Ticking all Raft groups"
            );
        }

        for ((topic, partition), replica) in replicas.iter() {
            if let Err(e) = replica.tick() {
                warn!(
                    node_id = self.node_id,
                    topic = %topic,
                    partition = partition,
                    error = %e,
                    "Failed to tick replica"
                );
            }
        }

        Ok(())
    }

    /// Process ready states for all replicas
    ///
    /// This should be called after tick_all() to handle committed entries
    /// and send messages to peers.
    ///
    /// # Returns
    /// Map of (topic, partition) -> (messages, committed_entries)
    pub async fn ready_all(&self) -> Result<HashMap<PartitionKey, (Vec<raft::prelude::Message>, Vec<raft::prelude::Entry>)>> {
        let replicas = self.replicas.read();
        let mut results = HashMap::new();

        for ((topic, partition), replica) in replicas.iter() {
            match replica.ready().await {
                Ok((messages, committed)) => {
                    if !messages.is_empty() || !committed.is_empty() {
                        debug!(
                            "Replica {}-{} ready: {} messages, {} committed entries",
                            topic,
                            partition,
                            messages.len(),
                            committed.len()
                        );
                        results.insert((topic.clone(), *partition), (messages, committed));
                    }
                }
                Err(e) => {
                    warn!("Failed to process ready for {}-{}: {}", topic, partition, e);
                }
            }
        }

        Ok(results)
    }

    /// Get this node's ID
    pub fn node_id(&self) -> u64 {
        self.node_id
    }

    /// Get the Raft configuration
    pub fn config(&self) -> &RaftConfig {
        &self.config
    }

    /// Check if this node is the leader for a partition
    ///
    /// # Arguments
    /// * `topic` - Topic name
    /// * `partition` - Partition ID
    ///
    /// # Returns
    /// true if this node is the Raft leader, false otherwise (including if no replica exists)
    pub fn is_leader_for_partition(&self, topic: &str, partition: i32) -> bool {
        if let Some(replica) = self.get_replica(topic, partition) {
            replica.is_leader()
        } else {
            false
        }
    }

    /// Get the current leader node ID for a partition
    ///
    /// # Arguments
    /// * `topic` - Topic name
    /// * `partition` - Partition ID
    ///
    /// # Returns
    /// Some(leader_id) if known, None if unknown or no replica exists
    pub fn get_leader_for_partition(&self, topic: &str, partition: i32) -> Option<u64> {
        if let Some(replica) = self.get_replica(topic, partition) {
            let leader_id = replica.leader_id();
            if leader_id == 0 {
                None // Unknown leader
            } else {
                Some(leader_id)
            }
        } else {
            None // No replica = not managed by Raft
        }
    }

    /// Propose an entry to a partition's Raft log
    ///
    /// # Arguments
    /// * `topic` - Topic name
    /// * `partition` - Partition ID
    /// * `data` - Data to propose
    ///
    /// # Returns
    /// Ok(index) if successful, Err if not leader or replica doesn't exist
    pub async fn propose(&self, topic: &str, partition: i32, data: Vec<u8>) -> Result<u64> {
        let replica = self.get_replica(topic, partition)
            .ok_or_else(|| RaftError::Config(format!(
                "Partition {}-{} is not managed by Raft",
                topic, partition
            )))?;

        replica.propose(data).await
    }

    /// Receive and process an incoming Raft message
    ///
    /// This method is used to deliver messages from the transport layer to the appropriate
    /// partition replica. In production, this is called by the gRPC server. In tests with
    /// InMemoryTransport, this is called by a message routing loop.
    ///
    /// # Arguments
    /// * `topic` - Topic name
    /// * `partition` - Partition ID
    /// * `msg` - Raft message to process
    ///
    /// # Returns
    /// Ok(()) if message was processed, Err if replica doesn't exist
    pub async fn receive_message(&self, topic: &str, partition: i32, msg: raft::prelude::Message) -> Result<()> {
        let replica = self.get_replica(topic, partition)
            .ok_or_else(|| RaftError::Config(format!(
                "Partition {}-{} is not managed by Raft",
                topic, partition
            )))?;

        replica.step(msg).await
    }

    /// Spawn background tick loop
    ///
    /// This creates a tokio task that continuously ticks all Raft groups
    /// at the configured tick_interval. The handle is stored internally
    /// and will be awaited on shutdown().
    pub fn spawn_tick_loop(&self) {
        let replicas = self.replicas.clone();
        let shutdown = self.shutdown.clone();
        let tick_interval = self.tick_interval;
        let raft_transport = Arc::clone(&self.raft_transport);
        let node_id = self.node_id;
        let metrics = RaftMetrics::new(); // Create metrics for tick loop

        let handle = tokio::spawn(async move {
            info!("RaftGroupManager tick loop started");

            while !shutdown.load(Ordering::Relaxed) {
                // Collect all replicas (clone Arc to avoid holding lock across await)
                let replicas_snapshot: Vec<((String, i32), Arc<PartitionReplica>)> = {
                    let replicas_guard = replicas.read();
                    replicas_guard
                        .iter()
                        .map(|((topic, partition), replica)| {
                            ((topic.clone(), *partition), Arc::clone(replica))
                        })
                        .collect()
                };

                let count = replicas_snapshot.len();
                if count > 0 {
                    trace!("Tick loop processing {} replicas", count);

                    for ((topic, partition), replica) in &replicas_snapshot {
                        // Tick the replica
                        if let Err(e) = replica.tick() {
                            warn!(
                                "Tick loop: Failed to tick {}-{}: {}",
                                topic, partition, e
                            );
                        }

                        // Process ready states
                        match replica.ready().await {
                            Ok((messages, committed)) => {
                                // Send messages to peers via gRPC
                                if !messages.is_empty() {
                                    debug!(
                                        "Tick loop: {}-{} has {} messages to send",
                                        topic, partition, messages.len()
                                    );

                                    for msg in messages {
                                        let to = msg.to;
                                        if to == 0 || to == node_id {
                                            // Skip sending to ourselves or invalid destination
                                            continue;
                                        }

                                        let raft_transport_clone = Arc::clone(&raft_transport);
                                        let topic_clone = topic.clone();
                                        let partition_clone = *partition;

                                        // Send message asynchronously (don't block tick loop)
                                        tokio::spawn(async move {
                                            if let Err(e) = raft_transport_clone.send_message(
                                                &topic_clone,
                                                partition_clone,
                                                to,
                                                msg,
                                            ).await {
                                                warn!(
                                                    "Failed to send Raft message to node {}: {}",
                                                    to, e
                                                );
                                            }
                                        });
                                    }
                                }

                                if !committed.is_empty() {
                                    debug!(
                                        "Tick loop: {}-{} applied {} committed entries to state machine",
                                        topic, partition, committed.len()
                                    );
                                    // Note: Entries are already applied to state machine by PartitionReplica::ready()
                                }
                            }
                            Err(e) => {
                                warn!(
                                    "Tick loop: Failed to process ready for {}-{}: {}",
                                    topic, partition, e
                                );
                            }
                        }
                    }

                    // Update leader/follower counts after processing all replicas
                    let leader_count = replicas_snapshot.iter()
                        .filter(|(_, r)| r.role() == StateRole::Leader)
                        .count();
                    let follower_count = replicas_snapshot.iter()
                        .filter(|(_, r)| r.role() == StateRole::Follower)
                        .count();

                    // Log leader partitions for debugging
                    let leader_partitions: Vec<String> = replicas_snapshot.iter()
                        .filter(|(_, r)| r.role() == StateRole::Leader)
                        .map(|((t, p), _)| format!("{}-{}", t, p))
                        .collect();

                    if !leader_partitions.is_empty() {
                        debug!(
                            node_id = node_id,
                            leader_count = leader_count,
                            follower_count = follower_count,
                            leader_partitions = ?leader_partitions,
                            "Tick loop: Node role summary"
                        );
                    }

                    let node_id_str = node_id.to_string();
                    metrics.set_leader_count(&node_id_str, leader_count);
                    metrics.set_follower_count(&node_id_str, follower_count);
                }

                // Sleep until next tick
                tokio::time::sleep(tick_interval).await;
            }

            info!("RaftGroupManager tick loop stopped");
        });

        // Store the handle so we can await it on shutdown
        *self.tick_task.write() = Some(handle);

        // Return unit since we stored the handle internally
        ()
    }

    /// Stop the background tick loop
    pub async fn shutdown(&self) {
        info!("Shutting down RaftGroupManager");

        // Signal shutdown
        self.shutdown.store(true, Ordering::Relaxed);

        // Wait for tick task to complete
        if let Some(handle) = self.tick_task.write().take() {
            if let Err(e) = handle.await {
                error!("Error waiting for tick task to complete: {}", e);
            }
        }

        debug!("RaftGroupManager shutdown complete");
    }

    /// Route a Raft message to the appropriate partition replica
    ///
    /// # Arguments
    /// * `topic` - Topic name
    /// * `partition` - Partition ID
    /// * `message` - Raft message to route
    ///
    /// # Errors
    /// Returns error if group does not exist
    pub async fn route_message(
        &self,
        topic: &str,
        partition: i32,
        message: Message,
    ) -> Result<()> {
        let replica = self.get_replica(topic, partition).ok_or_else(|| {
            RaftError::Config(format!(
                "No Raft group found for {}-{}",
                topic, partition
            ))
        })?;

        debug!(
            "Routing message type {:?} to {}-{}",
            message.get_msg_type(),
            topic,
            partition
        );

        replica.step(message).await?;

        // Process ready after stepping
        let (messages, committed) = replica.ready().await?;

        // Send messages to peers via gRPC
        if !messages.is_empty() {
            trace!("Route message: {} messages to send", messages.len());

            for msg in messages {
                let to = msg.to;
                if to == 0 || to == self.node_id {
                    // Skip sending to ourselves or invalid destination
                    continue;
                }

                let raft_transport = Arc::clone(&self.raft_transport);
                let topic_clone = topic.to_string();
                let partition_clone = partition;

                // Send message asynchronously
                tokio::spawn(async move {
                    if let Err(e) = raft_transport.send_message(
                        &topic_clone,
                        partition_clone,
                        to,
                        msg,
                    ).await {
                        warn!(
                            "Failed to send Raft message to node {}: {}",
                            to, e
                        );
                    }
                });
            }
        }

        if !committed.is_empty() {
            debug!(
                "Route message: Applied {} committed entries to state machine for {}-{}",
                committed.len(),
                topic,
                partition
            );
            // Note: Entries are already applied to state machine by PartitionReplica::ready()
        }

        Ok(())
    }

    /// Check health of all Raft groups
    ///
    /// # Returns
    /// Vector of GroupHealth for each active group
    pub fn health_check(&self) -> Vec<GroupHealth> {
        let replicas = self.replicas.read();

        replicas
            .iter()
            .map(|((topic, partition), replica)| {
                let status = match replica.role() {
                    StateRole::Leader => HealthStatus::Leader,
                    StateRole::Follower => HealthStatus::Follower,
                    StateRole::Candidate => HealthStatus::Candidate,
                    StateRole::PreCandidate => HealthStatus::Candidate,
                };

                GroupHealth {
                    topic: topic.clone(),
                    partition: *partition,
                    status,
                    term: replica.term(),
                    leader_id: replica.leader_id(),
                    commit_index: replica.commit_index(),
                    applied_index: replica.applied_index(),
                }
            })
            .collect()
    }
}

impl Drop for RaftGroupManager {
    fn drop(&mut self) {
        // Signal shutdown
        self.shutdown.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemoryLogStorage;
    use raft::StateRole;
    use std::time::Duration;

    fn create_test_manager() -> RaftGroupManager {
        let config = RaftConfig {
            node_id: 1,
            listen_addr: "127.0.0.1:5001".to_string(),
            election_timeout_ms: 1000,
            heartbeat_interval_ms: 100,
            max_entries_per_batch: 100,
            snapshot_threshold: 10_000,
        };

        RaftGroupManager::new(
            1,
            config,
            || Arc::new(MemoryLogStorage::new()),
        )
    }

    #[test]
    fn test_create_manager() {
        let manager = create_test_manager();
        assert_eq!(manager.node_id(), 1);
        assert_eq!(manager.replica_count(), 0);
    }

    #[test]
    fn test_get_or_create_replica() {
        let manager = create_test_manager();

        // Create first replica
        let replica1 = manager.get_or_create_replica("test-topic", 0, vec![]).unwrap();
        assert_eq!(manager.replica_count(), 1);

        // Get same replica again
        let replica2 = manager.get_or_create_replica("test-topic", 0, vec![]).unwrap();
        assert_eq!(manager.replica_count(), 1);

        // Should be same Arc
        assert!(Arc::ptr_eq(&replica1, &replica2));
    }

    #[test]
    fn test_has_replica() {
        let manager = create_test_manager();

        assert!(!manager.has_replica("test-topic", 0));

        manager.get_or_create_replica("test-topic", 0, vec![]).unwrap();

        assert!(manager.has_replica("test-topic", 0));
        assert!(!manager.has_replica("test-topic", 1));
    }

    #[test]
    fn test_remove_replica() {
        let manager = create_test_manager();

        manager.get_or_create_replica("test-topic", 0, vec![]).unwrap();
        assert_eq!(manager.replica_count(), 1);

        let removed = manager.remove_replica("test-topic", 0);
        assert!(removed.is_some());
        assert_eq!(manager.replica_count(), 0);

        let removed_again = manager.remove_replica("test-topic", 0);
        assert!(removed_again.is_none());
    }

    #[test]
    fn test_list_replicas() {
        let manager = create_test_manager();

        manager.get_or_create_replica("topic-a", 0, vec![]).unwrap();
        manager.get_or_create_replica("topic-a", 1, vec![]).unwrap();
        manager.get_or_create_replica("topic-b", 0, vec![]).unwrap();

        let replicas = manager.list_replicas();
        assert_eq!(replicas.len(), 3);
        assert!(replicas.contains(&("topic-a".to_string(), 0)));
        assert!(replicas.contains(&("topic-a".to_string(), 1)));
        assert!(replicas.contains(&("topic-b".to_string(), 0)));
    }

    #[tokio::test]
    async fn test_tick_all() {
        let manager = create_test_manager();

        manager.get_or_create_replica("test-topic", 0, vec![]).unwrap();
        manager.get_or_create_replica("test-topic", 1, vec![]).unwrap();

        // Should not error
        assert!(manager.tick_all().is_ok());
    }

    #[tokio::test]
    async fn test_ready_all() {
        let manager = create_test_manager();

        manager.get_or_create_replica("test-topic", 0, vec![]).unwrap();

        let results = manager.ready_all().await.unwrap();
        // No messages or commits without any activity
        assert!(results.is_empty() || results.values().all(|(m, c)| m.is_empty() && c.is_empty()));
    }

    #[tokio::test]
    async fn test_10_independent_groups() {
        let manager = create_test_manager();

        // Create 10 Raft groups
        for i in 0..10 {
            manager.get_or_create_replica("test-topic", i, vec![]).unwrap();
        }

        assert_eq!(manager.replica_count(), 10);

        // Tick all groups
        manager.tick_all().unwrap();

        // Campaign group 0 to leader
        let group0 = manager.get_replica("test-topic", 0).unwrap();
        group0.campaign().unwrap();
        group0.ready().await.unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;
        group0.tick().unwrap();
        group0.ready().await.unwrap();

        // Verify group 0 became leader (or candidate)
        assert!(group0.is_leader() || group0.role() == StateRole::Candidate);

        // Verify other groups are NOT affected
        for i in 1..10 {
            let group = manager.get_replica("test-topic", i).unwrap();
            assert!(!group.is_leader(), "Group {} should not be leader", i);
        }
    }

    #[tokio::test]
    async fn test_tick_loop() {
        let config = RaftConfig {
            node_id: 1,
            heartbeat_interval_ms: 10,
            ..Default::default()
        };

        let manager = Arc::new(RaftGroupManager::with_tick_interval(
            1,
            config.clone(),
            || Arc::new(MemoryLogStorage::new()),
            Duration::from_millis(50),
        ));

        // Create a few groups
        for i in 0..3 {
            manager.get_or_create_replica("test-topic", i, vec![]).unwrap();
        }

        // Spawn tick loop
        let _handle = manager.spawn_tick_loop();

        // Let it run for a bit
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Shutdown
        manager.shutdown().await;

        // Verify all groups still exist
        assert_eq!(manager.replica_count(), 3);
    }

    #[tokio::test]
    async fn test_health_check() {
        let manager = create_test_manager();

        // Create groups
        for i in 0..3 {
            manager.get_or_create_replica("test-topic", i, vec![]).unwrap();
        }

        // Check health
        let health = manager.health_check();
        assert_eq!(health.len(), 3);

        for h in health {
            assert_eq!(h.topic, "test-topic");
            assert!(h.partition < 3);
            // Single-node clusters start as followers until campaigned
            assert!(
                h.status == HealthStatus::Follower ||
                h.status == HealthStatus::Leader ||
                h.status == HealthStatus::Candidate
            );
        }
    }

    #[tokio::test]
    async fn test_route_message_nonexistent_group() {
        let manager = create_test_manager();

        let msg = Message::default();

        // Should fail - group doesn't exist
        let result = manager.route_message("nonexistent", 0, msg).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_shutdown_cleanup() {
        let config = RaftConfig {
            node_id: 1,
            ..Default::default()
        };

        let manager = Arc::new(RaftGroupManager::with_tick_interval(
            1,
            config,
            || Arc::new(MemoryLogStorage::new()),
            Duration::from_millis(50),
        ));

        // Create groups
        for i in 0..5 {
            manager.get_or_create_replica("test-topic", i, vec![]).unwrap();
        }

        // Spawn tick loop
        let _handle = manager.spawn_tick_loop();

        // Let it run
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Shutdown
        manager.shutdown().await;

        // Verify groups are still there (shutdown doesn't remove them)
        assert_eq!(manager.replica_count(), 5);
    }

    #[tokio::test]
    async fn test_multiple_groups_independence() {
        let config1 = RaftConfig {
            node_id: 1,
            ..Default::default()
        };

        let manager = RaftGroupManager::new(
            1,
            config1,
            || Arc::new(MemoryLogStorage::new()),
        );

        // Create two groups
        manager.get_or_create_replica("topic-a", 0, vec![]).unwrap();
        manager.get_or_create_replica("topic-b", 0, vec![]).unwrap();

        // Verify they are independent
        let replica1 = manager.get_replica("topic-a", 0).unwrap();
        let replica2 = manager.get_replica("topic-b", 0).unwrap();

        // Campaign replica1 to leader
        replica1.campaign().unwrap();
        replica1.ready().await.unwrap();

        // Verify replica1 is leader but replica2 is not
        tokio::time::sleep(Duration::from_millis(50)).await;
        replica1.tick().unwrap();
        replica1.ready().await.unwrap();

        assert!(replica1.is_leader() || replica1.role() == StateRole::Candidate);
        assert!(!replica2.is_leader());
    }
}
