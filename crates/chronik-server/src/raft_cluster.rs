//! RaftCluster wrapper (v2.5.0 Phase 2)
//!
//! This module wraps raft::RawNode with our MetadataStateMachine to provide
//! cluster metadata coordination (NOT data replication).
//!
//! Managed by this cluster:
//! - Cluster membership (which nodes are alive)
//! - Partition assignments (partition-0 → [node1, node2, node3])
//! - Partition leaders (partition-0 leader = node1)
//! - ISR tracking (in-sync replicas per partition)
//!
//! Usage:
//! ```rust
//! // Create Raft cluster
//! let cluster = RaftCluster::bootstrap(node_id, peers).await?;
//!
//! // Query partition metadata
//! let replicas = cluster.get_partition_replicas("orders", 0)?;
//! let leader = cluster.get_partition_leader("orders", 0)?;
//! ```

use crate::raft_metadata::{MetadataCommand, MetadataStateMachine, PartitionKey};
use anyhow::{Result, Context};
use std::sync::{Arc, RwLock};
use std::collections::HashMap;
use tokio::sync::{Mutex, mpsc};

use raft::{prelude::*, storage::MemStorage};
use slog::Drain;
use chronik_wal::{RaftWalStorage, GroupCommitWal, GroupCommitConfig};
use std::path::PathBuf;

// v2.5.0: gRPC transport for production Raft networking
use chronik_raft::{GrpcTransport, Transport, rpc::{RaftServiceImpl, raft_service_server}};
use tonic::transport::Server;

/// Raft cluster for metadata coordination
pub struct RaftCluster {
    /// Node ID in the cluster
    node_id: u64,

    /// Metadata state machine (shared with Raft)
    state_machine: Arc<RwLock<MetadataStateMachine>>,

    /// Raft node with WAL-backed persistent storage
    raft_node: Arc<RwLock<RawNode<RaftWalStorage>>>,

    /// Storage reference for async persist operations
    storage: Arc<RaftWalStorage>,

    /// gRPC transport for Raft messages (v2.5.0)
    transport: Arc<GrpcTransport>,

    /// gRPC server handle
    grpc_server_handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,

    /// Channel for sending Raft messages asynchronously
    /// Messages are taken from Ready and sent to this channel (non-blocking)
    /// A background task reads from channel and sends via gRPC
    message_sender: mpsc::UnboundedSender<(u64, Message)>,
}

impl RaftCluster {
    /// Bootstrap a new Raft cluster
    ///
    /// # Arguments
    /// - `node_id`: This node's ID in the cluster
    /// - `peers`: List of (node_id, address) for other nodes
    /// - `data_dir`: Data directory for WAL storage
    ///
    /// # Returns
    /// RaftCluster ready for metadata operations
    pub async fn bootstrap(node_id: u64, peers: Vec<(u64, String)>, data_dir: PathBuf) -> Result<Self> {
        tracing::info!(
            "Bootstrapping Raft cluster: node_id={}, peers={:?}",
            node_id,
            peers
        );

        // Create Raft configuration
        let config = Config {
            id: node_id,
            election_tick: 10,
            heartbeat_tick: 3,
            max_size_per_msg: 1024 * 1024,
            max_inflight_msgs: 256,
            ..Default::default()
        };

        // CRITICAL FIX: Build list of all nodes (self + peers) for voter configuration
        let mut all_nodes = vec![node_id];
        for (peer_id, _) in &peers {
            all_nodes.push(*peer_id);
        }

        tracing::info!("Initializing Raft cluster with voters: {:?}", all_nodes);

        // Create WAL-backed persistent storage for Raft log
        let meta_wal_dir = data_dir.join("wal").join("__meta");
        tokio::fs::create_dir_all(&meta_wal_dir).await
            .context("Failed to create metadata WAL directory")?;

        let meta_wal_config = GroupCommitConfig::default();

        let meta_wal = Arc::new(GroupCommitWal::new(meta_wal_dir.clone(), meta_wal_config));
        let wal_storage = RaftWalStorage::new(meta_wal);

        // Recover existing Raft log from WAL (if any)
        wal_storage.recover(&data_dir).await
            .context("Failed to recover Raft log from WAL")?;

        tracing::info!("✓ Raft WAL storage initialized (persistent)");

        // Initialize with voter configuration ONLY if no state was recovered
        if !wal_storage.has_recovered_state() {
            tracing::info!("No recovered Raft state - initializing fresh cluster");
            wal_storage.set_raft_state(RaftState {
                hard_state: HardState::default(),
                conf_state: ConfState {
                    voters: all_nodes.clone(),
                    learners: vec![],
                    ..Default::default()
                },
            });
        } else {
            tracing::info!("✓ Using recovered Raft state from WAL");
        }

        // Create logger for Raft (it uses slog)
        let logger = slog::Logger::root(slog_stdlog::StdLog.fuse(), slog::o!());

        // Clone storage for later persistence operations
        let storage_for_async = wal_storage.clone();

        // Create Raft node with WAL-backed storage
        let raft_node = RawNode::new(&config, wal_storage, &logger)
            .context("Failed to create Raft node")?;

        // Create state machine
        let state_machine = Arc::new(RwLock::new(MetadataStateMachine::new()));

        // Create gRPC transport
        let transport = Arc::new(GrpcTransport::new());

        // Register peers with gRPC transport
        for (peer_id, peer_addr) in peers {
            // Convert Raft address (e.g., "0.0.0.0:5001") to gRPC URL (e.g., "http://localhost:5001")
            let grpc_url = if peer_addr.starts_with("http") {
                peer_addr
            } else {
                format!("http://{}", peer_addr)
            };

            transport.add_peer(peer_id, grpc_url).await
                .context(format!("Failed to add peer {} to transport", peer_id))?;

            tracing::info!("✓ Registered peer {} in gRPC transport", peer_id);
        }

        tracing::info!("✓ Raft cluster initialized with {} voters and gRPC transport", all_nodes.len());

        // Create channel for async message sending
        // CRITICAL: This allows us to take_messages() from Ready (satisfying Raft),
        // then send to channel (non-blocking), while a background task does actual gRPC sends
        let (message_sender, mut message_receiver) = mpsc::unbounded_channel::<(u64, Message)>();

        // Start background message sender task
        let transport_for_sender = transport.clone();
        tokio::spawn(async move {
            while let Some((peer_id, msg)) = message_receiver.recv().await {
                let msg_type = msg.msg_type;
                tracing::debug!("Message sender: sending {:?} to peer {}", msg_type, peer_id);

                // Use gRPC transport to send the message
                const METADATA_TOPIC: &str = "__raft_metadata";
                const METADATA_PARTITION: i32 = 0;

                match transport_for_sender
                    .send_message(METADATA_TOPIC, METADATA_PARTITION, peer_id, msg)
                    .await
                {
                    Ok(_) => {
                        tracing::debug!("✓ Message sender: sent {:?} to peer {}", msg_type, peer_id);
                    }
                    Err(e) => {
                        tracing::warn!("Message sender: failed to send {:?} to peer {}: {}", msg_type, peer_id, e);
                    }
                }
            }
            tracing::info!("Message sender task shutting down");
        });

        tracing::info!("✓ Raft message sender task started");

        Ok(Self {
            node_id,
            state_machine,
            raft_node: Arc::new(RwLock::new(raft_node)),
            storage: Arc::new(storage_for_async),
            transport,
            grpc_server_handle: Arc::new(Mutex::new(None)),
            message_sender,
        })
    }

