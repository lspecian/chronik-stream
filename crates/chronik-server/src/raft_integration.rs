//! Raft consensus integration for Chronik Server
//!
//! This module provides production Raft integration, including:
//! - ChronikStateMachine for applying committed entries to storage
//! - RaftReplicaManager for managing per-partition Raft groups
//! - Integration with ProduceHandler for replicated writes

use async_trait::async_trait;
use bytes::Bytes;
use chronik_common::metadata::MetadataStore;
use chronik_raft::{
    MemoryStateMachine, PartitionReplica, RaftClient, RaftConfig, RaftEntry, RaftEvent, RaftLogStorage,
    Result as RaftResult, SnapshotData, StateMachine,
};
use chronik_wal::{GroupCommitWal, GroupCommitConfig, RaftWalStorage};
use raft::prelude::{ConfChange, ConfChangeType};
use chronik_storage::{CanonicalRecord, SegmentWriter};
use dashmap::DashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use tracing::{debug, error, info, warn};

/// Partition identifier (topic, partition)
pub type PartitionKey = (String, i32);

/// Chronik state machine that applies committed entries to segment storage
pub struct ChronikStateMachine {
    /// Topic name
    topic: String,

    /// Partition ID
    partition: i32,

    /// WAL manager for durable storage
    wal_manager: Arc<chronik_wal::WalManager>,

    /// Metadata store for high watermark updates
    metadata: Arc<dyn MetadataStore>,

    /// Last applied index
    last_applied: u64,
}

impl ChronikStateMachine {
    /// Create a new Chronik state machine
    pub fn new(
        topic: String,
        partition: i32,
        wal_manager: Arc<chronik_wal::WalManager>,
        metadata: Arc<dyn MetadataStore>,
    ) -> Self {
        Self {
            topic,
            partition,
            wal_manager,
            metadata,
            last_applied: 0,
        }
    }

    /// Get topic name
    pub fn topic(&self) -> &str {
        &self.topic
    }

    /// Get partition ID
    pub fn partition(&self) -> i32 {
        self.partition
    }
}

#[async_trait]
impl StateMachine for ChronikStateMachine {
    async fn apply(&mut self, entry: &RaftEntry) -> RaftResult<Bytes> {
        // Deserialize as CanonicalRecord
        let record: CanonicalRecord = bincode::deserialize(&entry.data)
            .map_err(|e| chronik_raft::RaftError::SerializationError(e.to_string()))?;

        info!(
            "STATE_MACHINE: Applying Raft entry {} to {}-{}: base_offset={}, num_records={}",
            entry.index, self.topic, self.partition, record.base_offset, record.records.len()
        );

        // Write directly to WAL (same path as ProduceHandler)
        // Re-serialize the CanonicalRecord to Vec<u8>
        let canonical_bytes = bincode::serialize(&record)
            .map_err(|e| chronik_raft::RaftError::SerializationError(format!("Failed to serialize: {}", e)))?;

        let last_offset = record.records.last()
            .map(|r| r.offset)
            .unwrap_or(record.base_offset);

        let record_count = record.records.len() as i32;

        self.wal_manager
            .append_canonical(
                self.topic.clone(),
                self.partition,
                canonical_bytes,
                record.base_offset,
                last_offset,
                record_count,
            )
            .await
            .map_err(|e| chronik_raft::RaftError::StorageError(format!("WAL write failed: {}", e)))?;

        // Update high watermark in metadata (async)
        let last_offset = record.records.last()
            .map(|r| r.offset + 1)
            .unwrap_or(record.base_offset);

        self.metadata
            .update_partition_offset(
                &self.topic,
                self.partition as u32,
                last_offset,
                0  // log_start_offset
            )
            .await
            .map_err(|e| chronik_raft::RaftError::StorageError(e.to_string()))?;

        // Update last applied
        self.last_applied = entry.index;

        info!(
            "STATE_MACHINE: Applied entry {} to {}-{}: new HWM={}, wrote to WAL",
            entry.index,
            self.topic,
            self.partition,
            last_offset
        );

        Ok(Bytes::from(format!("applied:{}", entry.index)))
    }

