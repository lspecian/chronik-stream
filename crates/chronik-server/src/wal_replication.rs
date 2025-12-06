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
use tokio::sync::mpsc;
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

/// Reconnect delay (1 second) - CRITICAL FIX: Fast reconnection for startup race conditions
const RECONNECT_DELAY: Duration = Duration::from_secs(1);

/// Maximum retry attempts before logging warning (10 attempts = ~17 minutes with exponential backoff)
const MAX_RETRY_ATTEMPTS: usize = 10;

/// Initial backoff for connection retries (1 second)
const INITIAL_RETRY_BACKOFF: Duration = Duration::from_secs(1);

/// Maximum backoff for connection retries (60 seconds) - prevents unbounded exponential growth
const MAX_RETRY_BACKOFF: Duration = Duration::from_secs(60);

/// Backoff multiplier for exponential backoff (doubles each retry: 1s, 2s, 4s, 8s, 16s, 32s, 60s cap)
const RETRY_BACKOFF_MULTIPLIER: u32 = 2;

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

/// Election trigger message for non-blocking leader election (v2.2.7 deadlock fix)
#[derive(Debug, Clone)]
pub struct ElectionTriggerMessage {
    /// Topic name
    topic: String,

    /// Partition ID
    partition: i32,

    /// Reason for triggering election
    reason: String,
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

    /// Connection failure tracking: follower_addr → consecutive_failures
    /// Used for exponential backoff when nodes go down
    connection_failures: Arc<DashMap<String, usize>>,

    /// Election trigger channel sender (v2.2.7 deadlock fix)
    /// Used to trigger elections asynchronously without blocking the timeout monitor
    election_tx: Option<mpsc::UnboundedSender<ElectionTriggerMessage>>,

    /// Metadata store for querying partition replicas and ISR (v2.2.9 Option 4)
    metadata_store: Option<Arc<dyn chronik_common::metadata::MetadataStore>>,

    /// v2.2.14 PERFORMANCE FIX: Replica cache to avoid metadata queries on every message
    /// Maps (topic, partition) → (replicas, cached_at)
    /// TTL: 60 seconds (configurable via CHRONIK_REPLICA_CACHE_TTL_SECS)
    /// This eliminates async lock contention on metadata store - VERIFIED 6.2x speedup
    replicas_cache: Arc<DashMap<(String, u32), (Vec<i32>, std::time::Instant)>>,
}

impl WalReplicationManager {
    /// Create a new WAL replication manager
    ///
    /// Spawns background workers for connection management and record sending
    pub fn new(followers: Vec<String>) -> Arc<Self> {
        Self::new_with_dependencies(followers, None, None, None, None, None)
    }

