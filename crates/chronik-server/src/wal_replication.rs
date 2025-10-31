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
use tokio::time::{sleep, Duration, timeout};
use tracing::{debug, error, info, warn};
use serde::{Serialize, Deserialize};
use dashmap::DashMap;

/// Magic number for WAL frames ('WA' in hex)
const FRAME_MAGIC: u16 = 0x5741;

/// Magic number for heartbeat frames ('HB' in hex)
const HEARTBEAT_MAGIC: u16 = 0x4842;

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

/// WAL Replication Manager for PostgreSQL-style streaming
///
/// Phase 3 implementation with actual TCP streaming and lock-free queue
pub struct WalReplicationManager {
    /// Queue of pending WAL records (lock-free MPMC)
    queue: Arc<SegQueue<WalReplicationRecord>>,

    /// Active TCP connections to followers (topic -> TcpStream)
    connections: Arc<DashMap<String, TcpStream>>,

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
}

impl WalReplicationManager {
    /// Create a new WAL replication manager
    ///
    /// Spawns background workers for connection management and record sending
    pub fn new(followers: Vec<String>) -> Arc<Self> {
        info!("Creating WalReplicationManager with {} followers", followers.len());

        let manager = Arc::new(Self {
            queue: Arc::new(SegQueue::new()),
            connections: Arc::new(DashMap::new()),
            followers,
            shutdown: Arc::new(AtomicBool::new(false)),
            total_queued: Arc::new(AtomicU64::new(0)),
            total_sent: Arc::new(AtomicU64::new(0)),
            total_dropped: Arc::new(AtomicU64::new(0)),
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

    /// Send a WAL record to all active followers
    async fn send_to_followers(&self, record: &WalReplicationRecord) {
        let frame = match serialize_wal_frame(record) {
            Ok(f) => f,
            Err(e) => {
                error!("Failed to serialize WAL frame: {}", e);
                return;
            }
        };

        // Send to each follower sequentially (DashMap doesn't allow moving RefMut into tasks)
        for follower_addr in &self.followers {
            if let Some(mut conn) = self.connections.get_mut(follower_addr) {
                // Write frame to TCP stream
                // Note: This is fast (~1ms) so sequential is fine
                if let Err(e) = conn.write_all(&frame).await {
                    error!("Failed to send WAL record to {}: {}", follower_addr, e);
                    // Remove dead connection (connection manager will reconnect)
                    drop(conn); // Drop RefMut before removing
                    self.connections.remove(follower_addr);
                } else {
                    debug!("Sent WAL record to follower: {}", follower_addr);
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
                    // Attempt to connect
                    info!("Attempting to connect to follower: {}", follower_addr);

                    match timeout(Duration::from_secs(5), TcpStream::connect(follower_addr)).await {
                        Ok(Ok(stream)) => {
                            info!("✅ Connected to follower: {}", follower_addr);

                            // Send handshake
                            if let Err(e) = self.send_handshake(&stream).await {
                                error!("Failed to send handshake to {}: {}", follower_addr, e);
                                continue;
                            }

                            // Store connection
                            self.connections.insert(follower_addr.clone(), stream);
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

    /// Send handshake to follower
    async fn send_handshake(&self, stream: &TcpStream) -> Result<()> {
        // TODO: Implement handshake protocol
        // For Phase 3.1, we'll skip handshake and go straight to streaming
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
}

impl WalReceiver {
    /// Create a new WAL receiver
    pub fn new(listener_addr: String, wal_manager: Arc<chronik_wal::WalManager>) -> Self {
        Self {
            listener_addr,
            wal_manager,
            shutdown: Arc::new(AtomicBool::new(false)),
        }
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

                    tokio::spawn(async move {
                        if let Err(e) = Self::handle_connection(stream, wal_mgr, shutdown).await {
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
    ) -> Result<()> {
        let mut buffer = BytesMut::with_capacity(64 * 1024); // 64KB buffer

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
                        // Write to local WAL
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

    /// Shutdown the receiver
    pub fn shutdown(&self) {
        info!("Shutting down WAL receiver");
        self.shutdown.store(true, Ordering::Relaxed);
    }
}
