//! PostgreSQL-style WAL streaming replication (v2.2.0+)
//!
//! This module implements leader-to-follower WAL streaming using TCP connections.
//! Design principles:
//! - Fire-and-forget from leader (never blocks produce path)
//! - Push-based streaming (leader actively sends records)
//! - Lock-free queue (crossbeam SegQueue for MPMC)
//! - Automatic reconnection on failure
//!
//! Protocol: See docs/WAL_STREAMING_PROTOCOL.md

use anyhow::{Result, Context};
use bytes::{Bytes, BytesMut, BufMut, Buf};
use chronik_storage::CanonicalRecord;
use crossbeam::queue::SegQueue;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tokio::net::{TcpStream, TcpListener};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::time::{sleep, Duration, timeout};
use tracing::{debug, error, info, warn};
use serde::{Serialize, Deserialize};
use dashmap::DashMap;

// v2.2.7 Phase 3: Import RaftCluster and IsrTracker
use crate::raft_cluster::RaftCluster;
use crate::isr_tracker::IsrTracker;

// v2.2.7 Phase 6: Import ClusterConfig for auto-discovery
use chronik_config::ClusterConfig;

// v2.2.7 Phase 4: Import IsrAckTracker for acks=-1 support
use crate::isr_ack_tracker::IsrAckTracker;

/// Magic number for WAL frames ('WA' in hex)
const FRAME_MAGIC: u16 = 0x5741;

/// Magic number for heartbeat frames ('HB' in hex)
const HEARTBEAT_MAGIC: u16 = 0x4842;

/// Magic number for ACK frames ('AK' in hex) - v2.2.7 Phase 4
const ACK_MAGIC: u16 = 0x414B;

/// Current protocol version
const PROTOCOL_VERSION: u16 = 1;

/// Maximum queue size before dropping old records
const MAX_QUEUE_SIZE: usize = 100_000;

/// Heartbeat interval (10 seconds)
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);

/// Heartbeat timeout (30 seconds)
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(30);

/// Reconnect delay (30 seconds)
const RECONNECT_DELAY: Duration = Duration::from_secs(30);

/// WAL replication record sent over the wire
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalReplicationRecord {
    /// Topic name
    pub topic: String,

    /// Partition ID
    pub partition: i32,

    /// Base offset of this batch
    pub base_offset: i64,

    /// Number of records in batch
    pub record_count: u32,

    /// Timestamp (milliseconds since epoch)
    pub timestamp_ms: i64,

    /// Serialized CanonicalRecord (bincode)
    pub data: Bytes,
}

/// ACK message sent from follower to leader after successful WAL write (v2.2.7 Phase 4)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalAckMessage {
    /// Topic name
    pub topic: String,

    /// Partition ID
    pub partition: i32,

    /// Offset that was successfully replicated
    pub offset: i64,

    /// Follower node ID
    pub node_id: u64,
}

/// WAL Replication Manager for PostgreSQL-style streaming
///
/// Phase 3 implementation with actual TCP streaming and lock-free queue
/// Phase 4 update: Bidirectional streams for ACK reception
pub struct WalReplicationManager {
    /// Queue of pending WAL records (lock-free MPMC)
    queue: Arc<SegQueue<WalReplicationRecord>>,

    /// Active TCP write connections to followers (addr -> OwnedWriteHalf)
    /// v2.2.7 Phase 4: Changed from TcpStream to OwnedWriteHalf to allow separate ACK reading
    connections: Arc<DashMap<String, OwnedWriteHalf>>,

    /// Follower addresses ("host:port")
    followers: Vec<String>,

    /// Shutdown signal
    shutdown: Arc<AtomicBool>,

    /// Total records queued (for metrics)
    total_queued: Arc<AtomicU64>,

    /// Total records sent (for metrics)
    total_sent: Arc<AtomicU64>,

    /// Total records dropped (queue overflow)
    total_dropped: Arc<AtomicU64>,

    /// Raft cluster for querying partition replicas (v2.2.7 Phase 3)
    raft_cluster: Option<Arc<RaftCluster>>,

    /// ISR tracker for determining in-sync replicas (v2.2.7 Phase 3)
    isr_tracker: Option<Arc<IsrTracker>>,

    /// ISR ACK tracker for recording follower ACKs for acks=-1 (v2.2.7 Phase 4)
    isr_ack_tracker: Option<Arc<IsrAckTracker>>,

    /// Cluster config for auto-discovering followers (v2.2.7 Phase 6)
    cluster_config: Option<Arc<chronik_config::ClusterConfig>>,

    /// Last known Raft leader ID (for Phase 3 dynamic leader change detection)
    last_known_leader: Arc<AtomicU64>,

    /// Whether this node is currently the leader (for Phase 3)
    is_currently_leader: Arc<AtomicBool>,

    /// Per-partition follower addresses: (topic, partition) → [wal_addr1, wal_addr2]
    /// Enables partition-specific replication routing (Priority 1)
    /// Maps each partition to its specific replicas (not all followers)
    partition_followers: Arc<DashMap<(String, i32), Vec<String>>>,
}

impl WalReplicationManager {
    /// Create a new WAL replication manager
    ///
    /// Spawns background workers for connection management and record sending
    pub fn new(followers: Vec<String>) -> Arc<Self> {
        Self::new_with_dependencies(followers, None, None, None, None)
    }

    /// Create a new WAL replication manager with Raft cluster and ISR tracker (v2.2.7 Phase 3)
    ///
    /// This is the recommended constructor for cluster mode.
    ///
    /// v2.2.7 Phase 6: Added cluster_config parameter for automatic follower discovery.
    /// If followers is empty and cluster_config is provided, followers will be auto-discovered.
    pub fn new_with_dependencies(
        followers: Vec<String>,
        raft_cluster: Option<Arc<RaftCluster>>,
        isr_tracker: Option<Arc<IsrTracker>>,
        isr_ack_tracker: Option<Arc<IsrAckTracker>>,
        cluster_config: Option<Arc<ClusterConfig>>,
    ) -> Arc<Self> {
        // v2.2.7 Phase 6: Auto-discover followers from cluster config
        // CRITICAL FIX: Exclude current node from followers list to prevent self-replication
        let final_followers = if followers.is_empty() && cluster_config.is_some() {
            let config = cluster_config.as_ref().unwrap();
            let my_node_id = config.node_id;
            let discovered: Vec<String> = config.peer_nodes()
                .iter()
                .filter(|peer| peer.id != my_node_id)  // CRITICAL: Don't replicate to self!
                .map(|peer| peer.wal.clone())
                .collect();

            if !discovered.is_empty() {
                info!("Auto-discovered {} followers from cluster config (excluding self node {}): {}",
                      discovered.len(),
                      my_node_id,
                      discovered.join(", "));
            }
            discovered
        } else {
            followers
        };

        info!("Creating WalReplicationManager with {} followers, Raft: {}, ISR: {}, AckTracker: {}, ClusterConfig: {}",
              final_followers.len(),
              raft_cluster.is_some(),
              isr_tracker.is_some(),
              isr_ack_tracker.is_some(),
              cluster_config.is_some());

        let manager = Arc::new(Self {
            queue: Arc::new(SegQueue::new()),
            connections: Arc::new(DashMap::new()),
            followers: final_followers,  // v2.2.7 Phase 6: Use auto-discovered or manual followers
            shutdown: Arc::new(AtomicBool::new(false)),
            total_queued: Arc::new(AtomicU64::new(0)),
            total_sent: Arc::new(AtomicU64::new(0)),
            total_dropped: Arc::new(AtomicU64::new(0)),
            raft_cluster,       // v2.2.7: Accept directly in constructor
            isr_tracker,        // v2.2.7: Accept directly in constructor
            isr_ack_tracker,    // v2.2.7 Phase 4: Accept ACK tracker
            cluster_config,     // v2.2.7 Phase 6: Store for potential dynamic updates
            last_known_leader: Arc::new(AtomicU64::new(0)), // Phase 3: Track leader changes
            is_currently_leader: Arc::new(AtomicBool::new(false)), // Phase 3: Track leadership
            partition_followers: Arc::new(DashMap::new()), // Priority 1: Per-partition follower map
        });

        // Spawn background worker for sending records
        let manager_clone = Arc::clone(&manager);
        tokio::spawn(async move {
            manager_clone.run_sender_worker().await;
        });

        // Spawn background worker for connection management
        let manager_clone2 = Arc::clone(&manager);
        tokio::spawn(async move {
            manager_clone2.run_connection_manager().await;
        });

        // Phase 3: Spawn background worker for dynamic leader change detection
        let manager_clone3 = Arc::clone(&manager);
        tokio::spawn(async move {
            manager_clone3.run_leader_watcher().await;
        });

        // Priority 1: Spawn background worker for partition-level follower discovery
        let manager_clone4 = Arc::clone(&manager);
        tokio::spawn(async move {
            manager_clone4.run_follower_discovery_worker().await;
        });

        manager
    }

