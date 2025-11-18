# Chronik Cluster Performance Analysis v2.2.9

## Executive Summary

**Current Performance**: 201 msg/s (99.7% below target)
**Target Performance**: 15,000 msg/s with acks=-1
**Status**: ✅ Functional but critically slow

After implementing produce request forwarding (v2.2.9), the cluster is stable and functional with 100% success rate, but performance is **75x slower than target**.

## Critical Issues Discovered

### 1. WAL Worker Thrashing (HIGH PRIORITY)

**Problem**: WAL commit workers fire every 100ms across ALL partitions, creating massive log spam even when idle.

**Evidence**:
```
[2025-11-17T22:51:44.963Z] DEBUG chronik_wal::group_commit: ⏰ WORKER_TICK: Interval tick fired
[2025-11-17T22:51:44.964Z] DEBUG chronik_wal::group_commit: ⚪ COMMIT_EMPTY: queue is empty
[2025-11-17T22:51:44.995Z] DEBUG chronik_wal::group_commit: ⏰ WORKER_TICK: Interval tick fired
[2025-11-17T22:51:44.995Z] DEBUG chronik_wal::group_commit: ⚪ COMMIT_EMPTY: queue is empty
```

**Root Cause**:
- 23 active partitions × 10 ticks/second = **230 empty commit checks/second**
- Each check acquires locks, checks queues, logs debug messages
- Wastes CPU cycles and fills logs with noise

**Impact**:
- CPU overhead from constant empty polling
- Log bloat makes debugging difficult
- Prevents identifying actual bottlenecks

**Fix Options**:
1. **Conditional logging**: Only log when queue is non-empty
2. **Adaptive intervals**: Increase interval when idle, decrease when active
3. **Shared worker**: Single worker handling multiple partitions

**Recommended Fix** (v2.2.10):
```rust
// In worker_loop, only log when there's actual work
if !pending.is_empty() {
    debug!("⏰ WORKER_TICK: Processing {} pending writes", pending.len());
}

// Adaptive interval based on activity
let interval_ms = if last_commit_had_work {
    config.max_wait_time_ms  // 100ms when active
} else {
    1000  // 1 second when idle
};
```

---

### 2. Node 1 Deadlock (CRITICAL - ROOT CAUSE UNKNOWN)

**Problem**: Node 1 completely froze for 3+ hours, causing Raft cluster dysfunction.

**Evidence**:
```
Last log entry: 2025-11-17T20:04:44.987658Z
Current time:   2025-11-17T23:45:00Z
Duration:       3 hours 40 minutes FROZEN
```

**Symptoms**:
- Node 2 couldn't reach Node 1 via Raft:
  ```
  ERROR chronik_server::raft_cluster: FAILED to send to peer 1 after 10 retries - MESSAGE LOST
  WARN chronik_raft::transport::grpc: Raft RPC failed ... to=1 msg_type=MsgHeartbeat error=Timeout expired
  ```
- Node 1 was stuck in: `WORKER_LOOP: Waiting for notification or interval tick`
- Process still running but completely unresponsive

**Potential Causes**:
1. **Tokio runtime starvation**: Blocking operation in async context
2. **Deadlock**: Lock cycle between WAL, Raft, and metadata operations
3. **Infinite loop**: Worker loop got stuck in a tight loop
4. **Channel deadlock**: mpsc channel full, all senders blocked

**Investigation Needed**:
- [ ] Identify all `.await` points that could block indefinitely
- [ ] Map all lock acquisition order (prevent lock inversions)
- [ ] Add timeouts to all channel sends
- [ ] Add deadlock detection instrumentation

