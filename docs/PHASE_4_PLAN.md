# Phase 4: Separate acks=-1 Path with ISR Quorum

**Goal**: Implement fast path for acks=0/1 (existing), slow path for acks=-1 (NEW)
**Estimated Time**: 1-2 days
**Target Performance**: 40K+ msg/s for acks=-1, 55K+ msg/s for acks=0/1

---

## Overview

### The Problem

Currently, Chronik replicates asynchronously (fire-and-forget) for ALL producers:
- acks=0: No fsync, no ACK wait ✅ Fast
- acks=1: Local fsync, no replication wait ✅ Fast
- acks=-1: Should wait for ISR quorum ❌ **NOT IMPLEMENTED**

Kafka's acks=-1 guarantees:
> "Leader will wait for the full set of in-sync replicas to acknowledge the record"

### The Solution

**Two-Path Architecture**:
```
┌─────────────────────────────────────────────────────────────┐
│                    Producer Request                          │
│                   (acks parameter)                           │
└───────────────────────┬─────────────────────────────────────┘
                        │
                        ▼
                   Parse acks
                        │
        ┌───────────────┴───────────────┐
        │                               │
        ▼                               ▼
    acks=0/1                         acks=-1
    (FAST PATH)                   (SLOW PATH)
        │                               │
        ▼                               ▼
   1. WAL write                    1. WAL write
   2. Fire-and-forget              2. Register wait channel
      replication                  3. Replicate to ISR
   3. Return immediately           4. Wait for quorum ACKs
        │                          5. Return on quorum/timeout
        │                               │
        ▼                               ▼
   Response: offset              Response: offset or error
   Latency: ~2ms                 Latency: ~5-20ms
```

---

## Implementation Tasks

### Task 1: Create IsrAckTracker (New Component)

**File**: `crates/chronik-server/src/isr_ack_tracker.rs` (NEW)

**Purpose**: Track pending acks=-1 requests and notify when ISR quorum reached

```rust
use dashmap::DashMap;
use std::sync::Arc;
use tokio::sync::oneshot;

/// Tracks acks=-1 requests waiting for ISR quorum
pub struct IsrAckTracker {
    /// Pending acks: (topic, partition, offset) → WaitEntry
    pending: DashMap<(String, i32, i64), WaitEntry>,
}

struct WaitEntry {
    /// Channel to notify when quorum reached
    tx: oneshot::Sender<Result<()>>,
    /// Required quorum size (e.g., 2 for 3 replicas)
    quorum_size: usize,
    /// Nodes that have ACKed so far
    acked_nodes: Vec<u64>,
    /// Timestamp when registered (for timeout tracking)
    timestamp: std::time::Instant,
}

impl IsrAckTracker {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            pending: DashMap::new(),
        })
    }

    /// Register a new acks=-1 request waiting for quorum
    pub fn register_wait(
        &self,
        topic: String,
        partition: i32,
        offset: i64,
        quorum_size: usize,
        tx: oneshot::Sender<Result<()>>,
    ) {
        let key = (topic, partition, offset);
        self.pending.insert(key, WaitEntry {
            tx,
            quorum_size,
            acked_nodes: Vec::new(),
            timestamp: std::time::Instant::now(),
        });
    }

    /// Record an ACK from a follower
    pub fn record_ack(&self, topic: &str, partition: i32, offset: i64, node_id: u64) {
        let key = (topic.to_string(), partition, offset);

        if let Some(mut entry) = self.pending.get_mut(&key) {
            // Add node to acked list (if not already present)
            if !entry.acked_nodes.contains(&node_id) {
                entry.acked_nodes.push(node_id);
            }

            // Check if quorum reached
            if entry.acked_nodes.len() >= entry.quorum_size {
                // Remove from pending (get ownership of tx)
                drop(entry);  // Release RefMut before removing
                if let Some((_, wait_entry)) = self.pending.remove(&key) {
                    // Notify waiting producer
                    let _ = wait_entry.tx.send(Ok(()));
                    debug!(
                        "ISR quorum reached for {}-{} offset {}: {}/{} ACKs",
                        topic, partition, offset,
                        wait_entry.acked_nodes.len(),
                        wait_entry.quorum_size
                    );
                }
            }
        }
    }

    /// Clean up timed-out entries (call periodically from background task)
    pub fn cleanup_expired(&self, timeout: std::time::Duration) {
        let now = std::time::Instant::now();
        self.pending.retain(|key, entry| {
            if now.duration_since(entry.timestamp) > timeout {
                warn!(
                    "ISR quorum timeout for {}-{} offset {}: {}/{} ACKs received",
                    key.0, key.1, key.2,
                    entry.acked_nodes.len(),
                    entry.quorum_size
                );
                // Notify with timeout error
                let _ = entry.tx.send(Err(anyhow::anyhow!("ISR quorum timeout")));
                false  // Remove from map
            } else {
                true  // Keep in map
            }
        });
    }
}
```