    /// Get partition replicas (nodes that should replicate this partition)
    pub fn get_partition_replicas(&self, topic: &str, partition: i32) -> Option<Vec<u64>> {
        let sm = self.state_machine.read().ok()?;
        sm.get_partition_replicas(topic, partition)
    }

    /// Get partition leader (node ID that's the leader for this partition)
    pub fn get_partition_leader(&self, topic: &str, partition: i32) -> Option<u64> {
        let sm = self.state_machine.read().ok()?;
        sm.get_partition_leader(topic, partition)
    }

    /// Get ISR (in-sync replicas) for a partition
    pub fn get_isr(&self, topic: &str, partition: i32) -> Option<Vec<u64>> {
        let sm = self.state_machine.read().ok()?;
        sm.get_isr(topic, partition)
    }

    /// Check if a node is in-sync for a partition
    pub fn is_in_sync(&self, topic: &str, partition: i32, node_id: u64) -> bool {
        self.state_machine
            .read()
            .ok()
            .map(|sm| sm.is_in_sync(topic, partition, node_id))
            .unwrap_or(false)
    }

    /// Get all partitions where the specified node is the leader
    ///
    /// Returns a list of (topic, partition) tuples where this node is the leader.
    /// Used by WAL replication manager to discover which partitions to replicate.
    pub fn get_partitions_where_leader(&self, node_id: u64) -> Vec<PartitionKey> {
        self.state_machine
            .read()
            .ok()
            .map(|sm| sm.get_partitions_where_leader(node_id))
            .unwrap_or_default()
    }

    /// Propose a metadata command to the Raft cluster
    ///
    /// This serializes the command and proposes it via Raft consensus.
    /// Once committed, it will be applied to the state machine.
    ///
    /// **CRITICAL**: Only the leader can propose. If this node is not the leader,
    /// this method returns an error.
    pub async fn propose(&self, cmd: MetadataCommand) -> Result<()> {
        // CRITICAL FIX: Check if we're the leader BEFORE proposing
        // Non-leader nodes MUST NOT call raft.propose() - this violates Raft protocol
        // and triggers panic: "not leader but has new msg after advance"
        {
            let raft = self.raft_node.read()
                .map_err(|e| anyhow::anyhow!("Failed to acquire Raft lock: {}", e))?;

            let state = raft.raft.state;
            let leader_id = raft.raft.leader_id;

            if state != raft::StateRole::Leader {
                return Err(anyhow::anyhow!(
                    "Cannot propose: this node (id={}) is not the leader (state={:?}, leader={})",
                    self.node_id,
                    state,
                    leader_id
                ));
            }
        }

        // Serialize command
        let data = bincode::serialize(&cmd)
            .context("Failed to serialize metadata command")?;

        // Propose to Raft (safe now - we verified we're the leader)
        let mut raft = self.raft_node.write()
            .map_err(|e| anyhow::anyhow!("Failed to acquire Raft lock: {}", e))?;

        raft.propose(vec![], data)
            .context("Failed to propose to Raft")?;

        Ok(())
    }