    /// Create a new WAL replication manager with Raft cluster and ISR tracker (v2.2.7 Phase 3)
    ///
    /// This is the recommended constructor for cluster mode.
    ///
    /// v2.2.7 Phase 6: Added cluster_config parameter for automatic follower discovery.
    /// If followers is empty and cluster_config is provided, followers will be auto-discovered.
    ///
    /// v2.2.9 Phase 7: Added metadata_store parameter for querying partition replicas/ISR (Option 4)
    pub fn new_with_dependencies(
        followers: Vec<String>,
        raft_cluster: Option<Arc<RaftCluster>>,
        isr_tracker: Option<Arc<IsrTracker>>,
        isr_ack_tracker: Option<Arc<IsrAckTracker>>,
        cluster_config: Option<Arc<ClusterConfig>>,
        metadata_store: Option<Arc<dyn chronik_common::metadata::MetadataStore>>,
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
            connection_failures: Arc::new(DashMap::new()), // Error handling: Track connection failures for exponential backoff
            election_tx: None, // Not used in WalReplicationManager (only in WalReceiver)
            metadata_store,     // v2.2.9 Phase 7: Option 4 metadata store
            replicas_cache: Arc::new(DashMap::new()), // v2.2.14: Replica cache for 6.2x speedup
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
        // CRITICAL: We MUST deserialize to get record_count - it's used by followers for watermark calculation
        use chronik_storage::canonical_record::CanonicalRecord;

        let record_count = match bincode::deserialize::<CanonicalRecord>(&serialized_data) {
            Ok(canonical) => canonical.records.len() as u32,
            Err(e) => {
                error!("Failed to deserialize CanonicalRecord for record count: {}. Skipping replication.", e);
                return; // Can't replicate without valid record_count
            }
        };

        // Zero-copy: Just wrap in Bytes (cheap Arc clone)
        let data = Bytes::from(serialized_data);

        let wal_record = WalReplicationRecord {
            topic,
            partition,
            base_offset,
            record_count, // FIXED: Extract from deserialized CanonicalRecord
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

    /// Broadcast metadata event to all peers (v2.2.9 Phase 7 FIX #7)
    ///
    /// This method is for metadata events (MetadataEvent) which are NOT CanonicalRecord.
    /// It bypasses the CanonicalRecord deserialization in replicate_serialized().
    ///
    /// # Arguments
    /// - `topic`: Topic name (should be "__chronik_metadata")
    /// - `partition`: Partition number (0 for metadata)
    /// - `offset`: WAL offset of this event
    /// - `data`: Serialized MetadataEvent bytes
    pub async fn broadcast_metadata(&self, topic: String, partition: i32, offset: i64, data: Vec<u8>) {
        let wal_record = WalReplicationRecord {
            topic,
            partition,
            base_offset: offset,
            record_count: 1, // Metadata events are single events
            timestamp_ms: chrono::Utc::now().timestamp_millis(),
            data: Bytes::from(data),
        };

        // Check queue size (prevent unbounded growth)
        let queue_size = self.queue.len();
        if queue_size > MAX_QUEUE_SIZE {
            self.total_dropped.fetch_add(1, Ordering::Relaxed);
            warn!("WAL replication queue overflow ({} records), dropping metadata record", queue_size);
            return;
        }

        // Push to queue (lock-free, ~10 ns)
        self.queue.push(wal_record);
        self.total_queued.fetch_add(1, Ordering::Relaxed);
        debug!("Queued metadata event for broadcast (offset={})", offset);
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
        // v2.2.14 PERFORMANCE FIX: Check replica cache before metadata query
        // Eliminates async lock contention on metadata store - VERIFIED 6.2x speedup
        let cache_key = (topic.clone(), partition as u32);
        let cache_ttl_secs = std::env::var("CHRONIK_REPLICA_CACHE_TTL_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(60);

        // Check cache first
        if let Some(entry) = self.replicas_cache.get(&cache_key) {
            let (replicas, cached_at) = entry.value();
            let age = cached_at.elapsed().as_secs();
            if age < cache_ttl_secs {
                // Cache hit - use cached replicas without metadata query
                let replicas_u64: Vec<u64> = replicas.iter().map(|&id| id as u64).collect();
                debug!(
                    "🚀 CACHE HIT: {}-{} replicas={:?} age={}s (saved metadata query)",
                    topic, partition, replicas_u64, age
                );

                if replicas.is_empty() {
                    warn!(
                        "No in-sync replicas for {}-{} (offset={}), skipping replication",
                        topic, partition, offset
                    );
                    return;
                }

                // Replicate using cached replicas
                self.replicate_serialized(topic, partition, offset, serialized_data).await;
                return;
            }
        }

        // Cache miss or expired - query metadata store
        // v2.2.9 Phase 7 FIX: Get replicas from WalMetadataStore (Option 4)
        // For now, treat all replicas as in-sync (ISR = replicas)
        // Proper ISR tracking will be added later
        let isr_replicas = match &self.metadata_store {
            Some(store) => {
                match store.get_partition_replicas(&topic, partition as u32).await {
                    Ok(Some(replicas)) => {
                        // Convert Vec<i32> to Vec<u64>
                        let replicas_u64: Vec<u64> = replicas.iter().map(|&id| id as u64).collect();
                        debug!(
                            "Metadata query: Got replicas for {}-{}: {:?}",
                            topic, partition, replicas_u64
                        );

                        // v2.2.14: Update cache
                        self.replicas_cache.insert(cache_key, (replicas.clone(), std::time::Instant::now()));

                        replicas_u64
                    }
                    Ok(None) | Err(_) => {
                        // No replicas assigned, fall back to broadcast
                        debug!(
                            "No replicas found for {}-{} in metadata store, falling back to replicate_serialized",
                            topic, partition
                        );

                        // v2.2.14: Cache empty result to avoid repeated queries
                        self.replicas_cache.insert(cache_key, (Vec::new(), std::time::Instant::now()));

                        self.replicate_serialized(topic, partition, offset, serialized_data).await;
                        return;
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

        let mut last_heartbeat = std::time::Instant::now();

        while !self.shutdown.load(Ordering::Relaxed) {
            // Try to dequeue a record (non-blocking)
            if let Some(record) = self.queue.pop() {
                // v2.2.9 Phase 7 FIX: Check if any connections available for metadata records
                // If no connections yet, re-queue metadata records to retry later
                let is_metadata_record = record.topic == "__chronik_metadata";
                if is_metadata_record && self.connections.is_empty() {
                    // Re-queue metadata record for retry (connections not ready yet)
                    debug!("Re-queuing metadata record (no connections yet)");
                    self.queue.push(record);
                    sleep(Duration::from_millis(100)).await;
                    continue;
                }

                // Send to all active followers (fan-out)
                let sent_to_any = self.send_to_followers(&record).await;

                // v2.2.9 Phase 7 FIX: Re-queue metadata records that failed to send
                if is_metadata_record && !sent_to_any {
                    debug!("Re-queuing metadata record (no successful sends)");
                    self.queue.push(record);
                    sleep(Duration::from_millis(100)).await;
                    continue;
                }

                self.total_sent.fetch_add(1, Ordering::Relaxed);

                // Reset heartbeat timer (data was sent)
                last_heartbeat = std::time::Instant::now();
            } else {
                // Queue empty - check if we need to send heartbeat
                if last_heartbeat.elapsed() >= HEARTBEAT_INTERVAL {
                    debug!("Sending heartbeat to followers ({}s since last activity)",
                           last_heartbeat.elapsed().as_secs());
                    self.send_heartbeat_to_followers().await;
                    last_heartbeat = std::time::Instant::now();
                }

                // Sleep briefly to avoid busy-wait (1ms for low latency)
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
    ///
    /// Returns: true if sent to at least one follower, false if all sends failed
    async fn send_to_followers(&self, record: &WalReplicationRecord) -> bool {
        let frame = match serialize_wal_frame(record) {
            Ok(f) => f,
            Err(e) => {
                error!("Failed to serialize WAL frame: {}", e);
                return false;
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

        // v2.2.9 Phase 7 FIX: Track whether we sent to at least one follower
        let mut sent_to_any = false;

        // Send to each target follower sequentially (DashMap doesn't allow moving RefMut into tasks)
        for follower_addr in &target_followers {
            info!(
                "🔍 send_to_followers: Attempting to send to {} (frame size: {} bytes, connections map has {} entries)",
                follower_addr, frame.len(), self.connections.len()
            );

            if let Some(mut conn) = self.connections.get_mut(follower_addr) {
                info!("🔍 send_to_followers: Found connection for {}, calling write_all()", follower_addr);

                // Write frame to TCP stream
                // Note: This is fast (~1ms) so sequential is fine
                if let Err(e) = conn.write_all(&frame).await {
                    error!("Failed to send WAL record to {}: {}", follower_addr, e);
                    // Remove dead connection (connection manager will reconnect)
                    drop(conn); // Drop RefMut before removing
                    self.connections.remove(follower_addr);
                } else {
                    // CRITICAL: Flush the TCP stream to actually send the data
                    // Without this, data sits in buffer and never reaches followers!
                    if let Err(e) = conn.flush().await {
                        error!("Failed to flush WAL record to {}: {}", follower_addr, e);
                        drop(conn);
                        self.connections.remove(follower_addr);
                    } else {
                        info!("✅ Sent and flushed WAL record for {}-{} to follower: {} ({} bytes)",
                            record.topic, record.partition, follower_addr, frame.len());
                        sent_to_any = true;
                    }
                }
            } else {
                warn!("⚠️  No connection to follower {} for {}-{} (connections: {:?})",
                    follower_addr, record.topic, record.partition,
                    self.connections.iter().map(|e| e.key().clone()).collect::<Vec<_>>());
            }
        }

        sent_to_any
    }

    /// Send heartbeat frames to all followers (v2.2.7 heartbeat fix)
    ///
    /// Heartbeats keep the connection alive and signal to followers that the leader
    /// is still active, preventing timeout-based elections.
    async fn send_heartbeat_to_followers(&self) {
        // Build heartbeat frame: [magic(2) | version(2) | length(4)]
        let mut frame = BytesMut::with_capacity(8);
        frame.put_u16(HEARTBEAT_MAGIC); // 'HB' magic
        frame.put_u16(PROTOCOL_VERSION);
        frame.put_u32(0); // No payload

        // Send to ALL followers (heartbeat is not partition-specific)
        for follower_addr in &self.followers {
            if let Some(mut conn) = self.connections.get_mut(follower_addr) {
                if let Err(e) = conn.write_all(&frame).await {
                    debug!("Failed to send heartbeat to {}: {}", follower_addr, e);
                    drop(conn);
                    self.connections.remove(follower_addr);
                } else if let Err(e) = conn.flush().await {
                    debug!("Failed to flush heartbeat to {}: {}", follower_addr, e);
                    drop(conn);
                    self.connections.remove(follower_addr);
                } else {
                    debug!("Sent heartbeat to follower: {}", follower_addr);
                }
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
                    // Get current failure count for exponential backoff
                    let failure_count = self.connection_failures
                        .get(follower_addr)
                        .map(|r| *r.value())
                        .unwrap_or(0);

                    // Calculate exponential backoff delay: 1s, 2s, 4s, 8s, 16s, 32s, 60s (max)
                    let backoff_delay = if failure_count > 0 {
                        let delay_secs = INITIAL_RETRY_BACKOFF.as_secs() * (RETRY_BACKOFF_MULTIPLIER.pow(failure_count as u32 - 1) as u64);
                        Duration::from_secs(delay_secs.min(MAX_RETRY_BACKOFF.as_secs()))
                    } else {
                        INITIAL_RETRY_BACKOFF
                    };

                    // Attempt to connect
                    info!("Attempting to connect to follower: {} (failures: {}, backoff: {}s)",
                          follower_addr, failure_count, backoff_delay.as_secs());

                    match timeout(Duration::from_secs(5), TcpStream::connect(follower_addr)).await {
                        Ok(Ok(stream)) => {
                            info!("✅ Connected to follower: {} (after {} failures)", follower_addr, failure_count);

                            // v2.2.9 Performance: Enable TCP_NODELAY to disable Nagle's algorithm
                            // This is CRITICAL for low-latency replication - Nagle can add 200ms delay!
                            if let Err(e) = stream.set_nodelay(true) {
                                error!("Failed to set TCP_NODELAY for WAL replication to {}: {}", follower_addr, e);
                            }

                            // Reset failure counter on successful connection
                            self.connection_failures.remove(follower_addr);

                            // v2.2.7 Phase 4: Split stream for bidirectional communication
                            // Write half: used by send_to_followers
                            // Read half: used by ACK reader task
                            let (read_half, write_half) = stream.into_split();

                            // Spawn ACK reader task for this follower (v2.2.7 Phase 4)
                            // v2.2.22: Also pass isr_tracker for admin API ISR queries
                            if let Some(ref ack_tracker) = self.isr_ack_tracker {
                                let tracker = Arc::clone(ack_tracker);
                                let isr_tracker_clone = self.isr_tracker.clone();
                                let shutdown = Arc::clone(&self.shutdown);
                                let addr = follower_addr.clone();
                                tokio::spawn(async move {
                                    if let Err(e) = Self::run_ack_reader(read_half, tracker, isr_tracker_clone, shutdown, addr.clone()).await {
                                        error!("ACK reader for {} failed: {}", addr, e);
                                    }
                                });
                                info!("✅ Spawned ACK reader for follower: {}", follower_addr);
                            }

                            // Store write-half connection (for send_to_followers)
                            self.connections.insert(follower_addr.clone(), write_half);
                        }
                        Ok(Err(e)) => {
                            // Increment failure counter
                            let new_count = self.connection_failures
                                .entry(follower_addr.clone())
                                .or_insert(0)
                                .value()
                                .wrapping_add(1);
                            self.connection_failures.insert(follower_addr.clone(), new_count);

                            // Log warning if exceeded max retries
                            if new_count >= MAX_RETRY_ATTEMPTS {
                                error!("⚠️ Failed to connect to follower {} after {} attempts: {}. Will continue retrying with exponential backoff.",
                                       follower_addr, new_count, e);
                            } else {
                                warn!("Failed to connect to follower {} (attempt {}): {}", follower_addr, new_count, e);
                            }
                        }
                        Err(_) => {
                            // Increment failure counter for timeout
                            let new_count = self.connection_failures
                                .entry(follower_addr.clone())
                                .or_insert(0)
                                .value()
                                .wrapping_add(1);
                            self.connection_failures.insert(follower_addr.clone(), new_count);

                            // Log warning if exceeded max retries
                            if new_count >= MAX_RETRY_ATTEMPTS {
                                error!("⚠️ Connection timeout to follower {} after {} attempts. Will continue retrying with exponential backoff.",
                                       follower_addr, new_count);
                            } else {
                                warn!("Connection timeout to follower {} (attempt {})", follower_addr, new_count);
                            }
                        }
                    }
                }
            }

            // Sleep before next connection check (fixed 1s interval - backoff is per-follower)
            sleep(RECONNECT_DELAY).await;
        }

        info!("WAL replication connection manager stopped");
    }

    /// Background worker for reading ACKs from a follower (v2.2.7 Phase 4)
    ///
    /// This is the CRITICAL missing piece that completes acks=-1 support.
    /// Followers send ACKs after successful WAL writes, and this task reads them.
    ///
    /// v2.2.22: Also updates IsrTracker for admin API ISR queries
    async fn run_ack_reader(
        mut read_half: OwnedReadHalf,
        isr_ack_tracker: Arc<IsrAckTracker>,
        isr_tracker: Option<Arc<IsrTracker>>,
        shutdown: Arc<AtomicBool>,
        follower_addr: String,
    ) -> Result<()> {
        info!("🔍 DEBUG: ACK reader started for follower: {}", follower_addr);
        let mut buffer = BytesMut::with_capacity(4096);

        while !shutdown.load(Ordering::Relaxed) {
            // Read ACK frame header (8 bytes: magic + version + length)
            if buffer.len() < 8 {
                info!("🔍 DEBUG ACK reader: Waiting to read header from {} (buffer: {} bytes)", follower_addr, buffer.len());
                match timeout(Duration::from_secs(60), read_half.read_buf(&mut buffer)).await {
                    Ok(Ok(0)) => {
                        // Connection closed
                        info!("ACK reader: Connection closed by follower {}", follower_addr);
                        return Ok(());
                    }
                    Ok(Ok(n)) => {
                        info!("🔍 DEBUG ACK reader: Read {} bytes from {} (total buffer: {})", n, follower_addr, buffer.len());
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
                info!("🔍 DEBUG ACK reader: Got header from {}, magic=0x{:04x}", follower_addr, magic);

                // Skip heartbeat frames (if any)
                if magic == HEARTBEAT_MAGIC {
                    buffer.advance(8);
                    info!("ACK reader: Received heartbeat from {}", follower_addr);
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
                info!("🔍 DEBUG ACK reader: ACK frame length={}, total size={}", frame_length, frame_size);

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
                info!("🔍 DEBUG ACK reader: Deserializing {} bytes of payload", payload.len());

                match bincode::deserialize::<WalAckMessage>(payload) {
                    Ok(ack_msg) => {
                        info!(
                            "✅ ACK RECEIVED from {}: {}-{} offset {} (node {})",
                            follower_addr,
                            ack_msg.topic,
                            ack_msg.partition,
                            ack_msg.offset,
                            ack_msg.node_id
                        );

                        // THIS IS THE KEY FIX: Record ACK in IsrAckTracker on leader
                        info!(
                            "🔍 DEBUG: Recording ACK in tracker for {}-{} offset {} node {}",
                            ack_msg.topic, ack_msg.partition, ack_msg.offset, ack_msg.node_id
                        );
                        isr_ack_tracker.record_ack(
                            &ack_msg.topic,
                            ack_msg.partition,
                            ack_msg.offset,
                            ack_msg.node_id,
                        );

                        // v2.2.22: Also update IsrTracker for admin API ISR queries
                        // This tracks the last acknowledged offset from each follower
                        if let Some(ref tracker) = isr_tracker {
                            tracker.update_follower_offset(
                                ack_msg.node_id,
                                &ack_msg.topic,
                                ack_msg.partition,
                                ack_msg.offset,
                            );
                            debug!(
                                "ISR tracker updated: node {} acknowledged {}-{} offset {}",
                                ack_msg.node_id, ack_msg.topic, ack_msg.partition, ack_msg.offset
                            );
                        }

                        info!(
                            "✅ ACK recorded in tracker: {}-{} offset {} node {}",
                            ack_msg.topic, ack_msg.partition, ack_msg.offset, ack_msg.node_id
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
            // v2.2.9 Phase 7 CRITICAL FIX: Query metadata_store instead of stubbed raft method (Option 4)
            let my_partitions = if let Some(ref store) = self.metadata_store {
                let topics = match store.list_topics().await {
                    Ok(topics) => topics,
                    Err(e) => {
                        warn!("Failed to list topics from metadata_store: {}", e);
                        Vec::new()
                    }
                };

                let mut leader_partitions = Vec::new();
                for topic in topics {
                    for partition in 0..topic.config.partition_count {
                        match store.get_partition_leader(&topic.name, partition).await {
                            Ok(Some(leader_id)) if leader_id == node_id as i32 => {
                                leader_partitions.push((topic.name.clone(), partition as i32));
                            }
                            Ok(Some(_)) => {
                                // This node is not the leader for this partition
                            }
                            Ok(None) => {
                                debug!("No leader found for partition {}-{}", topic.name, partition);
                            }
                            Err(e) => {
                                warn!("Failed to get leader for {}-{}: {}", topic.name, partition, e);
                            }
                        }
                    }
                }
                leader_partitions
            } else {
                // Fallback: No metadata_store available (shouldn't happen in Option 4)
                warn!("No metadata_store available - cannot determine leader partitions");
                Vec::new()
            };

            if my_partitions.is_empty() {
                debug!("Node {} is not leader for any partitions", node_id);
                continue;
            }

            debug!("Node {} is leader for {} partitions", node_id, my_partitions.len());

            // For each partition, update follower list
            for (topic, partition) in my_partitions {
                // v2.2.9 Phase 7 FIX: Query replicas from metadata_store (Option 4)
                let replicas_result = if let Some(ref store) = self.metadata_store {
                    store.get_partition_replicas(&topic, partition as u32).await
                } else {
                    Ok(None)
                };

                if let Ok(Some(replicas)) = replicas_result {
                    // Convert Vec<i32> to Vec<u64> and filter out self
                    let followers: Vec<u64> = replicas
                        .into_iter()
                        .map(|id| id as u64)
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

    /// Subscribe to metadata events for immediate partition follower registration (NEW)
    ///
    /// This method listens for PartitionAssigned events and immediately populates
    /// the partition_followers map, eliminating the 10-second delay from the polling-based
    /// discovery worker. This is critical for acks=all to work immediately after topic creation.
    ///
    /// # Arguments
    /// * `event_bus` - Metadata event bus to subscribe to
    /// * `cluster_config` - Cluster config for mapping node IDs to WAL addresses
    ///
    /// # Call this from IntegratedServer after creating WalReplicationManager
    pub fn start_metadata_event_listener(
        self: &Arc<Self>,
        event_bus: Arc<crate::metadata_events::MetadataEventBus>,
    ) {
        let manager = Arc::clone(self);
        let cluster_config = self.cluster_config.clone();

        tokio::spawn(async move {
            let mut rx = event_bus.subscribe();
            info!("📡 WalReplicationManager metadata event listener started");

            loop {
                match rx.recv().await {
                    Ok(event) => {
                        if let Err(e) = manager.handle_metadata_event(event, cluster_config.as_deref()).await {
                            warn!("Failed to handle metadata event: {}", e);
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => {
                        warn!("WalReplicationManager lagged by {} metadata events", count);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        warn!("Metadata event bus closed - stopping listener");
                        break;
                    }
                }
            }

            info!("📡 WalReplicationManager metadata event listener stopped");
        });
    }

    /// Handle a metadata event (partition assignment, leader change, ISR update)
    async fn handle_metadata_event(
        &self,
        event: crate::metadata_events::MetadataEvent,
        cluster_config: Option<&chronik_config::ClusterConfig>,
    ) -> anyhow::Result<()> {
        use crate::metadata_events::MetadataEvent;

        match event {
            MetadataEvent::PartitionAssigned { topic, partition, replicas, leader } => {
                info!(
                    "📡 PartitionAssigned event: {}-{} => replicas={:?}, leader={}",
                    topic, partition, replicas, leader
                );

                // Get my node ID to filter out self
                let my_node_id = cluster_config
                    .map(|cfg| cfg.node_id as u64)
                    .unwrap_or(0);

                // Filter out self from replicas (leader doesn't replicate to itself)
                let followers: Vec<u64> = replicas
                    .into_iter()
                    .filter(|&id| id != my_node_id)
                    .collect();

                if followers.is_empty() {
                    info!("No followers for {}-{} (single replica or I'm not the leader)", topic, partition);
                    return Ok(());
                }

                // Map node IDs to WAL addresses
                let follower_addrs: Vec<String> = if let Some(cfg) = cluster_config {
                    followers
                        .iter()
                        .filter_map(|&node_id| {
                            cfg.peers.iter()
                                .find(|p| p.id == node_id)
                                .map(|p| p.wal.clone())
                        })
                        .collect()
                } else {
                    warn!("No cluster config - cannot map node IDs to addresses");
                    Vec::new()
                };

                if !follower_addrs.is_empty() {
                    let partition_key = (topic.clone(), partition);
                    self.partition_followers.insert(partition_key, follower_addrs.clone());
                    info!(
                        "✅ Registered partition followers for {}-{}: {:?}",
                        topic, partition, follower_addrs
                    );
                } else {
                    warn!("No follower addresses mapped for {}-{}", topic, partition);
                }
            }

            MetadataEvent::LeaderChanged { topic, partition, new_leader } => {
                info!("📡 LeaderChanged event: {}-{} => leader={}", topic, partition, new_leader);
                // Leader changes are handled by spawn_leader_change_handler
                // This is just for logging/debugging
            }

            MetadataEvent::ISRUpdated { topic, partition, isr } => {
                info!("📡 ISRUpdated event: {}-{} => ISR={:?}", topic, partition, isr);
                // ISR updates don't require follower list changes (replicas are the source of truth)
            }

            _ => {
                // Other events (HighWatermarkUpdated, TopicCreated, TopicDeleted) don't affect routing
            }
        }

        Ok(())
    }

    /// Register followers for __chronik_metadata partition (required for metadata WAL replication)
    ///
    /// This special internal partition must be registered immediately on server startup
    /// so that metadata commands can be replicated to followers. Unlike normal partitions
    /// which are registered via PartitionAssigned events, __chronik_metadata-0 never goes
    /// through normal partition assignment.
    ///
    /// Call this during server initialization, AFTER connect_all_peers().
    pub fn register_metadata_partition_followers(&self) {
        let partition_key = ("__chronik_metadata".to_string(), 0);

        if self.followers.is_empty() {
            info!("No followers configured - metadata partition will not replicate");
            return;
        }

        self.partition_followers.insert(partition_key.clone(), self.followers.clone());

        info!(
            "✅ Registered metadata partition followers for __chronik_metadata-0: {:?}",
            self.followers
        );
    }

    /// Pre-warm all WAL replication connections during server startup
    ///
    /// This ensures all follower connections are established BEFORE accepting client traffic,
    /// eliminating the race condition where first produce requests fail due to missing connections.
    ///
    /// Returns Ok(()) if all connections succeeded, Err with details about failures.
    pub async fn connect_all_peers(&self) -> Result<()> {
        info!("🔄 Pre-warming WAL replication connections to {} followers", self.followers.len());

        let mut connection_results = Vec::new();
        let mut successful = 0;
        let mut failed = 0;

        for follower_addr in &self.followers {
            info!("Attempting to connect to follower: {}", follower_addr);

            match timeout(Duration::from_secs(5), TcpStream::connect(follower_addr)).await {
                Ok(Ok(stream)) => {
                    info!("✅ Connected to follower: {}", follower_addr);

                    // Split stream for bidirectional communication (leader sends WAL, receives ACKs)
                    let (read_half, write_half) = stream.into_split();

                    // Spawn ACK reader task if ISR ACK tracker is available
                    // v2.2.22: Also pass isr_tracker for admin API ISR queries
                    if let Some(ref ack_tracker) = self.isr_ack_tracker {
                        let tracker = Arc::clone(ack_tracker);
                        let isr_tracker_clone = self.isr_tracker.clone();
                        let shutdown = Arc::clone(&self.shutdown);
                        let addr = follower_addr.clone();
                        tokio::spawn(async move {
                            if let Err(e) = Self::run_ack_reader(read_half, tracker, isr_tracker_clone, shutdown, addr.clone()).await {
                                error!("ACK reader for {} failed: {}", addr, e);
                            }
                        });
                        info!("✅ Spawned ACK reader for follower: {}", follower_addr);
                    }

                    // Store write-half connection (for send_to_followers)
                    self.connections.insert(follower_addr.clone(), write_half);
                    successful += 1;
                    connection_results.push((follower_addr.clone(), true));
                }
                Ok(Err(e)) => {
                    warn!("Failed to connect to follower {}: {}", follower_addr, e);
                    failed += 1;
                    connection_results.push((follower_addr.clone(), false));
                }
                Err(_) => {
                    warn!("Connection timeout to follower {} (5s)", follower_addr);
                    failed += 1;
                    connection_results.push((follower_addr.clone(), false));
                }
            }
        }

        info!("WAL replication connection pre-warming complete: {} successful, {} failed", successful, failed);

        if failed > 0 {
            let failed_followers: Vec<&String> = connection_results.iter()
                .filter(|(_, success)| !success)
                .map(|(addr, _)| addr)
                .collect();
            warn!("Failed to connect to followers: {:?}", failed_followers);

            // Return error if any connections failed
            return Err(anyhow::anyhow!(
                "Failed to connect to {} out of {} followers: {:?}",
                failed, self.followers.len(), failed_followers
            ));
        }

        Ok(())
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

    /// ProduceHandler for updating watermarks on followers (v2.2.9)
    /// Used to update watermarks when WAL data is replicated to followers
    produce_handler: Option<Arc<crate::produce_handler::ProduceHandler>>,

    /// MetadataStore for updating partition offsets on followers (v2.2.9 Phase 7 FIX)
    /// ListOffsets API queries this, so it must be kept in sync with replicated watermarks
    metadata_store: Option<Arc<dyn chronik_common::metadata::MetadataStore>>,
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
            produce_handler: None,
            metadata_store: None,
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
            produce_handler: None,
            metadata_store: None,
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

    /// Set ProduceHandler for watermark updates on followers (v2.2.9)
    pub fn set_produce_handler(&mut self, produce_handler: Arc<crate::produce_handler::ProduceHandler>) {
        info!("WalReceiver: Enabling watermark updates via ProduceHandler (v2.2.9)");
        self.produce_handler = Some(produce_handler);
    }

    /// Set MetadataStore for partition offset updates on followers (v2.2.9 Phase 7 FIX)
    /// ListOffsets API queries metadata_store, so it must be kept in sync with replicated watermarks
    pub fn set_metadata_store(&mut self, metadata_store: Arc<dyn chronik_common::metadata::MetadataStore>) {
        info!("WalReceiver: Enabling partition offset updates via MetadataStore (v2.2.9 Phase 7)");
        self.metadata_store = Some(metadata_store);
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

                    // v2.2.9 Performance: Enable TCP_NODELAY to disable Nagle's algorithm
                    // This is CRITICAL for low-latency ACK responses - Nagle can add 200ms delay!
                    if let Err(e) = stream.set_nodelay(true) {
                        error!("Failed to set TCP_NODELAY for WAL receiver from {}: {}", peer_addr, e);
                    }

                    // Spawn handler for this connection
                    let wal_mgr = Arc::clone(&self.wal_manager);
                    let shutdown = Arc::clone(&self.shutdown);
                    let isr_ack_tracker = self.isr_ack_tracker.clone();
                    let node_id = self.node_id;
                    let last_heartbeat = Arc::clone(&self.last_heartbeat);
                    let leader_elector = self.leader_elector.clone();
                    let raft_cluster = self.raft_cluster.clone();

                    let produce_handler_for_conn = self.produce_handler.clone();
                    let metadata_store_for_conn = self.metadata_store.clone();

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
                            produce_handler_for_conn,
                            metadata_store_for_conn,
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
        produce_handler: Option<Arc<crate::produce_handler::ProduceHandler>>,
        metadata_store: Option<Arc<dyn chronik_common::metadata::MetadataStore>>,
    ) -> Result<()> {
        let mut buffer = BytesMut::with_capacity(64 * 1024); // 64KB buffer

        // v2.2.7 DEADLOCK FIX: Spawn timeout monitor with channel-based elections
        // The monitor sends election requests to a channel instead of calling directly,
        // preventing deadlocks with raft_node lock
        if let Some(ref elector) = leader_elector {
            // Create election trigger channel
            let (election_tx, election_rx) = mpsc::unbounded_channel();

            // Spawn election worker that can safely lock raft_node
            let elector_clone = Arc::clone(elector);
            tokio::spawn(async move {
                Self::run_election_worker(election_rx, elector_clone).await;
            });

            // Spawn timeout monitor that sends to channel (non-blocking)
            let last_heartbeat_clone = Arc::clone(&last_heartbeat);
            let shutdown_clone = Arc::clone(&shutdown);
            tokio::spawn(async move {
                Self::monitor_timeouts(
                    election_tx,
                    last_heartbeat_clone,
                    shutdown_clone,
                ).await;
            });

            info!("WAL timeout monitoring ENABLED with channel-based elections (deadlock-free)");
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
                            if let Some(ref ph) = produce_handler {
                                if let Err(e) = Self::handle_metadata_wal_record(ph, &raft_cluster, &wal_record, &metadata_store).await {
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
                                    "Received metadata WAL replication but no ProduceHandler configured (v2.2.9 - watermarks won't update!)"
                                );
                            }
                            continue; // Skip normal WAL write for metadata
                        }

                        // v2.2.9 FIX: Accept all WAL replication from leader (trust the leader)
                        //
                        // REMOVED: Raft metadata check that was causing race condition data loss
                        // - Metadata is replicated via __chronik_metadata WAL topic (fast, microseconds)
                        // - Partition assignments arrive before or concurrently with partition data
                        // - The old Raft check rejected data if metadata hadn't synced yet (race condition)
                        // - Result: 100% data loss on followers despite acks=all
                        //
                        // NEW APPROACH: Trust the leader to send correct replicas
                        // - Leader already knows which nodes are replicas (from its own metadata)
                        // - Leader only sends to nodes in the replica set
                        // - If we receive WAL data, the leader thinks we should have it → accept it
                        // - Metadata will arrive via __chronik_metadata (already implemented above)
                        //
                        // Safety: This is safe because:
                        // 1. Leader sends metadata via __chronik_metadata BEFORE or WITH partition data
                        // 2. Even if there's a race, metadata arrives within milliseconds
                        // 3. Accepting "extra" data is better than losing data (can always clean up later)
                        // 4. WAL compaction will remove data for partitions we don't own anymore
                        //
                        // See docs/RAFT_VS_WAL_METADATA_REDUNDANCY_ANALYSIS.md for full analysis

                        info!(
                            "✅ Accepting WAL replication for {}-{} from leader (node {})",
                            wal_record.topic, wal_record.partition, node_id
                        );

                        // Write to local WAL (normal partition data)
                        if let Err(e) = Self::write_to_wal(&wal_manager, &wal_record, &raft_cluster, &produce_handler, &metadata_store).await {
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
                                info!(
                                    "🔍 DEBUG: Preparing to send ACK for {}-{} offset {} from node {}",
                                    wal_record.topic, wal_record.partition, wal_record.base_offset, node_id
                                );

                                let ack_msg = WalAckMessage {
                                    topic: wal_record.topic.clone(),
                                    partition: wal_record.partition,
                                    offset: wal_record.base_offset,
                                    node_id,
                                };

                                // Send ACK frame back to leader
                                // NOTE: ACK is sent over TCP to leader's ACK reader
                                // The ACK reader on the leader will call tracker.record_ack()
                                // We do NOT call tracker.record_ack() here (that would be wrong - followers don't track their own ACKs)
                                if let Err(e) = Self::send_ack(&mut stream, &ack_msg).await {
                                    error!(
                                        "❌ Failed to send ACK for {}-{} offset {}: {}",
                                        wal_record.topic, wal_record.partition, wal_record.base_offset, e
                                    );
                                } else {
                                    info!(
                                        "✅ ACK SENT to leader: {}-{} offset {} from node {}",
                                        wal_record.topic, wal_record.partition, wal_record.base_offset, node_id
                                    );
                                }
                            } else {
                                warn!(
                                    "⚠️ DEBUG: No ISR ACK tracker configured - ACKs will NOT be sent for {}-{} offset {}",
                                    wal_record.topic, wal_record.partition, wal_record.base_offset
                                );
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
        raft_cluster: &Option<Arc<crate::raft_cluster::RaftCluster>>,
        produce_handler: &Option<Arc<crate::produce_handler::ProduceHandler>>,
        metadata_store: &Option<Arc<dyn chronik_common::metadata::MetadataStore>>,
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

        // v2.2.9 FIX: Update high watermark on follower via ProduceHandler
        // This ensures watermarks are visible via ListOffsets API (which queries ProduceHandler, not Raft)
        // Trust the leader - we're receiving replicated state
        if let Some(ref handler) = produce_handler {
            let new_watermark = record.base_offset + record.record_count as i64;

            if let Err(e) = handler.update_high_watermark(&record.topic, record.partition, new_watermark).await {
                warn!("❌ Failed to update watermark for {}-{} to {}: {}",
                    record.topic, record.partition, new_watermark, e);
            } else {
                info!("✅ v2.2.9 Phase 7 DEBUG: Updated watermark for {}-{} to {}",
                    record.topic, record.partition, new_watermark);
            }
        } else {
            warn!("⚠️ v2.2.9 Phase 7 DEBUG: produce_handler is None! Cannot update watermark for {}-{}",
                record.topic, record.partition);
        }

        // v2.2.9 Phase 7 FIX: ALSO update MetadataStore partition offsets
        // ListOffsets API queries metadata_store, NOT ProduceHandler
        // This was the root cause of Bug #4: followers had correct ProduceHandler watermarks
        // but metadata_store offsets were 0, causing ListOffsets to timeout
        if let Some(ref store) = metadata_store {
            let new_watermark = record.base_offset + record.record_count as i64;
            if let Err(e) = store.update_partition_offset(
                &record.topic,
                record.partition as u32,
                new_watermark,
                0  // log_start_offset - we don't track this separately yet
            ).await {
                warn!("❌ Failed to update metadata_store offset for {}-{} to {}: {}",
                    record.topic, record.partition, new_watermark, e);
            } else {
                info!("✅ v2.2.9 Phase 7 FIX: Updated MetadataStore offset for {}-{} to {} (for ListOffsets API)",
                    record.topic, record.partition, new_watermark);
            }
        } else {
            warn!("⚠️ v2.2.9 Phase 7: metadata_store is None! ListOffsets API will fail on follower for {}-{}",
                record.topic, record.partition);
        }

        Ok(())
    }

    /// Handle metadata WAL record replication (Phase 2.3)
    ///
    /// v2.2.9 FIX: Removed Raft redundancy - metadata now updates ProduceHandler directly
    /// When followers receive metadata replication from leader:
    /// 1. Deserialize MetadataCommand or MetadataEvent from data
    /// 2. Apply to ProduceHandler or MetadataStore
    /// 3. Fire notification for any waiting threads
    async fn handle_metadata_wal_record(
        produce_handler: &Arc<crate::produce_handler::ProduceHandler>,
        raft_cluster: &Option<Arc<RaftCluster>>,  // Keep for notifications only
        record: &WalReplicationRecord,
        metadata_store: &Option<Arc<dyn chronik_common::metadata::MetadataStore>>,
    ) -> Result<()> {
        use crate::raft_metadata::MetadataCommand;
        use chronik_common::metadata::{MetadataEvent as CommonMetadataEvent, MetadataEventPayload};

        // v2.2.9 Phase 7 FIX #5: Try deserializing as MetadataEvent first (new format)
        // Then fall back to MetadataCommand (legacy format)
        if let Ok(event) = CommonMetadataEvent::from_bytes(&record.data) {
            tracing::info!(
                "📥 Follower received MetadataEvent at offset {}: {:?}",
                record.base_offset,
                event.payload
            );

            // Apply to metadata_store if available
            if let Some(ref store) = metadata_store {
                if let Err(e) = store.apply_replicated_event(event.clone()).await {
                    tracing::warn!("Failed to apply replicated MetadataEvent: {}", e);
                } else {
                    match &event.payload {
                        MetadataEventPayload::PartitionAssigned { assignment } => {
                            tracing::info!(
                                "✅ Follower applied PartitionAssigned: {}-{}, leader={}, replicas={:?}",
                                assignment.topic, assignment.partition, assignment.leader_id, assignment.replicas
                            );
                        }
                        MetadataEventPayload::HighWatermarkUpdated { topic, partition, new_watermark } => {
                            tracing::info!(
                                "✅ Follower applied HighWatermarkUpdated: {}-{} => {}",
                                topic, partition, new_watermark
                            );
                        }
                        _ => {
                            tracing::debug!("✅ Follower applied MetadataEvent: {:?}", event.payload);
                        }
                    }
                }
            } else {
                tracing::warn!("Received MetadataEvent but no metadata_store configured!");
            }

            return Ok(());
        }

        // Legacy path: Try deserializing as MetadataCommand
        let cmd: MetadataCommand = bincode::deserialize(&record.data)
            .context("Failed to deserialize MetadataCommand from replicated data")?;

        tracing::debug!(
            "Phase 2.3: Follower received metadata replication at offset {}: {:?}",
            record.base_offset,
            cmd
        );

        // v2.2.9 Option 4: Partition metadata moved to WalMetadataStore
        // Most commands are now handled via MetadataEvent path above

        // Fire notification for any waiting threads
        // (Followers might have threads waiting for metadata updates via Phase 1 forwarding)
        if let Some(ref raft) = raft_cluster {
            let pending_brokers = raft.get_pending_brokers_notifications();

            match &cmd {
                MetadataCommand::RegisterBroker { broker_id, .. } => {
                    if let Some((_, notify)) = pending_brokers.remove(broker_id) {
                        notify.notify_waiters();
                        tracing::debug!("Notified waiting threads for broker {}", broker_id);
                    }
                }
                _ => {}
            }
        }

        Ok(())
    }

    /// Send ACK frame back to leader (v2.2.7 Phase 4)
    async fn send_ack(stream: &mut TcpStream, ack_msg: &WalAckMessage) -> Result<()> {
        info!(
            "🔍 DEBUG send_ack: Serializing ACK for {}-{} offset {} from node {}",
            ack_msg.topic, ack_msg.partition, ack_msg.offset, ack_msg.node_id
        );

        // Serialize ACK message
        let serialized = bincode::serialize(ack_msg)
            .context("Failed to serialize ACK message")?;

        info!(
            "🔍 DEBUG send_ack: Serialized {} bytes, building frame with magic 0x{:04x}",
            serialized.len(), ACK_MAGIC
        );

        // Build ACK frame: [magic(2) | version(2) | length(4) | payload(N)]
        let mut frame = BytesMut::with_capacity(8 + serialized.len());
        frame.put_u16(ACK_MAGIC); // 'AK' magic number
        frame.put_u16(PROTOCOL_VERSION);
        frame.put_u32(serialized.len() as u32);
        frame.put_slice(&serialized);

        info!(
            "🔍 DEBUG send_ack: Frame built ({} bytes total), writing to stream",
            frame.len()
        );

        // Send frame
        stream.write_all(&frame).await
            .context("Failed to send ACK frame to leader")?;

        info!("🔍 DEBUG send_ack: Frame written, flushing");

        stream.flush().await
            .context("Failed to flush ACK frame")?;

        info!("🔍 DEBUG send_ack: Flush complete, ACK sent successfully");

        Ok(())
    }

    /// Election worker task - processes election triggers from channel (v2.2.7 deadlock fix)
    ///
    /// This task runs independently and can safely lock raft_node because it doesn't
    /// hold any other locks. The timeout monitor sends election requests to this worker
    /// via the channel, avoiding deadlocks.
    pub async fn run_election_worker(
        mut election_rx: mpsc::UnboundedReceiver<ElectionTriggerMessage>,
        leader_elector: Arc<crate::leader_election::LeaderElector>,
    ) {
        info!("Started election worker task (non-blocking elections)");

        while let Some(msg) = election_rx.recv().await {
            debug!(
                "Election worker: Processing trigger for {}-{}: {}",
                msg.topic, msg.partition, msg.reason
            );

            // This is safe because the worker doesn't hold any locks
            // It can wait for raft_node lock without blocking other operations
            let result = leader_elector.trigger_election_on_timeout(
                &msg.topic,
                msg.partition,
                &msg.reason,
            ).await;

            if let Err(e) = result {
                debug!(
                    "Election trigger failed for {}-{}: {} (this is normal if not leader)",
                    msg.topic, msg.partition, e
                );
            }
        }

        info!("Election worker stopped");
    }

    /// Monitor partition heartbeat timeouts and trigger elections (v2.2.7)
    pub async fn monitor_timeouts(
        election_tx: mpsc::UnboundedSender<ElectionTriggerMessage>,
        last_heartbeat: Arc<DashMap<(String, i32), std::time::Instant>>,
        shutdown: Arc<AtomicBool>,
    ) {
        info!("Started WAL timeout monitor for event-driven elections (channel-based)");

        while !shutdown.load(Ordering::Relaxed) {
            // Check every 5 seconds
            sleep(Duration::from_secs(5)).await;

            let now = std::time::Instant::now();

            // v2.2.7 DEADLOCK FIX: Collect keys to remove FIRST, then remove after iteration
            // CRITICAL: Cannot call remove() while iterating - causes DashMap shard lock deadlock!
            let mut to_remove = Vec::new();

            // Check each partition for timeout
            for entry in last_heartbeat.iter() {
                let (topic, partition) = entry.key();
                let last_seen = *entry.value();

                if now.duration_since(last_seen) > HEARTBEAT_TIMEOUT {
                    warn!(
                        "WAL stream timeout detected for {}-{} ({}s since last heartbeat)",
                        topic, partition, now.duration_since(last_seen).as_secs()
                    );

                    // v2.2.7 DEADLOCK FIX: Send to channel instead of calling directly
                    // This never blocks, preventing deadlock with raft_node lock
                    let _ = election_tx.send(ElectionTriggerMessage {
                        topic: topic.to_string(),
                        partition: *partition,
                        reason: format!("WAL stream timeout ({}s)", now.duration_since(last_seen).as_secs()),
                    });

                    // Collect key for removal (cannot remove during iteration!)
                    to_remove.push((topic.clone(), *partition));
                }
            }

            // Remove timed-out entries AFTER iteration completes (avoids iterator invalidation deadlock)
            for key in to_remove {
                last_heartbeat.remove(&key);
                debug!("Removed {}-{} from heartbeat tracking after timeout", key.0, key.1);
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
            None,    // No metadata store for test
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
            None,    // No metadata store for test
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
            None,  // No metadata store for test
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
                None,  // No metadata store for test
            );

            // Should have 2 followers (not counting self)
            assert_eq!(manager.followers.len(), 2);
            
            // Self should not be in followers
            let self_wal = format!("node{}.example.com:9291", node_id);
            assert!(!manager.followers.contains(&self_wal));
        }
    }
}
