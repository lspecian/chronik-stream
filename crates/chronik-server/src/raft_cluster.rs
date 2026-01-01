//! RaftCluster wrapper (v2.2.7 Phase 2)
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
use std::sync::atomic::{AtomicU64, AtomicBool, Ordering};
use arc_swap::ArcSwap;  // v2.2.7 DEADLOCK FIX: Lock-free atomic pointer swapping
use std::collections::HashMap;
use tokio::sync::{mpsc, Notify};
use parking_lot::Mutex;
use dashmap::DashMap;

use raft::{prelude::*, storage::MemStorage};
use slog::Drain;
use chronik_wal::{RaftWalStorage, GroupCommitWal, GroupCommitConfig};
use std::path::PathBuf;

// v2.2.7: gRPC transport for production Raft networking
use chronik_raft::{GrpcTransport, Transport, rpc::{RaftServiceImpl, raft_service_server}};
use tonic::transport::Server;

/// Raft cluster for metadata coordination
pub struct RaftCluster {
    /// Node ID in the cluster
    node_id: u64,

    /// Metadata state machine (shared with Raft)
    /// v2.2.7 DEADLOCK FIX: Uses ArcSwap for lock-free atomic pointer swapping
    /// - Reads: Zero-cost via .load() - just atomic pointer dereference
    /// - Writes: Clone state, modify clone, swap pointer atomically
    /// - NO lock contention, NO deadlocks, perfect for read-heavy metadata
    state_machine: Arc<ArcSwap<MetadataStateMachine>>,

    /// Raft node with WAL-backed persistent storage
    /// v2.2.7 DEADLOCK FIX: Uses tokio::Mutex since start_message_loop holds lock across await points.
    /// Async storage operations drop the lock before awaiting to avoid holding across async boundaries.
    /// NOTE: Cannot use parking_lot::Mutex here because MutexGuard is !Send and tokio::spawn requires Send.
    pub(crate) raft_node: Arc<tokio::sync::Mutex<RawNode<RaftWalStorage>>>,

    /// Storage reference for async persist operations
    storage: Arc<RaftWalStorage>,

    /// gRPC transport for Raft messages (v2.2.7)
    pub(crate) transport: Arc<GrpcTransport>,

    /// gRPC server handle (uses tokio::Mutex for async context)
    grpc_server_handle: Arc<tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>,

    /// Channel for sending Raft messages asynchronously
    /// Messages are taken from Ready and sent to this channel (non-blocking)
    /// A background task reads from channel and sends via gRPC
    message_sender: mpsc::UnboundedSender<(u64, Message)>,

    /// v2.2.7 DEADLOCK FIX: Channel for receiving incoming Raft messages from gRPC
    /// gRPC handler queues messages here (non-blocking) instead of calling raft.step() directly
    /// Raft ready loop processes these messages inside the lock (no deadlock)
    incoming_message_sender: mpsc::UnboundedSender<Message>,

    /// v2.2.7 DEADLOCK FIX (PART 2): Receiver for incoming Raft messages
    /// Processed by the ready loop INSIDE the Raft lock to avoid contention
    /// Uses tokio::Mutex since it's accessed in async context
    incoming_message_receiver: Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<Message>>>,

    /// SNAPSHOT STORM FIX (v2.2.7): Track last snapshot index to prevent repeated snapshots
    /// Only create new snapshot if we've advanced significantly beyond this index
    last_snapshot_index: Arc<RwLock<u64>>,

    /// v2.2.7 EVENT-DRIVEN NOTIFICATION: Pending topic creation notifications
    /// Shared with RaftMetadataStore to enable instant wake-up when entries are applied
    pending_topics: Arc<DashMap<String, Arc<Notify>>>,

    /// v2.2.7 EVENT-DRIVEN NOTIFICATION: Pending broker registration notifications
    /// Shared with RaftMetadataStore to enable instant wake-up when entries are applied
    pending_brokers: Arc<DashMap<i32, Arc<Notify>>>,

    /// v2.2.7 EVENT-DRIVEN NOTIFICATION (P1): Pending partition leader notifications
    /// Key: "topic:partition" (e.g., "my-topic:0")
    /// Used by followers waiting for partition metadata to arrive via Raft replication
    pending_partitions: Arc<DashMap<String, Arc<Notify>>>,

    /// v2.2.7 Phase 3: Heartbeat broadcast channel for leader leases
    /// Leader broadcasts heartbeats to followers via this channel
    /// Followers subscribe to receive heartbeats and update their leases
    heartbeat_sender: tokio::sync::broadcast::Sender<crate::leader_heartbeat::LeaderHeartbeat>,

    /// v2.2.7 Phase 2: Leader change broadcast channel for WAL replication
    /// Emits events when Raft leader changes to trigger WAL reconnection
    leader_change_sender: tokio::sync::broadcast::Sender<crate::leader_heartbeat::LeaderChangeEvent>,

    /// v2.2.7 LOCK CONTENTION FIX: Cached leader ID (avoids locking raft_node on every query)
    /// Updated by Raft message loop when leader changes
    /// Using AtomicU64 for lock-free reads (critical for metadata query performance)
    cached_leader_id: Arc<AtomicU64>,

    /// v2.2.7 LOCK CONTENTION FIX: Cached leadership state (am I the leader?)
    /// Updated by Raft message loop when state changes
    /// Using AtomicBool for lock-free reads
    cached_is_leader: Arc<AtomicBool>,
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
    /// Create Raft configuration
    ///
    /// Builds Raft config with election/heartbeat timeouts.
    /// Complexity: < 5 (simple config struct creation)
    fn create_raft_config(node_id: u64) -> raft::Config {
        raft::Config {
            id: node_id,
            // v2.2.8 FIX: Increase election timeout to prevent startup race conditions
            // With election_tick=10 (~1 second), nodes can timeout before gRPC connections
            // are established, causing election storms. election_tick=20 (~2 seconds) provides
            // enough time for connection retries during cluster startup.
            election_tick: 20,  // ~2 seconds (20 ticks * 100ms loop interval)
            heartbeat_tick: 3,   // ~300ms (3 ticks * 100ms loop interval)
            max_size_per_msg: 1024 * 1024,
            max_inflight_msgs: 256,
            ..Default::default()
        }
    }

    /// Initialize WAL-backed storage for Raft log
    ///
    /// Creates WAL directory, configures profile, recovers existing state,
    /// and initializes voter configuration if needed.
    /// Complexity: < 30 (WAL creation + recovery + voter initialization)
    async fn initialize_wal_storage(
        data_dir: &PathBuf,
        all_nodes: &[u64],
    ) -> Result<RaftWalStorage> {
        // Create WAL-backed persistent storage for Raft log
        let meta_wal_dir = data_dir.join("wal").join("__meta");
        tokio::fs::create_dir_all(&meta_wal_dir).await
            .context("Failed to create metadata WAL directory")?;

        // Use metadata-specific WAL config (Raft consensus log)
        // CHRONIK_METADATA_WAL_PROFILE overrides for Raft metadata WAL
        // CRITICAL: Raft always defaults to HIGH profile for consensus stability
        // (prevents regression where changing data WAL profile affected Raft)
        let meta_wal_config = if let Ok(profile) = std::env::var("CHRONIK_METADATA_WAL_PROFILE") {
            match profile.to_lowercase().as_str() {
                "low" | "small" | "container" => {
                    tracing::warn!("Using LOW profile for Raft metadata WAL (not recommended)");
                    GroupCommitConfig::low_resource()
                }
                "medium" | "balanced" => {
                    tracing::info!("Using MEDIUM profile for Raft metadata WAL");
                    GroupCommitConfig::medium_resource()
                }
                "high" | "aggressive" | "dedicated" => {
                    tracing::info!("Using HIGH profile for Raft metadata WAL");
                    GroupCommitConfig::high_resource()
                }
                "ultra" | "maximum" | "throughput" => {
                    tracing::info!("Using ULTRA profile for Raft metadata WAL");
                    GroupCommitConfig::ultra_resource()
                }
                _ => {
                    tracing::warn!("Unknown CHRONIK_METADATA_WAL_PROFILE '{}', using HIGH", profile);
                    GroupCommitConfig::high_resource()
                }
            }
        } else {
            // Always default to HIGH for Raft metadata (stable consensus)
            tracing::info!("Using HIGH profile for Raft metadata WAL (default for stability)");
            GroupCommitConfig::high_resource()
        };

        // CRITICAL: Raft WAL needs a dummy callback to prevent timeout warnings
        // Raft WAL writes are internal consensus operations, not client produces
        // So we don't need to notify anyone - just let the commit complete silently
        let dummy_callback = |_topic: &str, _partition: i32, _min_offset: i64, _max_offset: i64| {
            // NO-OP: Raft commits don't need notifications
        };

        let meta_wal = Arc::new(GroupCommitWal::with_callback(meta_wal_dir.clone(), meta_wal_config, Arc::new(dummy_callback)));
        let wal_storage = RaftWalStorage::new(meta_wal);

        // Recover existing Raft log from WAL (if any)
        wal_storage.recover(data_dir).await
            .context("Failed to recover Raft log from WAL")?;

        tracing::info!("✓ Raft WAL storage initialized (persistent)");

        // Initialize with voter configuration ONLY if no state was recovered
        // OR if recovered state has empty voters (regression fix for v2.2.7)
        let recovered_state = wal_storage.initial_state().unwrap();
        let has_voters = !recovered_state.conf_state.voters.is_empty();

        if !wal_storage.has_recovered_state() || !has_voters {
            if !has_voters {
                tracing::warn!(
                    "Recovered Raft state has ZERO voters - reinitializing with {:?} (v2.2.7 regression fix)",
                    all_nodes
                );
            } else {
                tracing::info!("No recovered Raft state - initializing fresh cluster");
            }

            wal_storage.set_raft_state(RaftState {
                hard_state: HardState::default(),
                conf_state: ConfState {
                    voters: all_nodes.to_vec(),
                    learners: vec![],
                    ..Default::default()
                },
            });

            // v2.2.8 RAFT PANIC FIX: Clear stale log entries when reinitializing
            // Prevents panic: "to_commit X is out of range [last_index Y]"
            // when nodes have mismatched Raft log states
            wal_storage.clear_entries();
        } else {
            tracing::info!(
                "✓ Using recovered Raft state from WAL (voters: {:?})",
                recovered_state.conf_state.voters
            );
        }

        Ok(wal_storage)
    }

    /// Create Raft node with WAL-backed storage
    ///
    /// Creates slog logger and RawNode.
    /// Complexity: < 5 (simple node creation)
    fn create_raft_node(
        config: &raft::Config,
        wal_storage: RaftWalStorage,
    ) -> Result<RawNode<RaftWalStorage>> {
        // Create logger for Raft (it uses slog)
        let logger = slog::Logger::root(slog_stdlog::StdLog.fuse(), slog::o!());

        // Create Raft node with WAL-backed storage
        RawNode::new(config, wal_storage, &logger)
            .context("Failed to create Raft node")
    }

