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
use tokio::sync::Mutex;

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

        // Initialize with voter configuration
        wal_storage.set_raft_state(RaftState {
            hard_state: HardState::default(),
            conf_state: ConfState {
                voters: all_nodes.clone(),
                learners: vec![],
                ..Default::default()
            },
        });

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

        Ok(Self {
            node_id,
            state_machine,
            raft_node: Arc::new(RwLock::new(raft_node)),
            storage: Arc::new(storage_for_async),
            transport,
            grpc_server_handle: Arc::new(Mutex::new(None)),
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

    /// Apply committed Raft entries to the state machine
    ///
    /// This should be called by the Raft message processing loop when
    /// entries are committed.
    pub fn apply_committed_entries(&self, entries: &[Entry]) -> Result<()> {
        let mut sm = self.state_machine.write()
            .map_err(|e| anyhow::anyhow!("Failed to acquire state machine lock: {}", e))?;

        for entry in entries {
            // Skip empty entries (configuration changes)
            if entry.data.is_empty() {
                continue;
            }

            // Deserialize command
            let cmd: MetadataCommand = bincode::deserialize(&entry.data)
                .context("Failed to deserialize metadata command")?;

            // Apply to state machine
            sm.apply(cmd)?;
        }

        Ok(())
    }

    /// Get this node's ID
    pub fn node_id(&self) -> u64 {
        self.node_id
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
                let mut ready = {
                    let mut raft = match self.raft_node.write() {
                        Ok(r) => r,
                        Err(e) => {
                            tracing::error!("Failed to acquire Raft lock for ready: {}", e);
                            continue;
                        }
                    };

                    if !raft.has_ready() {
                        continue;
                    }

                    raft.ready()
                };

                // CRITICAL: Follow proper Raft Ready handling order
                // See: https://github.com/tikv/raft-rs/blob/master/examples/single_mem_node/main.rs

                // Step 1: Send unpersisted messages first
                if !ready.messages().is_empty() {
                    let messages = ready.take_messages();
                    tracing::info!("Raft ready: {} unpersisted messages to send", messages.len());

                    for msg in messages {
                        let peer_id = msg.to;
                        let msg_type = msg.msg_type;
                        let self_clone = self.clone();

                        tracing::info!("Sending unpersisted {:?} to peer {}", msg_type, peer_id);

                        tokio::spawn(async move {
                            match self_clone.send_raft_message(peer_id, msg).await {
                                Ok(_) => {
                                    tracing::info!("Successfully sent {:?} to peer {}", msg_type, peer_id);
                                }
                                Err(e) => {
                                    tracing::warn!("Failed to send {:?} to peer {}: {}", msg_type, peer_id, e);
                                }
                            }
                        });
                    }
                }

                // Step 2: Apply snapshot if present
                if !raft::is_empty_snap(ready.snapshot()) {
                    // TODO: Persist snapshot
                    tracing::debug!("Snapshot available (not persisted yet)");
                }

                // Step 3: Handle committed entries
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
                // CRITICAL FIX: Must AWAIT persistence before calling advance()
                // Spawning in background violates Raft safety - we MUST wait for disk sync
                if !ready.entries().is_empty() {
                    let entries = ready.entries();
                    tracing::info!("Persisting {} Raft entries to WAL", entries.len());

                    let entries_clone: Vec<_> = entries.iter().cloned().collect();

                    // AWAIT the persistence - don't spawn in background!
                    if let Err(e) = self.storage.append_entries(&entries_clone).await {
                        tracing::error!("Failed to persist Raft entries: {}", e);
                        // Continue anyway - Raft will recover on next restart
                    } else {
                        tracing::info!("✓ Persisted {} Raft entries to WAL", entries_clone.len());
                    }
                }

                // Step 5: Persist hard state (required before persisted_messages)
                // CRITICAL FIX: Must AWAIT persistence before calling advance()
                if let Some(hs) = ready.hs() {
                    tracing::info!("Persisting HardState: term={}, vote={}, commit={}", hs.term, hs.vote, hs.commit);

                    let hs_clone = hs.clone();

                    // AWAIT the persistence - don't spawn in background!
                    if let Err(e) = self.storage.persist_hard_state(&hs_clone).await {
                        tracing::error!("Failed to persist HardState: {}", e);
                        // Continue anyway - Raft will recover on next restart
                    } else {
                        tracing::info!("✓ Persisted HardState: term={}, vote={}, commit={}", hs_clone.term, hs_clone.vote, hs_clone.commit);
                    }
                }

                // Step 6: Send persisted messages (AFTER persistence)
                if !ready.persisted_messages().is_empty() {
                    let persisted_msgs = ready.take_persisted_messages();
                    tracing::info!("Raft ready: {} persisted messages to send", persisted_msgs.len());

                    for msg in persisted_msgs {
                        let peer_id = msg.to;
                        let msg_type = msg.msg_type;
                        let self_clone = self.clone();

                        tracing::info!("Sending persisted {:?} to peer {}", msg_type, peer_id);

                        tokio::spawn(async move {
                            match self_clone.send_raft_message(peer_id, msg).await {
                                Ok(_) => {
                                    tracing::info!("Successfully sent persisted {:?} to peer {}", msg_type, peer_id);
                                }
                                Err(e) => {
                                    tracing::warn!("Failed to send persisted {:?} to peer {}: {}", msg_type, peer_id, e);
                                }
                            }
                        });
                    }
                }

                // Step 7: Advance Raft (marks Ready as processed)
                {
                    let mut raft = match self.raft_node.write() {
                        Ok(r) => r,
                        Err(e) => {
                            tracing::error!("Failed to acquire Raft lock for advance: {}", e);
                            continue;
                        }
                    };
                    raft.advance(ready);
                }
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
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Raft feature is not enabled"));
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

    // Step 1: Bootstrap RaftCluster for metadata coordination
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
        replication_factor: 1,
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