    /// Propose adding a new node to the cluster (Priority 2: Zero-Downtime Node Addition)
    ///
    /// This creates a ConfChangeV2 entry that, when committed by Raft consensus,
    /// adds the node as a voting member.
    ///
    /// **CRITICAL**: This method MUST be called on the Raft leader. Non-leader
    /// calls will return an error.
    ///
    /// # Arguments
    /// - `node_id`: ID of the new node (must be unique)
    /// - `kafka_addr`: Kafka broker address for client connections
    /// - `wal_addr`: WAL replication receiver address
    /// - `raft_addr`: Raft gRPC server address
    ///
    /// # Returns
    /// - Ok(()) if ConfChange was proposed (not yet committed)
    /// - Err if not leader, node_id exists, or Raft error
    ///
    /// # Example
    /// ```rust
    /// // On the leader node:
    /// raft_cluster.propose_add_node(
    ///     4,
    ///     "node4.example.com:9092",
    ///     "node4.example.com:9291",
    ///     "node4.example.com:5001"
    /// ).await?;
    /// ```
    pub async fn propose_add_node(
        &self,
        node_id: u64,
        kafka_addr: String,
        wal_addr: String,
        raft_addr: String,
    ) -> Result<()> {
        // STEP 1: Validate we're the leader
        {
            let raft = self.raft_node.read()
                .map_err(|e| anyhow::anyhow!("Failed to acquire Raft lock: {}", e))?;

            if raft.raft.state != raft::StateRole::Leader {
                return Err(anyhow::anyhow!(
                    "Cannot add node: this node (id={}) is not the leader (state={:?}, leader={})",
                    self.node_id,
                    raft.raft.state,
                    raft.raft.leader_id
                ));
            }
        }

        // STEP 2: Check if node_id already exists
        let current_nodes = self.get_all_nodes();
        if current_nodes.contains(&node_id) {
            return Err(anyhow::anyhow!(
                "Node {} already exists in cluster (current nodes: {:?})",
                node_id,
                current_nodes
            ));
        }

        tracing::info!(
            "Proposing to add node {} to cluster (current nodes: {:?})",
            node_id,
            current_nodes
        );

        // STEP 3: Create ConfChangeV2 to add voter
        use raft::prelude::*;

        let mut cc = ConfChangeV2::default();
        cc.set_transition(ConfChangeTransition::Auto);

        let mut change = ConfChangeSingle::default();
        change.set_change_type(ConfChangeType::AddNode);
        change.set_node_id(node_id);

        cc.set_changes(vec![change].into());

        // Context: Store node addresses for later use
        // Format: "kafka_addr|wal_addr|raft_addr"
        let context = format!("{}|{}|{}", kafka_addr, wal_addr, raft_addr);
        tracing::debug!("ConfChangeV2 context: {}", context);

        cc.set_context(context.into_bytes());

        // STEP 4: Propose ConfChange via Raft
        // CRITICAL: propose_conf_change takes the ConfChangeV2 directly, NOT serialized bytes
        let mut raft = self.raft_node.write()
            .map_err(|e| anyhow::anyhow!("Failed to acquire Raft lock: {}", e))?;

        raft.propose_conf_change(vec![], cc)
            .context("Failed to propose ConfChange")?;

        tracing::info!(
            "✓ Proposed adding node {} to cluster (kafka={}, wal={}, raft={})",
            node_id, kafka_addr, wal_addr, raft_addr
        );

        Ok(())
    }

    /// Propose removing a node from the cluster (Priority 4: Zero-Downtime Node Removal)
    ///
    /// This creates a ConfChangeV2 entry that, when committed by Raft consensus,
    /// removes the node as a voting member.
    ///
    /// **CRITICAL**: This method MUST be called on the Raft leader. Non-leader
    /// calls will return an error.
    ///
    /// **SAFETY**: This method checks that removing the node won't break quorum.
    /// For a 3-node cluster, you cannot remove a node (would leave 2 nodes, no quorum).
    /// Minimum cluster size after removal is 3 nodes.
    ///
    /// # Arguments
    /// - `node_id`: ID of the node to remove
    /// - `force`: If true, skip partition reassignment (for dead nodes)
    ///
    /// # Returns
    /// - Ok(()) if ConfChange was proposed (not yet committed)
    /// - Err if not leader, node doesn't exist, or would break quorum
    ///
    /// # Example
    /// ```rust
    /// // On the leader node:
    /// // Graceful removal (reassigns partitions first)
    /// raft_cluster.propose_remove_node(3, false).await?;
    ///
    /// // Force removal (for dead node)
    /// raft_cluster.propose_remove_node(3, true).await?;
    /// ```
    pub async fn propose_remove_node(
        &self,
        node_id: u64,
        force: bool,
    ) -> Result<()> {
        // STEP 1: Validate we're the leader
        {
            let raft = self.raft_node.read()
                .map_err(|e| anyhow::anyhow!("Failed to acquire Raft lock: {}", e))?;

            if raft.raft.state != raft::StateRole::Leader {
                return Err(anyhow::anyhow!(
                    "Cannot remove node: this node (id={}) is not the leader (state={:?}, leader={})",
                    self.node_id,
                    raft.raft.state,
                    raft.raft.leader_id
                ));
            }
        }

        // STEP 2: Check if node exists
        let current_nodes = self.get_all_nodes();
        if !current_nodes.contains(&node_id) {
            return Err(anyhow::anyhow!(
                "Node {} does not exist in cluster (current nodes: {:?})",
                node_id,
                current_nodes
            ));
        }

        // STEP 3: Check quorum safety (need at least 3 nodes after removal for future operations)
        let nodes_after_removal = current_nodes.len() - 1;
        if nodes_after_removal < 3 && !force {
            return Err(anyhow::anyhow!(
                "Cannot remove node {}: would leave {} nodes (minimum 3 required for safe operations). \
                 Current nodes: {:?}. Use --force to override (WARNING: may cause cluster instability)",
                node_id,
                nodes_after_removal,
                current_nodes
            ));
        }

        // STEP 4: Check if we're trying to remove ourselves
        if node_id == self.node_id && !force {
            return Err(anyhow::anyhow!(
                "Cannot remove self (node {}): leader cannot remove itself gracefully. \
                 Transfer leadership first or use --force",
                node_id
            ));
        }

        tracing::info!(
            "Proposing to remove node {} from cluster (current nodes: {:?}, force={})",
            node_id,
            current_nodes,
            force
        );

        // STEP 5: If not force, reassign partitions away from this node first
        if !force {
            tracing::info!("Reassigning partitions away from node {} before removal", node_id);
            self.reassign_partitions_from_node(node_id).await?;
        } else {
            tracing::warn!("Force removal: skipping partition reassignment for node {}", node_id);
        }

        // STEP 6: Create ConfChangeV2 to remove voter
        use raft::prelude::*;

        let mut cc = ConfChangeV2::default();
        cc.set_transition(ConfChangeTransition::Auto);

        let mut change = ConfChangeSingle::default();
        change.set_change_type(ConfChangeType::RemoveNode);
        change.set_node_id(node_id);

        cc.set_changes(vec![change].into());

        // No context needed for removal
        cc.set_context(vec![]);

        // STEP 7: Propose ConfChange via Raft
        let mut raft = self.raft_node.write()
            .map_err(|e| anyhow::anyhow!("Failed to acquire Raft lock: {}", e))?;

        raft.propose_conf_change(vec![], cc)
            .context("Failed to propose ConfChange for node removal")?;

        tracing::info!(
            "✓ Proposed removing node {} from cluster (force={})",
            node_id, force
        );

        Ok(())
    }

