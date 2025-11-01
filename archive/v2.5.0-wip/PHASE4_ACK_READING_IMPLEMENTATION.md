# Phase 4 ACK Reading Loop Implementation

## Summary

Implemented the **critical missing piece** for acks=-1 support in multi-node Raft clusters: the ACK reading loop in `WalReplicationManager`.

## Problem

In Phase 4 WIP, WAL replication was working correctly:
- ✅ Leader sent WAL records to followers
- ✅ Followers received and wrote to local WAL
- ✅ Followers **SENT** ACKs back to leader (confirmed in logs: "ACK✓ Sent to leader")
- ❌ **Leader NEVER READ the ACKs** → `IsrAckTracker.record_ack()` never called → all acks=-1 requests timed out after 30 seconds

## Root Cause

The `WalReplicationManager.send_to_followers()` method was **write-only**:
```rust
async fn send_to_followers(&self, record: &WalReplicationRecord) {
    // ...
    conn.write_all(&frame).await  // Write WAL record
    // NO READ! ACKs sit in TCP buffer forever
}
```

Followers sent ACKs, but the leader's TCP stream was never read, so ACKs accumulated in the receive buffer unprocessed.

## Solution

### 1. Split TCP Streams

Changed `WalReplicationManager` connections from `TcpStream` to `OwnedWriteHalf`:

```rust
pub struct WalReplicationManager {
    // v2.5.0 Phase 4: Changed from TcpStream to OwnedWriteHalf
    connections: Arc<DashMap<String, OwnedWriteHalf>>,

    // Phase 4: Added ACK tracker for recording follower ACKs
    isr_ack_tracker: Option<Arc<IsrAckTracker>>,
}
```

### 2. Spawn ACK Reader Tasks

In `run_connection_manager()`, when connecting to followers:

```rust
// Split stream for bidirectional communication
let (read_half, write_half) = stream.into_split();

// Spawn ACK reader task (NEW!)
if let Some(ref ack_tracker) = self.isr_ack_tracker {
    let tracker = Arc::clone(ack_tracker);
    let shutdown = Arc::clone(&self.shutdown);
    let addr = follower_addr.clone();
    tokio::spawn(async move {
        Self::run_ack_reader(read_half, tracker, shutdown, addr).await
    });
}

// Store write-half for send_to_followers
self.connections.insert(follower_addr.clone(), write_half);
```

### 3. Implement ACK Reading Loop

New method `run_ack_reader()`:

```rust
async fn run_ack_reader(
    mut read_half: OwnedReadHalf,
    isr_ack_tracker: Arc<IsrAckTracker>,
    shutdown: Arc<AtomicBool>,
    follower_addr: String,
) -> Result<()> {
    while !shutdown.load(Ordering::Relaxed) {
        // 1. Read ACK frame header (magic=0x414B, version, length)
        // 2. Read complete ACK frame
        // 3. Deserialize WalAckMessage
        // 4. Call isr_ack_tracker.record_ack() ← KEY FIX!

        match bincode::deserialize::<WalAckMessage>(payload) {
            Ok(ack_msg) => {
                info!("ACK✓ Received from {}: {}-{} offset {} (node {})",
                      follower_addr, ack_msg.topic, ack_msg.partition,
                      ack_msg.offset, ack_msg.node_id);

                // THIS IS THE KEY FIX
                isr_ack_tracker.record_ack(
                    &ack_msg.topic,
                    ack_msg.partition,
                    ack_msg.offset,
                    ack_msg.node_id,
                );
            }
        }
    }
}
```

### 4. Share IsrAckTracker Between Components

In `integrated_server.rs`, created a **single** `IsrAckTracker` instance shared by both:
- `WalReplicationManager` (for recording ACKs from followers)
- `ProduceHandler` (for waiting on acks=-1 quorum)

```rust
// Create ONCE, use everywhere
let isr_ack_tracker = crate::isr_ack_tracker::IsrAckTracker::new();
produce_handler_inner.set_isr_ack_tracker(isr_ack_tracker.clone());

let replication_manager = WalReplicationManager::new_with_dependencies(
    followers,
    raft_cluster.clone(),
    Some(isr_tracker),
    Some(isr_ack_tracker.clone()),  // Same instance!
);
```

## Changes Made

### Files Modified

1. **crates/chronik-server/src/wal_replication.rs**
   - Import `OwnedReadHalf` and `OwnedWriteHalf`
   - Change `connections: DashMap<String, TcpStream>` → `DashMap<String, OwnedWriteHalf>`
   - Add `isr_ack_tracker: Option<Arc<IsrAckTracker>>` field
   - Update `new_with_dependencies()` to accept `isr_ack_tracker`
   - Update `run_connection_manager()` to split streams and spawn ACK readers
   - **NEW**: Implement `run_ack_reader()` method (110 lines)

2. **crates/chronik-server/src/integrated_server.rs**
   - Move `IsrAckTracker` creation before WAL replication setup
   - Share single `IsrAckTracker` instance between `ProduceHandler` and `WalReplicationManager`
   - Pass `isr_ack_tracker` to `WalReplicationManager::new_with_dependencies()`

## Expected Behavior After Fix