---

### Task 2: Update ProduceHandler for acks Routing

**File**: `crates/chronik-server/src/produce_handler.rs`

**Changes**:

**2.1: Add IsrAckTracker field**
```rust
pub struct ProduceHandler {
    // ... existing fields ...

    /// ISR ACK tracker for acks=-1 requests (v2.5.0 Phase 4)
    isr_ack_tracker: Option<Arc<IsrAckTracker>>,
}

// Add setter
impl ProduceHandler {
    pub fn set_isr_ack_tracker(&mut self, tracker: Arc<IsrAckTracker>) {
        info!("Setting IsrAckTracker for ProduceHandler - enables acks=-1 quorum");
        self.isr_ack_tracker = Some(tracker);
    }
}
```

**2.2: Extract acks parameter from ProduceRequest**
```rust
// In handle_produce() - around line 600
let acks = request.acks;  // -1, 0, or 1

debug!(
    "Handling produce request: topic={} acks={} partitions={}",
    request.topic,
    acks,
    request.partition_data.len()
);
```

**2.3: Update produce_to_partition signature**
```rust
// Change from:
async fn produce_to_partition(&self, topic: &str, partition: i32, batch: &[u8]) -> Result<i64>

// To:
async fn produce_to_partition(
    &self,
    topic: &str,
    partition: i32,
    batch: &[u8],
    acks: i16,  // NEW parameter
) -> Result<i64>
```

**2.4: Implement acks routing logic**
```rust
// In produce_to_partition() - AFTER WAL write completes (around line 1428)

// Replication already triggered above (fire-and-forget)
// Now decide whether to wait for ISR quorum

match acks {
    0 => {
        // FAST PATH: No fsync, no ACK
        // Return immediately
        debug!("acks=0: Returning immediately for {}-{} offset {}", topic, partition, base_offset);
        Ok(base_offset as i64)
    }
    1 => {
        // FAST PATH: Local fsync only
        // WAL fsync already happened above
        debug!("acks=1: Returning after local fsync for {}-{} offset {}", topic, partition, base_offset);
        Ok(base_offset as i64)
    }
    -1 => {
        // SLOW PATH: Wait for ISR quorum
        self.wait_for_isr_quorum(topic, partition, base_offset as i64).await
    }
    _ => {
        // Invalid acks value
        warn!("Invalid acks value: {}, treating as acks=1", acks);
        Ok(base_offset as i64)
    }
}
```

**2.5: Implement wait_for_isr_quorum()**
```rust
// Add new method to ProduceHandler
impl ProduceHandler {
    /// Wait for ISR quorum to acknowledge (acks=-1 path)
    async fn wait_for_isr_quorum(
        &self,
        topic: &str,
        partition: i32,
        offset: i64,
    ) -> Result<i64> {
        // Check if ISR tracking is enabled
        let ack_tracker = match &self.isr_ack_tracker {
            Some(tracker) => tracker,
            None => {
                // No ISR tracker configured, fall back to acks=1 behavior
                warn!(
                    "acks=-1 requested but no IsrAckTracker configured, falling back to acks=1"
                );
                return Ok(offset);
            }
        };

        // Get ISR for this partition
        let isr_nodes = match &self.raft_cluster {
            Some(cluster) => {
                match cluster.get_partition_replicas(topic, partition) {
                    Some(replicas) => {
                        // Filter to in-sync replicas
                        // For now, assume all replicas are in-sync (ISR filtering done by WalReplicationManager)
                        replicas
                    }
                    None => {
                        warn!("No replicas found for {}-{}, treating as standalone", topic, partition);
                        return Ok(offset);
                    }
                }
            }
            None => {
                // No Raft cluster, standalone mode
                debug!("No RaftCluster configured, treating acks=-1 as acks=1");
                return Ok(offset);
            }
        };

        // Calculate quorum size
        // Kafka quorum: min_isr (default 1) must ACK
        // For simplicity: quorum = ceil(replicas / 2)
        let quorum_size = (isr_nodes.len() + 1) / 2;

        if quorum_size == 0 {
            // No replicas, just return
            return Ok(offset);
        }

        info!(
            "acks=-1: Waiting for ISR quorum {}/{} for {}-{} offset {}",
            quorum_size,
            isr_nodes.len(),
            topic,
            partition,
            offset
        );

        // Create wait channel
        let (tx, rx) = tokio::sync::oneshot::channel();

        // Register wait
        ack_tracker.register_wait(
            topic.to_string(),
            partition,
            offset,
            quorum_size,
            tx,
        );

        // Wait for quorum with timeout (30 seconds - Kafka default)
        match tokio::time::timeout(Duration::from_secs(30), rx).await {
            Ok(Ok(Ok(()))) => {
                // Quorum reached!
                info!("ISR quorum reached for {}-{} offset {}", topic, partition, offset);
                Ok(offset)
            }
            Ok(Ok(Err(e))) => {
                // Internal error from tracker
                error!("ISR quorum error for {}-{} offset {}: {}", topic, partition, offset, e);
                Err(Error::Protocol(format!("ISR quorum failed: {}", e)))
            }
            Ok(Err(_)) => {
                // Channel closed (shouldn't happen)
                error!("ISR quorum channel closed for {}-{} offset {}", topic, partition, offset);
                Err(Error::Protocol("ISR quorum channel closed".into()))
            }
            Err(_) => {
                // Timeout!
                error!(
                    "ISR quorum timeout for {}-{} offset {} (waited 30s)",
                    topic, partition, offset
                );
                Err(Error::Protocol(format!(
                    "ISR quorum timeout for {}-{} offset {}",
                    topic, partition, offset
                )))
            }
        }
    }
}
```