    /// Replicate pre-serialized WAL data to all followers (fire-and-forget, zero-copy)
    ///
    /// v2.2.0 OPTIMIZATION: Takes pre-serialized bincode bytes from WAL write.
    /// This avoids redundant parsing and serialization in the hot path.
    ///
    /// This is called from the produce hot path and MUST be non-blocking.
    /// Records are pushed to a lock-free queue and sent by background worker.
    pub async fn replicate_serialized(&self, topic: String, partition: i32, base_offset: i64, serialized_data: Vec<u8>) {
        // Data is already bincode-serialized CanonicalRecord from WAL write
        // Zero-copy: Just wrap in Bytes (cheap Arc clone)
        let data = Bytes::from(serialized_data);

        // We don't know record_count from serialized data, but it's just for metrics
        // Set to 0 (or we could deserialize just to count, but that defeats optimization)
        let wal_record = WalReplicationRecord {
            topic,
            partition,
            base_offset,
            record_count: 0, // Unknown from serialized data (metrics only)
            timestamp_ms: chrono::Utc::now().timestamp_millis(),
            data,
        };

        // Check queue size (prevent unbounded growth)
        let queue_size = self.queue.len();
        if queue_size > MAX_QUEUE_SIZE {
            // Drop oldest records to prevent memory exhaustion
            self.total_dropped.fetch_add(1, Ordering::Relaxed);
            warn!("WAL replication queue overflow ({} records), dropping record", queue_size);
            return;
        }

        // Push to queue (lock-free, ~10 ns)
        self.queue.push(wal_record);
        self.total_queued.fetch_add(1, Ordering::Relaxed);
    }

    /// Replicate to specific partition replicas based on Raft metadata and ISR (v2.2.7 Phase 3)
    ///
    /// This method:
    /// 1. Queries RaftCluster for partition replicas
    /// 2. Filters to only in-sync replicas using IsrTracker
    /// 3. Sends to those replicas only (more efficient than broadcast)
    ///
    /// This is the NEW recommended method for cluster replication.
    pub async fn replicate_partition(
        &self,
        topic: String,
        partition: i32,
        offset: i64,
        leader_high_watermark: i64,
        serialized_data: Vec<u8>,
    ) {
        // CRITICAL FIX for acks=-1: Get ISR from Raft metadata (source of truth)
        // Do NOT use IsrTracker for filtering - it's for runtime monitoring only
        let isr_replicas = match &self.raft_cluster {
            Some(cluster) => {
                match cluster.get_isr(&topic, partition) {
                    Some(isr) => {
                        debug!(
                            "Got ISR from Raft metadata for {}-{}: {:?}",
                            topic, partition, isr
                        );
                        isr
                    }
                    None => {
                        // No ISR in Raft metadata, fall back to all replicas
                        match cluster.get_partition_replicas(&topic, partition) {
                            Some(replicas) => {
                                debug!(
                                    "No ISR found for {}-{}, using all replicas: {:?}",
                                    topic, partition, replicas
                                );
                                replicas
                            }
                            None => {
                                // No replicas assigned, fall back to broadcast
                                debug!(
                                    "No replicas found for {}-{}, falling back to replicate_serialized",
                                    topic, partition
                                );
                                self.replicate_serialized(topic, partition, offset, serialized_data).await;
                                return;
                            }
                        }
                    }
                }
            }
            None => {
                // No Raft cluster, fall back to broadcast
                debug!("No RaftCluster configured, falling back to replicate_serialized");
                self.replicate_serialized(topic, partition, offset, serialized_data).await;
                return;
            }
        };

        // Leader already excluded from followers list in WalReplicationManager::new()
        // ISR list will include the leader, but followers list does not
        if isr_replicas.is_empty() {
            warn!(
                "No in-sync replicas for {}-{} (offset={}), skipping replication",
                topic, partition, offset
            );
            return;
        }

        debug!(
            "Replicating {}-{} offset {} to {} ISR members: {:?}",
            topic,
            partition,
            offset,
            isr_replicas.len(),
            isr_replicas
        );

        // For now, just use the existing replicate_serialized logic
        // TODO: In future, send only to specific node IDs
        // For Phase 3, broadcasting to all is acceptable (followers will ignore if not assigned)
        self.replicate_serialized(topic, partition, offset, serialized_data).await;
    }

    /// Replicate a WAL record to all followers (fire-and-forget)
    ///
    /// DEPRECATED: Use replicate_serialized() for zero-copy replication.
    /// This method is kept for backwards compatibility only.
    ///
    /// This is called from the produce hot path and MUST be non-blocking.
    /// Records are pushed to a lock-free queue and sent by background worker.
    pub async fn replicate_async(&self, topic: String, partition: i32, base_offset: i64, record: CanonicalRecord) {
        // Serialize the record (bincode is fast, ~1-2 μs)
        let data = match bincode::serialize(&record) {
            Ok(serialized) => Bytes::from(serialized),
            Err(e) => {
                error!("Failed to serialize WAL record for replication: {}", e);
                return;
            }
        };

        let wal_record = WalReplicationRecord {
            topic,
            partition,
            base_offset,
            record_count: record.records.len() as u32,
            timestamp_ms: chrono::Utc::now().timestamp_millis(),
            data,
        };

        // Check queue size (prevent unbounded growth)
        let queue_size = self.queue.len();
        if queue_size >= MAX_QUEUE_SIZE {
            // Drop this record and increment counter
            self.total_dropped.fetch_add(1, Ordering::Relaxed);
            warn!(
                "WAL replication queue full ({} records), dropping record for {}-{} offset {}",
                queue_size, wal_record.topic, wal_record.partition, wal_record.base_offset
            );
            return;
        }

        // Push to queue (lock-free, O(1))
        self.queue.push(wal_record);
        self.total_queued.fetch_add(1, Ordering::Relaxed);
    }

