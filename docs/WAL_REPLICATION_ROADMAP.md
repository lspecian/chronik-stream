# WAL Replication Implementation Roadmap

**Current Status**: v2.2.0 Phase 3.4 Complete ✅ (Zero-Copy Optimization)
**Next Steps**: Phase 3.5 (Further Optimization) → Phase 4 (Advanced Features)

---

## Completed Phases

### ✅ Phase 3.1: Wire Up Replication to Produce Handler
- Added WalReplicationManager re-export
- Added set_wal_replication_manager() method
- Integrated replication hook in produce path (after WAL write)
- Status: Complete

### ✅ Phase 3.2: Configuration & WAL Receiver
- Added CHRONIK_REPLICATION_FOLLOWERS env var parsing
- Implemented WalReceiver (follower side)
- TCP listener with frame deserialization
- Status: Complete

### ✅ Phase 3.3: End-to-End Testing
- Created start-cluster.sh for 2-node setup
- Created test-replication.py integration test
- Verified replication works (WAL files replicated)
- Identified limitations: follower not readable, 60s timeout
- Status: Complete

### ✅ Phase 3.4: Zero-Copy Optimization
- **CRITICAL FIX**: Eliminated double-parsing bug (60% regression)
- Parse CanonicalRecord ONCE, reuse serialized bincode for replication
- Added replicate_serialized() method (zero-copy)
- Conditional cloning only when replication enabled
- Performance: ~52K msg/s baseline (same as Phase 2), ~28K msg/s with replication (46% overhead)
- Status: Complete

---

## Table of Contents