---

### Task 3: Add ACK Reception from Followers

**Problem**: Followers need to send ACKs back to leader after writing to local WAL.

**Solution**: Add ACK response to WAL replication protocol.

**3.1: Update WAL replication frame format**

**File**: `crates/chronik-server/src/wal_replication.rs`

```rust
// Add new frame type for ACKs
const ACK_MAGIC: u16 = 0x4143;  // 'AC'

/// ACK frame structure:
/// 0-1: Magic (0x4143 = 'AC')
/// 2-3: Version (1)
/// 4-7: Payload length (N)
/// 8+: Payload (AckMessage)

#[derive(Serialize, Deserialize)]
pub struct AckMessage {
    pub topic: String,
    pub partition: i32,
    pub offset: i64,
    pub node_id: u64,  // Which follower is ACKing
}

/// Create ACK frame
fn create_ack_frame(ack: &AckMessage) -> Result<Bytes> {
    let payload = bincode::serialize(ack)?;
    let mut buf = BytesMut::with_capacity(8 + payload.len());

    buf.put_u16(ACK_MAGIC);
    buf.put_u16(PROTOCOL_VERSION);
    buf.put_u32(payload.len() as u32);
    buf.put_slice(&payload);

    Ok(buf.freeze())
}
```

**3.2: Follower sends ACK after WAL write**

**File**: `crates/chronik-server/src/wal_replication.rs` (WalReceiver)

```rust
// In handle_connection() - after WAL write succeeds (around line 490)

// Write received data to local WAL
write_to_wal(&wal_manager, &record).await?;

// NEW: Send ACK back to leader
let ack = AckMessage {
    topic: record.topic.clone(),
    partition: record.partition,
    offset: record.base_offset,
    node_id: self.node_id,  // Follower's node ID
};

let ack_frame = create_ack_frame(&ack)?;
stream.write_all(&ack_frame).await?;

debug!(
    "Sent ACK to leader for {}-{} offset {}",
    record.topic, record.partition, record.base_offset
);
```

**3.3: Leader receives ACK and notifies tracker**

**File**: `crates/chronik-server/src/wal_replication.rs` (WalReplicationManager)

```rust
// Add field to WalReplicationManager
pub struct WalReplicationManager {
    // ... existing fields ...

    /// ISR ACK tracker for acks=-1 support (v2.5.0 Phase 4)
    isr_ack_tracker: Option<Arc<IsrAckTracker>>,
}

// Update new_with_dependencies() to accept IsrAckTracker
pub fn new_with_dependencies(
    followers: Vec<String>,
    raft_cluster: Option<Arc<RaftCluster>>,
    isr_tracker: Option<Arc<IsrTracker>>,
    ack_tracker: Option<Arc<IsrAckTracker>>,  // NEW
) -> Arc<Self> {
    // ... existing code ...
    Self {
        // ... existing fields ...
        isr_ack_tracker: ack_tracker,
    }
}

// Add ACK receiver in connection handler
// NEW: Spawn background task to listen for ACKs from this follower
let ack_tracker_clone = Arc::clone(&self.isr_ack_tracker);
tokio::spawn(async move {
    loop {
        // Read ACK frame from follower connection
        match read_ack_frame(&mut stream).await {
            Ok(ack) => {
                if let Some(ref tracker) = ack_tracker_clone {
                    tracker.record_ack(&ack.topic, ack.partition, ack.offset, ack.node_id);
                }
            }
            Err(e) => {
                warn!("Failed to read ACK from follower: {}", e);
                break;
            }
        }
    }
});
```