    /// Reassign partitions away from a node before removal (Priority 4)
    ///
    /// This method finds all partitions where the target node is a replica
    /// and proposes new partition assignments excluding that node.
    async fn reassign_partitions_from_node(&self, node_id: u64) -> Result<()> {
        // Scope the lock guard explicitly to ensure it's dropped before await
        let partitions_to_reassign = {
            let sm = self.state_machine.read()
                .map_err(|e| anyhow::anyhow!("Failed to acquire state machine lock: {}", e))?;

            let mut partitions = Vec::new();

            // Find all partitions where node_id is a replica
            for ((topic, partition), replicas) in &sm.partition_assignments {
                if replicas.contains(&node_id) {
                    partitions.push((topic.clone(), *partition, replicas.clone()));
                }
            }

            partitions
        }; // sm lock guard dropped here

        if partitions_to_reassign.is_empty() {
            tracing::info!("No partitions assigned to node {}, nothing to reassign", node_id);
            return Ok(());
        }

        tracing::info!(
            "Reassigning {} partitions away from node {}",
            partitions_to_reassign.len(),
            node_id
        );

        // Get all nodes except the one being removed
        let available_nodes: Vec<u64> = self.get_all_nodes()
            .into_iter()
            .filter(|&id| id != node_id)
            .collect();

        if available_nodes.is_empty() {
            return Err(anyhow::anyhow!(
                "Cannot reassign partitions: no other nodes available"
            ));
        }

        // For each partition, create new replica set without node_id
        for (topic, partition, old_replicas) in partitions_to_reassign {
            let mut new_replicas: Vec<u64> = old_replicas
                .into_iter()
                .filter(|&id| id != node_id)
                .collect();

            // If we removed a replica, add a new one from available nodes
            // to maintain replication factor
            if new_replicas.len() < 3 && !available_nodes.is_empty() {
                // Find a node not already in new_replicas
                for &candidate in &available_nodes {
                    if !new_replicas.contains(&candidate) {
                        new_replicas.push(candidate);
                        break;
                    }
                }
            }

            tracing::info!(
                "Reassigning partition {}-{}: removing node {}, new replicas: {:?}",
                topic, partition, node_id, new_replicas
            );

            // Propose new partition assignment
            let cmd = crate::raft_metadata::MetadataCommand::AssignPartition {
                topic: topic.clone(),
                partition,
                replicas: new_replicas,
            };

            self.propose(cmd).await?;
        }

        Ok(())
    }

    /// Apply committed Raft entries to the state machine
    ///
    /// This should be called by the Raft message processing loop when
    /// entries are committed.
    ///
    /// **Priority 2 Enhancement**: Now handles ConfChangeV2 entries for dynamic
    /// node addition/removal.
    pub fn apply_committed_entries(&self, entries: &[Entry]) -> Result<()> {
        use raft::prelude::*;

        let mut sm = self.state_machine.write()
            .map_err(|e| anyhow::anyhow!("Failed to acquire state machine lock: {}", e))?;

        for entry in entries {
            // Skip empty entries
            if entry.data.is_empty() {
                continue;
            }

            // Check entry type
            let entry_type = entry.get_entry_type();

            if entry_type == EntryType::EntryConfChangeV2 {
                // PRIORITY 2: Handle ConfChangeV2 (node addition/removal)
                // NOTE: ConfChange entries are handled by the message loop before committed entries
                // We don't process them here - they're automatically applied by Raft
                tracing::debug!("Skipping ConfChangeV2 entry (already processed by message loop)");

            } else if entry_type == EntryType::EntryNormal {
                // Normal metadata command
                let cmd: MetadataCommand = bincode::deserialize(&entry.data)
                    .context("Failed to deserialize metadata command")?;

                // Apply to state machine
                sm.apply(cmd)?;
            } else {
                tracing::debug!("Skipping entry type: {:?}", entry_type);
            }
        }

        Ok(())
    }

    /// Get this node's ID
    pub fn node_id(&self) -> u64 {
        self.node_id
    }

    /// Check if this node is currently the Raft leader
    pub async fn is_leader(&self) -> bool {
        let raft = match self.raft_node.read() {
            Ok(r) => r,
            Err(_) => return false,
        };
        raft.raft.state == raft::StateRole::Leader
    }

    /// List all partitions tracked in metadata
    ///
    /// Returns a list of (topic, partition) tuples for all known partitions.
    pub fn list_all_partitions(&self) -> Vec<(String, i32)> {
        let sm = self.state_machine.read().ok();
        match sm {
            Some(sm) => sm.partition_assignments.keys().cloned().collect(),
            None => vec![],
        }
    }

    /// Get all nodes currently in the cluster (voting members)
    ///
    /// Returns the list of node IDs that are currently voting members
    /// of the Raft cluster.
    ///
    /// # Returns
    /// Vector of node IDs (e.g., [1, 2, 3])
    pub fn get_all_nodes(&self) -> Vec<u64> {
        let raft = match self.raft_node.read() {
            Ok(r) => r,
            Err(_) => return vec![],
        };

        // Get voter IDs from Raft's configuration state
        // For a simple approach, iterate over the progress tracker
        let mut voters = Vec::new();
        for (id, _progress) in raft.raft.prs().iter() {
            voters.push(*id);
        }
        voters
    }

    /// Get node information (ID -> address mapping)
    ///
    /// # Returns
    /// Vector of (node_id, address) tuples
    pub fn get_node_info(&self) -> Vec<(u64, String)> {
        let sm = match self.state_machine.read() {
            Ok(s) => s,
            Err(_) => return vec![],
        };

        sm.nodes.iter().map(|(id, addr)| (*id, addr.clone())).collect()
    }

