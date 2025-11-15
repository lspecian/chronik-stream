# Produce Path Bottleneck Analysis (v2.2.7)

## Executive Summary

**Current Performance**: ~9K msg/s with 3-node cluster
**Expected Performance**: ~15K msg/s (66% improvement possible)
**Primary Bottleneck**: Raft metadata proposals sleeping 150ms on EVERY produce batch

## Complete Produce Path Analysis

###1 Request Flow

```
Client → ProduceHandler::handle_produce()
  ↓
 ProduceHandler::process_produce_request()
  ↓
 Auto-create topic (if needed) → RaftCluster::propose()  ❌ 150ms SLEEP!
  ↓
 Check leadership → RaftCluster.get_partition_leader()
  ↓
 Write to partition buffer → PartitionState.pending_batches
  ↓
 Update offsets → AtomicU64
  ↓
 Write to WAL (GroupCommitWal)
  ↓
 Return response
```

## Critical Bottlenecks (Prioritized)

### P0: Raft Proposal Sleep (150ms)

**Location**: [crates/chronik-server/src/raft_cluster.rs:416](crates/chronik-server/src/raft_cluster.rs#L416)

```rust
async fn propose_via_raft(&self, cmd: MetadataCommand) -> Result<()> {
    // ... propose to Raft ...

    // Wait for apply (via background message loop)
    // TODO: Implement proper wait mechanism (watch channel, condition var, etc.)
    tokio::time::sleep(tokio::time::Duration::from_millis(150)).await;  // ❌ KILLER!

    Ok(())
}
```

**Impact**:
- Every auto-created topic: +150ms
- Every partition assignment: +150ms
- Every metadata update: +150ms

**Fix**: Implement proper `tokio::sync::watch` channel or `Condvar` to wait for actual Raft commit

**Estimated Gain**: 30-40% throughput improvement

### P1: Serial Partition Processing

**Location**: [crates/chronik-server/src/produce_handler.rs:1020](crates/chronik-server/src/produce_handler.rs#L1020)

```rust
for partition_data in topic_data.partitions {
    // Process each partition serially
    // No parallelism!
}
```

**Impact**:
- With 3 partitions, 3x latency multiplier
- Each partition: leadership check + buffer write + WAL write

**Fix**: Use `futures::stream::FuturesUnordered` or `tokio::spawn` to process partitions concurrently

**Estimated Gain**: 20-30% throughput improvement

### P2: RwLock Contention on partition_states

**Location**: [crates/chronik-server/src/produce_handler.rs:454](crates/chronik-server/src/produce_handler.rs#L454)

```rust
let states = self.partition_states.read().await;
```

**Impact**:
- Every produce request acquires read lock
- 128 concurrent producers = heavy lock contention

**Fix**: Replace `Arc<RwLock<HashMap>>` with `Arc<DashMap>` (lock-free concurrent hashmap)

**Estimated Gain**: 10-15% throughput improvement

### P3: Pending Batches Mutex

**Location**: [crates/chronik-server/src/produce_handler.rs:500](crates/chronik-server/src/produce_handler.rs#L500)

```rust
let pending = state.pending_batches.lock().await;
```

**Impact**:
- Every batch write acquires mutex
- Serializes all writes to same partition

**Fix**: Use lock-free queue (`crossbeam::queue::SegQueue` or `tokio::sync::mpsc`)

**Estimated Gain**: 5-10% throughput improvement

### P4: GroupCommitWal Batching Not Optimal

**Location**: [crates/chronik-wal/src/group_commit.rs](crates/chronik-wal/src/group_commit.rs)

**Current Behavior**:
- `max_batch_size`: 1000 records
- `max_wait_time_ms`: 100ms (default profile)

**Issue**: With high concurrency (128 threads), batches fill up quickly but commit thread might not be fast enough

**Fix**:
- Increase `max_batch_size` to 5000 for cluster mode
- Reduce `max_wait_time_ms` to 50ms
- Or switch to `CHRONIK_WAL_PROFILE=ultra`

**Estimated Gain**: 10-15% throughput improvement

## Proposed Optimization Plan

### Phase 1: Quick Wins (1-2 hours)

**Goal**: 12K+ msg/s (33% improvement)

1. **Replace Raft sleep with proper wait mechanism**
   - Implement `tokio::sync::watch` channel for Raft commit notifications
   - Replace 150ms sleep with actual event wait
   - Test: Auto-create 10 topics, measure latency

2. **Parallel partition processing**
   - Wrap partition loop with `futures::stream::iter().for_each_concurrent()`
   - Test: Produce to 3 partitions, measure latency

### Phase 2: Lock-Free Data Structures (2-3 hours)

**Goal**: 14K+ msg/s (55% improvement)

3. **Replace RwLock with DashMap**
   - Change `partition_states: Arc<RwLock<HashMap>>` → `Arc<DashMap>`
   - Remove explicit `.read()/.write()` calls
   - Test: 128 concurrent producers, measure lock contention

4. **Lock-free pending batches**
   - Change `pending_batches: Arc<Mutex<Vec<BatchedRecord>>>` → `crossbeam::queue::SegQueue`
   - Or use `tokio::sync::mpsc` channel for batch pipelining
   - Test: Single partition, high concurrency

### Phase 3: WAL Tuning (30 minutes)

**Goal**: 15K+ msg/s (66% improvement)

5. **Enable ultra WAL profile**
   - Set `CHRONIK_WAL_PROFILE=ultra` in cluster config
   - Increase batch sizes for cluster workload
   - Test: Full benchmark with 128 concurrency

## Code Changes Required

### Change 1: Raft Commit Wait Mechanism

**File**: [crates/chronik-server/src/raft_cluster.rs](crates/chronik-server/src/raft_cluster.rs)

```rust
// Add to RaftCluster struct:
commit_notify: Arc<tokio::sync::watch::Sender<u64>>,
commit_watch: Arc<tokio::sync::watch::Receiver<u64>>,

// In propose_via_raft():
async fn propose_via_raft(&self, cmd: MetadataCommand) -> Result<()> {
    let proposed_index = {
        let mut raft = self.raft_node.write()?;
        raft.propose(vec![], data)?;
        raft.raft.raft_log.last_index()  // Get proposed index
    };

    // Wait for commit (with timeout)
    let mut watch = self.commit_watch.clone();
    tokio::time::timeout(
        Duration::from_millis(500),
        async {
            loop {
                watch.changed().await?;
                if *watch.borrow() >= proposed_index {
                    break;
                }
            }
            Ok::<_, anyhow::Error>(())
        }
    ).await??;

    Ok(())
}

// In message loop (where entries are applied):
fn apply_committed_entries(&self, entries: &[Entry]) {
    for entry in entries {
        // Apply to state machine...

        // Notify waiters
        self.commit_notify.send(entry.index).ok();
    }
}
```

### Change 2: Parallel Partition Processing

**File**: [crates/chronik-server/src/produce_handler.rs](crates/chronik-server/src/produce_handler.rs)

```rust
use futures::stream::{self, StreamExt};

async fn process_produce_request(&self, request: ProduceRequest, acks: i16) -> Result<Vec<ProduceResponseTopic>> {
    let mut response_topics = Vec::new();

    for topic_data in request.topics {
        // ... topic validation ...

        // BEFORE (serial):
        // let mut response_partitions = Vec::new();
        // for partition_data in topic_data.partitions {
        //     let result = self.process_partition(partition_data).await?;
        //     response_partitions.push(result);
        // }

        // AFTER (parallel):
        let response_partitions = stream::iter(topic_data.partitions)
            .map(|partition_data| async move {
                self.process_partition(&topic_data.name, partition_data).await
            })
            .buffer_unordered(16)  // Process up to 16 partitions concurrently
            .collect::<Vec<_>>()
            .await;

        response_topics.push(ProduceResponseTopic {
            name: topic_data.name,
            partitions: response_partitions.into_iter().collect::<Result<Vec<_>>>()?,
        });
    }

    Ok(response_topics)
}
```

### Change 3: DashMap for partition_states

**File**: [crates/chronik-server/src/produce_handler.rs](crates/chronik-server/src/produce_handler.rs)

```rust
use dashmap::DashMap;

pub struct ProduceHandler {
    // BEFORE:
    // partition_states: Arc<RwLock<HashMap<(String, i32), Arc<PartitionState>>>>,

    // AFTER:
    partition_states: Arc<DashMap<(String, i32), Arc<PartitionState>>>,
    ...
}

// Usage changes from:
// let states = self.partition_states.read().await;
// if let Some(state) = states.get(&key) { ... }

// To:
// if let Some(state) = self.partition_states.get(&key) {
//     let state = state.value().clone();
//     ...
// }
```

## Expected Results

### Before Optimizations
- **Throughput**: 9,056 msg/s
- **Latency p99**: 15.05ms
- **Bottleneck**: Raft proposal sleep (150ms per auto-create)

### After Phase 1 (Quick Wins)
- **Throughput**: ~12,000 msg/s (+33%)
- **Latency p99**: ~10ms (-33%)
- **Fix**: Proper Raft commit wait + parallel partitions

### After Phase 2 (Lock-Free)
- **Throughput**: ~14,000 msg/s (+55%)
- **Latency p99**: ~8ms (-47%)
- **Fix**: DashMap + lock-free batches

### After Phase 3 (WAL Tuning)
- **Throughput**: ~15,000+ msg/s (+66%)
- **Latency p99**: ~7ms (-53%)
- **Fix**: Ultra WAL profile

## Testing Strategy

### Test 1: Raft Proposal Latency
```bash
# Measure topic auto-creation time
time python3 -c "
from kafka import KafkaProducer
p = KafkaProducer(bootstrap_servers='localhost:9092')
for i in range(10):
    p.send(f'test-topic-{i}', b'msg').get(timeout=10)
"
```

**Before**: ~1.5 seconds (10 topics × 150ms)
**After**: < 500ms (actual Raft commit time)

### Test 2: Throughput Benchmark
```bash
./target/release/chronik-bench \
  --bootstrap-servers localhost:9092,localhost:9093,localhost:9094 \
  --topic bench-optimized \
  --concurrency 128 \
  --message-size 256 \
  --duration 60s \
  --acks 1
```

**Target**: 15K+ msg/s, p99 < 10ms

### Test 3: Partition Parallelism
```bash
# Produce to topic with 3 partitions
python3 -c "
from kafka import KafkaProducer
import time
p = KafkaProducer(bootstrap_servers='localhost:9092', acks=1)
start = time.time()
for i in range(300):
    p.send('test-3part', f'msg-{i}'.encode())
p.flush()
print(f'Time: {time.time() - start:.2f}s')
"
```

**Before**: Serial processing (3x latency)
**After**: Parallel processing (1x latency)

## Implementation Priority

**Immediate (today)**:
1. ✅ Document bottlenecks (this file)
2. ⏳ Fix Raft proposal sleep (P0)
3. ⏳ Parallel partition processing (P1)

**Short-term (this week)**:
4. ⏳ DashMap for partition_states (P2)
5. ⏳ Lock-free pending batches (P3)
6. ⏳ WAL tuning (P4)

**Nice-to-have**:
- Benchmark each optimization individually
- Create performance regression tests
- Document cluster performance tuning guide

---

**Status**: ANALYSIS COMPLETE
**Next Step**: Implement P0 (Raft proposal wait mechanism)
**Target Version**: v2.2.9
**Estimated Total Effort**: 4-6 hours