    async fn snapshot(&self, last_index: u64, last_term: u64) -> RaftResult<SnapshotData> {
        info!(
            "Creating snapshot for {}-{}: last_index={}, last_term={}",
            self.topic, self.partition, last_index, last_term
        );

        // For now, use simple snapshot (just metadata)
        // In production, this would create S3 snapshot of segment data
        let hwm = match self.metadata
            .get_partition_offset(&self.topic, self.partition as u32)
            .await {
            Ok(Some((hwm, _lso))) => hwm,
            Ok(None) | Err(_) => 0,
        };

        let snapshot_meta = serde_json::json!({
            "topic": self.topic,
            "partition": self.partition,
            "last_index": last_index,
            "last_term": last_term,
            "high_watermark": hwm,
        });

        let data = serde_json::to_vec(&snapshot_meta)
            .map_err(|e| chronik_raft::RaftError::SerializationError(e.to_string()))?;

        Ok(SnapshotData {
            last_index,
            last_term,
            conf_state: vec![],
            data,
        })
    }

    async fn restore(&mut self, snapshot: &SnapshotData) -> RaftResult<()> {
        info!(
            "Restoring snapshot for {}-{}: last_index={}",
            self.topic, self.partition, snapshot.last_index
        );

        // Parse snapshot metadata
        let snapshot_meta: serde_json::Value = serde_json::from_slice(&snapshot.data)
            .map_err(|e| chronik_raft::RaftError::SerializationError(e.to_string()))?;

        // Restore high watermark
        if let Some(hwm) = snapshot_meta["high_watermark"].as_i64() {
            self.metadata
                .update_partition_offset(&self.topic, self.partition as u32, hwm, 0)
                .await
                .map_err(|e| chronik_raft::RaftError::StorageError(e.to_string()))?;
        }

        self.last_applied = snapshot.last_index;

        info!(
            "Restored snapshot for {}-{}: last_applied={}",
            self.topic, self.partition, self.last_applied
        );

        Ok(())
    }

    fn last_applied(&self) -> u64 {
        self.last_applied
    }
}

/// Configuration for Raft replica manager
#[derive(Debug, Clone)]
pub struct RaftManagerConfig {
    /// Raft configuration
    pub raft_config: RaftConfig,

    /// Enable Raft clustering
    pub enabled: bool,

    /// Tick interval in milliseconds
    pub tick_interval_ms: u64,

    /// Pre-populated list of peer node IDs (for immediate availability)
    pub initial_peers: Vec<u64>,
}

impl Default for RaftManagerConfig {
    fn default() -> Self {
        Self {
            raft_config: RaftConfig::default(),
            enabled: false,
            tick_interval_ms: 10,
            initial_peers: Vec::new(),
        }
    }
}

/// Manages Raft replicas for all partitions
pub struct RaftReplicaManager {
    /// Configuration
    pub config: RaftManagerConfig,

    /// Map of partition -> replica
    replicas: Arc<DashMap<PartitionKey, Arc<PartitionReplica>>>,

    /// Map of partition -> state machine
    state_machines: Arc<DashMap<PartitionKey, Arc<tokio::sync::RwLock<dyn StateMachine>>>>,

    /// Metadata store (wrapped in RwLock to allow replacement with RaftMetaLog)
    metadata: Arc<RwLock<Arc<dyn MetadataStore>>>,

    /// WAL manager for writing committed entries
    wal_manager: Arc<chronik_wal::WalManager>,

    /// gRPC client for peer communication
    raft_client: Arc<RaftClient>,

    /// gRPC service for handling incoming Raft RPCs
    raft_service: Arc<tokio::sync::RwLock<Option<Arc<chronik_raft::rpc::RaftServiceImpl>>>>,

    /// Peer node IDs in the cluster (for replica creation)
    peer_nodes: Arc<RwLock<Vec<u64>>>,

    /// Shared metadata state for __meta partition (created by raft_cluster.rs)
    /// This is the SAME state used by MetadataStateMachine in the __meta replica
    meta_partition_state: Arc<RwLock<Option<(
        Arc<parking_lot::RwLock<chronik_raft::MetadataState>>,
        Arc<std::sync::atomic::AtomicU64>,
    )>>>,