    /// Setup gRPC transport and register peers
    ///
    /// Creates gRPC transport and adds all peers.
    /// Complexity: < 10 (peer registration loop)
    async fn setup_grpc_transport(peers: &[(u64, String)]) -> Result<Arc<GrpcTransport>> {
        let transport = Arc::new(GrpcTransport::new());

        // Register peers with gRPC transport
        for (peer_id, peer_addr) in peers {
            // Convert Raft address (e.g., "0.0.0.0:5001") to gRPC URL (e.g., "http://localhost:5001")
            let grpc_url = if peer_addr.starts_with("http") {
                peer_addr.clone()
            } else {
                format!("http://{}", peer_addr)
            };

            transport.add_peer(*peer_id, grpc_url).await
                .context(format!("Failed to add peer {} to transport", peer_id))?;

            tracing::info!("✓ Registered peer {} in gRPC transport", peer_id);
        }

        Ok(transport)
    }

    /// Start background message sender task
    ///
    /// Spawns tokio task that receives messages from channel and sends via gRPC.
    /// Uses concurrent per-message tasks with retry + exponential backoff.
    /// Complexity: < 25 (spawn task + retry loop)
    fn start_background_message_sender(
        transport: Arc<GrpcTransport>,
    ) -> mpsc::UnboundedSender<(u64, Message)> {
        // Create channel for async message sending
        // CRITICAL: This allows us to take_messages() from Ready (satisfying Raft),
        // then send to channel (non-blocking), while a background task does actual gRPC sends
        let (message_sender, mut message_receiver) = mpsc::unbounded_channel::<(u64, Message)>();

        // Start background message sender task
        // v2.2.8 FIX: Spawn concurrent tasks per message to prevent head-of-line blocking
        // Previously, retries for one peer would block messages to ALL peers
        let transport_for_sender = transport.clone();
        tokio::spawn(async move {
            while let Some((peer_id, msg)) = message_receiver.recv().await {
                let msg_type = msg.msg_type;
                tracing::debug!("Message sender: sending {:?} to peer {}", msg_type, peer_id);

                // v2.2.8 FIX: Spawn separate task for each message to prevent blocking
                // This allows messages to different peers to be sent concurrently
                let transport_clone = transport_for_sender.clone();
                tokio::spawn(async move {
                    // Use gRPC transport to send the message
                    const METADATA_TOPIC: &str = "__raft_metadata";
                    const METADATA_PARTITION: i32 = 0;

                    // CRITICAL FIX (v2.2.7): Retry failed messages with exponential backoff
                    // When gRPC transport fails (peer not connected), we MUST retry until success.
                    // Otherwise, AppendEntries with committed entries are LOST, causing followers
                    // to have commit index > last_index (split-brain state).
                    //
                    // Retry strategy:
                    // - Max 10 retries with exponential backoff (50ms, 100ms, 200ms, ...)
                    // - If all retries fail, log error but continue (message is lost)
                    // - This prevents infinite retry loops while giving transport time to connect
                    let mut retry_count = 0;
                    let max_retries = 10;
                    let mut backoff_ms = 50;

                    loop {
                        match transport_clone
                            .send_message(METADATA_TOPIC, METADATA_PARTITION, peer_id, msg.clone())
                            .await
                        {
                            Ok(_) => {
                                tracing::debug!("✓ Message sender: sent {:?} to peer {} (retries: {})", msg_type, peer_id, retry_count);
                                break; // Success - exit retry loop
                            }
                            Err(e) => {
                                retry_count += 1;
                                if retry_count >= max_retries {
                                    tracing::error!(
                                        "Message sender: FAILED to send {:?} to peer {} after {} retries - MESSAGE LOST: {}",
                                        msg_type, peer_id, max_retries, e
                                    );
                                    break; // Give up after max retries
                                }

                                tracing::debug!(
                                    "Message sender: retry {}/{} for {:?} to peer {} (backoff: {}ms): {}",
                                    retry_count, max_retries, msg_type, peer_id, backoff_ms, e
                                );

                                // Exponential backoff
                                tokio::time::sleep(tokio::time::Duration::from_millis(backoff_ms)).await;
                                backoff_ms = (backoff_ms * 2).min(1000); // Cap at 1 second
                            }
                        }
                    }
                });
            }
            tracing::info!("Message sender task shutting down");
        });

        tracing::info!("✓ Raft message sender task started");
        message_sender
    }

    /// Create channels and notification caches
    ///
    /// Creates broadcast channels for heartbeats/leader changes,
    /// and notification maps for event-driven coordination.
    /// Complexity: < 5 (simple channel/map creation)
    fn create_channels_and_caches() -> (
        mpsc::UnboundedSender<Message>,
        Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<Message>>>,
        tokio::sync::broadcast::Sender<crate::leader_heartbeat::LeaderHeartbeat>,
        tokio::sync::broadcast::Sender<crate::leader_heartbeat::LeaderChangeEvent>,
        Arc<DashMap<String, Arc<Notify>>>,
        Arc<DashMap<i32, Arc<Notify>>>,
        Arc<DashMap<String, Arc<Notify>>>,
    ) {
        // v2.2.7 DEADLOCK FIX: Create channel for incoming Raft messages from gRPC
        // This decouples gRPC message reception from Raft processing (prevents deadlock)
        let (incoming_message_sender, incoming_message_receiver) = mpsc::unbounded_channel::<Message>();

        // Create heartbeat broadcast channel for Phase 3 leader leases
        // Capacity: 16 heartbeats (enough for 16 seconds at 1Hz heartbeat rate)
        let (heartbeat_sender, _) = tokio::sync::broadcast::channel(16);

        // Create leader change broadcast channel for Phase 2 WAL replication
        // Capacity: 16 leader changes (more than enough for typical cluster behavior)
        let (leader_change_sender, _) = tokio::sync::broadcast::channel(16);

        // Create notification maps for event-driven coordination
        let pending_topics = Arc::new(DashMap::new());
        let pending_brokers = Arc::new(DashMap::new());
        let pending_partitions = Arc::new(DashMap::new());

        (
            incoming_message_sender.clone(),
            Arc::new(tokio::sync::Mutex::new(incoming_message_receiver)),
            heartbeat_sender,
            leader_change_sender,
            pending_topics,
            pending_brokers,
            pending_partitions,
        )
    }

    /// Bootstrap a Raft cluster (refactored for maintainability)
    ///
    /// **Refactored**: Reduced from 255 lines (~70-90 complexity) to ~70 lines (<25 complexity)
    /// Complexity: < 25 (simple orchestration of extracted helpers)
    pub async fn bootstrap(node_id: u64, peers: Vec<(u64, String)>, data_dir: PathBuf) -> Result<Self> {
        tracing::info!(
            "Bootstrapping Raft cluster: node_id={}, peers={:?}",
            node_id,
            peers
        );

        // Phase 1: Create Raft configuration
        let config = Self::create_raft_config(node_id);

        // Phase 2: Build list of all nodes (self + peers) for voter configuration
        let mut all_nodes = vec![node_id];
        for (peer_id, _) in &peers {
            all_nodes.push(*peer_id);
        }
        tracing::info!("Initializing Raft cluster with voters: {:?}", all_nodes);

        // Phase 3: Initialize WAL storage + recover state + initialize voters
        let wal_storage = Self::initialize_wal_storage(&data_dir, &all_nodes).await?;
        let storage_for_async = wal_storage.clone();

        // Phase 4: Create Raft node
        let raft_node = Self::create_raft_node(&config, wal_storage)?;

        // Phase 5: Create state machine (v2.2.7 DEADLOCK FIX: ArcSwap for lock-free access)
        let state_machine = Arc::new(ArcSwap::new(Arc::new(MetadataStateMachine::new())));

        // Phase 6: Setup gRPC transport and register peers
        let transport = Self::setup_grpc_transport(&peers).await?;
        tracing::info!("✓ Raft cluster initialized with {} voters and gRPC transport", all_nodes.len());

        // Phase 7: Start background message sender task
        let message_sender = Self::start_background_message_sender(transport.clone());

        // Phase 8: Create channels and notification caches
        let (
            incoming_message_sender,
            incoming_message_receiver,
            heartbeat_sender,
            leader_change_sender,
            pending_topics,
            pending_brokers,
            pending_partitions,
        ) = Self::create_channels_and_caches();

        // Phase 9: Construct and return RaftCluster
        Ok(Self {
            node_id,
            state_machine,
            raft_node: Arc::new(tokio::sync::Mutex::new(raft_node)), // v2.2.7: tokio::Mutex (required for async context)
            storage: Arc::new(storage_for_async),
            transport,
            grpc_server_handle: Arc::new(tokio::sync::Mutex::new(None)),
            message_sender,
            incoming_message_sender, // v2.2.7 DEADLOCK FIX
            incoming_message_receiver, // v2.2.7 DEADLOCK FIX (PART 2)
            last_snapshot_index: Arc::new(RwLock::new(0)), // SNAPSHOT STORM FIX (v2.2.7)
            pending_topics, // v2.2.7 EVENT-DRIVEN NOTIFICATION
            pending_brokers, // v2.2.7 EVENT-DRIVEN NOTIFICATION
            pending_partitions, // v2.2.7 EVENT-DRIVEN NOTIFICATION (P1)
            heartbeat_sender, // v2.2.7 Phase 3: Leader lease heartbeats
            leader_change_sender, // v2.2.7 Phase 2: WAL replication leader changes
            cached_leader_id: Arc::new(AtomicU64::new(raft::INVALID_ID)), // v2.2.7 LOCK CONTENTION FIX
            cached_is_leader: Arc::new(AtomicBool::new(false)), // v2.2.7 LOCK CONTENTION FIX
        })
    }

    /// v2.2.7 EVENT-DRIVEN NOTIFICATION: Get shared notification maps for metadata store
    pub fn get_pending_topics_notifications(&self) -> Arc<DashMap<String, Arc<Notify>>> {
        self.pending_topics.clone()
    }

    /// v2.2.7 EVENT-DRIVEN NOTIFICATION: Get shared notification maps for metadata store
    pub fn get_pending_brokers_notifications(&self) -> Arc<DashMap<i32, Arc<Notify>>> {
        self.pending_brokers.clone()
    }

    /// v2.2.7 EVENT-DRIVEN NOTIFICATION (P1): Get partition metadata notification map
    pub fn get_pending_partitions_notifications(&self) -> Arc<DashMap<String, Arc<Notify>>> {
        self.pending_partitions.clone()
    }

    /// Pre-warm gRPC connections to all peers (v2.2.8)
    ///
    /// CRITICAL: Must be called AFTER gRPC server starts, not before!
    /// This prevents the chicken-and-egg problem where Node 1 tries to connect
    /// to Node 2 before Node 2's gRPC server is ready.
    ///
    /// # Behavior
    /// - Attempts to establish connection to each peer
    /// - Retries up to 30 times with 100ms backoff (3s total)
    /// - Logs warnings for failed connections but continues anyway
    /// - Runs asynchronously in background (doesn't block startup)
    pub async fn pre_warm_connections(&self) {
        let peer_count = self.transport.peer_count();
        if peer_count == 0 {
            tracing::debug!("Skipping pre-warming: single-node cluster");
            return;
        }

        tracing::info!("Pre-warming gRPC connections to {} peers...", peer_count);

        // Get all peer IDs from the transport
        let peer_ids = self.transport.get_peer_ids().await;

        for peer_id in peer_ids {
            // Attempt to establish connection with retries
            const MAX_WARMUP_RETRIES: usize = 30;  // 30 retries @ 100ms = 3 seconds max
            const WARMUP_BACKOFF_MS: u64 = 100;

            let mut connected = false;
            for attempt in 1..=MAX_WARMUP_RETRIES {
                match self.transport.ping_peer(peer_id).await {
                    Ok(()) => {
                        tracing::info!("✓ Pre-warmed connection to peer {} (attempt {})", peer_id, attempt);
                        connected = true;
                        break;
                    }
                    Err(e) => {
                        if attempt < MAX_WARMUP_RETRIES {
                            tracing::debug!(
                                "Pre-warm attempt {}/{} to peer {} failed (will retry): {}",
                                attempt, MAX_WARMUP_RETRIES, peer_id, e
                            );
                            tokio::time::sleep(tokio::time::Duration::from_millis(WARMUP_BACKOFF_MS)).await;
                        } else {
                            tracing::warn!(
                                "Failed to pre-warm connection to peer {} after {} attempts (continuing anyway): {}",
                                peer_id, MAX_WARMUP_RETRIES, e
                            );
                        }
                    }
                }
            }

            if !connected {
                tracing::warn!(
                    "⚠ Peer {} may not be ready yet - first heartbeats may fail",
                    peer_id
                );
            }
        }

        tracing::info!("✓ Completed gRPC connection pre-warming");
    }