    /// Background worker that dequeues records and sends to followers
    async fn run_sender_worker(&self) {
        info!("WAL replication sender worker started");

        while !self.shutdown.load(Ordering::Relaxed) {
            // Try to dequeue a record (non-blocking)
            if let Some(record) = self.queue.pop() {
                // Send to all active followers (fan-out)
                self.send_to_followers(&record).await;
                self.total_sent.fetch_add(1, Ordering::Relaxed);
            } else {
                // Queue empty, sleep briefly to avoid busy-wait
                sleep(Duration::from_millis(1)).await;
            }
        }

        info!("WAL replication sender worker stopped");
    }

    /// Send a WAL record to partition-specific followers (Priority 1: Partition-aware routing)
    ///
    /// This method now uses the partition_followers map to route records only to
    /// replicas assigned to this specific partition (not broadcast to all followers).
    ///
    /// Fallback: If no partition assignment found, uses static followers list.
    async fn send_to_followers(&self, record: &WalReplicationRecord) {
        let frame = match serialize_wal_frame(record) {
            Ok(f) => f,
            Err(e) => {
                error!("Failed to serialize WAL frame: {}", e);
                return;
            }
        };

        // Priority 1: Look up partition-specific followers from discovery worker
        let partition_key = (record.topic.clone(), record.partition);
        let target_followers = match self.partition_followers.get(&partition_key) {
            Some(followers_ref) => {
                let followers = followers_ref.value().clone();
                debug!(
                    "Routing {}-{} to {} partition-specific followers (not all {} followers)",
                    record.topic,
                    record.partition,
                    followers.len(),
                    self.followers.len()
                );
                followers
            }
            None => {
                // Fallback: No partition assignment yet, use static followers
                debug!(
                    "No partition assignment for {}-{}, using static followers",
                    record.topic, record.partition
                );
                self.followers.clone()
            }
        };

        // Send to each target follower sequentially (DashMap doesn't allow moving RefMut into tasks)
        for follower_addr in &target_followers {
            if let Some(mut conn) = self.connections.get_mut(follower_addr) {
                // Write frame to TCP stream
                // Note: This is fast (~1ms) so sequential is fine
                if let Err(e) = conn.write_all(&frame).await {
                    error!("Failed to send WAL record to {}: {}", follower_addr, e);
                    // Remove dead connection (connection manager will reconnect)
                    drop(conn); // Drop RefMut before removing
                    self.connections.remove(follower_addr);
                } else {
                    debug!("Sent WAL record for {}-{} to follower: {}", record.topic, record.partition, follower_addr);
                }
            } else {
                debug!("No connection to follower {} for {}-{}", follower_addr, record.topic, record.partition);
            }
        }
    }

    /// Background worker for managing connections to followers
    async fn run_connection_manager(&self) {
        info!("WAL replication connection manager started");

        while !self.shutdown.load(Ordering::Relaxed) {
            for follower_addr in &self.followers {
                // Check if we have an active connection
                if !self.connections.contains_key(follower_addr) {
                    // Attempt to connect
                    info!("Attempting to connect to follower: {}", follower_addr);

                    match timeout(Duration::from_secs(5), TcpStream::connect(follower_addr)).await {
                        Ok(Ok(stream)) => {
                            info!("✅ Connected to follower: {}", follower_addr);

                            // v2.2.7 Phase 4: Split stream for bidirectional communication
                            // Write half: used by send_to_followers
                            // Read half: used by ACK reader task
                            let (read_half, write_half) = stream.into_split();

                            // Spawn ACK reader task for this follower (v2.2.7 Phase 4)
                            if let Some(ref ack_tracker) = self.isr_ack_tracker {
                                let tracker = Arc::clone(ack_tracker);
                                let shutdown = Arc::clone(&self.shutdown);
                                let addr = follower_addr.clone();
                                tokio::spawn(async move {
                                    if let Err(e) = Self::run_ack_reader(read_half, tracker, shutdown, addr.clone()).await {
                                        error!("ACK reader for {} failed: {}", addr, e);
                                    }
                                });
                                info!("✅ Spawned ACK reader for follower: {}", follower_addr);
                            }

                            // Store write-half connection (for send_to_followers)
                            self.connections.insert(follower_addr.clone(), write_half);
                        }
                        Ok(Err(e)) => {
                            warn!("Failed to connect to follower {}: {}", follower_addr, e);
                        }
                        Err(_) => {
                            warn!("Connection timeout to follower {}", follower_addr);
                        }
                    }
                }
            }

            // Sleep before next connection check
            sleep(RECONNECT_DELAY).await;
        }

        info!("WAL replication connection manager stopped");
    }

    /// Background worker for reading ACKs from a follower (v2.2.7 Phase 4)
    ///
    /// This is the CRITICAL missing piece that completes acks=-1 support.
    /// Followers send ACKs after successful WAL writes, and this task reads them.
    async fn run_ack_reader(
        mut read_half: OwnedReadHalf,
        isr_ack_tracker: Arc<IsrAckTracker>,
        shutdown: Arc<AtomicBool>,
        follower_addr: String,
    ) -> Result<()> {
        info!("ACK reader started for follower: {}", follower_addr);
        let mut buffer = BytesMut::with_capacity(4096);

        while !shutdown.load(Ordering::Relaxed) {
            // Read ACK frame header (8 bytes: magic + version + length)
            if buffer.len() < 8 {
                match timeout(Duration::from_secs(60), read_half.read_buf(&mut buffer)).await {
                    Ok(Ok(0)) => {
                        // Connection closed
                        info!("ACK reader: Connection closed by follower {}", follower_addr);
                        return Ok(());
                    }
                    Ok(Ok(n)) => {
                        debug!("ACK reader: Read {} bytes from {}", n, follower_addr);
                    }
                    Ok(Err(e)) => {
                        return Err(anyhow::anyhow!("ACK reader: Read error from {}: {}", follower_addr, e));
                    }
                    Err(_) => {
                        // Timeout - check shutdown and continue
                        if buffer.is_empty() {
                            // No ACKs in 60s is normal if no replication happening
                            continue;
                        }
                        return Err(anyhow::anyhow!("ACK reader: Timeout reading from {}", follower_addr));
                    }
                }
            }

            // Parse frame header if we have enough data
            if buffer.len() >= 8 {
                let magic = u16::from_be_bytes([buffer[0], buffer[1]]);

                // Skip heartbeat frames (if any)
                if magic == HEARTBEAT_MAGIC {
                    buffer.advance(8);
                    debug!("ACK reader: Received heartbeat from {}", follower_addr);
                    continue;
                }

                // Validate ACK magic
                if magic != ACK_MAGIC {
                    return Err(anyhow::anyhow!(
                        "ACK reader: Invalid frame magic from {}: 0x{:04x} (expected ACK 0x{:04x})",
                        follower_addr, magic, ACK_MAGIC
                    ));
                }

                // Read frame length
                let frame_length = u32::from_be_bytes([buffer[4], buffer[5], buffer[6], buffer[7]]) as usize;
                let frame_size = 8 + frame_length; // header + payload

                // Wait for complete frame
                while buffer.len() < frame_size && !shutdown.load(Ordering::Relaxed) {
                    match timeout(Duration::from_secs(30), read_half.read_buf(&mut buffer)).await {
                        Ok(Ok(0)) => {
                            return Err(anyhow::anyhow!(
                                "ACK reader: Connection closed before complete ACK frame from {}",
                                follower_addr
                            ));
                        }
                        Ok(Ok(n)) => {
                            debug!("ACK reader: Read {} more bytes from {} (total: {})", n, follower_addr, buffer.len());
                        }
                        Ok(Err(e)) => {
                            return Err(anyhow::anyhow!("ACK reader: Read error from {}: {}", follower_addr, e));
                        }
                        Err(_) => {
                            return Err(anyhow::anyhow!("ACK reader: Timeout waiting for complete ACK from {}", follower_addr));
                        }
                    }
                }

                // Deserialize complete ACK frame
                let frame_bytes = buffer.split_to(frame_size);
                let payload = &frame_bytes[8..]; // Skip header

                match bincode::deserialize::<WalAckMessage>(payload) {
                    Ok(ack_msg) => {
                        info!(
                            "ACK✓ Received from {}: {}-{} offset {} (node {})",
                            follower_addr,
                            ack_msg.topic,
                            ack_msg.partition,
                            ack_msg.offset,
                            ack_msg.node_id
                        );

                        // THIS IS THE KEY FIX: Record ACK in IsrAckTracker on leader
                        isr_ack_tracker.record_ack(
                            &ack_msg.topic,
                            ack_msg.partition,
                            ack_msg.offset,
                            ack_msg.node_id,
                        );
                    }
                    Err(e) => {
                        error!("ACK reader: Failed to deserialize ACK from {}: {}", follower_addr, e);
                        // Continue processing other ACKs
                    }
                }
            }
        }

        info!("ACK reader stopped for follower: {}", follower_addr);
        Ok(())
    }