    /// Get all partition information
    ///
    /// # Returns
    /// Vector of PartitionInfo with topic, partition, leader, replicas, and ISR
    pub fn get_all_partition_info(&self) -> Vec<crate::admin_api::PartitionInfo> {
        let sm = match self.state_machine.read() {
            Ok(s) => s,
            Err(_) => return vec![],
        };

        let mut partitions = Vec::new();

        // Collect all unique partition keys from assignments
        for ((topic, partition), replicas) in &sm.partition_assignments {
            let leader = sm.partition_leaders.get(&(topic.clone(), *partition)).copied();
            let isr = sm.isr_sets.get(&(topic.clone(), *partition))
                .cloned()
                .unwrap_or_default();

            partitions.push(crate::admin_api::PartitionInfo {
                topic: topic.clone(),
                partition: *partition,
                leader,
                replicas: replicas.clone(),
                isr,
            });
        }

        partitions
    }

    /// Propose a partition leader change
    ///
    /// Helper method for leader election.
    pub async fn propose_set_partition_leader(
        &self,
        topic: &str,
        partition: i32,
        leader: u64,
    ) -> Result<()> {
        let cmd = MetadataCommand::SetPartitionLeader {
            topic: topic.to_string(),
            partition,
            leader,
        };

        self.propose(cmd).await
    }

    /// Check if Raft has a leader (either we're the leader or there's a valid leader)
    ///
    /// Returns (is_leader_ready, leader_id, state_role)
    pub fn is_leader_ready(&self) -> (bool, u64, String) {
        let raft_node = self.raft_node.read().unwrap();
        let state = raft_node.raft.state;
        let leader_id = raft_node.raft.leader_id;

        let is_ready = match state {
            raft::StateRole::Leader => true,
            raft::StateRole::Follower => leader_id != raft::INVALID_ID,
            _ => false,
        };

        let state_str = format!("{:?}", state);
        (is_ready, leader_id, state_str)
    }

    /// Send a Raft message to a peer node via TCP
    ///
    /// This creates a new TCP connection for each message. For production,
    /// consider implementing connection pooling.
    /// Send a Raft message to a peer via gRPC
    async fn send_raft_message(&self, peer_id: u64, msg: Message) -> Result<()> {
        tracing::debug!("Sending Raft {:?} to peer {}", msg.msg_type, peer_id);

        // Use gRPC transport to send the message
        // Topic and partition are special values for metadata messages
        const METADATA_TOPIC: &str = "__raft_metadata";
        const METADATA_PARTITION: i32 = 0;

        self.transport
            .send_message(METADATA_TOPIC, METADATA_PARTITION, peer_id, msg)
            .await
            .context(format!("Failed to send Raft message to peer {}", peer_id))?;

        tracing::debug!("✓ Sent Raft message to peer {} via gRPC", peer_id);
        Ok(())
    }

    /// Start gRPC server for incoming Raft messages
    ///
    /// This spawns a background task that runs a gRPC server listening for
    /// incoming Raft messages from peer nodes and feeds them into the local
    /// Raft node via step().
    pub async fn start_grpc_server(self: Arc<Self>, listen_addr: String) -> Result<()> {
        let addr = listen_addr.parse()
            .context(format!("Invalid gRPC listen address: {}", listen_addr))?;

        tracing::info!("Starting Raft gRPC server on {}", listen_addr);

        // Create message handler that feeds messages to Raft
        let cluster = self.clone();
        let message_handler = Arc::new(move |msg: Message| {
            // Feed message into Raft via step()
            let mut raft = cluster.raft_node.write()
                .map_err(|e| format!("Failed to acquire Raft lock: {}", e))?;

            raft.step(msg)
                .map_err(|e| format!("Failed to step Raft: {:?}", e))?;

            Ok(())
        });

        // Create RPC service
        let service = RaftServiceImpl::new(message_handler);

        // Start gRPC server
        let server = Server::builder()
            .add_service(raft_service_server::RaftServiceServer::new(service))
            .serve(addr);

        let handle = tokio::spawn(async move {
            if let Err(e) = server.await {
                tracing::error!("Raft gRPC server error: {}", e);
            }
        });

        // Store handle
        *self.grpc_server_handle.lock().await = Some(handle);

        tracing::info!("✓ Raft gRPC server started on {}", listen_addr);
        Ok(())
    }

    /// Start background Raft message processing loop
    ///
    /// This loop:
    /// 1. Ticks Raft every 100ms
    /// 2. Processes Ready states
    /// 3. Commits entries to state machine
    /// 4. Sends messages to peers (TODO: network)
    /// 5. Advances Raft
    ///
    /// This is CRITICAL for Phase 5 - without this loop, Raft proposals
    /// never get committed and metadata never replicates.
    pub fn start_message_loop(self: Arc<Self>) {
        tracing::info!("Starting Raft message processing loop");

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(100));

            loop {
                interval.tick().await;

                // Tick Raft
                {
                    let mut raft = match self.raft_node.write() {
                        Ok(r) => r,
                        Err(e) => {
                            tracing::error!("Failed to acquire Raft lock for tick: {}", e);
                            continue;
                        }
                    };
                    raft.tick();
                }

                // Process Ready state
                // CRITICAL: Must hold lock from ready() through advance() to prevent
                // other threads from calling step() and adding messages
                let mut raft_lock = match self.raft_node.write() {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::error!("Failed to acquire Raft lock for ready: {}", e);
                        continue;
                    }
                };

                if !raft_lock.has_ready() {
                    continue;
                }

                let mut ready = raft_lock.ready();

                // CRITICAL: Follow proper Raft Ready handling order
                // See: https://github.com/tikv/raft-rs/blob/master/examples/single_mem_node/main.rs

                // Step 1: Send unpersisted messages first
                // CRITICAL: MUST call take_messages() to satisfy Raft's ownership requirements
                // Then immediately send to channel (non-blocking) - background task handles actual sends
                if !ready.messages().is_empty() {
                    let messages = ready.take_messages();
                    tracing::info!("Raft ready: {} unpersisted messages to send", messages.len());

                    for msg in messages {
                        let peer_id = msg.to;
                        let msg_type = msg.msg_type;

                        tracing::debug!("Queueing unpersisted {:?} to peer {}", msg_type, peer_id);

                        // Send to channel - non-blocking, background task does gRPC
                        if let Err(e) = self.message_sender.send((peer_id, msg)) {
                            tracing::error!("Failed to queue message to peer {}: {}", peer_id, e);
                        }
                    }
                }