---

### Task 4: Wire IsrAckTracker in IntegratedKafkaServer

**File**: `crates/chronik-server/src/integrated_server.rs`

```rust
// Around line 443-455 (where we create WalReplicationManager)

// Create ISR ACK tracker for acks=-1 support
let isr_ack_tracker = Arc::new(crate::isr_ack_tracker::IsrAckTracker::new());
info!("Created IsrAckTracker for acks=-1 quorum support");

// Set on ProduceHandler
produce_handler_inner.set_isr_ack_tracker(Arc::clone(&isr_ack_tracker));

// Pass to WalReplicationManager
let replication_manager = crate::wal_replication::WalReplicationManager::new_with_dependencies(
    followers,
    raft_cluster.clone(),
    Some(isr_tracker),
    Some(isr_ack_tracker.clone()),  // NEW parameter
);

// Spawn cleanup task for expired acks
let ack_tracker_clone = Arc::clone(&isr_ack_tracker);
tokio::spawn(async move {
    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;
        ack_tracker_clone.cleanup_expired(Duration::from_secs(30));
    }
});
```

---

## Testing Strategy

### Test 1: acks=0 (Fast Path - No Change)

```bash
# Benchmark with acks=0
./target/release/chronik-bench --bootstrap-servers localhost:9092 \
  --duration 30 --concurrency 128 --message-size 256 --acks 0

# Expected: >= 60K msg/s (baseline)
```

### Test 2: acks=1 (Fast Path - No Change)

```bash
# Benchmark with acks=1 (default)
./target/release/chronik-bench --bootstrap-servers localhost:9092 \
  --duration 30 --concurrency 128 --message-size 256 --acks 1

# Expected: >= 55K msg/s (current baseline: 61,946 msg/s)
```

### Test 3: acks=-1 (Slow Path - NEW)

```python
# test_acks_minus_1.py
from kafka import KafkaProducer
import time

producer = KafkaProducer(
    bootstrap_servers='localhost:9092',
    acks='all',  # acks=-1
    request_timeout_ms=30000
)

start = time.time()
for i in range(100):
    future = producer.send('test-acks', f'Message {i}'.encode())
    result = future.get(timeout=35)
    print(f"✓ Offset {result.offset} (latency: {(time.time() - start) * 1000:.1f}ms)")
    start = time.time()

producer.flush()
```

**Expected**:
- ✅ All sends succeed
- ✅ Latency: 5-20ms (vs 2ms for acks=1)
- ✅ No timeouts

### Test 4: Cluster End-to-End

```bash
# Start 3-node cluster
# Node 1 (leader)
CHRONIK_REPLICATION_FOLLOWERS="localhost:9193,localhost:9194" \
./target/release/chronik-server --kafka-port 9092 --node-id 1 standalone

# Node 2 (follower)
CHRONIK_WAL_RECEIVER_ADDR="0.0.0.0:9193" \
./target/release/chronik-server --kafka-port 9093 --node-id 2 standalone

# Node 3 (follower)
CHRONIK_WAL_RECEIVER_ADDR="0.0.0.0:9194" \
./target/release/chronik-server --kafka-port 9094 --node-id 3 standalone

# Test acks=-1
python3 test_acks_minus_1.py

# Verify data on ALL nodes
for port in 9092 9093 9094; do
  kafka-console-consumer --bootstrap-server localhost:$port \
    --topic test-acks --from-beginning --max-messages 10
done
```

---

## Performance Targets

| acks | Expected Throughput | Latency |
|------|-------------------|---------|
| 0 | >= 60K msg/s | < 1ms |
| 1 | >= 55K msg/s | 2-3ms |
| -1 | >= 40K msg/s | 5-20ms |

---

## Success Criteria

- ✅ acks=0/1 maintain current performance (55K+ msg/s)
- ✅ acks=-1 achieves 40K+ msg/s
- ✅ acks=-1 waits for ISR quorum
- ✅ acks=-1 times out correctly (30s)
- ✅ Followers send ACKs after WAL write
- ✅ Leader tracks ACKs and notifies producers
- ✅ End-to-end cluster test passes

---

## Files to Create/Modify

### New Files
1. `crates/chronik-server/src/isr_ack_tracker.rs` - IsrAckTracker component

### Modified Files
1. `crates/chronik-server/src/produce_handler.rs` - acks routing logic
2. `crates/chronik-server/src/wal_replication.rs` - ACK protocol
3. `crates/chronik-server/src/integrated_server.rs` - Wiring
4. `crates/chronik-server/src/lib.rs` - Export new module

---

**Estimated Time**: 1-2 days
**Next Phase**: Phase 5 (Partition Leader Election)