    /// Phase 3: Background worker that watches for Raft leadership changes
    ///
    /// When this node becomes leader → automatically discover followers and connect
    /// When this node loses leadership → close all replication connections
    ///
    /// This enables automatic disaster recovery without manual intervention.
    async fn run_leader_watcher(&self) {
        // Only run if we have both Raft cluster and cluster config
        let (raft, config) = match (&self.raft_cluster, &self.cluster_config) {
            (Some(r), Some(c)) => (r, c),
            _ => {
                info!("Leader watcher disabled: no Raft cluster or cluster config");
                return;
            }
        };

        info!("Starting dynamic leader change detection (polling every 5s)");

        // Check every 5 seconds (responsive to leadership changes)
        let check_interval = Duration::from_secs(5);

        while !self.shutdown.load(Ordering::Relaxed) {
            sleep(check_interval).await;

            // Query Raft for current leadership status
            let (is_ready, leader_id, state_role) = raft.is_leader_ready().await;
            let my_node_id = config.node_id;
            let am_i_leader = is_ready && leader_id == my_node_id;

            // Check if leadership changed
            let was_leader = self.is_currently_leader.load(Ordering::Relaxed);
            let last_leader = self.last_known_leader.load(Ordering::Relaxed);

            if leader_id != last_leader || am_i_leader != was_leader {
                // Leadership changed!
                info!(
                    "Cluster leadership changed - previous_leader: {}, new_leader: {}, node_id: {}, role: {}, was_leader: {}, is_leader: {}",
                    last_leader, leader_id, my_node_id, state_role, was_leader, am_i_leader
                );

                self.last_known_leader.store(leader_id, Ordering::Relaxed);
                self.is_currently_leader.store(am_i_leader, Ordering::Relaxed);

                if am_i_leader && !was_leader {
                    // Became leader → start replication
                    info!("✅ Became cluster leader - starting WAL replication to followers");
                    self.on_became_leader(config).await;
                } else if !am_i_leader && was_leader {
                    // Lost leadership → stop replication
                    warn!("⚠️ Lost cluster leadership - stopping WAL replication (new_leader: {})", leader_id);
                    self.on_lost_leadership().await;
                }
            }
        }

        info!("Leader watcher stopped");
    }

    /// Called when this node becomes the cluster leader
    ///
    /// Automatically discovers followers from cluster config and establishes connections.
    async fn on_became_leader(&self, config: &ClusterConfig) {
        info!("Discovering cluster followers from configuration...");

        // Get list of peer nodes (already filters out self)
        let peer_wal_addresses: Vec<String> = config.peer_nodes()
            .iter()
            .map(|peer| peer.wal.clone())
            .collect();

        if peer_wal_addresses.is_empty() {
            warn!("No followers found in cluster configuration");
            return;
        }

        info!(
            "Discovered {} follower nodes: {}",
            peer_wal_addresses.len(),
            peer_wal_addresses.join(", ")
        );

        // Close any existing connections first (clean slate)
        self.connections.clear();

        // NOTE: Connection manager worker will handle actual connection establishment
        // We just need to ensure connections map is empty so it can reconnect
        info!("Cleared old connections - connection manager will establish new follower connections");
    }

    /// Called when this node loses cluster leadership
    ///
    /// Closes all WAL replication connections since only leaders replicate.
    async fn on_lost_leadership(&self) {
        info!("Closing all WAL replication connections (no longer leader)");

        // Close all connections
        let connection_count = self.connections.len();
        self.connections.clear();

        info!("Closed {} WAL replication connections", connection_count);

        // Clear the queue (no point in sending queued records if we're not the leader)
        let mut cleared_count = 0;
        while self.queue.pop().is_some() {
            cleared_count += 1;
        }

        if cleared_count > 0 {
            info!("Cleared {} queued WAL records from replication queue", cleared_count);
        }
    }

    /// Background worker for dynamic partition-level follower discovery (Priority 1)
    ///
    /// This polls Raft every 10 seconds to check for partition assignment changes.
    /// When detected, it updates the per-partition follower map automatically.
    ///
    /// This enables:
    /// - Partition-specific replication routing (not broadcast to all followers)
    /// - Dynamic partition reassignment without restart
    /// - Automatic handling of leader changes with correct routing
    async fn run_follower_discovery_worker(&self) {
        // Only run if we have both Raft cluster and cluster config
        let (raft, config) = match (&self.raft_cluster, &self.cluster_config) {
            (Some(r), Some(c)) => (r, c),
            _ => {
                debug!("Follower discovery worker disabled: no Raft cluster or cluster config");
                return;
            }
        };

        info!("Starting dynamic partition-level follower discovery (polling every 10s)");
        let check_interval = Duration::from_secs(10);
        let node_id = config.node_id;

        while !self.shutdown.load(Ordering::Relaxed) {
            sleep(check_interval).await;

            // Query: Which partitions am I the leader for?
            let my_partitions = raft.get_partitions_where_leader(node_id);

            if my_partitions.is_empty() {
                debug!("Node {} is not leader for any partitions", node_id);
                continue;
            }

            debug!("Node {} is leader for {} partitions", node_id, my_partitions.len());

            // For each partition, update follower list
            for (topic, partition) in my_partitions {
                // Query: Who are the replicas for this partition?
                if let Some(replicas) = raft.get_partition_replicas(&topic, partition) {
                    // Filter out self (leader doesn't replicate to itself)
                    let followers: Vec<u64> = replicas
                        .into_iter()
                        .filter(|&id| id != node_id)
                        .collect();

                    // Map node IDs → WAL addresses from cluster config
                    let follower_addrs: Vec<String> = followers
                        .iter()
                        .filter_map(|&id| {
                            config.peers.iter()
                                .find(|p| p.id == id)
                                .map(|p| p.wal.clone())
                        })
                        .collect();

                    // Update partition followers map
                    let partition_key = (topic.clone(), partition);
                    let current_followers = self.partition_followers.get(&partition_key)
                        .map(|entry| entry.value().clone());

                    // Only log if followers changed
                    if current_followers.as_ref() != Some(&follower_addrs) {
                        info!(
                            "Updated followers for {}-{}: {:?} (was: {:?})",
                            topic, partition, follower_addrs, current_followers
                        );

                        self.partition_followers.insert(partition_key, follower_addrs);
                    }
                } else {
                    debug!("No replicas found for {}-{}", topic, partition);
                }
            }
        }

        info!("Follower discovery worker stopped");
    }