    /// Event channel for receiving Raft state changes
    event_tx: mpsc::UnboundedSender<RaftEvent>,
    event_rx: Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<RaftEvent>>>,
}

impl RaftReplicaManager {
    /// Create a new Raft replica manager
    ///
    /// # Validation
    /// - Raft requires 3+ nodes for quorum-based replication
    /// - Single-node deployments should use standalone mode (without Raft)
    pub fn new(
        config: RaftManagerConfig,
        metadata: Arc<dyn MetadataStore>,
        wal_manager: Arc<chronik_wal::WalManager>,
    ) -> Self {
        // Log warning if Raft is enabled - this is informational only
        // The actual validation happens during replica creation when peers are known
        if config.enabled {
            info!(
                "Raft clustering enabled on node {}. Raft requires 3+ nodes for production use.",
                config.raft_config.node_id
            );
            info!(
                "For single-node deployments, use standalone mode (without --raft flag) for better performance."
            );
        }

        // Pre-populate peer_nodes from config for immediate availability
        let initial_peers = config.initial_peers.clone();

        // Create event channel for Raft state changes
        let (event_tx, event_rx) = mpsc::unbounded_channel();

        Self {
            config,
            replicas: Arc::new(DashMap::new()),
            state_machines: Arc::new(DashMap::new()),
            metadata: Arc::new(RwLock::new(metadata)),
            wal_manager,
            raft_client: Arc::new(RaftClient::new()),
            raft_service: Arc::new(tokio::sync::RwLock::new(None)),
            peer_nodes: Arc::new(RwLock::new(initial_peers)),
            meta_partition_state: Arc::new(RwLock::new(None)),
            event_tx,
            event_rx: Arc::new(tokio::sync::Mutex::new(event_rx)),
        }
    }

    /// Set the Raft gRPC service for handling incoming RPCs
    pub async fn set_raft_service(&self, service: Arc<chronik_raft::rpc::RaftServiceImpl>) {
        // Store the service synchronously to avoid race condition with replica registration
        let mut guard = self.raft_service.write().await;
        *guard = Some(service);
        info!("Raft gRPC service set successfully");
    }

    /// Set the shared metadata state for __meta partition
    /// This is called by raft_cluster.rs after creating the __meta replica
    pub async fn set_meta_partition_state(
        &self,
        local_state: Arc<parking_lot::RwLock<chronik_raft::MetadataState>>,
        applied_index: Arc<std::sync::atomic::AtomicU64>,
    ) {
        let mut state = self.meta_partition_state.write().await;
        *state = Some((local_state, applied_index));
        info!("Meta partition state registered with RaftReplicaManager");
    }

    /// Replace the metadata store with a Raft-replicated one
    /// This is called after initializing RaftMetaLog to replace the temporary FileMetadataStore
    pub async fn set_metadata_store(&self, metadata: Arc<dyn MetadataStore>) {
        let mut store = self.metadata.write().await;
        *store = metadata;
        info!("Metadata store replaced with Raft-replicated version");
    }

    /// Get the shared metadata state for __meta partition
    /// Returns None if not yet initialized
    pub async fn get_meta_partition_state(
        &self,
    ) -> Option<(
        Arc<parking_lot::RwLock<chronik_raft::MetadataState>>,
        Arc<std::sync::atomic::AtomicU64>,
    )> {
        self.meta_partition_state.read().await.clone()
    }

    /// Add a peer node with its Raft gRPC address
    pub async fn add_peer(&self, node_id: u64, addr: String) -> RaftResult<()> {
        // Add to gRPC client for communication
        self.raft_client.add_peer(node_id, addr).await?;

        // Add to peer list for replica creation
        let mut peers = self.peer_nodes.write().await;
        if !peers.contains(&node_id) {
            peers.push(node_id);
            info!("Added peer {} to RaftReplicaManager (total peers: {})", node_id, peers.len());
        }
        Ok(())
    }

    /// Remove a peer node
    pub async fn remove_peer(&self, node_id: u64) {
        self.raft_client.remove_peer(node_id).await;

        // Remove from peer list
        let mut peers = self.peer_nodes.write().await;
        peers.retain(|&id| id != node_id);
        info!("Removed peer {} from RaftReplicaManager (remaining peers: {})", node_id, peers.len());
    }