1. [Phase 3.5: Further Performance Optimization](#phase-35-further-performance-optimization)
2. [Phase 4.1: Heartbeat Mechanism](#phase-41-heartbeat-mechanism)
3. [Phase 4.2: Readable Replicas](#phase-42-readable-replicas)
4. [Phase 4.3: Leader Election & Failover](#phase-43-leader-election--failover)
5. [Phase 4.4: Multi-Follower Optimization](#phase-44-multi-follower-optimization)
6. [Phase 4.5: Monitoring & Observability](#phase-45-monitoring--observability)
7. [Testing Strategy](#testing-strategy)

---

## Phase 3.5: Further Performance Optimization

**Goal**: Reduce the current 46% replication overhead to match v2.1.0 baseline performance.

**Current State**:
- Baseline (no replication): ~52K msg/s
- With replication (1 follower): ~28K msg/s
- **Overhead**: 46% (24K msg/s loss)

**Target**: < 10% overhead (~47K+ msg/s with replication)

### Optimization Opportunities

1. **Eliminate `.clone()` on serialized data** (line 1360):
   - Current: `serialized_for_replication = Some(serialized.clone())`
   - Problem: Clones entire bincode Vec (megabytes per second)
   - Solution: Use `Arc<Vec<u8>>` or refactor to avoid clone
   - Expected gain: 10-15%

2. **Batch frame sends**:
   - Current: Send 1 TCP frame per produce batch
   - Better: Accumulate N frames, send together (reduce syscalls)
   - Expected gain: 10-15%

3. **Use io_uring or sendfile() for zero-copy TCP**:
   - Current: Standard tokio TCP writes
   - Better: Kernel-level zero-copy
   - Expected gain: 5-10%

4. **Dedicated replication thread**:
   - Current: Tokio worker pool (shared with produce)
   - Better: Pin replication to dedicated thread
   - Expected gain: 5-10%

**Total Potential**: 30-50% improvement → Target throughput: 40-50K msg/s with replication ✅

---

## Phase 3.6: Production Hardening

**Goal**: Make current implementation production-ready with proper error handling, monitoring, and operational controls.

**Estimated Time**: 4-6 hours

### 3.6.1 Add Replication Metrics

**Objective**: Track replication health and performance.

**Metrics to Add**:
```rust
// crates/chronik-server/src/wal_replication.rs

pub struct WalReplicationMetrics {
    // Queue metrics
    pub queue_depth: AtomicU64,          // Current queue size
    pub queue_high_water: AtomicU64,     // Max queue size reached

    // Send metrics
    pub records_queued: AtomicU64,       // Total records queued
    pub records_sent: AtomicU64,         // Total records sent
    pub records_dropped: AtomicU64,      // Total records dropped (overflow)
    pub bytes_sent: AtomicU64,           // Total bytes sent

    // Connection metrics
    pub connections_active: AtomicU64,   // Active follower connections
    pub connections_failed: AtomicU64,   // Failed connection attempts
    pub reconnect_count: AtomicU64,      // Total reconnections

    // Latency metrics (use histograms)
    pub send_latency_ms: AtomicU64,      // Average send latency
    pub queue_wait_ms: AtomicU64,        // Time record waits in queue
}
```

**Implementation Steps**:

1. Add metrics struct to `WalReplicationManager`:
```rust
impl WalReplicationManager {
    pub fn new(followers: Vec<String>) -> Arc<Self> {
        let metrics = Arc::new(WalReplicationMetrics::default());

        // ... existing code ...

        Self {
            queue,
            connections,
            followers,
            shutdown,
            metrics,  // NEW
        }
    }

    // Add getter for metrics
    pub fn get_metrics(&self) -> &WalReplicationMetrics {
        &self.metrics
    }
}
```

2. Update metrics in replication code:
```rust
pub async fn replicate_async(&self, ...) {
    // Increment queue depth
    self.metrics.queue_depth.fetch_add(1, Ordering::Relaxed);

    // Check overflow
    if queue_size >= MAX_QUEUE_SIZE {
        self.metrics.records_dropped.fetch_add(1, Ordering::Relaxed);
        // ... existing overflow handling ...
        return;
    }

    self.queue.push(wal_record);
    self.metrics.records_queued.fetch_add(1, Ordering::Relaxed);
}

async fn send_to_followers(&self, record: &WalReplicationRecord) {
    let start = Instant::now();

    // ... send logic ...

    self.metrics.records_sent.fetch_add(1, Ordering::Relaxed);
    self.metrics.bytes_sent.fetch_add(frame.len() as u64, Ordering::Relaxed);
    self.metrics.send_latency_ms.store(start.elapsed().as_millis() as u64, Ordering::Relaxed);
}
```

3. Expose metrics via Prometheus endpoint:
```rust
// crates/chronik-monitoring/src/replication.rs

use prometheus::{IntGauge, IntCounter, register_int_gauge, register_int_counter};

pub fn register_replication_metrics() {
    // Queue metrics
    register_int_gauge!("chronik_replication_queue_depth", "Current replication queue depth").unwrap();
    register_int_counter!("chronik_replication_records_sent_total", "Total records sent to followers").unwrap();
    register_int_counter!("chronik_replication_records_dropped_total", "Total records dropped due to overflow").unwrap();

    // Connection metrics
    register_int_gauge!("chronik_replication_connections_active", "Active follower connections").unwrap();
    register_int_counter!("chronik_replication_reconnects_total", "Total reconnection attempts").unwrap();
}
```

**Testing**:
```bash
# Start cluster with replication
./start-cluster.sh

# Send messages
python3 test-replication.py

# Check metrics
curl http://localhost:9094/metrics | grep chronik_replication
```

**Success Criteria**:
- ✅ Metrics visible in Prometheus endpoint
- ✅ Queue depth increases/decreases correctly
- ✅ Dropped counter increments on overflow

---

### 3.3.2 Add Configuration Validation

**Objective**: Validate configuration on startup to prevent runtime errors.

**Implementation**:
```rust
// crates/chronik-server/src/wal_replication.rs

impl WalReplicationManager {
    pub fn new(followers: Vec<String>) -> Result<Arc<Self>> {
        // Validate follower addresses
        if followers.is_empty() {
            return Err(anyhow::anyhow!("No followers configured"));
        }

        for follower in &followers {
            // Validate format: host:port
            if !follower.contains(':') {
                return Err(anyhow::anyhow!(
                    "Invalid follower address '{}': must be host:port format",
                    follower
                ));
            }

            // Validate port is numeric
            let parts: Vec<&str> = follower.split(':').collect();
            if parts.len() != 2 {
                return Err(anyhow::anyhow!(
                    "Invalid follower address '{}': expected host:port",
                    follower
                ));
            }

            if parts[1].parse::<u16>().is_err() {
                return Err(anyhow::anyhow!(
                    "Invalid port in '{}': must be numeric",
                    follower
                ));
            }

            info!("Validated follower address: {}", follower);
        }

        // ... rest of initialization ...
    }
}
```

**Update integrated_server.rs**:
```rust
// Replace unwrap() with proper error handling
if !followers.is_empty() {
    info!("WAL replication enabled with {} followers: {}", followers.len(), followers.join(", "));

    match crate::wal_replication::WalReplicationManager::new(followers) {
        Ok(replication_manager) => {
            produce_handler_inner.set_wal_replication_manager(replication_manager);
        }
        Err(e) => {
            error!("Failed to initialize WAL replication: {}", e);
            return Err(e);  // Fail fast on startup
        }
    }
}
```

**Testing**:
```bash
# Test invalid configurations
CHRONIK_REPLICATION_FOLLOWERS="invalid" ./target/release/chronik-server ...
# Should fail with clear error message

CHRONIK_REPLICATION_FOLLOWERS="localhost:abc" ./target/release/chronik-server ...
# Should fail: port must be numeric

CHRONIK_REPLICATION_FOLLOWERS="" ./target/release/chronik-server ...
# Should skip replication (empty string)
```

---

### 3.3.3 Graceful Shutdown

**Objective**: Ensure clean shutdown without losing queued records.

**Implementation**:
```rust
impl WalReplicationManager {
    pub async fn shutdown(&self) {
        info!("Shutting down WAL replication manager");
        self.shutdown.store(true, Ordering::Relaxed);

        // Wait for queue to drain (with timeout)
        let mut wait_iterations = 0;
        const MAX_WAIT_ITERATIONS: u64 = 50;  // 5 seconds max

        while !self.queue.is_empty() && wait_iterations < MAX_WAIT_ITERATIONS {
            let queue_size = self.queue.len();
            info!("Waiting for replication queue to drain: {} records remaining", queue_size);
            tokio::time::sleep(Duration::from_millis(100)).await;
            wait_iterations += 1;
        }

        if !self.queue.is_empty() {
            warn!("Shutdown timeout: {} records still in queue (will be lost)", self.queue.len());
        }

        // Close all connections gracefully
        for entry in self.connections.iter() {
            let (addr, _) = entry.pair();
            info!("Closing connection to follower: {}", addr);
        }
        self.connections.clear();

        info!(
            "WAL replication stats at shutdown: queued={} sent={} dropped={}",
            self.total_queued.load(Ordering::Relaxed),
            self.total_sent.load(Ordering::Relaxed),
            self.total_dropped.load(Ordering::Relaxed)
        );
    }
}
```

**Wire up in integrated_server.rs shutdown**:
```rust
// In IntegratedKafkaServer shutdown path
if let Some(ref repl_mgr) = self.replication_manager {
    repl_mgr.shutdown().await;
}
```

---

### 3.3.4 Add Logging Levels

**Objective**: Reduce log noise, make it configurable.

**Implementation**:
```rust
// Replace info! with appropriate levels

// Connection events: INFO
info!("✅ Connected to follower: {}", follower_addr);

// Send events: DEBUG (too noisy for production)
debug!("Sent WAL record to follower: {}", follower_addr);

// Errors: ERROR
error!("Failed to send WAL record to {}: {}", follower_addr, e);

// Dropped records: WARN
warn!(
    "WAL replication queue full ({} records), dropping record for {}-{} offset {}",
    queue_size, topic, partition, base_offset
);

// Metrics logging: Only every 60s
if self.last_metrics_log.elapsed() > Duration::from_secs(60) {
    info!(
        "Replication stats: queue={} sent={}/min dropped={}/min",
        queue_size, sent_per_min, dropped_per_min
    );
    self.last_metrics_log = Instant::now();
}
```

**Add configuration**:
```bash
# Production: Only show warnings and errors
RUST_LOG=chronik_server::wal_replication=warn

# Development: Show all debug logs
RUST_LOG=chronik_server::wal_replication=debug

# Default: Info level
RUST_LOG=info
```

---

### 3.3.5 Add Health Checks

**Objective**: Expose replication health via HTTP endpoint.

**Implementation**:
```rust
// crates/chronik-server/src/health.rs (new file)

use axum::{Json, Router, routing::get};
use serde::Serialize;

#[derive(Serialize)]
pub struct ReplicationHealth {
    pub enabled: bool,
    pub followers: Vec<FollowerHealth>,
    pub queue_depth: u64,
    pub records_sent: u64,
    pub records_dropped: u64,
}

#[derive(Serialize)]
pub struct FollowerHealth {
    pub address: String,
    pub connected: bool,
    pub last_send: Option<String>,  // ISO 8601 timestamp
}

pub async fn replication_health(
    repl_mgr: Arc<WalReplicationManager>
) -> Json<ReplicationHealth> {
    let followers = repl_mgr.get_followers()
        .iter()
        .map(|addr| FollowerHealth {
            address: addr.clone(),
            connected: repl_mgr.is_connected(addr),
            last_send: repl_mgr.get_last_send_time(addr),
        })
        .collect();

    Json(ReplicationHealth {
        enabled: true,
        followers,
        queue_depth: repl_mgr.queue.len() as u64,
        records_sent: repl_mgr.total_sent.load(Ordering::Relaxed),
        records_dropped: repl_mgr.total_dropped.load(Ordering::Relaxed),
    })
}

pub fn health_router(repl_mgr: Arc<WalReplicationManager>) -> Router {
    Router::new()
        .route("/health/replication", get(|| async { replication_health(repl_mgr) }))
}
```

**Wire up in main.rs**:
```rust
// Add to existing Axum server
let health_routes = health_router(replication_manager.clone());
```

**Testing**:
```bash
curl http://localhost:9094/health/replication
# {
#   "enabled": true,
#   "followers": [
#     {
#       "address": "localhost:15432",
#       "connected": true,
#       "last_send": "2025-10-31T12:34:56Z"
#     }
#   ],
#   "queue_depth": 0,
#   "records_sent": 1234,
#   "records_dropped": 0
# }
```

---

## Phase 4.1: Heartbeat Mechanism

**Goal**: Prevent connection timeouts with periodic heartbeat frames.

**Estimated Time**: 2-3 hours

### Design

**Protocol Extension**:
```
Heartbeat Frame:
  0-1: Magic (0x4842 = 'HB')
  2-3: Version (1)
  4-7: Timestamp (u32, seconds since epoch)
```

**Implementation Steps**:

### 4.1.1 Add Heartbeat Sender (Leader Side)

```rust
// crates/chronik-server/src/wal_replication.rs

impl WalReplicationManager {
    pub fn new(followers: Vec<String>) -> Arc<Self> {
        // ... existing code ...

        // Spawn heartbeat worker
        let manager_clone = Arc::clone(&manager);
        tokio::spawn(async move {
            manager_clone.run_heartbeat_worker().await;
        });

        manager
    }

    /// Background worker that sends heartbeats every 10 seconds
    async fn run_heartbeat_worker(&self) {
        info!("WAL replication heartbeat worker started");

        while !self.shutdown.load(Ordering::Relaxed) {
            // Wait 10 seconds
            sleep(HEARTBEAT_INTERVAL).await;

            // Send heartbeat to all followers
            if let Err(e) = self.send_heartbeat().await {
                warn!("Failed to send heartbeat: {}", e);
            }
        }

        info!("WAL replication heartbeat worker stopped");
    }

    /// Send heartbeat frame to all followers
    async fn send_heartbeat(&self) -> Result<()> {
        let heartbeat_frame = create_heartbeat_frame()?;

        for follower_addr in &self.followers {
            if let Some(mut conn) = self.connections.get_mut(follower_addr) {
                if let Err(e) = conn.write_all(&heartbeat_frame).await {
                    warn!("Failed to send heartbeat to {}: {}", follower_addr, e);
                    // Remove dead connection
                    drop(conn);
                    self.connections.remove(follower_addr);
                } else {
                    debug!("Sent heartbeat to {}", follower_addr);
                }
            }
        }

        Ok(())
    }
}

/// Create a heartbeat frame
fn create_heartbeat_frame() -> Result<Bytes> {
    let mut buf = BytesMut::with_capacity(8);

    // Heartbeat header (8 bytes)
    buf.put_u16(HEARTBEAT_MAGIC);  // 0x4842
    buf.put_u16(PROTOCOL_VERSION); // 1
    buf.put_u32(chrono::Utc::now().timestamp() as u32);

    Ok(buf.freeze())
}
```

### 4.1.2 Update Receiver to Handle Heartbeats

The receiver already handles heartbeats (line 478-485 in current code):
```rust
if magic == HEARTBEAT_MAGIC {
    // Heartbeat frame - just consume it
    if buffer.len() >= 8 {
        buffer.advance(8);
        debug!("WAL receiver: Received heartbeat");
    }
    continue;
}
```

**Enhancement**: Track last heartbeat time:
```rust
pub struct WalReceiver {
    // ... existing fields ...
    last_heartbeat: Arc<Mutex<Instant>>,
}

async fn handle_connection(...) {
    // ... in heartbeat handling ...
    if magic == HEARTBEAT_MAGIC {
        buffer.advance(8);
        *self.last_heartbeat.lock().await = Instant::now();
        debug!("WAL receiver: Received heartbeat");
        continue;
    }
}
```

### 4.1.3 Testing

```bash
# Start cluster
./start-cluster.sh

# Watch logs for heartbeat messages (debug level)
RUST_LOG=chronik_server::wal_replication=debug ./target/release/chronik-server ...

# Verify heartbeats every 10 seconds
tail -f leader.log | grep "Sent heartbeat"
tail -f follower.log | grep "Received heartbeat"

# Wait 65 seconds without sending messages
# Connection should NOT timeout (previously would timeout at 60s)
sleep 65

# Send test message - should still work
python3 -c "
from kafka import KafkaProducer
p = KafkaProducer(bootstrap_servers='localhost:9092')
p.send('test', b'Still connected!')
p.flush()
"

# Check follower received it
strings follower.log | grep "Replicated"
```

**Success Criteria**:
- ✅ Heartbeats sent every 10 seconds
- ✅ Connection survives 60+ seconds of no data
- ✅ Messages replicate correctly after long idle period

---

## Phase 4.2: Readable Replicas

**Goal**: Make follower WAL data visible to consumers.

**Estimated Time**: 6-8 hours

### Design

**Approach**: Follower replays received WAL records to populate in-memory buffers and update high watermarks.

### 4.2.1 Add Real-Time WAL Replay on Follower

**Implementation**:
```rust
// crates/chronik-server/src/wal_replication.rs

impl WalReceiver {
    pub fn new(
        listener_addr: String,
        wal_manager: Arc<WalManager>,
        produce_handler: Arc<ProduceHandler>,  // NEW: Need access to produce handler
    ) -> Self {
        Self {
            listener_addr,
            wal_manager,
            produce_handler,  // NEW
            shutdown: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Write a replicated WAL record to local WAL AND replay to buffers
    async fn write_to_wal(
        wal_manager: &Arc<WalManager>,
        produce_handler: &Arc<ProduceHandler>,
        record: &WalReplicationRecord,
    ) -> Result<()> {
        // 1. Write to local WAL (existing code)
        let canonical_record: CanonicalRecord = bincode::deserialize(&record.data)
            .context("Failed to deserialize CanonicalRecord")?;

        let serialized = bincode::serialize(&canonical_record)?;

        wal_manager.append_canonical_with_acks(
            record.topic.clone(),
            record.partition,
            serialized,
            record.base_offset,
            record.base_offset + record.record_count as i64 - 1,
            record.record_count as i32,
            1,  // acks=1 for fsync
        ).await?;

        // 2. NEW: Replay to in-memory buffers
        produce_handler.replay_from_wal(
            &record.topic,
            record.partition,
            &canonical_record,
        ).await?;

        // 3. NEW: Update high watermark
        produce_handler.update_high_watermark(
            &record.topic,
            record.partition,
            record.base_offset + record.record_count as i64,
        ).await?;

        info!(
            "WAL✓ Replicated+Replayed: {}-{} offset {} ({} records)",
            record.topic, record.partition, record.base_offset, record.record_count
        );

        Ok(())
    }
}
```

### 4.2.2 Add Replay Method to ProduceHandler

```rust
// crates/chronik-server/src/produce_handler.rs

impl ProduceHandler {
    /// Replay a WAL record to in-memory buffers (for replication follower)
    pub async fn replay_from_wal(
        &self,
        topic: &str,
        partition: i32,
        canonical_record: &CanonicalRecord,
    ) -> Result<()> {
        // Get or create partition state
        let partition_state = self.get_or_create_partition_state(topic, partition).await?;

        // Convert to BufferedBatch format
        let buffered_batch = BufferedBatch {
            raw_bytes: canonical_record.compressed_records_wire_bytes
                .clone()
                .unwrap_or_default(),
            records: canonical_record.records.iter().map(|r| ProduceRecord {
                offset: r.offset,
                timestamp: r.timestamp,
                key: r.key.clone(),
                value: r.value.clone(),
                headers: r.headers.clone(),
                producer_id: -1,
                producer_epoch: -1,
                sequence: -1,
                is_transactional: false,
                is_control: false,
            }).collect(),
            base_offset: canonical_record.base_offset,
        };

        // Add to pending batches
        let mut pending = partition_state.pending_batches.lock().await;
        pending.push(buffered_batch);

        debug!(
            "Replayed {}-{} to in-memory buffers: {} records at offset {}",
            topic, partition, canonical_record.records.len(), canonical_record.base_offset
        );

        Ok(())
    }

    /// Update high watermark (for replication follower)
    pub async fn update_high_watermark(
        &self,
        topic: &str,
        partition: i32,
        new_hwm: i64,
    ) -> Result<()> {
        let partition_state = self.get_or_create_partition_state(topic, partition).await?;

        let old_hwm = partition_state.high_watermark.load(Ordering::SeqCst);
        if new_hwm > old_hwm as i64 {
            partition_state.high_watermark.store(new_hwm as u64, Ordering::SeqCst);

            // Persist to metadata store
            self.metadata_store.update_partition_offset(
                topic,
                partition as u32,
                new_hwm,
                partition_state.log_start_offset.load(Ordering::SeqCst) as i64,
            ).await?;

            debug!("Updated high watermark for {}-{}: {} -> {}", topic, partition, old_hwm, new_hwm);
        }

        Ok(())
    }
}
```

### 4.2.3 Wire Up in integrated_server.rs

```rust
// When starting WAL receiver, pass produce_handler
if let Ok(receiver_addr) = std::env::var("CHRONIK_WAL_RECEIVER_ADDR") {
    if !receiver_addr.is_empty() {
        info!("WAL receiver enabled on {}", receiver_addr);
        let wal_receiver = crate::wal_replication::WalReceiver::new(
            receiver_addr.clone(),
            wal_manager.clone(),
            produce_handler_base.clone(),  // NEW: Pass produce handler
        );

        // Spawn receiver
        tokio::spawn(async move {
            if let Err(e) = wal_receiver.run().await {
                error!("WAL receiver failed: {}", e);
            }
        });

        info!("✅ WAL receiver started on {} (readable replica mode)", receiver_addr);
    }
}
```

### 4.2.4 Testing

```bash
# Start cluster
./start-cluster.sh

# Send messages to leader
python3 test-replication.py

# NEW: Consume from FOLLOWER (should work now)
python3 -c "
from kafka import KafkaConsumer
consumer = KafkaConsumer(
    'test-replication',
    bootstrap_servers='localhost:9093',  # FOLLOWER port
    auto_offset_reset='earliest',
    consumer_timeout_ms=5000
)

count = 0
for msg in consumer:
    count += 1
    print(f'✓ Received from follower: {msg.value.decode()}')

print(f'Total messages from follower: {count}')
"
```

**Success Criteria**:
- ✅ Messages visible to consumers on follower
- ✅ High watermark matches leader
- ✅ Consumer fetch requests succeed on follower

---

## Phase 4.3: Leader Election & Failover

**Goal**: Automatic promotion when leader fails.

**Estimated Time**: 8-10 hours

### Design

**Simple Approach** (No Raft/Paxos):
1. Health check: Followers ping leader every 5s
2. Leader failure detected after 3 missed pings (15s)
3. Election: Follower with highest WAL offset becomes leader
4. Promotion: New leader starts accepting writes

### 4.3.1 Add Leader Health Check

```rust
// crates/chronik-server/src/leader_election.rs (new file)

pub struct LeaderHealthChecker {
    leader_addr: String,
    last_ping: Arc<Mutex<Instant>>,
    is_leader_alive: Arc<AtomicBool>,
}

impl LeaderHealthChecker {
    pub fn new(leader_addr: String) -> Arc<Self> {
        let checker = Arc::new(Self {
            leader_addr,
            last_ping: Arc::new(Mutex::new(Instant::now())),
            is_leader_alive: Arc::new(AtomicBool::new(true)),
        });

        // Spawn health check worker
        let checker_clone = Arc::clone(&checker);
        tokio::spawn(async move {
            checker_clone.run_health_checks().await;
        });

        checker
    }

    async fn run_health_checks(&self) {
        loop {
            sleep(Duration::from_secs(5)).await;

            // Try to ping leader
            match self.ping_leader().await {
                Ok(_) => {
                    *self.last_ping.lock().await = Instant::now();
                    self.is_leader_alive.store(true, Ordering::Relaxed);
                }
                Err(e) => {
                    warn!("Leader health check failed: {}", e);

                    // Check if leader has been down for 15s (3 missed pings)
                    let last_ping = *self.last_ping.lock().await;
                    if last_ping.elapsed() > Duration::from_secs(15) {
                        error!("Leader appears to be down for 15s - initiating failover");
                        self.is_leader_alive.store(false, Ordering::Relaxed);
                        // Trigger election
                        self.trigger_election().await;
                    }
                }
            }
        }
    }

    async fn ping_leader(&self) -> Result<()> {
        // Simple TCP connection test
        let _ = timeout(Duration::from_secs(2), TcpStream::connect(&self.leader_addr)).await??;
        Ok(())
    }

    async fn trigger_election(&self) {
        // Notify election coordinator
        warn!("Triggering leader election");
        // ... election logic ...
    }
}
```

### 4.3.2 Add Election Coordinator

```rust
pub struct ElectionCoordinator {
    node_id: u32,
    peers: Vec<PeerInfo>,
    current_leader: Arc<RwLock<Option<u32>>>,
    wal_manager: Arc<WalManager>,
}

#[derive(Clone)]
pub struct PeerInfo {
    pub id: u32,
    pub addr: String,
}

impl ElectionCoordinator {
    /// Start election when leader fails
    pub async fn start_election(&self) -> Result<u32> {
        info!("Starting leader election (node {})", self.node_id);

        // 1. Get our highest WAL offset
        let our_offset = self.get_highest_wal_offset().await?;
        info!("Our highest WAL offset: {}", our_offset);

        // 2. Request offsets from all peers
        let mut peer_offsets = vec![];
        for peer in &self.peers {
            match self.request_peer_offset(peer).await {
                Ok(offset) => {
                    info!("Peer {} has offset {}", peer.id, offset);
                    peer_offsets.push((peer.id, offset));
                }
                Err(e) => {
                    warn!("Failed to get offset from peer {}: {}", peer.id, e);
                }
            }
        }

        // 3. Find node with highest offset
        let mut winner = (self.node_id, our_offset);
        for (peer_id, offset) in peer_offsets {
            if offset > winner.1 || (offset == winner.1 && peer_id < winner.0) {
                winner = (peer_id, offset);
            }
        }

        info!("Election winner: node {} with offset {}", winner.0, winner.1);

        // 4. If we won, promote ourselves
        if winner.0 == self.node_id {
            self.promote_to_leader().await?;
        }

        Ok(winner.0)
    }

    async fn get_highest_wal_offset(&self) -> Result<i64> {
        // Query all partitions in WAL
        let partitions = self.wal_manager.get_partitions();
        let mut max_offset = 0i64;

        for tp in partitions {
            if let Ok(records) = self.wal_manager.read_from(&tp.topic, tp.partition, 0, usize::MAX).await {
                for record in records {
                    max_offset = max_offset.max(record.offset);
                }
            }
        }

        Ok(max_offset)
    }

    async fn promote_to_leader(&self) -> Result<()> {
        info!("🎖️  Promoting to LEADER");

        // 1. Update local state
        *self.current_leader.write().await = Some(self.node_id);

        // 2. Start accepting writes (disable read-only mode)
        // ... implementation depends on how read-only mode is implemented ...

        // 3. Start replicating to other followers
        // ... configure CHRONIK_REPLICATION_FOLLOWERS ...

        info!("✅ Successfully promoted to leader");
        Ok(())
    }
}
```

### 4.3.3 Testing

```bash
# Start 3-node cluster (1 leader + 2 followers)
# Leader: port 9092
# Follower1: port 9093
# Follower2: port 9094

# Send messages to leader
python3 test-replication.py

# Kill leader
pkill -9 chronik-server  # (kill only the leader process)

# Wait 15 seconds for failover
sleep 20

# Check which follower became leader
curl http://localhost:9093/health/election
curl http://localhost:9094/health/election

# Send messages to new leader
python3 -c "
from kafka import KafkaProducer
p = KafkaProducer(bootstrap_servers='localhost:9093')  # New leader
p.send('test', b'After failover')
p.flush()
"

# Verify replication continues
```

**Success Criteria**:
- ✅ Leader failure detected within 15 seconds
- ✅ Election completes in < 5 seconds
- ✅ New leader accepts writes
- ✅ Replication resumes to remaining followers

---

## Phase 4.4: Multi-Follower Optimization

**Goal**: Optimize for 3+ followers with parallel writes.

**Estimated Time**: 3-4 hours

### 4.4.1 Parallel Writes to Followers

**Current**: Sequential writes to followers (~1ms each)
**Goal**: Parallel writes to all followers (still ~1ms total)

**Implementation**:
```rust
// crates/chronik-server/src/wal_replication.rs

async fn send_to_followers(&self, record: &WalReplicationRecord) {
    let frame = match serialize_wal_frame(record) {
        Ok(f) => f,
        Err(e) => {
            error!("Failed to serialize WAL frame: {}", e);
            return;
        }
    };

    // NEW: Parallel writes using futures
    let send_futures: Vec<_> = self.followers.iter().map(|follower_addr| {
        let connections = Arc::clone(&self.connections);
        let frame = frame.clone();
        let addr = follower_addr.clone();

        async move {
            if let Some(mut conn) = connections.get_mut(&addr) {
                if let Err(e) = conn.write_all(&frame).await {
                    error!("Failed to send WAL record to {}: {}", addr, e);
                    return Err(e);
                }
                debug!("Sent WAL record to follower: {}", addr);
                Ok(())
            } else {
                Err(std::io::Error::new(std::io::ErrorKind::NotConnected, "No connection"))
            }
        }
    }).collect();

    // Wait for all sends concurrently
    let results = futures::future::join_all(send_futures).await;

    // Count successes/failures
    let successes = results.iter().filter(|r| r.is_ok()).count();
    let failures = results.len() - successes;

    if failures > 0 {
        warn!("Sent to {}/{} followers ({} failed)", successes, results.len(), failures);
    } else {
        debug!("Sent to all {} followers", successes);
    }
}
```

**Dependency**:
```toml
# Cargo.toml
futures = { workspace = true }
```

**Performance Impact**:
- Sequential (2 followers): 2ms
- Parallel (2 followers): 1ms
- Sequential (5 followers): 5ms
- Parallel (5 followers): 1ms

---

## Phase 4.5: Monitoring & Observability

**Goal**: Production-grade observability.

**Estimated Time**: 4-5 hours

### 4.5.1 Prometheus Metrics

**Full Metric Set**:
```rust
// crates/chronik-monitoring/src/replication.rs

use prometheus::*;

lazy_static! {
    // Queue metrics
    pub static ref REPLICATION_QUEUE_DEPTH: IntGauge =
        register_int_gauge!("chronik_replication_queue_depth",
        "Current number of records in replication queue").unwrap();

    pub static ref REPLICATION_QUEUE_CAPACITY: IntGauge =
        register_int_gauge!("chronik_replication_queue_capacity",
        "Maximum queue capacity").unwrap();

    // Send metrics
    pub static ref REPLICATION_RECORDS_SENT: IntCounterVec =
        register_int_counter_vec!(
            "chronik_replication_records_sent_total",
            "Total records sent to followers",
            &["follower"]
        ).unwrap();

    pub static ref REPLICATION_BYTES_SENT: IntCounterVec =
        register_int_counter_vec!(
            "chronik_replication_bytes_sent_total",
            "Total bytes sent to followers",
            &["follower"]
        ).unwrap();

    pub static ref REPLICATION_RECORDS_DROPPED: IntCounter =
        register_int_counter!(
            "chronik_replication_records_dropped_total",
            "Total records dropped due to overflow"
        ).unwrap();

    // Latency metrics (histograms)
    pub static ref REPLICATION_SEND_DURATION: HistogramVec =
        register_histogram_vec!(
            "chronik_replication_send_duration_seconds",
            "Time to send a record to follower",
            &["follower"],
            vec![0.001, 0.002, 0.005, 0.01, 0.025, 0.05, 0.1]
        ).unwrap();

    pub static ref REPLICATION_QUEUE_WAIT_DURATION: Histogram =
        register_histogram!(
            "chronik_replication_queue_wait_duration_seconds",
            "Time a record waits in queue before being sent",
            vec![0.001, 0.005, 0.01, 0.05, 0.1, 0.5, 1.0]
        ).unwrap();

    // Connection metrics
    pub static ref REPLICATION_CONNECTIONS_ACTIVE: IntGaugeVec =
        register_int_gauge_vec!(
            "chronik_replication_connections_active",
            "Active follower connections",
            &["follower"]
        ).unwrap();

    pub static ref REPLICATION_RECONNECTS: IntCounterVec =
        register_int_counter_vec!(
            "chronik_replication_reconnects_total",
            "Total reconnection attempts",
            &["follower"]
        ).unwrap();

    // Replication lag (follower side)
    pub static ref REPLICATION_LAG_RECORDS: IntGauge =
        register_int_gauge!(
            "chronik_replication_lag_records",
            "Number of records behind leader (follower only)"
        ).unwrap();

    pub static ref REPLICATION_LAG_SECONDS: Gauge =
        register_gauge!(
            "chronik_replication_lag_seconds",
            "Time lag behind leader in seconds (follower only)"
        ).unwrap();
}
```

### 4.5.2 Grafana Dashboard

**Create dashboard JSON**:
```json
{
  "dashboard": {
    "title": "Chronik WAL Replication",
    "panels": [
      {
        "title": "Replication Queue Depth",
        "targets": [
          {
            "expr": "chronik_replication_queue_depth"
          }
        ],
        "type": "graph"
      },
      {
        "title": "Records Sent Rate",
        "targets": [
          {
            "expr": "rate(chronik_replication_records_sent_total[1m])"
          }
        ]
      },
      {
        "title": "Replication Latency p99",
        "targets": [
          {
            "expr": "histogram_quantile(0.99, chronik_replication_send_duration_seconds)"
          }
        ]
      },
      {
        "title": "Active Connections",
        "targets": [
          {
            "expr": "chronik_replication_connections_active"
          }
        ]
      },
      {
        "title": "Replication Lag (Follower)",
        "targets": [
          {
            "expr": "chronik_replication_lag_seconds"
          }
        ]
      }
    ]
  }
}
```

### 4.5.3 Structured Logging

**Add tracing spans**:
```rust
use tracing::{span, Level};

async fn replicate_async(&self, ...) {
    let span = span!(Level::INFO, "replicate",
        topic = %topic,
        partition = partition,
        offset = base_offset
    );
    let _enter = span.enter();

    // ... replication logic ...
}
```

### 4.5.4 Alerting Rules

**Prometheus alert rules**:
```yaml
# alerts.yml
groups:
  - name: chronik_replication
    rules:
      # Queue overflow alert
      - alert: ReplicationQueueFull
        expr: chronik_replication_queue_depth / chronik_replication_queue_capacity > 0.9
        for: 1m
        labels:
          severity: warning
        annotations:
          summary: "Replication queue is 90% full"

      # Dropped records alert
      - alert: ReplicationRecordsDropped
        expr: rate(chronik_replication_records_dropped_total[5m]) > 0
        for: 1m
        labels:
          severity: critical
        annotations:
          summary: "Records are being dropped due to queue overflow"

      # Connection down alert
      - alert: ReplicationConnectionDown
        expr: chronik_replication_connections_active == 0
        for: 30s
        labels:
          severity: critical
        annotations:
          summary: "No active replication connections"

      # High latency alert
      - alert: ReplicationHighLatency
        expr: histogram_quantile(0.99, chronik_replication_send_duration_seconds) > 0.1
        for: 5m
        labels:
          severity: warning
        annotations:
          summary: "Replication latency p99 > 100ms"

      # Replication lag alert (follower)
      - alert: ReplicationLagHigh
        expr: chronik_replication_lag_seconds > 60
        for: 1m
        labels:
          severity: warning
        annotations:
          summary: "Follower is more than 60 seconds behind leader"
```

---

## Testing Strategy

### Unit Tests
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_heartbeat_prevents_timeout() {
        // Start receiver
        // Start sender with heartbeat
        // Wait 65 seconds
        // Send message
        // Verify received
    }

    #[tokio::test]
    async fn test_readable_replica() {
        // Start cluster
        // Send to leader
        // Consume from follower
        // Verify same data
    }

    #[tokio::test]
    async fn test_leader_election() {
        // Start 3 nodes
        // Kill leader
        // Wait for election
        // Verify new leader accepts writes
    }
}
```

### Integration Tests
```bash
# tests/replication_integration_test.sh

#!/bin/bash
set -e

echo "Test 1: Basic Replication"
./start-cluster.sh
python3 test-replication.py
pkill chronik-server

echo "Test 2: Heartbeat"
./start-cluster.sh
sleep 70  # Wait past timeout
python3 test-replication.py  # Should still work
pkill chronik-server

echo "Test 3: Readable Replica"
./start-cluster.sh
python3 test-readable-replica.py
pkill chronik-server

echo "Test 4: Failover"
./test-leader-failover.sh

echo "✅ All integration tests passed"
```

### Performance Tests
```bash
# Benchmark with replication
./start-cluster.sh

# Test leader throughput
./target/release/chronik-bench \
  --bootstrap-servers localhost:9092 \
  --topic bench-replicated \
  --mode produce \
  --concurrency 128 \
  --duration 60s \
  --message-size 256

# Expected: >= 70K msg/s (vs 78K without replication)

# Test follower lag
# Send 100K messages
# Measure time until follower has all messages
```

---

## Success Criteria

### Phase 3.3 (Production Hardening)
- ✅ Metrics exposed via Prometheus
- ✅ Configuration validated on startup
- ✅ Graceful shutdown without data loss
- ✅ Appropriate log levels
- ✅ Health check endpoint

### Phase 4.1 (Heartbeat)
- ✅ Heartbeats sent every 10s
- ✅ Connection survives 60s idle
- ✅ No broken pipe errors

### Phase 4.2 (Readable Replicas)
- ✅ Consumers can read from followers
- ✅ High watermarks match leader
- ✅ Lag < 1 second

### Phase 4.3 (Leader Election)
- ✅ Failure detected in < 15s
- ✅ Election completes in < 5s
- ✅ New leader accepts writes
- ✅ No data loss during failover

### Phase 4.4 (Multi-Follower)
- ✅ Parallel writes work correctly
- ✅ 5 followers: < 2ms latency
- ✅ All followers receive data

### Phase 4.5 (Monitoring)
- ✅ All metrics visible in Grafana
- ✅ Alerts fire correctly
- ✅ Structured logs parseable

---

## Timeline Estimate

| Phase | Description | Time |
|-------|-------------|------|
| 3.3.1 | Metrics | 2h |
| 3.3.2 | Config validation | 1h |
| 3.3.3 | Graceful shutdown | 1h |
| 3.3.4 | Logging levels | 1h |
| 3.3.5 | Health checks | 2h |
| **3.3 Total** | **Production Hardening** | **7h** |
| 4.1 | Heartbeat mechanism | 3h |
| 4.2 | Readable replicas | 8h |
| 4.3 | Leader election | 10h |
| 4.4 | Multi-follower optimization | 4h |
| 4.5 | Monitoring & observability | 5h |
| **4.x Total** | **Advanced Features** | **30h** |
| **GRAND TOTAL** | **All Phases** | **37h** |

**Estimated Calendar Time**: 5-6 working days (assuming 6-8 hours per day)

---

## Priority Order

**Critical (Must Have)**:
1. ✅ Phase 3.3.1: Metrics
2. ✅ Phase 3.3.2: Config validation
3. ✅ Phase 4.1: Heartbeat mechanism
4. ✅ Phase 4.5: Basic monitoring

**Important (Should Have)**:
5. Phase 3.3.3: Graceful shutdown
6. Phase 4.2: Readable replicas
7. Phase 3.3.5: Health checks

**Nice to Have (Could Have)**:
8. Phase 4.3: Leader election
9. Phase 4.4: Multi-follower optimization
10. Phase 3.3.4: Advanced logging

---

## Next Steps

**Immediate** (This Week):
```bash
# 1. Implement Phase 3.3.1 (Metrics)
git checkout -b feat/v2.2.0-metrics
# ... implement metrics ...
git commit -m "feat(v2.2.0): Add replication metrics"

# 2. Implement Phase 4.1 (Heartbeat)
git checkout -b feat/v2.2.0-heartbeat
# ... implement heartbeat ...
git commit -m "feat(v2.2.0): Add heartbeat mechanism"
```

**Short Term** (Next 2 Weeks):
- Complete all Phase 3.3 tasks
- Implement Phase 4.1 (Heartbeat)
- Add basic Phase 4.5 (Monitoring)

**Medium Term** (Next Month):
- Implement Phase 4.2 (Readable Replicas)
- Production testing with real workloads

**Long Term** (Future):
- Phase 4.3 (Leader Election)
- Phase 4.4 (Multi-Follower)
- Advanced Phase 4.5 features

---

## Questions/Decisions Needed

1. **Leader Election Algorithm**:
   - Simple offset-based (proposed)
   - Raft consensus (more complex but safer)
   - Zookeeper/etcd integration (external dependency)

2. **Read Consistency**:
   - Eventual consistency (proposed - simple)
   - Read-your-writes (requires session tracking)
   - Strong consistency (requires synchronous replication)

3. **Failure Detection**:
   - TCP-based health checks (proposed)
   - Gossip protocol (more robust)
   - External monitoring (Prometheus alerts)

4. **Replication Guarantees**:
   - At-least-once (current - possible duplicates)
   - Exactly-once (requires deduplication)
   - At-most-once (fire-and-forget, data loss possible)

---

This roadmap provides a complete path from current state to a production-grade WAL replication system with all advanced features. Each phase is self-contained and can be implemented incrementally.