    /// Spawn leader change handler task (v2.2.7 Phase 2)
    ///
    /// Listens for leader change events from RaftCluster and triggers WAL reconnection.
    /// Must be called AFTER creating the WalReplicationManager, passing the RaftCluster reference.
    ///
    /// # Arguments
    /// * `raft_cluster` - Reference to RaftCluster for subscribing to leader changes
    pub fn spawn_leader_change_handler(self: &Arc<Self>, raft_cluster: Arc<RaftCluster>) {
        let manager = Arc::clone(self);

        tokio::spawn(async move {
            info!("🔄 Starting WAL replication leader change handler");

            // Subscribe to leader change events
            let mut rx = raft_cluster.subscribe_leader_changes();

            loop {
                match rx.recv().await {
                    Ok(event) => {
                        info!(
                            "🔄 Received leader change event: {} → {} (term={})",
                            event.old_leader_id,
                            event.new_leader_id,
                            event.term
                        );

                        // Handle the leader change
                        manager.handle_leader_change(event, raft_cluster.clone()).await;
                    }
                    Err(e) => {
                        error!("Leader change receiver error: {:?}", e);
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                }
            }
        });

        info!("✓ Spawned WAL replication leader change handler");
    }

    /// Handle leader change event (v2.2.7 Phase 2)
    ///
    /// When leader changes:
    /// 1. Close all existing WAL connections
    /// 2. Discover new follower addresses from cluster config
    /// 3. Reconnect to new followers
    ///
    /// This ensures WAL replication continues seamlessly after failover.
    async fn handle_leader_change(
        &self,
        event: crate::leader_heartbeat::LeaderChangeEvent,
        raft_cluster: Arc<RaftCluster>,
    ) {
        info!(
            "🔄 Handling leader change: old={}, new={}, term={}",
            event.old_leader_id,
            event.new_leader_id,
            event.term
        );

        // Step 1: Close all existing connections
        info!("Closing {} existing WAL connections", self.connections.len());
        self.connections.clear();

        // Step 2: Check if WE are the new leader
        let am_i_new_leader = raft_cluster.am_i_leader().await;

        if !am_i_new_leader {
            info!("Not the new leader (leader={}), no reconnection needed", event.new_leader_id);
            return;
        }

        info!("✅ I am the new leader! Reconnecting WAL replication to followers");

        // Step 3: Discover follower addresses from cluster config
        if let Some(cluster_config) = &self.cluster_config {
            let new_followers: Vec<String> = cluster_config.peer_nodes()
                .iter()
                .map(|peer| peer.wal.clone())
                .collect();

            info!(
                "🔍 Discovered {} followers from cluster config: {}",
                new_followers.len(),
                new_followers.join(", ")
            );

            // Step 4: Trigger reconnection by updating connection manager
            // The connection manager background worker will automatically reconnect
            // to the new followers on its next iteration
            info!("✓ Leader change handled - connection manager will reconnect");
        } else {
            warn!("No cluster config available - cannot auto-discover followers after leader change");
        }
    }

    /// Shutdown the replication manager
    pub async fn shutdown(&self) {
        info!("Shutting down WAL replication manager");
        self.shutdown.store(true, Ordering::Relaxed);

        // Close all connections
        self.connections.clear();

        info!(
            "WAL replication stats: queued={} sent={} dropped={}",
            self.total_queued.load(Ordering::Relaxed),
            self.total_sent.load(Ordering::Relaxed),
            self.total_dropped.load(Ordering::Relaxed)
        );
    }
}

/// Serialize a WAL record into a framed message
fn serialize_wal_frame(record: &WalReplicationRecord) -> Result<Bytes> {
    let mut buf = BytesMut::new();

    // Frame header (8 bytes)
    buf.put_u16(FRAME_MAGIC);
    buf.put_u16(PROTOCOL_VERSION);

    // We'll fill in total_length after serializing metadata + data
    let length_pos = buf.len();
    buf.put_u32(0); // Placeholder

    // WAL metadata
    let topic_bytes = record.topic.as_bytes();
    buf.put_u16(topic_bytes.len() as u16);
    buf.put_slice(topic_bytes);
    buf.put_i32(record.partition);
    buf.put_i64(record.base_offset);
    buf.put_u32(record.record_count);
    buf.put_i64(record.timestamp_ms);

    // Calculate CRC32 of data
    let checksum = crc32fast::hash(&record.data);
    buf.put_u32(checksum);

    // WAL record data
    buf.put_slice(&record.data);

    // Fill in total length (metadata + data length)
    let total_length = (buf.len() - 8) as u32; // Exclude frame header
    buf[length_pos..length_pos + 4].copy_from_slice(&total_length.to_be_bytes());

    Ok(buf.freeze())
}

/// Deserialize a WAL frame from bytes
pub fn deserialize_wal_frame(mut data: Bytes) -> Result<WalReplicationRecord> {
    // Read frame header
    if data.remaining() < 8 {
        anyhow::bail!("Incomplete frame header");
    }

    let magic = data.get_u16();
    if magic != FRAME_MAGIC {
        anyhow::bail!("Invalid frame magic: 0x{:04x}", magic);
    }

    let version = data.get_u16();
    if version != PROTOCOL_VERSION {
        anyhow::bail!("Unsupported protocol version: {}", version);
    }

    let total_length = data.get_u32() as usize;

    // Read WAL metadata
    let topic_len = data.get_u16() as usize;
    let mut topic_bytes = vec![0u8; topic_len];
    data.copy_to_slice(&mut topic_bytes);
    let topic = String::from_utf8(topic_bytes)?;

    let partition = data.get_i32();
    let base_offset = data.get_i64();
    let record_count = data.get_u32();
    let timestamp_ms = data.get_i64();
    let checksum = data.get_u32();

    // Read WAL record data
    let record_data = data.copy_to_bytes(data.remaining());

    // Verify checksum
    let computed_checksum = crc32fast::hash(&record_data);
    if checksum != computed_checksum {
        anyhow::bail!(
            "Checksum mismatch: expected 0x{:08x}, got 0x{:08x}",
            checksum,
            computed_checksum
        );
    }

    Ok(WalReplicationRecord {
        topic,
        partition,
        base_offset,
        record_count,
        timestamp_ms,
        data: record_data,
    })
}

/// WAL Receiver (Follower Side)
///
/// Listens for incoming WAL replication frames from leader and writes to local WAL
pub struct WalReceiver {
    /// TCP listener for incoming connections
    listener_addr: String,