    /// Get list of peer node IDs
    pub async fn get_peers(&self) -> Vec<u64> {
        self.peer_nodes.read().await.clone()
    }

    /// Check if Raft clustering is enabled
    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }

    /// Create a Raft replica for a partition
    ///
    /// # Arguments
    /// * `topic` - Topic name
    /// * `partition` - Partition ID
    /// * `log_storage` - Raft log storage backend
    /// * `peers` - List of peer node IDs
    pub async fn create_replica(
        &self,
        topic: String,
        partition: i32,
        log_storage: Arc<dyn RaftLogStorage>,
        peers: Vec<u64>,
    ) -> RaftResult<()> {
        let key = (topic.clone(), partition);

        if self.replicas.contains_key(&key) {
            warn!(
                "Replica for {}-{} already exists, skipping creation",
                topic, partition
            );
            return Ok(());
        }

        info!(
            "Creating Raft replica for {}-{} with peers {:?}",
            topic, partition, peers
        );

        // Create state machine (directly as dyn StateMachine)
        let metadata_store = self.metadata.read().await.clone();
        let state_machine: Arc<tokio::sync::RwLock<dyn StateMachine>> = Arc::new(tokio::sync::RwLock::new(ChronikStateMachine::new(
            topic.clone(),
            partition,
            self.wal_manager.clone(),
            metadata_store,
        )));

        // Create replica with event channel
        let replica = Arc::new(PartitionReplica::new(
            topic.clone(),
            partition,
            self.config.raft_config.clone(),
            log_storage,
            state_machine.clone(),
            peers,
            Some(self.event_tx.clone()),
        )?);

        // Store replica and state machine
        self.replicas.insert(key.clone(), replica.clone());
        self.state_machines.insert(key.clone(), state_machine.clone());

        // Register replica with Raft service (for handling incoming RPCs)
        if let Some(service) = self.raft_service.read().await.as_ref() {
            service.register_replica(replica.clone());
            info!("Registered {}-{} with Raft gRPC service", topic, partition);
        }

        info!("Created Raft replica for {}-{}", topic, partition);

        // Start background processing loop
        self.start_replica_loop(topic, partition).await;

        Ok(())
    }

    /// Create a metadata partition replica with NullStateMachine
    ///
    /// This is specifically for the __meta partition that stores cluster metadata.
    /// Unlike regular data partitions which use ChronikStateMachine,
    /// the __meta partition uses a simple MemoryStateMachine (null/pass-through).
    /// The actual metadata state is managed by chronik_raft::RaftMetaLog::BackgroundProcessor.
    ///
    /// # Arguments
    /// * `topic` - Topic name (should be "__meta")
    /// * `partition` - Partition ID (should be 0)
    /// * `log_storage` - Raft log storage backend
    /// * `peers` - List of peer node IDs
    pub async fn create_meta_replica(
        &self,
        topic: String,
        partition: i32,
        log_storage: Arc<dyn RaftLogStorage>,
        peers: Vec<u64>,
        state_machine: Arc<tokio::sync::RwLock<dyn StateMachine>>,
    ) -> RaftResult<()> {
        let key = (topic.clone(), partition);

        if self.replicas.contains_key(&key) {
            warn!(
                "Replica for {}-{} already exists, skipping creation",
                topic, partition
            );
            return Ok(());
        }

        info!(
            "Creating metadata Raft replica for {}-{} with peers {:?}",
            topic, partition, peers
        );

        // Use provided state machine (MetadataStateMachine from RaftMetaLog)
        // Create replica with event channel
        let replica = Arc::new(PartitionReplica::new(
            topic.clone(),
            partition,
            self.config.raft_config.clone(),
            log_storage,
            state_machine.clone(),
            peers,
            Some(self.event_tx.clone()),
        )?);

        // Store replica and state machine
        self.replicas.insert(key.clone(), replica.clone());
        self.state_machines.insert(key.clone(), state_machine.clone());

        // Register replica with Raft service (for handling incoming RPCs)
        if let Some(service) = self.raft_service.read().await.as_ref() {
            service.register_replica(replica.clone());
            info!("Registered {}-{} with Raft gRPC service", topic, partition);
        }

        info!("Created metadata Raft replica for {}-{}", topic, partition);

        // Start background processing loop
        self.start_replica_loop(topic, partition).await;

        Ok(())
    }

    /// Add a peer node to an existing replica via configuration change
    ///
    /// This properly initializes Raft's Progress tracker for the peer, which is
    /// required for Raft to send messages to that peer.
    ///
    /// # Arguments
    /// * `topic` - Topic name
    /// * `partition` - Partition ID
    /// * `peer_node_id` - Node ID of the peer to add
    pub async fn add_peer_to_replica(
        &self,
        topic: &str,
        partition: i32,
        peer_node_id: u64,
    ) -> RaftResult<()> {
        let key = (topic.to_string(), partition);

        let replica = match self.replicas.get(&key) {
            Some(r) => r.clone(),
            None => {
                return Err(chronik_raft::RaftError::Config(format!(
                    "Replica not found for {}-{}",
                    topic, partition
                )));
            }
        };

        // Check if this node is the leader
        if !replica.is_leader() {
            return Err(chronik_raft::RaftError::Config(format!(
                "Node {} is not leader for {}-{}, cannot add peer (current leader: {})",
                self.config.raft_config.node_id,
                topic,
                partition,
                replica.leader_id()
            )));
        }

        info!(
            "Adding peer {} to replica {}-{} via conf change",
            peer_node_id, topic, partition
        );

        // Create AddNode conf change
        let mut conf_change = ConfChange::default();
        conf_change.set_node_id(peer_node_id);
        conf_change.set_change_type(ConfChangeType::AddNode);

        // Propose the configuration change
        replica.propose_conf_change(conf_change).await?;

        info!(
            "Proposed AddNode conf change for peer {} on {}-{}",
            peer_node_id, topic, partition
        );

        Ok(())
    }

    /// Start background processing loop for a replica
    async fn start_replica_loop(&self, topic: String, partition: i32) {
        let key = (topic.clone(), partition);

        let replica = match self.replicas.get(&key) {
            Some(r) => r.clone(),
            None => {
                error!("Replica not found for {}-{}", topic, partition);
                return;
            }
        };

        let state_machine = match self.state_machines.get(&key) {
            Some(sm) => sm.clone(),
            None => {
                error!("State machine not found for {}-{}", topic, partition);
                return;
            }
        };

        let tick_interval_ms = self.config.tick_interval_ms;
        let raft_client = self.raft_client.clone();

        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(tokio::time::Duration::from_millis(tick_interval_ms));

            info!("Started Raft processing loop for {}-{}", topic, partition);

            loop {
                ticker.tick().await;

                // Drive Raft forward
                if let Err(e) = replica.tick() {
                    error!("Tick failed for {}-{}: {}", topic, partition, e);
                    continue;
                }

                // Process ready - this applies committed entries and sends notifications
                let (messages, committed) = match replica.ready().await {
                    Ok(result) => result,
                    Err(e) => {
                        error!("Ready failed for {}-{}: {}", topic, partition, e);
                        continue;
                    }
                };

                // ALWAYS log to see activity
                debug!(
                    "Tick loop for {}-{}: messages={}, committed={}",
                    topic, partition, messages.len(), committed.len()
                );

                // Send messages to peers via gRPC
                // CRITICAL FIX (v1.3.67): Send messages SEQUENTIALLY to preserve Raft message ordering!
                // Using tokio::spawn() creates unordered async tasks that violate Raft's ordering requirements,
                // causing vote responses to arrive out-of-term and triggering endless leader election churn.
                // See: docs/fixes/RAFT_MESSAGE_ORDERING_BUG.md
                if !messages.is_empty() {
                    info!("Sending {} messages to peers for {}-{}", messages.len(), topic, partition);

                    for msg in messages {
                        let to = msg.to;

                        // Send synchronously to guarantee ordering (await before next message)
                        if let Err(e) = raft_client.send_message(&topic, partition, to, msg).await {
                            error!("Failed to send message to peer {}: {}", to, e);
                        }
                    }
                }

                // Note: replica.ready() already applied committed entries and sent
                // commit notifications to pending proposals. No need to duplicate that work.
            }
        });
    }

    /// Get replica for a partition
    pub fn get_replica(&self, topic: &str, partition: i32) -> Option<Arc<PartitionReplica>> {
        let key = (topic.to_string(), partition);
        self.replicas.get(&key).map(|r| r.clone())
    }

    /// Get the WalManager instance (v1.3.66+: For sharing with IntegratedKafkaServer)
    ///
    /// This allows IntegratedKafkaServer to reuse the same WalManager instance
    /// that RaftReplicaManager uses, ensuring sealed_segments DashMap is shared.
    pub fn wal_manager(&self) -> Arc<chronik_wal::WalManager> {
        self.wal_manager.clone()
    }

    /// Check if a partition has a replica
    pub fn has_replica(&self, topic: &str, partition: i32) -> bool {
        let key = (topic.to_string(), partition);
        self.replicas.contains_key(&key)
    }

    /// List all partition replicas (topic, partition_id)
    pub fn list_replicas(&self) -> Vec<(String, i32)> {
        self.replicas
            .iter()
            .map(|entry| entry.key().clone())
            .collect()
    }

    /// Get the leader for a partition
    pub fn get_leader(&self, topic: &str, partition: i32) -> Option<u64> {
        let replica = self.get_replica(topic, partition)?;
        let leader_id = replica.leader_id();
        if leader_id == 0 {
            None
        } else {
            Some(leader_id)
        }
    }

    /// Check if this node is the leader for a partition
    pub fn is_leader(&self, topic: &str, partition: i32) -> bool {
        self.get_replica(topic, partition)
            .map(|r| r.is_leader())
            .unwrap_or(false)
    }

    /// Propose a write to a partition and wait for commit (must be leader)
    ///
    /// # Arguments
    /// * `topic` - Topic name
    /// * `partition` - Partition ID
    /// * `data` - Data to propose
    ///
    /// # Returns
    /// The committed index on success
    pub async fn propose(&self, topic: &str, partition: i32, data: Vec<u8>) -> RaftResult<u64> {
        let replica = self
            .get_replica(topic, partition)
            .ok_or_else(|| chronik_raft::RaftError::Config(format!("No replica for {}-{}", topic, partition)))?;

        // Use propose_and_wait which handles commit notification internally
        replica.propose_and_wait(data).await
    }

    /// Get the RaftClient for sending messages to peers
    pub fn raft_client(&self) -> Arc<RaftClient> {
        self.raft_client.clone()
    }

    /// Get count of active replicas
    pub fn replica_count(&self) -> usize {
        self.replicas.len()
    }

    /// List all partition keys
    pub fn list_partitions(&self) -> Vec<PartitionKey> {
        self.replicas.iter().map(|entry| entry.key().clone()).collect()
    }

    /// Start the event handler loop to process Raft state changes
    ///
    /// This should be spawned as a background task when the server starts.
    /// It listens for RaftEvent::BecameLeader events and updates partition
    /// assignments in the metadata store to keep them synchronized with
    /// actual Raft leaders.
    ///
    /// # Example
    /// ```ignore
    /// let manager = Arc::new(RaftReplicaManager::new(...));
    /// tokio::spawn(manager.clone().run_event_handler());
    /// ```
    pub async fn run_event_handler(self: Arc<Self>) {
        let mut rx = self.event_rx.lock().await;
        info!("Starting Raft event handler loop");

        while let Some(event) = rx.recv().await {
            match event {
                RaftEvent::BecameLeader { topic, partition, node_id, term } => {
                    info!(
                        "EVENT HANDLER: Node {} became leader for {}/{} at term {}",
                        node_id, topic, partition, term
                    );

                    // Update partition assignment to reflect new leader
                    let assignment = chronik_common::metadata::PartitionAssignment {
                        topic: topic.clone(),
                        partition,
                        broker_id: node_id as i32,
                        is_leader: true,
                    };

                    // Acquire read lock to access metadata store
                    let metadata = self.metadata.read().await;
                    if let Err(e) = metadata.assign_partition(assignment).await {
                        error!(
                            "Failed to update partition assignment for {}/{} leader {}: {:?}",
                            topic, partition, node_id, e
                        );
                    } else {
                        info!(
                            "Updated partition assignment: {}/{} leader is now node {}",
                            topic, partition, node_id
                        );
                    }
                }

                RaftEvent::BecameFollower { topic, partition, node_id, new_leader, term } => {
                    debug!(
                        "EVENT HANDLER: Node {} became follower for {}/{} (new leader: {:?}) at term {}",
                        node_id, topic, partition, new_leader, term
                    );
                    // No action needed - leader event will update the assignment
                }

                RaftEvent::LeaderChanged { topic, partition, node_id, new_leader, term } => {
                    debug!(
                        "EVENT HANDLER: Node {} detected leader change to {} for {}/{} at term {}",
                        node_id, new_leader, topic, partition, term
                    );
                    // No action needed - the new leader will send BecameLeader event
                }
            }
        }

        warn!("Raft event handler loop exited");
    }

    /// Get the committed offset for a partition (v1.3.66+)
    ///
    /// This is the highest offset that's been committed by Raft (replicated to majority).
    /// Safe to serve fetches up to this offset for Read Committed consistency.
    ///
    /// # Arguments
    /// * `topic` - Topic name
    /// * `partition` - Partition ID
    ///
    /// # Returns
    /// The committed offset as i64, or error if partition not found
    pub fn get_committed_offset(&self, topic: &str, partition: i32) -> chronik_raft::Result<i64> {
        let replica = self
            .get_replica(topic, partition)
            .ok_or_else(|| chronik_raft::RaftError::Config(format!("No replica for {}-{}", topic, partition)))?;
        Ok(replica.commit_index() as i64)
    }
}