### Before (Phase 4 WIP)
```
Produce acks=-1 → Wait for ACKs → Timeout after 30s ❌
Leader logs: "WAL✓ Replicated" (yes)
Follower logs: "ACK✓ Sent to leader" (yes)
Leader logs: "ACK✓ Received" (NO! ← PROBLEM)
```

### After (Phase 4 Complete)
```
Produce acks=-1 → Wait for ACKs → Succeed in < 1s ✅
Leader logs: "WAL✓ Replicated" (yes)
Follower logs: "ACK✓ Sent to leader" (yes)
Leader logs: "ACK✓ Received from localhost:9192: test-topic-0 offset 0 (node 2)" (YES! ← FIX)
ProduceHandler logs: "acks=-1 quorum achieved for test-topic-0 offset 0 (2/2 ACKs in 50ms)" (YES!)
```

## Testing Plan

### 1. Start 3-Node Cluster

```bash
# Node 1 (leader, port 9092)
./target/release/chronik-server \
  --kafka-port 9092 \
  --advertised-addr localhost \
  --node-id 1 \
  --data-dir /tmp/chronik-cluster-node1 \
  raft-cluster \
  --raft-addr 0.0.0.0:9192 \
  --peers "2@localhost:9193,3@localhost:9194" \
  --bootstrap

# Node 2 (follower, port 9093)
CHRONIK_REPLICATION_FOLLOWERS="localhost:9291" \
./target/release/chronik-server \
  --kafka-port 9093 \
  --advertised-addr localhost \
  --node-id 2 \
  --data-dir /tmp/chronik-cluster-node2 \
  raft-cluster \
  --raft-addr 0.0.0.0:9193 \
  --peers "1@localhost:9192,3@localhost:9194" \
  --bootstrap

# Node 3 (follower, port 9094)
CHRONIK_REPLICATION_FOLLOWERS="localhost:9291" \
./target/release/chronik-server \
  --kafka-port 9094 \
  --advertised-addr localhost \
  --node-id 3 \
  --data-dir /tmp/chronik-cluster-node3 \
  raft-cluster \
  --raft-addr 0.0.0.0:9194 \
  --peers "1@localhost:9192,2@localhost:9193" \
  --bootstrap
```

### 2. Test acks=-1

```bash
# Run test script
python3 test_cluster_acks_minus_one.py

# Expected output:
#  ✅ Message 0: offset=0, partition=0, latency=50ms
#  ✅ Message 1: offset=1, partition=0, latency=45ms
#  ...
#  ✅ SUCCESS: All messages succeeded in 1.2s!
```

### 3. Check Leader Logs

```bash
# Look for ACK reception
grep "ACK✓ Received" /tmp/chronik-cluster-node1.log

# Expected:
# ACK✓ Received from localhost:9193: test-topic-0 offset 0 (node 2)
# ACK✓ Received from localhost:9194: test-topic-0 offset 0 (node 3)
```

### 4. Benchmark Throughput

```bash
# Compare standalone vs cluster acks=-1
python3 tests/benchmark_cluster_acks.py

# Expected:
# Standalone acks=-1: 45K msg/s (baseline)
# Cluster acks=-1:    20-30K msg/s (acceptable, network overhead)
```

## Performance Expectations

| Configuration | Throughput | Latency (p99) | Notes |
|---------------|------------|---------------|-------|
| Standalone acks=1 | 100K msg/s | < 10ms | No replication |
| Standalone acks=-1 | 46K msg/s | < 50ms | Local IsrAckTracker only |
| Cluster acks=-1 (before fix) | N/A | 30s timeout | ACKs not read |
| **Cluster acks=-1 (after fix)** | **20-30K msg/s** | **< 100ms** | ACKs flow correctly |

## Success Criteria

✅ No timeouts on acks=-1 produce requests
✅ Leader logs show "ACK✓ Received" messages from all followers
✅ ProduceHandler logs show "acks=-1 quorum achieved"
✅ End-to-end latency < 1 second for 10 messages
✅ Throughput > 20K msg/s with 3-node cluster

## Next Steps

1. **Test the implementation** with 3-node cluster
2. **Benchmark throughput** and compare to Phase 3 (acks=1)
3. **Document performance** in [PHASE4_CLUSTER_BENCHMARK_RESULTS.md](PHASE4_CLUSTER_BENCHMARK_RESULTS.md)
4. **Update FINAL_BENCHMARK_RESULTS.md** with acks=-1 cluster results
5. Consider Phase 5: Auto-scaling ISR based on ACK latency

## References

- [PHASE4_PERFORMANCE_REPORT.md](PHASE4_PERFORMANCE_REPORT.md) - Initial Phase 4 WIP findings
- [PHASE4_CLUSTER_BENCHMARK_RESULTS.md](PHASE4_CLUSTER_BENCHMARK_RESULTS.md) - Cluster test results
- [docs/WAL_STREAMING_PROTOCOL.md](docs/WAL_STREAMING_PROTOCOL.md) - WAL replication protocol spec
- [crates/chronik-server/src/isr_ack_tracker.rs](crates/chronik-server/src/isr_ack_tracker.rs) - ACK tracking implementation