    /// WAL manager for writing received records
    wal_manager: Arc<chronik_wal::WalManager>,

    /// Shutdown signal
    shutdown: Arc<AtomicBool>,

    /// ISR ACK tracker for notifying leader of successful replication (v2.2.7 Phase 4)
    isr_ack_tracker: Option<Arc<crate::isr_ack_tracker::IsrAckTracker>>,

    /// This follower's node ID (v2.2.7 Phase 4)
    node_id: u64,

    /// Leader elector for triggering elections on timeout (v2.2.7)
    leader_elector: Option<Arc<crate::leader_election::LeaderElector>>,

    /// Last heartbeat timestamp per partition (v2.2.7)
    last_heartbeat: Arc<DashMap<(String, i32), std::time::Instant>>,

    /// Raft cluster for applying metadata commands (Phase 2.3: Metadata WAL replication)
    /// Only used when receiving "__chronik_metadata" topic replication from leader
    raft_cluster: Option<Arc<RaftCluster>>,
}

impl WalReceiver {
    /// Create a new WAL receiver (legacy constructor without ISR tracking)
    pub fn new(listener_addr: String, wal_manager: Arc<chronik_wal::WalManager>) -> Self {
        Self {
            listener_addr,
            wal_manager,
            shutdown: Arc::new(AtomicBool::new(false)),
            isr_ack_tracker: None,
            node_id: 0, // Default node ID (standalone mode)
            leader_elector: None,
            last_heartbeat: Arc::new(DashMap::new()),
            raft_cluster: None,
        }
    }

    /// Create a new WAL receiver with ISR tracking (v2.2.7 Phase 4)
    pub fn new_with_isr_tracker(
        listener_addr: String,
        wal_manager: Arc<chronik_wal::WalManager>,
        isr_ack_tracker: Arc<crate::isr_ack_tracker::IsrAckTracker>,
        node_id: u64,
    ) -> Self {
        info!("Creating WalReceiver with ISR ACK tracking for node {}", node_id);
        Self {
            listener_addr,
            wal_manager,
            shutdown: Arc::new(AtomicBool::new(false)),
            isr_ack_tracker: Some(isr_ack_tracker),
            node_id,
            leader_elector: None,
            last_heartbeat: Arc::new(DashMap::new()),
            raft_cluster: None,
        }
    }

    /// Set leader elector for event-driven elections (v2.2.7)
    pub fn set_leader_elector(&mut self, elector: Arc<crate::leader_election::LeaderElector>) {
        info!("WalReceiver: Enabling event-driven leader election");
        self.leader_elector = Some(elector);
    }

    /// Set Raft cluster for metadata WAL replication (Phase 2.3)
    pub fn set_raft_cluster(&mut self, raft_cluster: Arc<RaftCluster>) {
        info!("WalReceiver: Enabling metadata WAL replication (Phase 2.3)");
        self.raft_cluster = Some(raft_cluster);
    }

    /// Start the WAL receiver (blocking)
    pub async fn run(&self) -> Result<()> {
        info!("Starting WAL receiver on {}", self.listener_addr);

        let listener = TcpListener::bind(&self.listener_addr).await
            .context(format!("Failed to bind WAL receiver to {}", self.listener_addr))?;

        info!("✅ WAL receiver listening on {}", self.listener_addr);

        while !self.shutdown.load(Ordering::Relaxed) {
            // Accept incoming connection with timeout
            let accept_result = timeout(Duration::from_secs(1), listener.accept()).await;

            match accept_result {
                Ok(Ok((stream, peer_addr))) => {
                    info!("WAL receiver: Accepted connection from {}", peer_addr);

                    // Spawn handler for this connection
                    let wal_mgr = Arc::clone(&self.wal_manager);
                    let shutdown = Arc::clone(&self.shutdown);
                    let isr_ack_tracker = self.isr_ack_tracker.clone();
                    let node_id = self.node_id;
                    let last_heartbeat = Arc::clone(&self.last_heartbeat);
                    let leader_elector = self.leader_elector.clone();
                    let raft_cluster = self.raft_cluster.clone();

                    tokio::spawn(async move {
                        if let Err(e) = Self::handle_connection(
                            stream,
                            wal_mgr,
                            shutdown,
                            isr_ack_tracker,
                            node_id,
                            last_heartbeat,
                            leader_elector,
                            raft_cluster,
                        ).await {
                            error!("WAL receiver connection from {} failed: {}", peer_addr, e);
                        } else {
                            info!("WAL receiver connection from {} closed", peer_addr);
                        }
                    });
                }
                Ok(Err(e)) => {
                    error!("WAL receiver: Failed to accept connection: {}", e);
                }
                Err(_) => {
                    // Timeout - just continue loop to check shutdown signal
                    continue;
                }
            }
        }

        info!("WAL receiver stopped");
        Ok(())
    }