                // Step 2: Apply snapshot if present (Phase 5)
                if !raft::is_empty_snap(ready.snapshot()) {
                    let snapshot = ready.snapshot();
                    tracing::info!("Received snapshot from leader: index={}, term={}",
                        snapshot.get_metadata().index, snapshot.get_metadata().term);

                    // Clone storage before async operation
                    let storage_clone = self.storage.clone();
                    let state_machine_clone = self.state_machine.clone();
                    let snapshot_clone = snapshot.clone();

                    // Persist snapshot and apply to state machine
                    let apply_result = tokio::task::block_in_place(|| {
                        tokio::runtime::Handle::current().block_on(async {
                            // Save snapshot to disk
                            storage_clone.save_snapshot(&snapshot_clone).await?;

                            // Deserialize and apply state machine
                            let state_machine_data = snapshot_clone.get_data();
                            if !state_machine_data.is_empty() {
                                let new_state: MetadataStateMachine = bincode::deserialize(state_machine_data)
                                    .context("Failed to deserialize state machine from snapshot")?;

                                // Replace state machine with snapshot state
                                *state_machine_clone.write().unwrap() = new_state;

                                tracing::info!("✓ Applied state machine from snapshot");
                            }

                            // Truncate log entries covered by snapshot
                            storage_clone.truncate_log_to_snapshot(snapshot_clone.get_metadata().index).await?;

                            Ok::<(), anyhow::Error>(())
                        })
                    });

                    if let Err(e) = apply_result {
                        tracing::error!("Failed to apply snapshot: {}", e);
                    } else {
                        tracing::info!("✓ Snapshot applied successfully");
                    }
                }

                // Step 3a: Handle ConfChange entries (Priority 2: Zero-Downtime Node Addition)
                // CRITICAL: ConfChange must be processed BEFORE normal committed entries
                if !ready.committed_entries().is_empty() {
                    use raft::prelude::*;

                    for entry in ready.committed_entries() {
                        if entry.get_entry_type() == EntryType::EntryConfChangeV2 {
                            tracing::info!("Processing ConfChangeV2 entry (index={})", entry.index);

                            // Decode ConfChangeV2 from protobuf bytes in entry.data
                            // raft-rs stores ConfChange entries as protobuf-encoded bytes
                            // Use chronik-raft bridge to handle prost 0.11 / 0.13 compatibility
                            use chronik_raft::prost_bridge;
                            let cc = match prost_bridge::decode_conf_change_v2(&entry.data) {
                                Ok(cc) => cc,
                                Err(e) => {
                                    tracing::error!("Failed to decode ConfChangeV2: {}", e);
                                    continue;
                                }
                            };

                            // Apply to Raft (updates voter list)
                            let cs = {
                                let mut raft = match self.raft_node.write() {
                                    Ok(r) => r,
                                    Err(e) => {
                                        tracing::error!("Failed to acquire Raft lock: {}", e);
                                        continue;
                                    }
                                };

                                match raft.apply_conf_change(&cc) {
                                    Ok(cs) => cs,
                                    Err(e) => {
                                        tracing::error!("Failed to apply ConfChange: {:?}", e);
                                        continue;
                                    }
                                }
                            };

                            tracing::debug!("✓ Applied ConfChange to Raft (new config: {:?})", cs);

                            // Parse context to get node addresses
                            let context = String::from_utf8_lossy(&cc.context);
                            let parts: Vec<&str> = context.split('|').collect();

                            if parts.len() == 3 {
                                let (kafka_addr, wal_addr, raft_addr) = (parts[0], parts[1], parts[2]);

                                // Get change details
                                if let Some(change) = cc.changes.first() {
                                    let node_id = change.get_node_id();
                                    let change_type = change.get_change_type();

                                    match change_type {
                                        ConfChangeType::AddNode => {
                                            tracing::info!(
                                                "ConfChange AddNode: node_id={}, kafka={}, wal={}, raft={}",
                                                node_id, kafka_addr, wal_addr, raft_addr
                                            );

                                            // Register new peer with gRPC transport (async)
                                            let transport_clone = self.transport.clone();
                                            let raft_addr_owned = raft_addr.to_string();
                                            tokio::spawn(async move {
                                                let grpc_url = if raft_addr_owned.starts_with("http") {
                                                    raft_addr_owned.clone()
                                                } else {
                                                    format!("http://{}", raft_addr_owned)
                                                };

                                                if let Err(e) = transport_clone.add_peer(node_id, grpc_url).await {
                                                    tracing::error!("Failed to register peer {} in transport: {}", node_id, e);
                                                } else {
                                                    tracing::info!("✅ Registered peer {} in gRPC transport (raft={})", node_id, raft_addr_owned);
                                                }
                                            });

                                            tracing::info!("✅ Node {} successfully added to cluster", node_id);
                                        }
                                        ConfChangeType::RemoveNode => {
                                            tracing::info!("ConfChange RemoveNode: node_id={}", node_id);
                                            // TODO Priority 4: Remove peer from transport
                                            tracing::info!("✅ Node {} successfully removed from cluster", node_id);
                                        }
                                        _ => {
                                            tracing::warn!("Unsupported ConfChange type: {:?}", change_type);
                                        }
                                    }
                                }
                            } else {
                                tracing::warn!("Invalid ConfChange context format: '{}'", context);
                            }
                        }
                    }
                }

                // Step 3b: Handle normal committed entries
                if !ready.committed_entries().is_empty() {
                    tracing::debug!(
                        "Processing {} committed entries",
                        ready.committed_entries().len()
                    );

                    if let Err(e) = self.apply_committed_entries(ready.committed_entries()) {
                        tracing::error!("Failed to apply committed entries: {}", e);
                    } else {
                        tracing::info!(
                            "✓ Applied {} committed entries to state machine",
                            ready.committed_entries().len()
                        );
                    }
                }