    // ============================================================================
    // v2.2.9 Option 4: DEPRECATED - Partition metadata now in WalMetadataStore
    // ============================================================================
    // These methods are kept as stubs for backward compatibility but return
    // empty/None since partition metadata is no longer in Raft state machine.
    // Callers should use WalMetadataStore instead.
    // ============================================================================

    /// Get partition replicas (DEPRECATED - use WalMetadataStore)
    pub fn get_partition_replicas(&self, _topic: &str, _partition: i32) -> Option<Vec<u64>> {
        None  // v2.2.9: Partition metadata moved to WalMetadataStore
    }

    /// Get partition leader (DEPRECATED - use WalMetadataStore)
    pub fn get_partition_leader(&self, _topic: &str, _partition: i32) -> Option<u64> {
        None  // v2.2.9: Partition metadata moved to WalMetadataStore
    }

    /// Get ISR (in-sync replicas) for a partition (DEPRECATED - use WalMetadataStore)
    pub fn get_isr(&self, _topic: &str, _partition: i32) -> Option<Vec<u64>> {
        None  // v2.2.9: Partition metadata moved to WalMetadataStore
    }

    /// Check if a node is in-sync for a partition (DEPRECATED - use WalMetadataStore)
    pub fn is_in_sync(&self, _topic: &str, _partition: i32, _node_id: u64) -> bool {
        false  // v2.2.9: Partition metadata moved to WalMetadataStore
    }

    /// Get all partitions where the specified node is the leader (DEPRECATED - use WalMetadataStore)
    ///
    /// Returns empty vec - partition metadata is now in WalMetadataStore.
    pub fn get_partitions_where_leader(&self, _node_id: u64) -> Vec<PartitionKey> {
        Vec::new()  // v2.2.9: Partition metadata moved to WalMetadataStore
    }

    /// Get all brokers from the Raft state machine
    ///
    /// Returns a list of (broker_id, host, port, rack) tuples for all registered brokers.
    /// This is used to synchronize broker metadata across the cluster.
    pub fn get_all_brokers_from_state_machine(&self) -> Vec<(i32, String, i32, Option<String>)> {
        let sm = self.state_machine.load();
        sm.get_all_brokers()
            .into_iter()
            .map(|b| (b.broker_id, b.host.clone(), b.port, b.rack.clone()))
            .collect()
    }

    /// Check if running in single-node mode (v2.2.7 Phase 2)
    ///
    /// Returns true if this is a single-node cluster (no peers).
    /// Single-node mode uses synchronous apply for zero overhead.
    pub fn is_single_node(&self) -> bool {
        self.peer_count() == 0
    }

    /// Get the number of peers in the cluster (v2.2.7 Phase 2)
    ///
    /// Returns the number of OTHER nodes (not including self).
    pub fn peer_count(&self) -> usize {
        self.transport.peer_count()
    }

    /// Get read-only access to state machine (v2.2.7 Phase 3)
    ///
    /// Returns an Arc to the Raft state machine.
    /// Used by RaftMetadataStore to read metadata without proposing.
    pub fn get_state_machine(&self) -> Arc<crate::raft_metadata::MetadataStateMachine> {
        self.state_machine.load_full()
    }

    /// Apply metadata command directly to state machine (Phase 2.2: WAL-based metadata writes)
    ///
    /// # WARNING
    /// This bypasses Raft consensus! Only use when:
    /// 1. Command is already persisted to metadata WAL
    /// 2. Caller handles replication separately
    ///
    /// Phase 2 usage: Leader writes to WAL → applies via this method → async replicates
    pub fn apply_metadata_command_direct(&self, cmd: MetadataCommand) -> Result<()> {
        let current = self.state_machine.load();
        let mut new_state = (**current).clone();

        new_state.apply(cmd)?;

        self.state_machine.store(Arc::new(new_state));
        Ok(())
    }

    /// Propose a metadata command to the Raft cluster (v2.2.7 Phase 2)
    ///
    /// Optimized for both single-node and multi-node deployments:
    /// - **Single-node**: Applies command immediately (synchronous, <100μs)
    /// - **Multi-node**: Proposes via Raft consensus (async, 10-50ms)
    ///
    /// This provides zero overhead for single-node while maintaining full
    /// consensus for multi-node clusters.
    pub async fn propose(&self, cmd: MetadataCommand) -> Result<()> {
        if self.is_single_node() {
            // FAST PATH: Single-node mode - apply immediately
            return self.apply_immediately(cmd).await;
        }

        // NORMAL PATH: Multi-node Raft consensus
        self.propose_via_raft(cmd).await
    }

    /// Single-node fast path: Apply command immediately (v2.2.7 Phase 2)
    ///
    /// For single-node deployments, there's no need for Raft consensus.
    /// We apply the command directly to the state machine synchronously.
    ///
    /// Performance: <100μs (in-memory HashMap updates)
    ///
    /// CRITICAL OPTIMIZATION (v2.2.7 performance fix):
    /// We do NOT write to Raft log asynchronously because:
    /// 1. Spawning tokio tasks on every produce batch kills throughput (8K → 52K msg/s)
    /// 2. Single-node mode will never have followers to replicate to
    /// 3. If we add nodes later, we bootstrap from metadata snapshots (not Raft log)
    /// 4. Metadata is already persisted in RaftMetadataStore state machine
    async fn apply_immediately(&self, cmd: MetadataCommand) -> Result<()> {
        // Apply to state machine synchronously (just a HashMap update)
        let current = self.state_machine.load();
        let mut new_state = (**current).clone();
        new_state.apply(cmd.clone())?;
        self.state_machine.store(Arc::new(new_state));

        // v2.2.7 EVENT-DRIVEN NOTIFICATION: Fire notifications after applying command
        match &cmd {
            // v2.2.9 Option 4: Partition metadata moved to WalMetadataStore
            // MetadataCommand::CreateTopic { name, .. } => {
            //     if let Some((_, notify)) = self.pending_topics.remove(name) {
            //         notify.notify_waiters(); // Wake up ALL waiting threads
            //         tracing::debug!("✓ Notified waiting threads for topic '{}' (single-node mode)", name);
            //     }
            // }
            MetadataCommand::RegisterBroker { broker_id, .. } => {
                if let Some((_, notify)) = self.pending_brokers.remove(broker_id) {
                    notify.notify_waiters();
                    tracing::debug!("✓ Notified waiting threads for broker {} (single-node mode)", broker_id);
                }
            }
            // v2.2.9 Option 4: Partition metadata moved to WalMetadataStore
            // MetadataCommand::SetPartitionLeader { topic, partition, .. } => {
            //     // v2.2.7 EVENT-DRIVEN NOTIFICATION (P1): Notify followers waiting for partition metadata
            //     let key = format!("{}:{}", topic, partition);
            //     if let Some((_, notify)) = self.pending_partitions.remove(&key) {
            //         notify.notify_waiters();
            //         tracing::debug!("✓ Notified waiting threads for partition '{}' (single-node mode)", key);
            //     }
            // }
            _ => {} // Other commands don't need notifications yet
        }

        Ok(())
    }

    /// Multi-node path: Propose via Raft consensus (v2.2.7 Phase 2)
    ///
    /// For multi-node clusters, propose the command via Raft consensus.
    /// The command will be replicated to all nodes and applied when committed.
    ///
    /// Performance: 10-50ms (network RTT dominates)
    ///
    /// **CRITICAL**: Only the leader can propose. If this node is not the leader,
    /// this method returns an error.
    async fn propose_via_raft(&self, cmd: MetadataCommand) -> Result<()> {
        tracing::info!("🔍 DEBUG propose_via_raft: ENTER - command={:?}", cmd);

        // Check if we're the leader
        {
            let raft = self.raft_node.lock().await;

            let state = raft.raft.state;
            let leader_id = raft.raft.leader_id;

            tracing::info!("🔍 DEBUG propose_via_raft: Leadership check - node_id={}, state={:?}, leader_id={}",
                self.node_id, state, leader_id);

            if raft.raft.state != raft::StateRole::Leader {
                return Err(anyhow::anyhow!(
                    "Cannot propose: this node (id={}) is not the leader (state={:?}, leader={})",
                    self.node_id,
                    raft.raft.state,
                    raft.raft.leader_id
                ));
            }
        }

        // Serialize and propose
        let data = bincode::serialize(&cmd)
            .context("Failed to serialize metadata command")?;

        tracing::info!("🔍 DEBUG propose_via_raft: Serialized command, data.len()={}", data.len());

        // Propose to Raft - the entry will be committed asynchronously
        // The metadata store has its own retry logic to poll for the applied state
        {
            let mut raft = self.raft_node.lock().await;

            tracing::info!("🔍 DEBUG propose_via_raft: About to call raft.propose()");

            raft.propose(vec![], data.clone())
                .context("Failed to propose to Raft")?;

            tracing::info!("✅ DEBUG propose_via_raft: raft.propose() succeeded! Command will be committed asynchronously");
        }

        // PERFORMANCE (v2.2.7): Return immediately - don't wait for commit!
        // The RaftMetadataStore has event-driven notifications for instant wake-up.
        tracing::info!("✅ DEBUG propose_via_raft: EXIT - returning Ok()");
        Ok(())
    }