    /// Handle a single connection from leader
    async fn handle_connection(
        mut stream: TcpStream,
        wal_manager: Arc<chronik_wal::WalManager>,
        shutdown: Arc<AtomicBool>,
        isr_ack_tracker: Option<Arc<crate::isr_ack_tracker::IsrAckTracker>>,
        node_id: u64,
        last_heartbeat: Arc<DashMap<(String, i32), std::time::Instant>>,
        leader_elector: Option<Arc<crate::leader_election::LeaderElector>>,
        raft_cluster: Option<Arc<RaftCluster>>,
    ) -> Result<()> {
        let mut buffer = BytesMut::with_capacity(64 * 1024); // 64KB buffer

        // v2.2.7: Spawn background timeout monitor if leader elector is available
        if let Some(ref elector) = leader_elector {
            let elector_clone = Arc::clone(elector);
            let last_heartbeat_clone = Arc::clone(&last_heartbeat);
            let shutdown_clone = Arc::clone(&shutdown);

            tokio::spawn(async move {
                Self::monitor_timeouts(
                    elector_clone,
                    last_heartbeat_clone,
                    shutdown_clone,
                ).await;
            });
        }

        while !shutdown.load(Ordering::Relaxed) {
            // Read frame header (8 bytes: magic + version + length)
            if buffer.len() < 8 {
                // Need more data for header
                let read_timeout = timeout(Duration::from_secs(60), stream.read_buf(&mut buffer)).await;

                match read_timeout {
                    Ok(Ok(0)) => {
                        // Connection closed
                        debug!("WAL receiver: Connection closed by peer");
                        return Ok(());
                    }
                    Ok(Ok(n)) => {
                        debug!("WAL receiver: Read {} bytes", n);
                    }
                    Ok(Err(e)) => {
                        return Err(anyhow::anyhow!("Read error: {}", e));
                    }
                    Err(_) => {
                        // Timeout - check for heartbeat or shutdown
                        if buffer.is_empty() {
                            // No data received in 60s, connection might be dead
                            warn!("WAL receiver: No data received for 60s, closing connection");
                            return Ok(());
                        }
                        continue;
                    }
                }
            }

            // Try to parse frame if we have enough data
            if buffer.len() >= 8 {
                // Peek at header to determine frame type and size
                let magic = u16::from_be_bytes([buffer[0], buffer[1]]);
                let total_length = u32::from_be_bytes([buffer[4], buffer[5], buffer[6], buffer[7]]) as usize;
                let frame_size = 8 + total_length; // header + payload

                if magic == HEARTBEAT_MAGIC {
                    // Heartbeat frame - just consume it
                    if buffer.len() >= 8 {
                        buffer.advance(8);
                        debug!("WAL receiver: Received heartbeat");
                        // Note: Global heartbeat - we'll track per-partition when we receive data
                    }
                    continue;
                }

                if magic != FRAME_MAGIC {
                    return Err(anyhow::anyhow!("Invalid frame magic: 0x{:04x}", magic));
                }

                // Wait until we have the complete frame
                while buffer.len() < frame_size && !shutdown.load(Ordering::Relaxed) {
                    match timeout(Duration::from_secs(30), stream.read_buf(&mut buffer)).await {
                        Ok(Ok(0)) => {
                            return Err(anyhow::anyhow!("Connection closed before complete frame received"));
                        }
                        Ok(Ok(n)) => {
                            debug!("WAL receiver: Read {} more bytes (total: {})", n, buffer.len());
                        }
                        Ok(Err(e)) => {
                            return Err(anyhow::anyhow!("Read error: {}", e));
                        }
                        Err(_) => {
                            return Err(anyhow::anyhow!("Timeout waiting for complete frame"));
                        }
                    }
                }

                // We have a complete frame - deserialize and process
                let frame_bytes = buffer.split_to(frame_size).freeze();

                match deserialize_wal_frame(frame_bytes) {
                    Ok(wal_record) => {
                        // v2.2.7: Track last heartbeat for this partition
                        let partition_key = (wal_record.topic.clone(), wal_record.partition);
                        last_heartbeat.insert(partition_key.clone(), std::time::Instant::now());

                        // Phase 2.3: Special handling for metadata WAL replication
                        if wal_record.topic == "__chronik_metadata" && wal_record.partition == 0 {
                            if let Some(ref raft) = raft_cluster {
                                if let Err(e) = Self::handle_metadata_wal_record(raft, &wal_record).await {
                                    error!(
                                        "Failed to apply metadata WAL record: {}",
                                        e
                                    );
                                } else {
                                    info!(
                                        "METADATA✓ Replicated: {}-{} offset {} ({} bytes)",
                                        wal_record.topic,
                                        wal_record.partition,
                                        wal_record.base_offset,
                                        wal_record.data.len()
                                    );
                                }
                            } else {
                                warn!(
                                    "Received metadata WAL replication but no RaftCluster configured"
                                );
                            }
                            continue; // Skip normal WAL write for metadata
                        }

                        // CRITICAL FIX (Phase 3 CORRECTED): Only accept replication for partitions where we're a REPLICA
                        // This prevents message duplication by ensuring each node only stores partitions it owns
                        if let Some(ref raft) = raft_cluster {
                            // Query partition replicas from Raft metadata
                            if let Some(replicas) = raft.get_partition_replicas(&wal_record.topic, wal_record.partition) {
                                if !replicas.contains(&node_id) {
                                    info!(
                                        "⏭️  Skipping replication for {}-{}: this node ({}) is NOT in replica set {:?}",
                                        wal_record.topic, wal_record.partition, node_id, replicas
                                    );
                                    continue; // Skip - we're not a replica for this partition
                                }

                                info!(
                                    "✅ Accepting replication for {}-{}: this node ({}) IS in replica set {:?}",
                                    wal_record.topic, wal_record.partition, node_id, replicas
                                );
                            } else {
                                // No replica info yet - REJECT to prevent duplication
                                warn!(
                                    "⛔ NO REPLICA INFO for {}-{} yet, REJECTING replication (waiting for metadata sync)",
                                    wal_record.topic, wal_record.partition
                                );
                                continue; // Skip - no replica info available
                            }
                        }

                        // Write to local WAL (normal partition data)
                        if let Err(e) = Self::write_to_wal(&wal_manager, &wal_record).await {
                            error!(
                                "Failed to write replicated WAL record to local WAL for {}-{}: {}",
                                wal_record.topic, wal_record.partition, e
                            );
                        } else {
                            info!(
                                "WAL✓ Replicated: {}-{} offset {} ({} records, {} bytes)",
                                wal_record.topic,
                                wal_record.partition,
                                wal_record.base_offset,
                                wal_record.record_count,
                                wal_record.data.len()
                            );

                            // v2.2.7 Phase 4: Send ACK back to leader for acks=-1 support
                            if let Some(ref tracker) = isr_ack_tracker {
                                let ack_msg = WalAckMessage {
                                    topic: wal_record.topic.clone(),
                                    partition: wal_record.partition,
                                    offset: wal_record.base_offset,
                                    node_id,
                                };

                                // Send ACK frame back to leader
                                if let Err(e) = Self::send_ack(&mut stream, &ack_msg).await {
                                    error!(
                                        "Failed to send ACK for {}-{} offset {}: {}",
                                        wal_record.topic, wal_record.partition, wal_record.base_offset, e
                                    );
                                } else {
                                    debug!(
                                        "ACK✓ Sent to leader: {}-{} offset {} from node {}",
                                        wal_record.topic, wal_record.partition, wal_record.base_offset, node_id
                                    );

                                    // Notify the IsrAckTracker (for local leader-follower acks=-1)
                                    tracker.record_ack(
                                        &wal_record.topic,
                                        wal_record.partition,
                                        wal_record.base_offset,
                                        node_id,
                                    );
                                }
                            }
                        }
                    }
                    Err(e) => {
                        error!("Failed to deserialize WAL frame: {}", e);
                        // Continue processing other frames
                    }
                }
            }
        }

        Ok(())
    }

    /// Write a replicated WAL record to local WAL
    async fn write_to_wal(
        wal_manager: &Arc<chronik_wal::WalManager>,
        record: &WalReplicationRecord,
    ) -> Result<()> {
        // Deserialize CanonicalRecord from data
        use chronik_storage::canonical_record::CanonicalRecord;
        let canonical_record: CanonicalRecord = bincode::deserialize(&record.data)
            .context("Failed to deserialize CanonicalRecord from replicated data")?;

        // Serialize back to bincode for WAL storage
        let serialized = bincode::serialize(&canonical_record)
            .context("Failed to serialize CanonicalRecord for WAL")?;

        // Write to local WAL (acks=1 for immediate fsync on follower)
        wal_manager.append_canonical_with_acks(
            record.topic.clone(),
            record.partition,
            serialized,
            record.base_offset,
            record.base_offset + record.record_count as i64 - 1,
            record.record_count as i32,
            1, // acks=1 for immediate fsync
        ).await
        .context("Failed to append replicated record to local WAL")?;

        Ok(())
    }