                // Step 4: Persist entries (required before persisted_messages)
                // CRITICAL: Run async operations WHILE HOLDING LOCK using block_in_place
                // This prevents other threads from calling step() between ready() and advance()
                if !ready.entries().is_empty() {
                    let entries = ready.entries();
                    tracing::info!("Persisting {} Raft entries to WAL", entries.len());

                    let entries_clone: Vec<_> = entries.iter().cloned().collect();
                    let storage_clone = self.storage.clone();

                    // Block in place to run async operation synchronously
                    // This keeps the lock held but allows async runtime to work
                    let persist_result = tokio::task::block_in_place(|| {
                        tokio::runtime::Handle::current().block_on(async {
                            storage_clone.append_entries(&entries_clone).await
                        })
                    });

                    if let Err(e) = persist_result {
                        tracing::error!("Failed to persist Raft entries: {}", e);
                    } else {
                        tracing::info!("✓ Persisted {} Raft entries to WAL", entries_clone.len());
                    }
                }

                // Step 5: Persist hard state (required before persisted_messages)
                // CRITICAL: Run async operations WHILE HOLDING LOCK using block_in_place
                if let Some(hs) = ready.hs() {
                    tracing::info!("Persisting HardState: term={}, vote={}, commit={}", hs.term, hs.vote, hs.commit);

                    let hs_clone = hs.clone();
                    let storage_clone = self.storage.clone();

                    // Block in place to run async operation synchronously
                    let persist_result = tokio::task::block_in_place(|| {
                        tokio::runtime::Handle::current().block_on(async {
                            storage_clone.persist_hard_state(&hs_clone).await
                        })
                    });

                    if let Err(e) = persist_result {
                        tracing::error!("Failed to persist HardState: {}", e);
                    } else {
                        tracing::info!("✓ Persisted HardState: term={}, vote={}, commit={}", hs_clone.term, hs_clone.vote, hs_clone.commit);
                    }
                }

                // Step 6: Send persisted messages (AFTER persistence)
                // CRITICAL: MUST call take_persisted_messages() to satisfy Raft's ownership requirements
                // Then immediately send to channel (non-blocking) - background task handles actual sends
                if !ready.persisted_messages().is_empty() {
                    let persisted_msgs = ready.take_persisted_messages();
                    tracing::info!("Raft ready: {} persisted messages to send", persisted_msgs.len());

                    for msg in persisted_msgs {
                        let peer_id = msg.to;
                        let msg_type = msg.msg_type;

                        tracing::debug!("Queueing persisted {:?} to peer {}", msg_type, peer_id);

                        // Send to channel - non-blocking, background task does gRPC
                        if let Err(e) = self.message_sender.send((peer_id, msg)) {
                            tracing::error!("Failed to queue persisted message to peer {}: {}", peer_id, e);
                        }
                    }
                }

                // Step 7: Advance Raft (marks Ready as processed)
                // CRITICAL: Use SAME raft_lock instance that created ready
                // Lock has been held continuously from ready() to here
                tracing::debug!("Advancing Raft (state={:?})", raft_lock.raft.state);

                raft_lock.advance(ready);

                tracing::debug!("✓ Advanced Raft successfully");

                // Step 8: Check if we should create a snapshot (Phase 5)
                // Trigger when applied >= last_index - 1000
                let applied = {
                    let raft_state = self.storage.initial_state().unwrap();
                    raft_state.hard_state.commit
                };

                let last_index = self.storage.last_index().unwrap();

                if applied > 0 && last_index > 1000 && applied >= last_index - 1000 {
                    tracing::info!(
                        "Snapshot trigger: applied={}, last_index={} (threshold: {})",
                        applied,
                        last_index,
                        last_index - 1000
                    );

                    // Clone needed data before async operations
                    let state_machine_clone = self.state_machine.clone();
                    let storage_clone = self.storage.clone();

                    // Create snapshot (async operations using block_in_place)
                    let snapshot_result = tokio::task::block_in_place(|| {
                        tokio::runtime::Handle::current().block_on(async {
                            // Serialize state machine
                            let state_machine = state_machine_clone.read().unwrap();
                            let state_machine_bytes = bincode::serialize(&*state_machine)
                                .context("Failed to serialize state machine")?;

                            // Get applied term (from last applied entry)
                            let applied_term = if applied > 0 {
                                storage_clone.term(applied).unwrap_or(0)
                            } else {
                                0
                            };

                            // Create snapshot
                            let snapshot = storage_clone
                                .create_snapshot(state_machine_bytes, applied, applied_term)
                                .await?;

                            // Save snapshot to disk
                            storage_clone.save_snapshot(&snapshot).await?;

                            // Truncate log entries covered by snapshot
                            storage_clone.truncate_log_to_snapshot(applied).await?;

                            Ok::<(), anyhow::Error>(())
                        })
                    });

                    if let Err(e) = snapshot_result {
                        tracing::error!("Failed to create/save snapshot: {}", e);
                    } else {
                        tracing::info!("✓ Created and saved snapshot at index={}", applied);
                    }
                }

                // Lock is released here at end of loop iteration
            }
        });

        tracing::info!("✓ Raft message loop started");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_raft_cluster_bootstrap() {
        // Bootstrap a 3-node cluster
        let peers = vec![
            (2, "localhost:9093".to_string()),
            (3, "localhost:9094".to_string()),
        ];

        let cluster = RaftCluster::bootstrap(1, peers, PathBuf::from("/tmp/raft-test")).await.unwrap();

        assert_eq!(cluster.node_id(), 1);
    }

    #[tokio::test]
    async fn test_metadata_queries() {
        let cluster = RaftCluster::bootstrap(1, vec![], PathBuf::from("/tmp/raft-test2")).await.unwrap();

        // Initially no partition assignments
        assert_eq!(cluster.get_partition_replicas("test", 0), None);
        assert_eq!(cluster.get_partition_leader("test", 0), None);

        // Propose partition assignment
        cluster.propose(MetadataCommand::AssignPartition {
            topic: "test".to_string(),
            partition: 0,
            replicas: vec![1, 2, 3],
        }).await.unwrap();

        // Note: In real usage, this would require Raft consensus and applying
        // committed entries. For this test, we just verify the API works.
    }

    #[tokio::test]
    #[cfg(not(feature = "raft"))]
    async fn test_raft_disabled() {
        let result = RaftCluster::bootstrap(1, vec![], PathBuf::from("/tmp/raft-test3")).await;
        assert!(result.is_err(), "RaftCluster should fail when Raft feature is not enabled");
        // Note: Can't check error message without Debug trait on RaftCluster
    }
}