    /// Propose partition assignment and WAIT for Raft commit (RACE CONDITION FIX)
    ///
    /// This is a synchronous variant of `propose()` specifically for AssignPartition
    /// commands. It registers a notification, proposes the command, then waits for
    /// Raft consensus to complete before returning.
    ///
    /// **Why this is needed**: The standard `propose()` returns immediately, causing
    /// a race condition where clients query metadata before partition assignments
    /// are committed and visible.
    ///
    /// **Performance**: Blocks for Raft consensus duration (typically 10-100ms for
    /// 3-node cluster). This is acceptable for topic creation (rare operation).
    ///
    /// # Arguments
    /// * `topic` - Topic name
    /// * `partition` - Partition ID
    /// * `replicas` - Replica node IDs
    ///
    /// # Returns
    /// * `Ok(())` - Command committed and applied to state machine
    /// * `Err` - Failed to propose or timeout waiting for commit
    pub async fn propose_partition_assignment_and_wait(
        &self,
        topic: String,
        partition: i32,
        replicas: Vec<u64>,
    ) -> Result<()> {
        // v2.2.9 Option 4: Partition metadata moved to WalMetadataStore
        // This function is no longer needed - partition assignments handled by WalMetadataStore
        tracing::warn!("propose_partition_assignment_and_wait called but partition metadata now in WalMetadataStore");
        return Ok(());

        // // Single-node fast path: apply immediately (no waiting needed)
        // if self.is_single_node() {
        //     let cmd = MetadataCommand::AssignPartition {
        //         topic: topic.clone(),
        //         partition,
        //         replicas,
        //     };
        //     return self.apply_immediately(cmd).await;
        // }

        // // Multi-node: register notification BEFORE proposing
        // let key = format!("{}:{}", topic, partition);
        // let notify = Arc::new(Notify::new());
        // self.pending_partitions.insert(key.clone(), notify.clone());

        // tracing::info!("🔔 Registered wait notification for partition assignment '{}'", key);

        // // Propose command via Raft
        // let cmd = MetadataCommand::AssignPartition {
        //     topic: topic.clone(),
        //     partition,
        //     replicas,
        // };

        // if let Err(e) = self.propose_via_raft(cmd).await {
        //     // Cleanup notification on failure
        //     self.pending_partitions.remove(&key);
        //     return Err(e);
        // }

        // tracing::info!("⏳ Waiting for partition assignment '{}' to be committed...", key);

        // // Wait for notification (with timeout)
        // tokio::select! {
        //     _ = notify.notified() => {
        //         tracing::info!("✅ Partition assignment '{}' committed and applied!", key);
        //         Ok(())
        //     }
        //     _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {
        //         // Cleanup on timeout
        //         self.pending_partitions.remove(&key);
        //         Err(anyhow::anyhow!(
        //             "Timeout waiting for partition assignment '{}' to be committed (waited 5s)",
        //             key
        //         ))
        //     }
        // }
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
            let raft = self.raft_node.lock().await;

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
        let current_nodes = self.get_all_nodes().await;
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

        cc.set_context(context.into_bytes().into());

        // STEP 4: Propose ConfChange via Raft
        // CRITICAL: propose_conf_change takes the ConfChangeV2 directly, NOT serialized bytes
        let mut raft = self.raft_node.lock().await;

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
            let raft = self.raft_node.lock().await;

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
        let current_nodes = self.get_all_nodes().await;
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
        cc.set_context(bytes::Bytes::new());

        // STEP 7: Propose ConfChange via Raft
        let mut raft = self.raft_node.lock().await;

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
        // v2.2.9 Option 4: Partition metadata moved to WalMetadataStore
        // Partition reassignment now handled by WalMetadataStore
        tracing::warn!("reassign_partitions_from_node called but partition metadata now in WalMetadataStore");
        return Ok(());

        // // Load current state
        // let partitions_to_reassign = {
        //     let sm = self.state_machine.load();

        //     let mut partitions = Vec::new();

        //     // Find all partitions where node_id is a replica
        //     for ((topic, partition), replicas) in &sm.partition_assignments {
        //         if replicas.contains(&node_id) {
        //             partitions.push((topic.clone(), *partition, replicas.clone()));
        //         }
        //     }

        //     partitions
        // };

        // if partitions_to_reassign.is_empty() {
        //     tracing::info!("No partitions assigned to node {}, nothing to reassign", node_id);
        //     return Ok(());
        // }

        // tracing::info!(
        //     "Reassigning {} partitions away from node {}",
        //     partitions_to_reassign.len(),
        //     node_id
        // );

        // // Get all nodes except the one being removed
        // let available_nodes: Vec<u64> = self.get_all_nodes().await
        //     .into_iter()
        //     .filter(|&id| id != node_id)
        //     .collect();

        // if available_nodes.is_empty() {
        //     return Err(anyhow::anyhow!(
        //         "Cannot reassign partitions: no other nodes available"
        //     ));
        // }

        // // For each partition, create new replica set without node_id
        // for (topic, partition, old_replicas) in partitions_to_reassign {
        //     let mut new_replicas: Vec<u64> = old_replicas
        //         .into_iter()
        //         .filter(|&id| id != node_id)
        //         .collect();

        //     // If we removed a replica, add a new one from available nodes
        //     // to maintain replication factor
        //     if new_replicas.len() < 3 && !available_nodes.is_empty() {
        //         // Find a node not already in new_replicas
        //         for &candidate in &available_nodes {
        //             if !new_replicas.contains(&candidate) {
        //                 new_replicas.push(candidate);
        //                 break;
        //             }
        //         }
        //     }

            // tracing::info!(
            //     "Reassigning partition {}-{}: removing node {}, new replicas: {:?}",
            //     topic, partition, node_id, new_replicas
            // );

            // // v2.2.9 Option 4: Partition metadata moved to WalMetadataStore
            // // Propose new partition assignment
            // let cmd = crate::raft_metadata::MetadataCommand::AssignPartition {
            //     topic: topic.clone(),
            //     partition,
            //     replicas: new_replicas,
            // };

            // self.propose(cmd).await?;
        // }

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

        tracing::info!("🔍 DEBUG apply_committed_entries: Called with {} entries", entries.len());

        // Load current state once at the beginning
        let current = self.state_machine.load();
        let mut new_state = (**current).clone();

        let mut applied_count = 0;
        let mut skipped_count = 0;

        for (idx, entry) in entries.iter().enumerate() {
            tracing::info!("🔍 DEBUG apply_committed_entries: Entry {}/{} - index={}, data.len()={}",
                idx + 1, entries.len(), entry.index, entry.data.len());

            // Skip empty entries
            if entry.data.is_empty() {
                tracing::warn!("⚠️ DEBUG apply_committed_entries: Entry {} has empty data, skipping", idx + 1);
                skipped_count += 1;
                continue;
            }

            // Check entry type
            let entry_type = entry.get_entry_type();
            tracing::info!("🔍 DEBUG apply_committed_entries: Entry {} type={:?}", idx + 1, entry_type);

            if entry_type == EntryType::EntryConfChangeV2 {
                // PRIORITY 2: Handle ConfChangeV2 (node addition/removal)
                // NOTE: ConfChange entries are handled by the message loop before committed entries
                // We don't process them here - they're automatically applied by Raft
                tracing::info!("🔍 DEBUG apply_committed_entries: Skipping ConfChangeV2 entry (already processed by message loop)");
                skipped_count += 1;

            } else if entry_type == EntryType::EntryNormal {
                // Normal metadata command
                tracing::info!("🔍 DEBUG apply_committed_entries: Deserializing entry {}", idx + 1);
                let cmd: MetadataCommand = bincode::deserialize(&entry.data)
                    .context("Failed to deserialize metadata command")?;

                tracing::info!("🔍 DEBUG apply_committed_entries: Applying command: {:?}", cmd);

                // Apply to state machine
                new_state.apply(cmd.clone())?;
                applied_count += 1;

                // v2.2.7 EVENT-DRIVEN NOTIFICATION: Fire notifications after applying command
                match &cmd {
                    // v2.2.9 Option 4: Partition metadata moved to WalMetadataStore
                    // MetadataCommand::CreateTopic { name, .. } => {
                    //     if let Some((_, notify)) = self.pending_topics.remove(name) {
                    //         notify.notify_waiters(); // Wake up ALL waiting threads
                    //         tracing::debug!("✓ Notified waiting threads for topic '{}'", name);
                    //     }
                    // }
                    MetadataCommand::RegisterBroker { broker_id, .. } => {
                        if let Some((_, notify)) = self.pending_brokers.remove(broker_id) {
                            notify.notify_waiters();
                            tracing::debug!("✓ Notified waiting threads for broker {}", broker_id);
                        }
                    }
                    // v2.2.9 Option 4: Partition metadata moved to WalMetadataStore
                    // MetadataCommand::SetPartitionLeader { topic, partition, .. } => {
                    //     // v2.2.7 EVENT-DRIVEN NOTIFICATION (P1): Notify followers waiting for partition metadata
                    //     let key = format!("{}:{}", topic, partition);
                    //     if let Some((_, notify)) = self.pending_partitions.remove(&key) {
                    //         notify.notify_waiters();
                    //         tracing::debug!("✓ Notified waiting threads for partition '{}'", key);
                    //     }
                    // }
                    // MetadataCommand::AssignPartition { topic, partition, .. } => {
                    //     // RACE CONDITION FIX: Notify waiting threads for partition assignment
                    //     // This ensures produce_handler waits for Raft commit before returning
                    //     let key = format!("{}:{}", topic, partition);
                    //     if let Some((_, notify)) = self.pending_partitions.remove(&key) {
                    //         notify.notify_waiters();
                    //         tracing::debug!("✓ Notified waiting threads for partition assignment '{}'", key);
                    //     }
                    // }
                    _ => {} // Other commands don't need notifications yet
                }

                tracing::info!("✅ DEBUG apply_committed_entries: Successfully applied entry {}: {:?}", idx + 1, cmd);
            } else {
                tracing::warn!("⚠️ DEBUG apply_committed_entries: Skipping unknown entry type: {:?}", entry_type);
                skipped_count += 1;
            }
        }

        // Store the new state once at the end
        if applied_count > 0 {
            self.state_machine.store(Arc::new(new_state));
        }

        tracing::info!("🔍 DEBUG apply_committed_entries: Finished - applied={}, skipped={}, total={}",
            applied_count, skipped_count, entries.len());

        Ok(())
    }

    /// Get this node's ID
    pub fn node_id(&self) -> u64 {
        self.node_id
    }

    /// Check if this node is currently the Raft leader
    pub async fn is_leader(&self) -> bool {
        let raft = self.raft_node.lock().await;
        raft.raft.state == raft::StateRole::Leader
    }

    /// List all partitions tracked in metadata
    ///
    /// Returns a list of (topic, partition) tuples for all known partitions.
    pub fn list_all_partitions(&self) -> Vec<(String, i32)> {
        // v2.2.9 Option 4: Partition metadata moved to WalMetadataStore
        // Return empty list - partitions now tracked in WalMetadataStore
        Vec::new()

        // let sm = self.state_machine.load();
        // sm.partition_assignments.keys().cloned().collect()
    }

    /// Get all nodes currently in the cluster (voting members)
    ///
    /// Returns the list of node IDs that are currently voting members
    /// of the Raft cluster.
    ///
    /// # Returns
    /// Vector of node IDs (e.g., [1, 2, 3])
    pub async fn get_all_nodes(&self) -> Vec<u64> {
        let raft = self.raft_node.lock().await;

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
        let sm = self.state_machine.load();
        sm.nodes.iter().map(|(id, addr)| (*id, addr.clone())).collect()
    }