**Suspected Code Paths** (need review):
1. [raft_cluster.rs:1238-1435](file:///home/ubuntu/Development/chronik-stream/crates/chronik-server/src/raft_cluster.rs#L1238) - `forward_produce_to_leader()` holds connection during entire forward
2. [group_commit.rs:709-780](file:///home/ubuntu/Development/chronik-stream/crates/chronik-wal/src/group_commit.rs#L709) - Worker loop with `tokio::select!`
3. [raft_metadata_store.rs](file:///home/ubuntu/Development/chronik-stream/crates/chronik-server/src/raft_metadata_store.rs) - Raft propose could block

---

### 3. Topic Creation Slowness (HIGH PRIORITY)

**Problem**: Topic creation takes **30 seconds** (should be < 1 second).

**Evidence**:
```
[22:47:26] INFO  chronik_bench::benchmark: Creating topic 'bench-1000'...
[22:47:57] INFO  chronik_bench::benchmark: Running warmup for 5s...
Duration: 31 seconds
```

**Root Cause** (likely): Raft consensus + metadata replication overhead
- CreateTopics → Raft propose → Wait for majority ACK → Apply to state machine → Return
- Each step adds latency:
  1. Raft propose: ~100-200ms (network + log persistence)
  2. Leader election if needed: Up to 10 seconds
  3. Snapshot transfer if lagging: Up to 20 seconds
  4. Metadata state machine apply: ~10-50ms

**Contributing Factors**:
- Raft heartbeat interval: Default 100ms (can delay consensus)
- No caching of Raft leader (every CreateTopics queries leadership)
- Synchronous Raft propose without timeout
- Snapshot overhead if nodes are catching up

**Fix Options**:
1. **Cache Raft leader**: Avoid leader query on every request
2. **Async topic creation**: Return immediately, notify when ready
3. **Batching**: Create multiple topics in single Raft proposal
4. **Tune Raft timeouts**: Reduce heartbeat interval to 50ms

---

### 4. Throughput Analysis: Why Only 201 msg/s?

**Expected**: 15,000 msg/s with acks=-1 (3-node replication)
**Actual**: 201 msg/s
**Gap**: **99.7% slower than target**

**Throughput Breakdown** (estimated):
```
Component                   Latency    Max Throughput    Actual
------------------------------------------------------------
Kafka wire decode          ~100μs     10,000 msg/s       ✅ OK
Leadership check           ~50μs      20,000 msg/s       ✅ OK
WAL append (enqueue)       ~10μs      100,000 msg/s      ✅ OK
WAL fsync (group commit)   ~2ms       500 msg/s          ⚠️ BOTTLENECK
ISR quorum wait (acks=-1)  ~5ms       200 msg/s          🔴 BOTTLENECK
Response encode            ~100μs     10,000 msg/s       ✅ OK
```

**Identified Bottlenecks**:

#### 4a. WAL Group Commit Not Batching Effectively
- **Expected**: Batch 100+ writes per fsync
- **Actual**: Unknown (need instrumentation)
- **Suspicion**: Ultra profile has 100ms timeout, but with low traffic, most commits are < 10 writes

**Test Needed**:
```bash
# Add metric: wal_commit_batch_size_histogram
# Expected at 15k msg/s: ~1500 writes/batch (15000 msg/s ÷ 10 commits/s)
# Actual at 201 msg/s: ~20 writes/batch (201 msg/s ÷ 10 commits/s)
```

#### 4b. ISR Quorum Wait Overhead
- **Expected**: ISR ACKs arrive within 1-2ms (local network)
- **Actual**: Logs show ACKs arriving, but latency is ~5-7ms
- **Cause**: WAL replication socket I/O + follower processing

**Evidence from logs**:
```
p50: 4.24ms  ← Good
p99: 7.35ms  ← High (should be < 3ms for local cluster)
```

#### 4c. Produce Handler Serialization
- **Current**: Stream API with `buffered(10)` - processes 10 partitions concurrently
- **Issue**: Low concurrency limit (should be 64+ for high throughput)

**File**: [crates/chronik-server/src/produce_handler.rs:1076-1083](file:///home/ubuntu/Development/chronik-stream/crates/chronik-server/src/produce_handler.rs#L1076)
```rust
let response_partitions = stream::iter(topic_data.partitions)
    .map(|partition_data| {
        // Process partition
    })
    .buffered(10)  // ← Only 10 concurrent partitions
    .collect::<Vec<_>>()
    .await;
```

**Fix**: Increase to `.buffered(100)` or use `buffer_unordered(100)` for even better parallelism.

---

### 5. Produce Request Forwarding Overhead (NEW in v2.2.9)

**Problem**: Every non-leader produce creates a NEW TCP connection to leader.

**Code**: [crates/chronik-server/src/produce_handler.rs:1238-1435](file:///home/ubuntu/Development/chronik-stream/crates/chronik-server/src/produce_handler.rs#L1238)
```rust
async fn forward_produce_to_leader(...) {
    // Connect to leader's Kafka port
    let mut stream = TcpStream::connect(&leader_addr).await?;  // ← NEW CONNECTION

    // Send request
    stream.write_all(&frame_buf).await?;

    // Read response
    stream.read_exact(&mut response_buf).await?;

    // Connection closed
}
```

**Impact**:
- TCP 3-way handshake: ~1ms per request
- No connection pooling → massive overhead
- Scales terribly with high traffic

**Fix** (v2.2.10):
```rust
// Add connection pool field to ProduceHandler
connection_pool: Arc<DashMap<u64, tokio::net::TcpStream>>,

async fn forward_produce_to_leader(...) {
    // Reuse existing connection if available
    let mut stream = self.connection_pool
        .entry(leader_id)
        .or_insert_with(|| TcpStream::connect(&leader_addr).await?);

    // ... send/receive ...
}
```

---

### 6. Lock Contention Analysis

**Potential Hotspots** (need profiling):

1. **MetadataStateMachine** (read-heavy):
   - Uses `ArcSwap` for lock-free reads ✅ GOOD
   - But writes still clone entire state 🤔 EXPENSIVE

2. **Raft node lock**:
   - Single `tokio::Mutex<RawNode>` for entire cluster
   - Every propose, step, ready acquires this lock
   - Could be bottleneck at high throughput

3. **WAL partition queues**:
   - Per-partition `Mutex<VecDeque>` for pending writes
   - Should be fine since it's per-partition

**Instrumentation Needed**:
```bash
# Add lock hold time metrics
raft_lock_hold_duration_us
metadata_state_clone_duration_us
wal_queue_lock_contention_count
```

---

## Performance Optimization Plan (v2.2.10)

### Phase 1: Quick Wins (Target: 2,000 msg/s, 10x improvement)

1. ✅ **Remove WAL debug logging** (or make conditional)
   - File: `crates/chronik-wal/src/group_commit.rs:721-727`
   - Change: Only log when `!pending.is_empty()`
   - Impact: Reduce log I/O, make logs usable

2. ✅ **Add connection pooling to forwarding**
   - File: `crates/chronik-server/src/produce_handler.rs:1238-1435`
   - Change: Cache `TcpStream` per leader_id
   - Impact: Eliminate TCP handshake overhead (1-2ms → 0ms)

3. ✅ **Increase partition processing concurrency**
   - File: `crates/chronik-server/src/produce_handler.rs:1076`
   - Change: `.buffered(10)` → `.buffered(100)`
   - Impact: Process more partitions in parallel

**Expected Result**: 201 msg/s → 2,000 msg/s

---

### Phase 2: WAL Tuning (Target: 5,000 msg/s, 25x improvement)

4. ✅ **Add WAL batch size metrics**
   - File: `crates/chronik-wal/src/group_commit.rs:730-780`
   - Change: Emit `wal_commit_batch_size` histogram
   - Impact: Identify if batching is working

5. ✅ **Reduce WAL interval for low-latency**
   - File: `crates/chronik-wal/src/group_commit.rs:264-275`
   - Change: Ultra profile: `max_wait_time_ms: 10` (from 100)
   - Impact: 10x faster commits at low throughput

6. ✅ **Implement adaptive WAL intervals**
   - File: `crates/chronik-wal/src/group_commit.rs:709-780`
   - Change: Adjust interval based on queue depth
   - Impact: Low latency when idle, high throughput when busy

**Expected Result**: 2,000 msg/s → 5,000 msg/s

---

### Phase 3: Raft & Metadata (Target: 10,000 msg/s, 50x improvement)

7. ✅ **Cache Raft leader**
   - File: `crates/chronik-server/src/raft_metadata_store.rs`
   - Change: Cache leader_id, invalidate on election change
   - Impact: Reduce Raft queries from O(requests) to O(elections)

8. ✅ **Async topic creation**
   - File: `crates/chronik-protocol/src/handler.rs`
   - Change: Return immediately, use event-driven notification
   - Impact: 30s → < 1s for topic creation

9. ✅ **Batch Raft proposals**
   - File: `crates/chronik-server/src/raft_metadata_store.rs`
   - Change: Batch multiple metadata changes into single proposal
   - Impact: Reduce Raft consensus overhead

**Expected Result**: 5,000 msg/s → 10,000 msg/s

---

### Phase 4: Architecture Redesign (Target: 15,000+ msg/s)

10. ✅ **Partition Raft node per topic** (controversial)
    - Current: Single Raft cluster for ALL metadata
    - Proposed: Separate Raft cluster per topic (like Kafka controller)
    - Impact: Parallel metadata operations

11. ✅ **Lock-free produce path**
    - Current: Multiple locks per produce request
    - Proposed: Atomic operations + MPSC channels only
    - Impact: Eliminate lock contention

12. ✅ **Zero-copy forwarding**
    - Current: TCP socket with copy
    - Proposed: QUIC or shared memory for same-machine forwarding
    - Impact: Reduce forwarding overhead from 1-2ms to < 100μs

**Expected Result**: 10,000 msg/s → 15,000+ msg/s

---

## Deadlock Prevention Strategy

### Current Architecture Risks

**Lock Hierarchy** (needs verification):
```
Level 1: Raft node lock (tokio::Mutex<RawNode>)
Level 2: Metadata state machine (ArcSwap - lock-free reads, clone-write)
Level 3: WAL partition queues (parking_lot::Mutex<VecDeque>)
Level 4: TCP streams (no locks)
```

**Potential Deadlock Scenarios**:

1. **Raft → WAL → Raft**:
   - Thread A: Holds Raft lock, awaits WAL write
   - Thread B: WAL worker holds WAL lock, needs Raft for metadata
   - **Deadlock** if both wait indefinitely

2. **Metadata → Raft → Metadata**:
   - Thread A: Proposes metadata change (holds Raft lock), awaits state apply
   - Thread B: State machine apply needs Raft lock
   - **Deadlock** if poorly designed

3. **Channel Full → Blocking Send**:
   - Producer fills channel
   - Consumer blocked on lock
   - Producer blocks on channel send
   - **Deadlock**

### Deadlock Prevention Rules

1. ✅ **Never await while holding a lock**
   ```rust
   // BAD
   {
       let guard = mutex.lock().await;
       some_async_operation().await;  // ← Holds lock across await
   }

   // GOOD
   let data = {
       let guard = mutex.lock().await;
       guard.clone()
   };
   some_async_operation(data).await;  // ← Lock released before await
   ```

2. ✅ **Use `try_send()` for channels, never `send().await`**
   ```rust
   // BAD
   tx.send(msg).await?;  // ← Blocks if channel full

   // GOOD
   tx.try_send(msg).or_else(|_| Err(Error::Backpressure))?;
   ```

3. ✅ **Set timeouts on all `.await` operations**
   ```rust
   tokio::time::timeout(Duration::from_secs(30), operation())
       .await
       .context("Operation timed out after 30s")?
   ```

4. ✅ **Use lock-free data structures where possible**
   - `ArcSwap` for read-heavy state ✅
   - `DashMap` for concurrent maps ✅
   - Atomic primitives for counters ✅

### Code Review Checklist

**For every `.await` in the codebase:**
- [ ] Is a lock held across this await?
- [ ] If yes, can we release it before awaiting?
- [ ] Does this have a timeout?
- [ ] Can this block indefinitely?

**For every `Mutex`/`RwLock`:**
- [ ] What's the lock order?
- [ ] Is there a circular dependency?
- [ ] Can we use lock-free alternative?

---

## Instrumentation Gaps

**Missing Metrics** (add in v2.2.10):
```rust
// WAL performance
wal_commit_batch_size_histogram
wal_commit_duration_ms
wal_queue_depth_gauge

// Raft performance
raft_propose_duration_ms
raft_apply_duration_ms
raft_lock_hold_duration_us

// Produce path
produce_forward_count
produce_forward_duration_ms
produce_partition_concurrency_gauge
produce_end_to_end_latency_ms

// Lock contention
lock_acquisition_duration_us{"lock_name"}
lock_contention_count{"lock_name"}
```

**Missing Logs** (add with context):
```rust
// In produce handler
debug!("PRODUCE_START: topic={}, partitions={}, acks={}, size={}", ...);
debug!("LEADERSHIP_CHECK: topic={}, partition={}, is_leader={}, took={}μs", ...);
debug!("PRODUCE_COMPLETE: topic={}, partition={}, offset={}, took={}μs", ...);

// In WAL
debug!("WAL_COMMIT: batch_size={}, bytes={}, fsync_us={}", ...);

// In Raft
debug!("RAFT_PROPOSE: type={}, size={}, took={}μs", ...);
debug!("RAFT_APPLY: index={}, took={}μs", ...);
```

---

## Immediate Action Items

**Before next release (v2.2.10):**

1. 🔴 **CRITICAL**: Find and fix Node 1 deadlock root cause
   - Add comprehensive timeout instrumentation
   - Map all lock acquisition paths
   - Add deadlock detection

2. 🟠 **HIGH**: Implement connection pooling for forwarding
   - Target: Eliminate TCP handshake overhead
   - Expected: 1-2ms latency reduction

3. 🟠 **HIGH**: Remove/reduce WAL debug logging
   - Target: Make logs usable for debugging
   - Expected: Reduce log I/O, find real bottlenecks

4. 🟡 **MEDIUM**: Add performance instrumentation
   - Target: Identify actual bottlenecks with data
   - Expected: Metrics-driven optimization

5. 🟡 **MEDIUM**: Investigate topic creation slowness
   - Target: < 1 second topic creation
   - Expected: Better benchmark warmup times

---

## Testing Requirements

**Performance Regression Tests** (add to CI):
```bash
# Minimum acceptable throughput
cargo run --bin chronik-bench -- \
  --bootstrap-servers localhost:9092,localhost:9093,localhost:9094 \
  --topic perf-test \
  --message-count 10000 \
  --concurrency 10 \
  --acks 1 \
  --partitions 3 \
  --min-throughput 5000  # Fail if < 5k msg/s
```

**Stability Tests** (run before release):
```bash
# 24-hour soak test
cargo run --bin chronik-bench -- \
  --duration 24h \
  --message-count 0 \  # Unlimited
  --acks -1 \
  --partitions 10 \
  --concurrency 64

# Verify no deadlocks, memory leaks, or crashes
```

**Deadlock Detection** (add to integration tests):
```bash
# Run cluster with deadlock detector
TOKIO_DEADLOCK_DETECTION=true cargo test --test cluster_stress_test

# If deadlock detected, dump stack traces and fail test
```

---

## Conclusion

The cluster is **functional** but suffering from:
1. **Poor batching** → Low throughput
2. **Connection overhead** → High forwarding latency
3. **Unknown deadlock** → Stability risk
4. **Slow metadata ops** → Poor user experience

With systematic optimization (Phases 1-3), we can achieve **10,000+ msg/s** while maintaining stability.

**Next Steps**:
1. Implement Phase 1 quick wins → v2.2.10
2. Add comprehensive metrics → v2.2.11
3. Fix deadlock → v2.2.12
4. Full architecture redesign → v2.3.0 (if needed)

---

**Document Version**: v2.2.9
**Author**: Claude Code Assistant
**Date**: 2025-11-17
**Status**: Draft - Needs Review & Validation