/// Configuration for running a Raft cluster node
pub struct RaftClusterConfig {
    pub node_id: u64,
    pub raft_addr: String,
    pub peers: Vec<(u64, String)>,
    pub bootstrap: bool,
    pub kafka_port: u16,
    pub advertised_addr: String,
    pub data_dir: String,
}

/// Run a Raft cluster node (CLI entry point)
///
/// This function bootstraps a Raft cluster and starts the integrated Kafka server
/// with Raft metadata coordination.
pub async fn run_raft_cluster(config: RaftClusterConfig) -> Result<()> {
    use crate::integrated_server::{IntegratedKafkaServer, IntegratedServerConfig};
    use std::net::SocketAddr;
    
    tracing::info!(
        "Starting Raft cluster node: id={}, kafka_port={}, raft_addr={}",
        config.node_id,
        config.kafka_port,
        config.raft_addr
    );

    // Step 1: Clone peers before bootstrap (needed for WAL replication config later)
    let peers_for_replication = config.peers.clone();

    // Bootstrap RaftCluster for metadata coordination
    let raft_cluster = Arc::new(RaftCluster::bootstrap(
        config.node_id,
        config.peers,
        PathBuf::from(&config.data_dir)
    ).await?);

    tracing::info!("Raft cluster bootstrapped successfully");

    // Step 1.5: Start Raft message processing loop (v2.5.0 Phase 5 fix)
    // CRITICAL: Without this, Raft proposals never get committed!
    raft_cluster.clone().start_message_loop();
    tracing::info!("✓ Raft message processing loop started");

    // Step 1.6: Start Raft gRPC server (v2.5.0 - production gRPC transport)
    // CRITICAL: Without this, nodes can't communicate and leader election fails!
    raft_cluster.clone().start_grpc_server(config.raft_addr.clone()).await?;
    tracing::info!("✓ Raft gRPC server started on {}", config.raft_addr);

    // Step 1.7: Auto-configure WAL replication for Raft cluster (Phase 4 fix)
    // CRITICAL: Without this, acks=-1 hangs because no follower ACKs are sent!
    // Convert Raft peer addresses to WAL replication addresses
    // Port mapping: raft_port → kafka_port (raft - 100) → wal_port (kafka + 10)
    let wal_followers: Vec<String> = peers_for_replication.iter()
        .filter_map(|(peer_id, raft_addr)| {
            // Parse Raft address to extract host and port
            // Format: "host:raft_port" where kafka_port = raft_port - 100, wal_port = kafka_port + 10
            if let Some(colon_pos) = raft_addr.rfind(':') {
                let host = &raft_addr[..colon_pos];
                if let Ok(raft_port) = raft_addr[colon_pos+1..].parse::<u16>() {
                    let kafka_port = raft_port - 100;  // Kafka port = Raft port - 100
                    let wal_port = kafka_port + 10;    // WAL replication port = Kafka port + 10
                    let wal_addr = format!("{}:{}", host, wal_port);
                    tracing::info!("Mapped Raft peer {} (Raft={}:{}) to WAL follower: {} (Kafka={}, WAL={})",
                        peer_id, host, raft_port, wal_addr, kafka_port, wal_port);
                    return Some(wal_addr);
                }
            }
            tracing::warn!("Failed to parse Raft peer address: {}", raft_addr);
            None
        })
        .collect();

    // Set environment variable for WAL replication (leader → followers)
    if !wal_followers.is_empty() {
        let followers_str = wal_followers.join(",");
        tracing::info!("Auto-configuring WAL replication with {} followers: {}",
            wal_followers.len(), followers_str);
        std::env::set_var("CHRONIK_REPLICATION_FOLLOWERS", followers_str);
    } else {
        tracing::warn!("No WAL followers derived from Raft peers - WAL replication disabled");
    }

    // Step 1.8: Enable WalReceiver on THIS node (so it can receive replication as a follower)
    // CRITICAL: Without this, followers can't receive data and send ACKs back!
    // Use a separate port for WAL replication (Kafka port + 10) to avoid conflicts
    let wal_replication_port = config.kafka_port + 10;
    let this_wal_receiver_addr = format!("{}:{}", config.advertised_addr, wal_replication_port);
    tracing::info!("Enabling WAL receiver on this node: {} (WAL replication port)", this_wal_receiver_addr);
    std::env::set_var("CHRONIK_WAL_RECEIVER_ADDR", &this_wal_receiver_addr);

    // Step 2: Create IntegratedKafkaServer configuration
    let server_config = IntegratedServerConfig {
        node_id: config.node_id as i32,
        advertised_host: config.advertised_addr.clone(),
        advertised_port: config.kafka_port as i32,
        data_dir: config.data_dir,
        enable_indexing: false,
        enable_compression: true,
        auto_create_topics: true,
        num_partitions: 1,
        replication_factor: 3,  // CRITICAL: Raft cluster needs replication_factor=3 for ISR quorum!
        enable_wal_indexing: false,
        wal_indexing_interval_secs: 300,
        object_store_config: None,
        enable_metadata_dr: false,
        metadata_upload_interval_secs: 60,
        cluster_config: None,  // TODO(Phase 3): Wire RaftCluster to cluster_config
    };

    // Step 3: Create and start Kafka server with Raft cluster
    let server = IntegratedKafkaServer::new(server_config, Some(raft_cluster)).await?;

    // Bind Kafka server
    let kafka_addr = format!("0.0.0.0:{}", config.kafka_port);

    tracing::info!("Starting Kafka server on {}", kafka_addr);

    // Step 4: Run Kafka server (blocks until shutdown)
    // Note: Raft message loop is already running in background from start_message_loop()
    server.run(&kafka_addr).await?;

    Ok(())
}