    /// Get all partition information
    ///
    /// # Returns
    /// Vector of PartitionInfo with topic, partition, leader, replicas, and ISR
    pub fn get_all_partition_info(&self) -> Vec<crate::admin_api::PartitionInfo> {
        // v2.2.9 Option 4: Partition metadata moved to WalMetadataStore
        // Return empty list - partitions now tracked in WalMetadataStore
        Vec::new()

        // let sm = self.state_machine.load();

        // let mut partitions = Vec::new();

        // // Collect all unique partition keys from assignments
        // for ((topic, partition), replicas) in &sm.partition_assignments {
        //     let leader = sm.partition_leaders.get(&(topic.clone(), *partition)).copied();
        //     let isr = sm.isr_sets.get(&(topic.clone(), *partition))
        //         .cloned()
        //         .unwrap_or_default();

        //     partitions.push(crate::admin_api::PartitionInfo {
        //         topic: topic.clone(),
        //         partition: *partition,
        //         leader,
        //         replicas: replicas.clone(),
        //         isr,
        //     });
        // }

        // partitions
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
        // v2.2.9 Option 4: Partition metadata moved to WalMetadataStore
        // This function is no longer needed - partition leaders handled by WalMetadataStore
        tracing::warn!("propose_set_partition_leader called but partition metadata now in WalMetadataStore");
        Ok(())

        // let cmd = MetadataCommand::SetPartitionLeader {
        //     topic: topic.to_string(),
        //     partition,
        //     leader,
        // };

        // self.propose(cmd).await
    }

    /// Check if THIS node is the Raft leader
    ///
    /// CRITICAL (v2.2.3): Use this function to check leadership before proposing metadata changes.
    /// This fixes the root cause of partition leadership conflicts where followers were incorrectly
    /// attempting to propose partition leader elections.
    ///
    /// v2.2.7 LOCK CONTENTION FIX: Now uses cached value (lock-free atomic read)
    /// Updated by Raft message loop when leadership changes
    ///
    /// Returns true only if THIS node is currently the Raft leader.
    pub async fn am_i_leader(&self) -> bool {
        // v2.2.7: Lock-free read from cached atomic (no raft_node lock needed!)
        self.cached_is_leader.load(Ordering::Relaxed)
    }

    /// Get the current Raft leader ID (Phase 1.2)
    ///
    /// v2.2.7 LOCK CONTENTION FIX: Now uses cached value (lock-free atomic read)
    /// Updated by Raft message loop when leader changes
    ///
    /// Returns the node ID of the current Raft leader, or None if no leader elected.
    pub async fn get_leader_id(&self) -> Option<u64> {
        // v2.2.7: Lock-free read from cached atomic (no raft_node lock needed!)
        let leader_id = self.cached_leader_id.load(Ordering::Relaxed);

        if leader_id == raft::INVALID_ID {
            None
        } else {
            Some(leader_id)
        }
    }