/// Helper function to create RaftLogStorage for a partition
pub async fn create_raft_log_storage(
    data_dir: &std::path::Path,
    topic: &str,
    partition: i32,
) -> chronik_raft::Result<Arc<dyn chronik_raft::RaftLogStorage>> {
    #[cfg(feature = "raft-storage")]
    {
        use chronik_wal::group_commit::{GroupCommitWal, GroupCommitConfig};
        use chronik_wal::RaftWalStorage;

        // Create WAL for this partition's Raft log
        let wal_config = GroupCommitConfig::auto_select();
        let wal_dir = data_dir.join(format!("raft_log_{topic}_{partition}"));

        tokio::fs::create_dir_all(&wal_dir).await
            .map_err(|e| chronik_raft::RaftError::StorageError(e.to_string()))?;

        let wal = Arc::new(GroupCommitWal::new(wal_dir.clone(), wal_config));

        let storage = RaftWalStorage::new(wal);

        // Recover existing state if any
        storage.recover(&wal_dir).await?;

        Ok(Arc::new(storage) as Arc<dyn chronik_raft::RaftLogStorage>)
    }

    #[cfg(not(feature = "raft-storage"))]
    {
        // Fallback: Create WAL-backed storage even without raft-storage feature
        let wal_config = GroupCommitConfig::default();
        let wal_dir = data_dir.join(format!("raft_log_{topic}_{partition}"));

        tokio::fs::create_dir_all(&wal_dir).await
            .map_err(|e| chronik_raft::RaftError::StorageError(e.to_string()))?;

        let wal = Arc::new(GroupCommitWal::new(wal_dir.clone(), wal_config));
        let storage = RaftWalStorage::new(wal);

        // Recover existing state if any
        storage.recover(&wal_dir).await?;

        Ok(Arc::new(storage) as Arc<dyn chronik_raft::RaftLogStorage>)
    }
}

// Implement RaftReplicaProvider trait for SnapshotManager integration
impl chronik_raft::RaftReplicaProvider for RaftReplicaManager {
    fn get_replica(&self, topic: &str, partition: i32) -> Option<Arc<PartitionReplica>> {
        self.get_replica(topic, partition)
    }

    fn list_partitions(&self) -> Vec<(String, i32)> {
        self.list_replicas()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Note: Tests require SegmentWriter implementation
    // Full integration tests in tests/integration/

    #[test]
    fn test_partition_key_type() {
        let key: PartitionKey = ("test".to_string(), 0);
        assert_eq!(key.0, "test");
        assert_eq!(key.1, 0);
    }

    #[test]
    fn test_raft_manager_config_default() {
        let config = RaftManagerConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.tick_interval_ms, 10);
    }
}