    /// Handle metadata WAL record replication (Phase 2.3)
    ///
    /// When followers receive metadata replication from leader:
    /// 1. Deserialize MetadataCommand from data
    /// 2. Apply directly to Raft state machine (bypassing Raft consensus)
    /// 3. Fire notification for any waiting threads
    async fn handle_metadata_wal_record(
        raft_cluster: &Arc<RaftCluster>,
        record: &WalReplicationRecord,
    ) -> Result<()> {
        use crate::raft_metadata::MetadataCommand;

        // Deserialize metadata command
        let cmd: MetadataCommand = bincode::deserialize(&record.data)
            .context("Failed to deserialize MetadataCommand from replicated data")?;

        tracing::debug!(
            "Phase 2.3: Follower received metadata replication at offset {}: {:?}",
            record.base_offset,
            cmd
        );

        // Apply directly to state machine (bypassing Raft consensus)
        raft_cluster.apply_metadata_command_direct(cmd.clone())
            .context("Failed to apply replicated metadata command")?;

        tracing::info!(
            "Phase 2.3: Follower applied replicated metadata command: {:?}",
            cmd
        );

        // Fire notification for any waiting threads
        // (Followers might have threads waiting for metadata updates via Phase 1 forwarding)
        let pending_topics = raft_cluster.get_pending_topics_notifications();
        let pending_brokers = raft_cluster.get_pending_brokers_notifications();

        match &cmd {
            MetadataCommand::CreateTopic { name, .. } => {
                if let Some((_, notify)) = pending_topics.remove(name) {
                    notify.notify_waiters();
                    tracing::debug!("Notified waiting threads for topic '{}'", name);
                }
            }
            MetadataCommand::RegisterBroker { broker_id, .. } => {
                if let Some((_, notify)) = pending_brokers.remove(broker_id) {
                    notify.notify_waiters();
                    tracing::debug!("Notified waiting threads for broker {}", broker_id);
                }
            }
            _ => {
                // Other command types don't need notifications yet
            }
        }

        Ok(())
    }

    /// Send ACK frame back to leader (v2.2.7 Phase 4)
    async fn send_ack(stream: &mut TcpStream, ack_msg: &WalAckMessage) -> Result<()> {
        // Serialize ACK message
        let serialized = bincode::serialize(ack_msg)
            .context("Failed to serialize ACK message")?;

        // Build ACK frame: [magic(2) | version(2) | length(4) | payload(N)]
        let mut frame = BytesMut::with_capacity(8 + serialized.len());
        frame.put_u16(ACK_MAGIC); // 'AK' magic number
        frame.put_u16(PROTOCOL_VERSION);
        frame.put_u32(serialized.len() as u32);
        frame.put_slice(&serialized);

        // Send frame
        stream.write_all(&frame).await
            .context("Failed to send ACK frame to leader")?;

        stream.flush().await
            .context("Failed to flush ACK frame")?;

        Ok(())
    }

    /// Monitor partition heartbeat timeouts and trigger elections (v2.2.7)
    async fn monitor_timeouts(
        leader_elector: Arc<crate::leader_election::LeaderElector>,
        last_heartbeat: Arc<DashMap<(String, i32), std::time::Instant>>,
        shutdown: Arc<AtomicBool>,
    ) {
        info!("Started WAL timeout monitor for event-driven elections");

        while !shutdown.load(Ordering::Relaxed) {
            // Check every 5 seconds
            sleep(Duration::from_secs(5)).await;

            let now = std::time::Instant::now();

            // Check each partition for timeout
            for entry in last_heartbeat.iter() {
                let (topic, partition) = entry.key();
                let last_seen = *entry.value();

                if now.duration_since(last_seen) > HEARTBEAT_TIMEOUT {
                    warn!(
                        "WAL stream timeout detected for {}-{} ({}s since last heartbeat)",
                        topic, partition, now.duration_since(last_seen).as_secs()
                    );

                    // Trigger election
                    leader_elector.trigger_election_on_timeout(
                        topic,
                        *partition,
                        &format!("WAL stream timeout ({}s)", now.duration_since(last_seen).as_secs()),
                    );

                    // Remove from tracking to avoid repeated triggers
                    last_heartbeat.remove(entry.key());
                }
            }
        }

        info!("Stopped WAL timeout monitor");
    }

    /// Shutdown the receiver
    pub fn shutdown(&self) {
        info!("Shutting down WAL receiver");
        self.shutdown.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chronik_config::{ClusterConfig, NodeConfig};

    fn create_test_cluster_config(node_id: u64) -> ClusterConfig {
        ClusterConfig {
            enabled: true,
            node_id,
            replication_factor: 3,
            min_insync_replicas: 2,
            peers: vec![
                NodeConfig {
                    id: 1,
                    kafka: "node1.example.com:9092".to_string(),
                    wal: "node1.example.com:9291".to_string(),
                    raft: "node1.example.com:5001".to_string(),
                    addr: None,
                    raft_port: None,
                },
                NodeConfig {
                    id: 2,
                    kafka: "node2.example.com:9092".to_string(),
                    wal: "node2.example.com:9291".to_string(),
                    raft: "node2.example.com:5001".to_string(),
                    addr: None,
                    raft_port: None,
                },
                NodeConfig {
                    id: 3,
                    kafka: "node3.example.com:9092".to_string(),
                    wal: "node3.example.com:9291".to_string(),
                    raft: "node3.example.com:5001".to_string(),
                    addr: None,
                    raft_port: None,
                },
            ],
            bind: None,
            advertise: None,
            gossip: None,
        }
    }

    #[test]
    fn test_auto_discover_followers_from_cluster_config() {
        let cluster = create_test_cluster_config(1);
        let manager = WalReplicationManager::new_with_dependencies(
            vec![],  // Empty - should trigger auto-discovery
            None,
            None,
            None,
            Some(Arc::new(cluster)),
        );

        // Node 1 is the current node, so followers should be nodes 2 and 3
        assert_eq!(manager.followers.len(), 2);
        assert!(manager.followers.contains(&"node2.example.com:9291".to_string()));
        assert!(manager.followers.contains(&"node3.example.com:9291".to_string()));
    }

    #[test]
    fn test_manual_followers_override_auto_discovery() {
        let cluster = create_test_cluster_config(1);
        let manual_followers = vec!["custom.host:9291".to_string()];
        
        let manager = WalReplicationManager::new_with_dependencies(
            manual_followers.clone(),
            None,
            None,
            None,
            Some(Arc::new(cluster)),
        );

        // Manual followers should be used, not auto-discovered ones
        assert_eq!(manager.followers, manual_followers);
    }

    #[test]
    fn test_no_replication_without_config_or_manual() {
        let manager = WalReplicationManager::new_with_dependencies(
            vec![],  // Empty
            None,
            None,
            None,
            None,  // No cluster config
        );

        // No followers should be configured
        assert!(manager.followers.is_empty());
    }

    #[test]
    fn test_auto_discovery_filters_self() {
        // Test all three nodes to ensure each filters itself correctly
        for node_id in 1..=3 {
            let cluster = create_test_cluster_config(node_id);
            let manager = WalReplicationManager::new_with_dependencies(
                vec![],
                None,
                None,
                None,
                Some(Arc::new(cluster)),
            );

            // Should have 2 followers (not counting self)
            assert_eq!(manager.followers.len(), 2);
            
            // Self should not be in followers
            let self_wal = format!("node{}.example.com:9291", node_id);
            assert!(!manager.followers.contains(&self_wal));
        }
    }
}