    /// Query metadata from the leader (Phase 1.2)
    ///
    /// Forwards a metadata query to the Raft leader via gRPC.
    /// If this node is the leader, executes the query locally.
    ///
    /// Includes retry logic to handle intermittent leader election issues.
    ///
    /// # Arguments
    /// - `query`: The metadata query to execute
    ///
    /// # Returns
    /// - Ok(response) if query succeeded
    /// - Err if no leader, RPC failed, or query execution failed
    pub async fn query_leader(
        &self,
        query: crate::metadata_rpc::MetadataQuery,
    ) -> Result<crate::metadata_rpc::MetadataQueryResponse> {
        // Check if we're the leader
        if self.am_i_leader().await {
            // Execute query locally on state machine
            return self.execute_query_local(query);
        }

        // Retry logic to handle intermittent leader election issues
        const MAX_RETRIES: usize = 3;
        const INITIAL_DELAY_MS: u64 = 50;

        for attempt in 0..=MAX_RETRIES {
            // Get leader ID
            match self.get_leader_id().await {
                Some(leader_id) => {
                    // Leader found - attempt to forward query
                    match self.forward_query_to_leader(leader_id, &query).await {
                        Ok(response) => {
                            if attempt > 0 {
                                tracing::debug!(
                                    "Successfully queried leader after {} retries",
                                    attempt
                                );
                            }
                            return Ok(response);
                        }
                        Err(e) => {
                            tracing::warn!(
                                "Failed to query leader {} (attempt {}/{}): {:?}",
                                leader_id, attempt + 1, MAX_RETRIES + 1, e
                            );
                            if attempt < MAX_RETRIES {
                                let delay_ms = INITIAL_DELAY_MS * 2_u64.pow(attempt as u32);
                                tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms)).await;
                            }
                        }
                    }
                }
                None => {
                    // No leader yet
                    if attempt < MAX_RETRIES {
                        let delay_ms = INITIAL_DELAY_MS * 2_u64.pow(attempt as u32);
                        tracing::debug!(
                            "No Raft leader elected (attempt {}/{}), retrying in {}ms",
                            attempt + 1, MAX_RETRIES + 1, delay_ms
                        );
                        tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms)).await;
                    } else {
                        return Err(anyhow::anyhow!("No Raft leader elected after {} retries", MAX_RETRIES + 1));
                    }
                }
            }
        }

        Err(anyhow::anyhow!("Failed to query leader after {} retries", MAX_RETRIES + 1))
    }

    /// Forward query to leader via gRPC (helper for query_leader)
    async fn forward_query_to_leader(
        &self,
        leader_id: u64,
        query: &crate::metadata_rpc::MetadataQuery,
    ) -> Result<crate::metadata_rpc::MetadataQueryResponse> {
        // Serialize query
        let query_data = bincode::serialize(query)
            .context("Failed to serialize metadata query")?;

        // Forward to leader via gRPC
        use chronik_raft::rpc::raft_service_client::RaftServiceClient;
        use chronik_raft::rpc::QueryMetadataRequest;

        // Get leader's Raft address from transport
        let leader_addr = self.transport.get_peer_address(leader_id).await
            .ok_or_else(|| anyhow::anyhow!("Leader {} not found in transport", leader_id))?;

        tracing::debug!("Forwarding metadata query to leader {} at {}", leader_id, leader_addr);

        let mut client = RaftServiceClient::connect(leader_addr.clone()).await
            .context(format!("Failed to connect to leader {} at {}", leader_id, leader_addr))?;

        let request = tonic::Request::new(QueryMetadataRequest {
            query_data,
        });

        let response = client.query_metadata(request).await
            .context("QueryMetadata RPC failed")?
            .into_inner();

        if !response.success {
            return Err(anyhow::anyhow!("Leader query failed: {}", response.error));
        }

        // Deserialize response
        let query_response = bincode::deserialize(&response.response_data)
            .context("Failed to deserialize query response")?;

        Ok(query_response)
    }

    /// Execute a metadata query locally on the state machine (Phase 1.2)
    ///
    /// Helper method called by query_leader when this node is the leader.
    fn execute_query_local(
        &self,
        query: crate::metadata_rpc::MetadataQuery,
    ) -> Result<crate::metadata_rpc::MetadataQueryResponse> {
        use crate::metadata_rpc::{MetadataQuery, MetadataQueryResponse};

        let state = self.state_machine.load();

        match query {
            // v2.2.9 Option 4: Partition metadata moved to WalMetadataStore
            MetadataQuery::GetTopic { name: _ } => {
                // Return None - topics now tracked in WalMetadataStore
                Ok(MetadataQueryResponse::Topic(None))
            }
            // v2.2.9 Option 4: Partition metadata moved to WalMetadataStore
            MetadataQuery::ListTopics => {
                // Return empty list - topics now tracked in WalMetadataStore
                Ok(MetadataQueryResponse::TopicList(Vec::new()))
                // let topics: Vec<_> = state.topics.values().map(|t| {
                //     chronik_common::metadata::TopicMetadata {
                //         id: uuid::Uuid::new_v4(),
                //         name: t.name.clone(),
                //         config: t.config.clone(),
                //         created_at: chrono::Utc::now(),
                //         updated_at: chrono::Utc::now(),
                //     }
                // }).collect();
                // Ok(MetadataQueryResponse::TopicList(topics))
            }
            MetadataQuery::GetBroker { broker_id } => {
                let broker = state.brokers.get(&broker_id).map(|b| {
                    chronik_common::metadata::BrokerMetadata {
                        broker_id,
                        host: b.host.clone(),
                        port: b.port,
                        rack: b.rack.clone(),
                        status: chronik_common::metadata::BrokerStatus::Online,
                        created_at: chrono::Utc::now(),
                        updated_at: chrono::Utc::now(),
                    }
                });
                Ok(MetadataQueryResponse::Broker(broker))
            }
            MetadataQuery::ListBrokers => {
                let brokers: Vec<_> = state.brokers.iter().map(|(&broker_id, b)| {
                    chronik_common::metadata::BrokerMetadata {
                        broker_id,
                        host: b.host.clone(),
                        port: b.port,
                        rack: b.rack.clone(),
                        status: chronik_common::metadata::BrokerStatus::Online,
                        created_at: chrono::Utc::now(),
                        updated_at: chrono::Utc::now(),
                    }
                }).collect();
                Ok(MetadataQueryResponse::BrokerList(brokers))
            }
            // v2.2.9 Option 4: Partition metadata moved to WalMetadataStore
            MetadataQuery::GetPartitionAssignment { topic: _, partition: _ } => {
                // Return None - partition assignments now tracked in WalMetadataStore
                Ok(MetadataQueryResponse::PartitionAssignment(None))
                // let key = (topic.clone(), partition);
                // let replicas = state.partition_assignments.get(&key);
                // let leader = state.partition_leaders.get(&key).copied();

                // let assignment = replicas.and_then(|replicas_vec| {
                //     leader.map(|leader_node_id| {
                //         chronik_common::metadata::PartitionAssignment {
                //             topic,
                //             partition: partition as u32,
                //             broker_id: leader_node_id as i32,
                //             is_leader: true,
                //             replicas: replicas_vec.clone(),
                //             leader_id: leader_node_id as u64,
                //         }
                //     })
                // });

                // Ok(MetadataQueryResponse::PartitionAssignment(assignment))
            }
            // v2.2.9 Option 4: Partition metadata moved to WalMetadataStore
            MetadataQuery::GetHighWatermark { topic: _, partition: _ } => {
                // Return 0 - high watermarks now tracked in WalMetadataStore
                Ok(MetadataQueryResponse::HighWatermark(0))
                // let key = (topic, partition);
                // let hw = state.partition_high_watermarks.get(&key).copied().unwrap_or(0);
                // Ok(MetadataQueryResponse::HighWatermark(hw))
            }
            // v2.2.9 Option 4: Partition metadata moved to WalMetadataStore
            MetadataQuery::GetPartitionCount { topic: _ } => {
                // Return 0 - partition counts now tracked in WalMetadataStore
                Ok(MetadataQueryResponse::PartitionCount(0))
                // let count = state.topics.get(&topic)
                //     .map(|t| t.config.partition_count as i32)
                //     .unwrap_or(0);
                // Ok(MetadataQueryResponse::PartitionCount(count))
            }
        }
    }

    /// Forward a write command to the leader (Phase 1.2)
    ///
    /// Forwards a metadata write command to the Raft leader via gRPC.
    /// If this node is the leader, proposes the command locally via Raft.
    ///
    /// Includes retry logic to handle intermittent leader election issues.
    ///
    /// # Arguments
    /// - `command`: The metadata write command to execute
    ///
    /// # Returns
    /// - Ok(()) if write succeeded
    /// - Err if no leader, RPC failed, or write execution failed
    pub async fn forward_write_to_leader(
        &self,
        command: crate::metadata_rpc::MetadataWriteCommand,
    ) -> Result<()> {
        // Check if we're the leader
        if self.am_i_leader().await {
            // Execute write locally via Raft proposal
            return self.execute_write_local(command).await;
        }

        // Retry logic to handle intermittent leader election issues
        const MAX_RETRIES: usize = 3;
        const INITIAL_DELAY_MS: u64 = 50;

        for attempt in 0..=MAX_RETRIES {
            // Get leader ID
            match self.get_leader_id().await {
                Some(leader_id) => {
                    // Leader found - attempt to forward write
                    match self.do_forward_write_to_leader(leader_id, &command).await {
                        Ok(()) => {
                            if attempt > 0 {
                                tracing::debug!(
                                    "Successfully forwarded write to leader after {} retries",
                                    attempt
                                );
                            }
                            return Ok(());
                        }
                        Err(e) => {
                            tracing::warn!(
                                "Failed to forward write to leader {} (attempt {}/{}): {:?}",
                                leader_id, attempt + 1, MAX_RETRIES + 1, e
                            );
                            if attempt < MAX_RETRIES {
                                let delay_ms = INITIAL_DELAY_MS * 2_u64.pow(attempt as u32);
                                tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms)).await;
                            }
                        }
                    }
                }
                None => {
                    // No leader yet
                    if attempt < MAX_RETRIES {
                        let delay_ms = INITIAL_DELAY_MS * 2_u64.pow(attempt as u32);
                        tracing::debug!(
                            "No Raft leader elected for write (attempt {}/{}), retrying in {}ms",
                            attempt + 1, MAX_RETRIES + 1, delay_ms
                        );
                        tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms)).await;
                    } else {
                        return Err(anyhow::anyhow!("No Raft leader elected for write after {} retries", MAX_RETRIES + 1));
                    }
                }
            }
        }

        Err(anyhow::anyhow!("Failed to forward write to leader after {} retries", MAX_RETRIES + 1))
    }

    /// Actually forward write to leader via gRPC (helper for forward_write_to_leader)
    async fn do_forward_write_to_leader(
        &self,
        leader_id: u64,
        command: &crate::metadata_rpc::MetadataWriteCommand,
    ) -> Result<()> {
        // Serialize command
        let command_data = bincode::serialize(command)
            .context("Failed to serialize metadata write command")?;

        // Forward to leader via gRPC
        use chronik_raft::rpc::raft_service_client::RaftServiceClient;
        use chronik_raft::rpc::ForwardWriteRequest;

        // Get leader's Raft address from transport
        let leader_addr = self.transport.get_peer_address(leader_id).await
            .ok_or_else(|| anyhow::anyhow!("Leader {} not found in transport", leader_id))?;

        tracing::debug!("Forwarding metadata write to leader {} at {}", leader_id, leader_addr);

        let mut client = RaftServiceClient::connect(leader_addr.clone()).await
            .context(format!("Failed to connect to leader {} at {}", leader_id, leader_addr))?;

        let request = tonic::Request::new(ForwardWriteRequest {
            command_data,
        });

        let response = client.forward_write(request).await
            .context("ForwardWrite RPC failed")?
            .into_inner();

        if !response.success {
            return Err(anyhow::anyhow!("Leader write failed: {}", response.error));
        }

        Ok(())
    }

    /// Execute a metadata write locally via Raft proposal (Phase 1.2)
    ///
    /// Helper method called by forward_write_to_leader when this node is the leader.
    async fn execute_write_local(
        &self,
        command: crate::metadata_rpc::MetadataWriteCommand,
    ) -> Result<()> {
        use crate::metadata_rpc::MetadataWriteCommand;

        // Convert MetadataWriteCommand to MetadataCommand and propose via Raft
        let raft_command = match command {
            // v2.2.9 Option 4: Partition metadata moved to WalMetadataStore
            MetadataWriteCommand::CreateTopic { name: _, partition_count: _, replication_factor: _, config: _ } => {
                return Err(anyhow::anyhow!("CreateTopic: partition metadata now in WalMetadataStore"));
                // MetadataCommand::CreateTopic {
                //     name,
                //     partition_count: partition_count as u32,  // Convert i32 to u32
                //     replication_factor: replication_factor as u32,  // Convert i32 to u32
                //     config,
                // }
            }
            MetadataWriteCommand::RegisterBroker { broker_id, host, port, rack } => {
                MetadataCommand::RegisterBroker {
                    broker_id,
                    host,
                    port,
                    rack,
                }
            }
            // v2.2.9 Option 4: Partition metadata moved to WalMetadataStore
            MetadataWriteCommand::SetPartitionLeader { topic: _, partition: _, leader_id: _ } => {
                return Err(anyhow::anyhow!("SetPartitionLeader: partition metadata now in WalMetadataStore"));
                // MetadataCommand::SetPartitionLeader {
                //     topic,
                //     partition,  // Already i32
                //     leader: leader_id,
                // }
            }
            // v2.2.9 Option 4: Partition metadata moved to WalMetadataStore
            MetadataWriteCommand::UpdateHighWatermark { topic: _, partition: _, offset: _ } => {
                return Err(anyhow::anyhow!("UpdateHighWatermark: partition metadata now in WalMetadataStore"));
                // MetadataCommand::UpdatePartitionOffset {
                //     topic,
                //     partition: partition as u32,  // Convert i32 to u32
                //     high_watermark: offset,
                //     log_start_offset: 0, // Not provided in write command
                // }
            }
            // v2.2.9 Option 4: Consumer groups moved to WalMetadataStore
            MetadataWriteCommand::CreateConsumerGroup { group_id: _, protocol_type: _, protocol: _ } => {
                return Err(anyhow::anyhow!("CreateConsumerGroup: consumer group metadata now in WalMetadataStore"));
                // MetadataCommand::CreateConsumerGroup {
                //     group_id,
                //     protocol_type,
                //     protocol,
                // }
            }
            // v2.2.9 Option 4: Partition metadata moved to WalMetadataStore
            MetadataWriteCommand::DeleteTopic { name: _ } => {
                return Err(anyhow::anyhow!("DeleteTopic: partition metadata now in WalMetadataStore"));
                // MetadataCommand::DeleteTopic { name }
            }
            // v2.2.9 Option 4: Consumer groups moved to WalMetadataStore
            MetadataWriteCommand::CommitOffset { group_id: _, topic: _, partition: _, offset: _ } => {
                return Err(anyhow::anyhow!("CommitOffset: consumer group metadata now in WalMetadataStore"));
                // MetadataCommand::CommitOffset {
                //     group_id,
                //     topic,
                //     partition: partition as u32,
                //     offset,
                //     metadata: None,  // RPC doesn't carry metadata
                // }
            }
            // v2.2.9 Option 4: Partition metadata moved to WalMetadataStore
            MetadataWriteCommand::AssignPartition { topic: _, partition: _, replicas: _ } => {
                return Err(anyhow::anyhow!("AssignPartition: partition metadata now in WalMetadataStore"));
                // MetadataCommand::AssignPartition {
                //     topic,
                //     partition,
                //     replicas,
                // }
            }
            MetadataWriteCommand::UpdateBrokerStatus { broker_id, status } => {
                MetadataCommand::UpdateBrokerStatus {
                    broker_id,
                    status,
                }
            }
            // v2.2.9 Option 4: Consumer groups moved to WalMetadataStore
            MetadataWriteCommand::UpdateConsumerGroup { group_id: _, state: _, generation_id: _, leader: _ } => {
                return Err(anyhow::anyhow!("UpdateConsumerGroup: consumer group metadata now in WalMetadataStore"));
                // MetadataCommand::UpdateConsumerGroup {
                //     group_id,
                //     state,
                //     generation_id,
                //     leader: leader.unwrap_or_default(),  // Unwrap Option<String> to String
                // }
            }
            // v2.2.9 Option 4: Partition metadata moved to WalMetadataStore
            MetadataWriteCommand::UpdatePartitionOffset { topic: _, partition: _, high_watermark: _, log_start_offset: _ } => {
                return Err(anyhow::anyhow!("UpdatePartitionOffset: partition metadata now in WalMetadataStore"));
                // MetadataCommand::UpdatePartitionOffset {
                //     topic,
                //     partition,
                //     high_watermark,
                //     log_start_offset,
                // }
            }
        };

        // Propose via Raft (handles single-node vs multi-node)
        self.propose(raft_command).await
    }

    /// Check if Raft has a leader (either we're the leader or there's a valid leader)
    ///
    /// NOTE: This function returns true for BOTH leaders AND followers with a known leader.
    /// If you want to check if THIS node is the leader, use `am_i_leader()` instead.
    ///
    /// Returns (has_leader, leader_id, state_role)
    pub async fn is_leader_ready(&self) -> (bool, u64, String) {
        let raft_node = self.raft_node.lock().await;
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
        // v2.2.7 DEADLOCK FIX: Queue messages instead of blocking on Raft lock
        let cluster = self.clone();
        let message_handler = Arc::new(move |msg: Message| {
            // Queue message for asynchronous processing instead of blocking
            cluster.incoming_message_sender.send(msg)
                .map_err(|e| format!("Failed to queue incoming Raft message: {}", e))?;

            Ok(())
        });

        // Create query handler for metadata queries (Phase 1.2)
        let cluster_for_query = self.clone();
        let query_handler = Arc::new(move |query_data: Vec<u8>| {
            // Deserialize query
            let query: crate::metadata_rpc::MetadataQuery = bincode::deserialize(&query_data)
                .map_err(|e| format!("Failed to deserialize query: {}", e))?;

            // Execute query on local state machine
            let response = cluster_for_query.execute_query_local(query)
                .map_err(|e| format!("Failed to execute query: {}", e))?;

            // Serialize response
            bincode::serialize(&response)
                .map_err(|e| format!("Failed to serialize response: {}", e))
        });

        // Create write handler for metadata writes (Phase 1.2)
        let cluster_for_write = self.clone();
        let write_handler = Arc::new(move |command_data: Vec<u8>| {
            // Deserialize command
            let command: crate::metadata_rpc::MetadataWriteCommand = bincode::deserialize(&command_data)
                .map_err(|e| format!("Failed to deserialize command: {}", e))?;

            // Execute write via Raft proposal
            // Use spawn to avoid blocking inside async runtime
            let cluster_clone = cluster_for_write.clone();
            let (tx, rx) = std::sync::mpsc::sync_channel(1);

            tokio::spawn(async move {
                let result = cluster_clone.execute_write_local(command).await
                    .map_err(|e| format!("Failed to execute write: {}", e));
                let _ = tx.send(result);
            });

            // Wait for result with timeout (prevents infinite blocking if Raft is stuck)
            match rx.recv_timeout(std::time::Duration::from_secs(10)) {
                Ok(Ok(())) => Ok(()),
                Ok(Err(e)) => Err(e),
                Err(e) => Err(format!("Write task timed out after 10s: {}", e)),
            }
        });

        // Create RPC service with all handlers (Phase 1.2)
        let service = RaftServiceImpl::new(message_handler)
            .with_query_handler(query_handler)
            .with_write_handler(write_handler);

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

    /// Persist Raft Ready state to storage (OUTSIDE lock)
    ///
    /// This is a helper function extracted from the message loop to enable
    /// persistence to happen OUTSIDE the raft_node lock, which dramatically
    /// reduces lock contention during leader elections.
    ///
    /// P0.2 LOCK OPTIMIZATION: By moving I/O outside the lock, we reduce
    /// lock hold time from 20+ seconds to < 50ms during leader elections.
    async fn persist_ready_state(
        &self,
        entries: Option<Vec<raft::prelude::Entry>>,
        hard_state: Option<raft::prelude::HardState>,
    ) -> Result<()> {
        // Persist entries if present
        if let Some(entries) = entries {
            let entries_len = entries.len();
            tracing::info!("Persisting {} Raft entries to WAL (OUTSIDE lock)", entries_len);

            match self.storage.append_entries(&entries).await {
                Ok(()) => {
                    tracing::info!("✓ Persisted {} Raft entries to WAL", entries_len);
                }
                Err(e) => {
                    tracing::error!("Failed to persist Raft entries: {}", e);
                    return Err(e);
                }
            }
        }

        // Persist hard state if present
        if let Some(hs) = hard_state {
            let term = hs.term;
            let vote = hs.vote;
            let commit = hs.commit;
            tracing::info!("Persisting HardState (OUTSIDE lock): term={}, vote={}, commit={}", term, vote, commit);

            match self.storage.persist_hard_state(&hs).await {
                Ok(()) => {
                    tracing::info!("✓ Persisted HardState: term={}, vote={}, commit={}", term, vote, commit);
                }
                Err(e) => {
                    tracing::error!("Failed to persist HardState: {}", e);
                    return Err(e);
                }
            }
        }

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
        if self.is_single_node() {
            tracing::info!("Single-node mode: starting Raft message loop (required for leader election)");
        } else {
            tracing::info!("Multi-node mode: starting Raft message processing loop");
        }

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(100));

            // v2.2.7 Phase 2: Track previous leader for change detection
            let mut previous_leader_id: Option<u64> = None;

            loop {
                interval.tick().await;

                // v2.2.7 DEADLOCK FIX: Process ALL incoming messages BEFORE acquiring the Raft lock
                // This ensures messages don't pile up while we hold the lock
                let incoming_messages: Vec<Message> = {
                    let mut receiver = self.incoming_message_receiver.lock().await;
                    let mut messages = Vec::new();
                    while let Ok(msg) = receiver.try_recv() {
                        messages.push(msg);
                    }
                    messages
                };

                // Acquire Raft lock for the tick + step + ready cycle
                // CRITICAL: Lock must be held from ready() to advance()
                // P0.2 METRICS: Track lock hold time to verify optimization
                let lock_acquire_start = std::time::Instant::now();
                let mut raft_lock = self.raft_node.lock().await;
                let lock_acquired_at = std::time::Instant::now();

                // Step 1: Tick Raft (while holding lock)
                raft_lock.tick();

                // Step 2: Process incoming messages from gRPC (while holding lock)
                // This is the KEY fix - we process messages INSIDE the lock, not competing for it
                if !incoming_messages.is_empty() {
                    tracing::debug!("Processing {} queued incoming Raft messages", incoming_messages.len());
                    for msg in incoming_messages {
                        if let Err(e) = raft_lock.step(msg) {
                            tracing::error!("Failed to step Raft with incoming message: {:?}", e);
                        }
                    }
                }

                // Step 3: Check if Raft has anything ready to process
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
                    // v2.2.14: Changed from info to debug (was logging 259 times = 3MB logs)
                    tracing::debug!("Raft ready: {} unpersisted messages to send", messages.len());

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

                    // Drop lock before async operations
                    drop(raft_lock);

                    // Persist snapshot and apply to state machine
                    let apply_result = async {
                        // Save snapshot to disk
                        storage_clone.save_snapshot(&snapshot_clone).await?;

                        // Deserialize and apply state machine
                        let state_machine_data = snapshot_clone.get_data();
                        if !state_machine_data.is_empty() {
                            let new_state: MetadataStateMachine = bincode::deserialize(state_machine_data)
                                .context("Failed to deserialize state machine from snapshot")?;

                            // Replace state machine with snapshot state
                            state_machine_clone.store(Arc::new(new_state));

                            tracing::info!("✓ Applied state machine from snapshot");
                        }

                        // Truncate log entries covered by snapshot
                        storage_clone.truncate_log_to_snapshot(snapshot_clone.get_metadata().index).await?;

                        Ok::<(), anyhow::Error>(())
                    }.await;

                    if let Err(e) = apply_result {
                        tracing::error!("Failed to apply snapshot: {}", e);
                    } else {
                        tracing::info!("✓ Snapshot applied successfully");
                    }

                    // Lock was dropped, skip to next iteration
                    continue;
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
                            // CRITICAL: Use existing raft_lock from line 1006 - DO NOT acquire lock again (deadlock!)
                            let cs = match raft_lock.apply_conf_change(&cc) {
                                Ok(cs) => cs,
                                Err(e) => {
                                    tracing::error!("Failed to apply ConfChange: {:?}", e);
                                    continue;
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

                // Step 3b: Collect normal committed entries (SKIP ConfChange entries - already processed in Step 3a)
                // DEADLOCK FIX (v2.2.7): DON'T apply entries yet - just collect them for later.
                // We must keep raft_lock held through advance() to satisfy Raft's API contract.
                // We'll apply entries AFTER advance() by dropping the lock temporarily.
                let entries_to_apply = if !ready.committed_entries().is_empty() {
                    // Filter out ConfChange entries - only process normal entries
                    let normal_entries: Vec<_> = ready.committed_entries()
                        .iter()
                        .filter(|entry| entry.get_entry_type() != raft::prelude::EntryType::EntryConfChangeV2)
                        .cloned()
                        .collect();

                    if !normal_entries.is_empty() {
                        tracing::info!(
                            "🔍 DEBUG: Collected {} normal committed entries for application (filtered out {} ConfChange)",
                            normal_entries.len(),
                            ready.committed_entries().len() - normal_entries.len()
                        );
                        Some(normal_entries)
                    } else {
                        tracing::debug!("🔍 DEBUG: No normal entries in this Ready (all were ConfChange)");
                        None
                    }
                } else {
                    tracing::debug!("🔍 DEBUG: No committed entries in this Ready");
                    None
                };

                // Step 4: Persist entries (required before persisted_messages)
                // NOTE: tokio::Mutex allows holding lock across await points
                // METRICS: Track I/O time within lock to understand contention
                let io_start = std::time::Instant::now();

                if !ready.entries().is_empty() {
                    let entries = ready.entries();
                    let entries_len = entries.len();
                    tracing::info!("Persisting {} Raft entries to WAL (holding lock)", entries_len);

                    let entries_clone: Vec<_> = entries.iter().cloned().collect();

                    // Persist while holding lock (tokio::Mutex is async-safe)
                    // CRITICAL: Lock MUST be held from ready() to advance() for raft-rs correctness
                    match self.storage.append_entries(&entries_clone).await {
                        Ok(()) => {
                            tracing::info!("✓ Persisted {} Raft entries to WAL", entries_len);
                        }
                        Err(e) => {
                            tracing::error!("Failed to persist Raft entries: {}", e);
                        }
                    }
                }

                // Step 5: Persist hard state (required before persisted_messages)
                // NOTE: tokio::Mutex allows holding lock across await points
                if let Some(hs) = ready.hs() {
                    let term = hs.term;
                    let vote = hs.vote;
                    let commit = hs.commit;
                    tracing::info!("Persisting HardState (holding lock): term={}, vote={}, commit={}", term, vote, commit);

                    // Persist while holding lock (tokio::Mutex is async-safe)
                    match self.storage.persist_hard_state(hs).await {
                        Ok(()) => {
                            tracing::info!("✓ Persisted HardState: term={}, vote={}, commit={}", term, vote, commit);
                        }
                        Err(e) => {
                            tracing::error!("Failed to persist HardState: {}", e);
                        }
                    }
                }

                let io_duration_ms = io_start.elapsed().as_millis();
                if io_duration_ms > 100 {
                    tracing::warn!("⚠️  Raft I/O took {}ms (>100ms) - may cause lock contention", io_duration_ms);
                }

                // Step 6: Send persisted messages (AFTER persistence)
                // CRITICAL: MUST call take_persisted_messages() to satisfy Raft's ownership requirements
                // Then immediately send to channel (non-blocking) - background task handles actual sends
                if !ready.persisted_messages().is_empty() {
                    let persisted_msgs = ready.take_persisted_messages();
                    tracing::debug!("Raft ready: {} persisted messages to send", persisted_msgs.len());

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
                // Lock has been held continuously from ready() to here (raft-rs requirement)
                tracing::debug!("Advancing Raft (state={:?})", raft_lock.raft.state);

                raft_lock.advance(ready);

                tracing::debug!("✓ Advanced Raft successfully");

                // v2.2.7 LOCK CONTENTION FIX: Update cached leader state for lock-free reads
                // This enables metadata queries to check leadership without locking raft_node
                let current_leader_id_val = raft_lock.raft.leader_id;
                let current_state = raft_lock.raft.state;
                self.cached_leader_id.store(current_leader_id_val, Ordering::Relaxed);
                self.cached_is_leader.store(
                    current_state == raft::StateRole::Leader,
                    Ordering::Relaxed
                );

                // METRICS: Calculate total lock hold time (including I/O)
                let lock_released_at = std::time::Instant::now();
                let total_lock_hold_ms = lock_released_at.duration_since(lock_acquired_at).as_millis();

                // v2.2.14: Changed from info to debug (was logging 250 times = 3MB logs)
                tracing::debug!(
                    "🔒 Raft lock metrics: total={}ms (includes {}ms I/O)",
                    total_lock_hold_ms,
                    io_duration_ms
                );

                // Alert if lock hold time is excessive (indicates potential contention source)
                if total_lock_hold_ms > 1000 {
                    tracing::warn!(
                        "⚠️  High Raft lock hold time: {}ms (includes {}ms I/O) - this may cause contention",
                        total_lock_hold_ms,
                        io_duration_ms
                    );
                }

                // DEADLOCK FIX (v2.2.7): Now that advance() is called, we can safely drop the lock
                // and apply committed entries to the state machine. This prevents deadlock with
                // metadata queries that hold state_machine.read() and try to acquire raft_lock.
                //
                // KEY INSIGHT: After dropping the lock and applying entries, we MUST continue to
                // the next iteration of the loop (not re-acquire the lock). The next iteration
                // will acquire a fresh lock and get the next Ready state from Raft.
                if let Some(entries) = entries_to_apply {
                    // Drop the lock before applying entries
                    drop(raft_lock);

                    tracing::info!(
                        "🔍 DEBUG: Applying {} committed entries to state machine (AFTER advance)",
                        entries.len()
                    );

                    if let Err(e) = self.apply_committed_entries(&entries) {
                        tracing::error!("❌ DEBUG: Failed to apply committed entries: {}", e);
                    } else {
                        tracing::info!(
                            "✅ DEBUG: Successfully applied {} committed entries to state machine",
                            entries.len()
                        );
                    }

                    // Continue to next iteration (will acquire fresh lock)
                    // DO NOT re-acquire the lock here - it causes Raft state machine issues
                    continue;
                }

                // v2.2.7 Phase 2: Detect leader changes for WAL replication
                let current_leader_id = raft_lock.raft.leader_id;
                let current_term = raft_lock.raft.term;

                if current_leader_id != raft::INVALID_ID {
                    if let Some(prev_leader) = previous_leader_id {
                        if prev_leader != current_leader_id {
                            // Leader changed!
                            tracing::info!(
                                "🔄 Leader change detected: {} → {} (term={})",
                                prev_leader,
                                current_leader_id,
                                current_term
                            );

                            // Emit leader change event
                            let event = crate::leader_heartbeat::LeaderChangeEvent::new(
                                current_leader_id,
                                prev_leader,
                                current_term,
                            );

                            match self.leader_change_sender.send(event) {
                                Ok(receiver_count) => {
                                    tracing::info!(
                                        "📢 Broadcast leader change event: new_leader={}, old_leader={}, receivers={}",
                                        current_leader_id,
                                        prev_leader,
                                        receiver_count
                                    );
                                }
                                Err(e) => {
                                    tracing::trace!("Leader change broadcast skipped (no receivers): {}", e);
                                }
                            }
                        }
                    }
                    previous_leader_id = Some(current_leader_id);
                }

                // Step 8: Check if we should create a snapshot (Phase 5)
                // SNAPSHOT STORM FIX (v2.2.7): Only create snapshot if we've advanced significantly
                // since the last snapshot. This prevents the same snapshot from being created
                // repeatedly on every Raft ready event.
                let applied = {
                    let raft_state = self.storage.initial_state().unwrap();
                    raft_state.hard_state.commit
                };

                let last_index = self.storage.last_index().unwrap();
                let last_snapshot_idx = *self.last_snapshot_index.read().unwrap();

                // Only snapshot if:
                // 1. We have enough log entries (last_index > 1000)
                // 2. We're close to the end of the log (applied >= last_index - 1000)
                // 3. We've advanced significantly since last snapshot (applied >= last_snapshot_idx + 500)
                //    This ensures we don't create the same snapshot repeatedly
                let should_snapshot = applied > 0
                    && last_index > 1000
                    && applied >= last_index - 1000
                    && applied >= last_snapshot_idx + 500;

                if should_snapshot {
                    tracing::info!(
                        "Snapshot trigger: applied={}, last_index={}, last_snapshot={} (will create new snapshot)",
                        applied,
                        last_index,
                        last_snapshot_idx
                    );

                    // Clone needed data before async operations
                    let state_machine_clone = self.state_machine.clone();
                    let storage_clone = self.storage.clone();
                    let last_snapshot_index_clone = self.last_snapshot_index.clone();

                    // Create snapshot (run async operations directly)
                    let snapshot_result = async {
                        // Serialize state machine
                        let state_machine_bytes = {
                            let state_machine = state_machine_clone.load();
                            bincode::serialize(&**state_machine)
                                .context("Failed to serialize state machine")?
                        };

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

                        // SNAPSHOT STORM FIX: Update last snapshot index
                        *last_snapshot_index_clone.write().unwrap() = applied;

                        Ok::<(), anyhow::Error>(())
                    }.await;

                    if let Err(e) = snapshot_result {
                        tracing::error!("Failed to create/save snapshot: {}", e);
                    } else {
                        tracing::info!("✓ Created and saved snapshot at index={}", applied);
                    }
                } else if applied > 0 && last_index > 1000 && applied >= last_index - 1000 {
                    // Would have triggered snapshot but already have recent one
                    tracing::debug!(
                        "Snapshot skipped: applied={}, last_snapshot={} (need +500 progress)",
                        applied,
                        last_snapshot_idx
                    );
                }

                // Lock is released here at end of loop iteration
            }
        });

        tracing::info!("✓ Raft message loop started");
    }

    /// v2.2.7 Phase 3: Broadcast heartbeat to followers
    ///
    /// Called by HeartbeatSender on the leader to send periodic heartbeats.
    /// Followers receive these heartbeats and update their leases.
    pub async fn broadcast_heartbeat(
        &self,
        heartbeat: crate::leader_heartbeat::LeaderHeartbeat,
    ) -> Result<()> {
        // Send heartbeat to all subscribed followers
        // Broadcast channel automatically delivers to all receivers
        match self.heartbeat_sender.send(heartbeat) {
            Ok(receiver_count) => {
                tracing::trace!(
                    "Broadcast heartbeat: leader={}, term={}, receivers={}",
                    self.node_id,
                    self.current_term().await,
                    receiver_count
                );
                Ok(())
            }
            Err(e) => {
                // This only fails if there are no receivers, which is fine
                tracing::trace!("Heartbeat broadcast skipped (no receivers): {}", e);
                Ok(())
            }
        }
    }

    /// v2.2.7 Phase 3: Subscribe to heartbeat messages
    ///
    /// Called by HeartbeatReceiver on followers to receive heartbeats from leader.
    /// Returns a receiver that gets notified when leader sends heartbeats.
    pub fn subscribe_heartbeats(
        &self,
    ) -> tokio::sync::broadcast::Receiver<crate::leader_heartbeat::LeaderHeartbeat> {
        self.heartbeat_sender.subscribe()
    }

    /// Subscribe to leader change events (v2.2.7 Phase 2: WAL Replication)
    ///
    /// Returns a receiver that gets notified when the Raft leader changes.
    /// Used by WalReplicationManager to reconnect to the new leader.
    pub fn subscribe_leader_changes(
        &self,
    ) -> tokio::sync::broadcast::Receiver<crate::leader_heartbeat::LeaderChangeEvent> {
        self.leader_change_sender.subscribe()
    }

    /// Get current Raft term for heartbeat messages
    pub async fn current_term(&self) -> u64 {
        let node = self.raft_node.lock().await;
        node.raft.term
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
    pub peers: Vec<chronik_config::NodeConfig>,  // Full peer info (Kafka, WAL, Raft)
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
    use crate::integrated_server::{IntegratedKafkaServer, IntegratedServerConfig, IntegratedKafkaServerBuilder};
    use std::net::SocketAddr;
    
    tracing::info!(
        "Starting Raft cluster node: id={}, kafka_port={}, raft_addr={}",
        config.node_id,
        config.kafka_port,
        config.raft_addr
    );

    // Step 1: Extract Raft addresses for bootstrap (node_id, raft_addr)
    // Keep full peer configs for later use (broker registration, WAL replication)
    let peers_for_replication = config.peers.clone();
    let raft_peers: Vec<(u64, String)> = config.peers.iter()
        .map(|peer| (peer.id, peer.raft.clone()))
        .collect();

    // Bootstrap RaftCluster for metadata coordination
    let raft_cluster = Arc::new(RaftCluster::bootstrap(
        config.node_id,
        raft_peers,
        PathBuf::from(&config.data_dir)
    ).await?);

    tracing::info!("Raft cluster bootstrapped successfully");

    // Step 1.5: Start Raft message processing loop (v2.2.7 Phase 5 fix)
    // CRITICAL: Without this, Raft proposals never get committed!
    raft_cluster.clone().start_message_loop();
    tracing::info!("✓ Raft message processing loop started");

    // Step 1.6: Start Raft gRPC server (v2.2.7 - production gRPC transport)
    // CRITICAL: Without this, nodes can't communicate and leader election fails!
    raft_cluster.clone().start_grpc_server(config.raft_addr.clone()).await?;
    tracing::info!("✓ Raft gRPC server started on {}", config.raft_addr);

    // v2.2.8 FIX: Pre-warm connections AFTER gRPC server starts
    // This prevents chicken-and-egg problem during cluster startup
    // Spawn in background so it doesn't block
    let cluster_for_prewarm = raft_cluster.clone();
    tokio::spawn(async move {
        cluster_for_prewarm.pre_warm_connections().await;
    });

    // Step 1.7: Auto-configure WAL replication for Raft cluster (v2.2.7 fix)
    // CRITICAL: Without this, acks=-1 hangs because no follower ACKs are sent!
    // Use actual WAL addresses from peer configs (not calculated from Raft ports)
    let wal_followers: Vec<String> = peers_for_replication.iter()
        .map(|peer| {
            tracing::info!("Mapped peer {} to WAL follower: {} (Kafka={}, Raft={})",
                peer.id, peer.wal, peer.kafka, peer.raft);
            peer.wal.clone()
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
        tls_config: None,  // TLS configured via env vars
    };

    // Step 3: Create and start Kafka server with Raft cluster using builder pattern
    tracing::info!("Creating IntegratedKafkaServer with builder (14-stage initialization)...");
    let server = IntegratedKafkaServerBuilder::new(server_config.clone())
        .with_raft_cluster(raft_cluster.clone())
        .build()
        .await?;
    tracing::info!("IntegratedKafkaServer created successfully via builder");

    // Step 3.5: Register broker NOW (after Raft message loop started)
    // ARCHITECTURAL FIX: Multi-node broker registration happens here, AFTER:
    // 1. Raft message loop started (leader election can proceed)
    // 2. gRPC server started (nodes can communicate)
    // 3. IntegratedServer created (metadata store ready)
    tracing::info!("Waiting for Raft leader election before broker registration...");

    // Wait for leader election (max 10 seconds)
    let mut election_attempts = 0;
    let max_election_wait = 100; // 100 * 100ms = 10 seconds
    let mut is_leader = false;
    while election_attempts < max_election_wait {
        let (has_leader, leader_id, state) = raft_cluster.is_leader_ready().await;
        if has_leader {
            is_leader = leader_id == config.node_id;
            tracing::info!("✓ Raft leader elected: leader_id={}, this_node={}, is_leader={}",
                leader_id, config.node_id, is_leader);
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        election_attempts += 1;
    }

    // CRITICAL FIX: Only the leader registers brokers
    // Followers will receive broker metadata via Raft replication
    if !is_leader {
        tracing::info!("This node is a follower - skipping broker registration (will receive via Raft replication)");
    } else {
        // Leader registers ALL brokers from config (v2.2.7: use actual peer configs)
        use chronik_common::metadata::traits::BrokerMetadata;

        tracing::info!("This node is the Raft leader - registering all brokers from config");

        // Get metadata store from server
        let metadata_store = server.metadata_store();

        // Register all peers from config (using actual Kafka addresses)
        for peer in &peers_for_replication {
            // Parse Kafka address to extract host and port
            let (host, port) = if let Some(colon_pos) = peer.kafka.rfind(':') {
                let host = peer.kafka[..colon_pos].to_string();
                let port = peer.kafka[colon_pos+1..].parse::<i32>()
                    .unwrap_or_else(|_| {
                        tracing::warn!("Failed to parse Kafka port from '{}', using default 9092", peer.kafka);
                        9092
                    });
                (host, port)
            } else {
                tracing::warn!("Invalid Kafka address format '{}', using defaults", peer.kafka);
                (peer.kafka.clone(), 9092)
            };

            let broker_metadata = BrokerMetadata {
                broker_id: peer.id as i32,
                host,
                port,
                rack: None,
                status: chronik_common::metadata::traits::BrokerStatus::Online,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            };

            match metadata_store.register_broker(broker_metadata.clone()).await {
                Ok(_) => {
                    tracing::info!("✅ Successfully registered broker {} ({}) via Raft", peer.id, peer.kafka);
                }
                Err(e) => {
                    tracing::error!("Failed to register broker {} ({}): {:?}", peer.id, peer.kafka, e);
                    return Err(anyhow::anyhow!("Broker registration failed for broker {} ({}): {}", peer.id, peer.kafka, e));
                }
            }
        }
    }

    // Bind Kafka server
    let kafka_addr = format!("0.0.0.0:{}", config.kafka_port);

    tracing::info!("Starting Kafka server on {}", kafka_addr);

    // Step 4: Run Kafka server (blocks until shutdown)
    // Note: Raft message loop is already running in background from start_message_loop()
    server.run(&kafka_addr).await?;

    Ok(())
}
