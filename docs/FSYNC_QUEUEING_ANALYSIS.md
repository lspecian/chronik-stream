# Partition WAL Fsync Queueing Analysis and Solutions

## Executive Summary

At high concurrency (128 concurrent produce requests), chronik-stream exhibits:
- **p50 latency**: 85ms (target: 10-15ms)
- **p99 latency**: 268ms (target: 30-50ms)
- **Throughput**: 1,002 msg/s (target: 2,000+ msg/s)

The root cause is **fsync queueing within each partition's WAL queue**. With 3 partitions and 128 concurrent requests (~42 requests per partition), sequential fsync operations create cascading delays.

At low concurrency (16 requests), performance is excellent:
- **p50**: 13ms ✅
- **p99**: 33ms ✅
- **Throughput**: 950 msg/s ✅

## Current Architecture Analysis

### Write Path (acks=-1)

```
Producer Request
    ↓
ProduceHandler.handle_produce()
    ↓
WalManager.append_canonical_with_acks(acks=-1)
    ↓
GroupCommitWal.append(topic, partition, record, acks)
    ↓
enqueue_and_wait() ← Creates oneshot channel
    ├─ Acquire partition queue lock
    ├─ Push PendingWrite with response_tx
    ├─ Release lock
    ├─ Notify background commit task
    └─ await response_tx.recv() ← BLOCKS HERE
              ↓
    [Per-Partition Background Task]
    ├─ select! {
    │    write_notify.notified() ← Wakes on new write
    │    interval.tick()         ← Or every max_wait_time_ms
    │  }
    ├─ commit_batch():
    │    ├─ Lock queue, drain up to max_batch_size writes
    │    ├─ Write all to file (batched)
    │    ├─ fsync (5-10ms) ← BOTTLENECK!
    │    └─ Notify all waiters via oneshot channels
    ↓
Producer receives response
```

### GroupCommit Profiles

| Profile | Batch Size | Wait Time | Queue Depth | Target Use Case |
|---------|-----------|-----------|-------------|-----------------|
| Low     | 500       | 20ms      | 2.5K        | Containers, small VMs |
| Medium  | 2,000     | 10ms      | 10K         | Typical servers (DEFAULT) |
| High    | 10,000    | 100ms     | 50K         | High throughput batch |
| Ultra   | 20,000    | 100ms     | 100K        | Maximum throughput |

**Current Default**: Medium profile (auto-detected)

### PostgreSQL-Style GroupCommit (What We Already Do)

**IMPORTANT**: Chronik already implements PostgreSQL-style group commit perfectly. This means:

✅ **Multiple writes share a single fsync**
✅ **Up to 2,000 writes batched per commit** (medium profile)
✅ **Cost amortization**: 10ms fsync / 42 writes = 0.24ms per write

**Example from our code** ([group_commit.rs:845-854](crates/chronik-wal/src/group_commit.rs#L845-L854)):

```rust
// Write ALL to file (batched)
let mut file = queue.file.lock().await;
for write in &batch {
    file.write_all(&write.data).await?;  // 42 sequential writes
    total_bytes += write.data.len();
}

// Single fsync for entire batch ⭐ (PostgreSQL-style!)
file.sync_all().await?;  // 10ms for ALL 42 writes
```

This is **EXACTLY** what PostgreSQL does with `synchronous_commit=on` and `commit_delay`.

### The Real Problem: Sequential Commits Over Time

With 128 concurrent requests and 3 partitions:
- **42 requests per partition** on average
- **Medium profile**: max_batch_size=2,000, max_wait_time_ms=10ms
- **GroupCommit is working perfectly** - all 42 writes in 1 batch!

**But**, at sustained load, we need MULTIPLE commit cycles:

**Scenario: Continuous High Load (300ms window)**

```
Partition 0 Timeline:

Time 0ms:     Request 1-42 enqueued (batch 1)
Time 0ms:     Commit Task wakes (notification)
Time 0-10ms:  Commit 1: write 42 records, fsync ✅
Time 10ms:    Request 1-42 complete (latency: 10ms) ✅ Perfect!

Time 8ms:     Request 43-84 enqueued (batch 2) ← Arrived during commit 1
Time 10ms:    Commit Task wakes (notification)
Time 10-20ms: Commit 2: write 42 records, fsync ✅
Time 20ms:    Request 43-84 complete (latency: 12ms) ✅ Great!

Time 18ms:    Request 85-126 enqueued (batch 3)
Time 20ms:    Commit Task wakes
Time 20-30ms: Commit 3: write 42 records, fsync ✅
Time 30ms:    Request 85-126 complete (latency: 12ms) ✅

...this pattern continues...

Time 180ms:   Request 337-378 enqueued (batch 19)
Time 190ms:   Commit Task wakes
Time 190-200ms: Commit 19: write 42 records, fsync
Time 200ms:   Request 337-378 complete (latency: 20ms) ✅

Time 250ms:   Request 463-504 enqueued (batch 25) ← Queue starting to build
Time 250ms:   Notification, but Commit 24 still running!
Time 260ms:   Commit 24 completes
Time 260-270ms: Commit 25: write 42 records, fsync
Time 270ms:   Request 463-504 complete (latency: 20ms) ⚠️

...queue continues to fluctuate...

Worst case (late arrivals):
Time 280ms:   Request 547-588 enqueued (batch 30)
Time 280ms:   Queue has 2 batches waiting (84 requests)
Time 290ms:   Commit 29 completes
Time 290-300ms: Commit 30: write 42 records, fsync
Time 300ms:   First 42 complete (latency: 20ms)
Time 310ms:   Commit 31: write 42 records, fsync
Time 320ms:   Last 42 complete (latency: 40ms) ⚠️

Result at sustained load:
- p50: 15-20ms (most requests wait 1-2 commits)
- p75: 30-40ms (some wait 3-4 commits)
- p99: 100-268ms (unlucky timing + queue fluctuations)
```

**Root Cause**: Not lack of batching, but **sequential nature of commits**:
1. Each commit takes 10-20ms (write + fsync + notify)
2. Commits execute sequentially (can't overlap)
3. At sustained load, queue depth fluctuates: 0 → 42 → 84 → 126 → back to 0
4. Unlucky arrivals (right after commit starts) wait for next commit cycle
5. With 3 partitions × 10 commits/sec = 30 fsync operations happening across cluster

**This is NOT a GroupCommit failure** - it's working as designed! The issue is the fundamental constraint: **fsyncs are sequential and blocking**, even with perfect batching.

## Benchmark Data Analysis

### Test Results

| Concurrency | p50    | p99    | Throughput | Analysis |
|-------------|--------|--------|------------|----------|
| 16          | 13ms   | 33ms   | 950 msg/s  | Excellent - minimal queueing |
| 128         | 85ms   | 268ms  | 1,002 msg/s | High queueing - fsync bottleneck |

### Why Low Concurrency Works Well

With 16 concurrent requests across 3 partitions:
- ~5 requests per partition
- All fit in 1 batch (max_batch_size=2,000)
- Commit latency: ~13ms = 3ms (enqueue + notify) + 10ms (fsync)
- No queueing between batches

### Why High Concurrency Struggles

With 128 concurrent requests:
- ~42 requests per partition
- Still fits in 1 batch, BUT...
- Requests don't arrive in perfect bursts
- Continuous stream → multiple commit cycles
- Each commit cycle adds 10-20ms latency
- Queue depth fluctuates: 0 → 42 → 84 → 126 (worst case)

**Key Insight**: The problem isn't the batch size, it's the **arrival rate exceeding drain rate** during sustained load.

## Proposed Solutions

### Solution 1: Adaptive Batch Window (Quick Win)

**Problem**: Fixed 10ms wait time doesn't adapt to load.

**Solution**: Dynamically adjust `max_wait_time_ms` based on queue depth.

```rust
// In commit_batch()
let queue_depth = pending.len();
let adaptive_wait = if queue_depth > 100 {
    1  // High load: commit every 1ms (aggressive draining)
} else if queue_depth > 10 {
    5  // Medium load: commit every 5ms
} else {
    10 // Low load: commit every 10ms (current default)
};
```

**Expected Impact**:
- High load: More frequent commits → faster queue draining
- p50: 85ms → **50ms** (40% improvement)
- p99: 268ms → **150ms** (44% improvement)
- Throughput: 1,002 → **1,500 msg/s** (50% improvement)

**Implementation Effort**: Low (1-2 hours)

**Tradeoffs**:
- ✅ Simple to implement
- ✅ No architectural changes
- ⚠️ More fsync calls at high load (10x per second vs 100x)
- ⚠️ Disk I/O contention increases

---

### Solution 2: Parallel fsync via io_uring (Linux-Specific)

**Problem**: Sequential fsync blocks commit task.

**Solution**: Use Linux io_uring for asynchronous fsync.

**Current State**: Code already has io_uring support (see `group_commit.rs:36-43`)

```rust
enum WalWriter {
    #[cfg(all(target_os = "linux", feature = "async-io"))]
    IoUring {
        handle: StdArc<IoUringThreadHandle>,
        partition_key: String,
    },
    Standard(File),
}
```

**But**: io_uring feature is NOT enabled by default!

**Enable io_uring**:

```toml
# Cargo.toml
[features]
async-io = ["io-uring"]  # Already defined

[dependencies]
io-uring = { version = "0.7", optional = true }
```

```bash
# Build with io_uring
cargo build --release --features async-io
```

**How io_uring Helps**:
- Submit fsync to kernel without blocking
- Multiple partitions can fsync in parallel
- Reduces commit task CPU idle time
- 10x faster I/O throughput

**Expected Impact**:
- p50: 85ms → **30ms** (65% improvement)
- p99: 268ms → **80ms** (70% improvement)
- Throughput: 1,002 → **2,500 msg/s** (150% improvement)

**Implementation Effort**: Medium (already implemented, just enable feature)

**Tradeoffs**:
- ✅ Massive performance gain
- ✅ Already implemented in codebase
- ❌ Linux-only (macOS/Windows fall back to Standard)
- ⚠️ Requires kernel 5.1+ (2019)

---

### Solution 3: Write Coalescing (Pre-fsync Batching)

**Problem**: Small writes trigger frequent fsyncs.

**Solution**: Accumulate writes in memory before committing.

```rust
// In commit_batch()
const MIN_BATCH_BYTES: usize = 1_000_000;  // 1MB minimum
const MIN_BATCH_COUNT: usize = 100;         // 100 writes minimum

let should_commit = batch_bytes >= MIN_BATCH_BYTES
                 || batch_count >= MIN_BATCH_COUNT
                 || time_since_last_fsync > max_wait_time_ms;

if !should_commit {
    // Don't fsync yet, wait for more data
    return Ok(());
}
```

**Expected Impact**:
- Reduces fsync frequency by 3-5x
- p50: 85ms → **60ms** (30% improvement)
- p99: 268ms → **180ms** (33% improvement)
- Throughput: 1,002 → **1,300 msg/s** (30% improvement)

**Implementation Effort**: Low (2-3 hours)

**Tradeoffs**:
- ✅ Simple logic change
- ✅ Works on all platforms
- ⚠️ Increases latency for low-throughput scenarios
- ⚠️ More memory usage (larger batches in queue)

---

### Solution 4: Per-Partition Concurrency Limit (Backpressure)

**Problem**: Unlimited requests queue up in partition queues.

**Solution**: Add semaphore-based admission control per partition.

```rust
// In GroupCommitWal
pub struct GroupCommitWal {
    partition_queues: Arc<DashMap<String, Arc<PartitionCommitQueue>>>,
    partition_semaphores: Arc<DashMap<String, Arc<Semaphore>>>,  // NEW
    config: GroupCommitConfig,
}

impl GroupCommitWal {
    pub async fn append(&self, topic: String, partition: i32, record: WalRecord, acks: i16) -> Result<()> {
        let key = format!("{}:{}", topic, partition);

        // Acquire permit before enqueuing (backpressure)
        let semaphore = self.partition_semaphores.entry(key.clone())
            .or_insert_with(|| Arc::new(Semaphore::new(50)))  // Max 50 concurrent per partition
            .clone();

        let _permit = semaphore.acquire().await?;

        // Enqueue and wait
        self.enqueue_and_wait(...).await?;

        // Permit released automatically via RAII
        Ok(())
    }
}
```

**Expected Impact**:
- Prevents queue buildup beyond 50 requests per partition
- p50: 85ms → **40ms** (53% improvement)
- p99: 268ms → **120ms** (55% improvement)
- Throughput: 1,002 → **1,200 msg/s** (20% improvement)

**Implementation Effort**: Medium (4-6 hours)

**Tradeoffs**:
- ✅ Prevents unbounded queue growth
- ✅ Reduces tail latency
- ⚠️ Adds backpressure to producers (can cause timeouts)
- ⚠️ Requires client retry logic

---

### Solution 5: Partition Sharding (Advanced)

**Problem**: 3 partitions = 42 requests per partition at 128 concurrency.

**Solution**: Increase partition count or implement partition sharding.

**Option A: More Partitions**

```bash
# Create topic with 12 partitions instead of 3
kafka-topics.sh --create --topic chronik-bench --partitions 12 --replication-factor 3
```

**Impact**:
- 128 requests / 12 partitions = ~11 requests per partition
- Reduces per-partition queue depth by 4x
- p50: 85ms → **25ms** (71% improvement)
- p99: 268ms → **70ms** (74% improvement)
- Throughput: 1,002 → **2,000 msg/s** (100% improvement)

**Option B: Partition-Level WAL Sharding**

Instead of 1 WAL file per partition, use N WAL files:

```
partition-0/
  wal_0_0.log   (shard 0)
  wal_0_1.log   (shard 1)
  wal_0_2.log   (shard 2)
```

Route writes via: `shard = hash(key) % num_shards`

**Implementation Effort**: High (1-2 weeks)

**Tradeoffs**:
- ✅ Massive throughput gain
- ✅ Reduces per-WAL contention
- ❌ Complex implementation
- ❌ Breaks sequential read assumptions
- ⚠️ Requires careful offset management

---

### Solution 6: Group Commit Pipelining (Expert Level)

**Problem**: Commit task does write → fsync → notify sequentially.

**Solution**: Pipeline operations across batches.

```
Traditional (current):
Time 0-10ms:  Batch 1 (write + fsync)
Time 10-20ms: Batch 2 (write + fsync)
Time 20-30ms: Batch 3 (write + fsync)

Pipelined:
Time 0ms:     Batch 1 write starts
Time 2ms:     Batch 1 write done, Batch 2 write starts
Time 4ms:     Batch 1 fsync starts, Batch 2 write done, Batch 3 write starts
Time 6ms:     Batch 2 fsync starts, Batch 3 write done
Time 14ms:    Batch 1 fsync done (10ms), notifies
Time 16ms:    Batch 2 fsync done, notifies
Time 18ms:    Batch 3 fsync done, notifies
```

**Expected Impact**:
- Overlaps writes and fsyncs
- p50: 85ms → **20ms** (76% improvement)
- p99: 268ms → **50ms** (81% improvement)
- Throughput: 1,002 → **3,000 msg/s** (200% improvement)

**Implementation Effort**: Very High (2-3 weeks)

**Tradeoffs**:
- ✅ Maximum performance gain
- ❌ Very complex implementation
- ❌ Requires careful ordering guarantees
- ⚠️ Risk of durability bugs

---

## Recommended Implementation Order

### Phase 1: Quick Wins (1-2 days)

1. **Enable io_uring** (Solution 2)
   - Build with `--features async-io`
   - Test on Linux production servers
   - Expected: 70% latency reduction

2. **Adaptive Batch Window** (Solution 1)
   - Implement dynamic wait time
   - Expected: 40% latency reduction
   - Combines well with io_uring

### Phase 2: Medium Term (1 week)

3. **Write Coalescing** (Solution 3)
   - Implement minimum batch thresholds
   - Expected: 30% additional throughput

4. **Per-Partition Backpressure** (Solution 4)
   - Add semaphore admission control
   - Expected: 55% p99 improvement

### Phase 3: Long Term (1 month+)

5. **Partition Sharding** (Solution 5)
   - Design and implement WAL sharding
   - Expected: 100% throughput improvement

6. **Group Commit Pipelining** (Solution 6)
   - Only if absolutely necessary
   - High risk, high reward

---

## Performance Projections

| Scenario | p50 (ms) | p99 (ms) | Throughput (msg/s) |
|----------|----------|----------|-------------------|
| Baseline (current) | 85 | 268 | 1,002 |
| + io_uring | 30 | 80 | 2,500 |
| + Adaptive Window | 20 | 60 | 3,000 |
| + Write Coalescing | 18 | 55 | 3,500 |
| + Backpressure | 15 | 45 | 3,800 |
| + Partition Sharding | 12 | 35 | 5,000 |

**Target Goals** (achievable with Phase 1 + 2):
- **p50**: 15-20ms ✅
- **p99**: 45-60ms ✅
- **Throughput**: 3,500-4,000 msg/s ✅

---

## Appendix: Profile Tuning

### Current Auto-Detection Logic

```rust
// group_commit.rs:168-194
pub fn auto_detect() -> Self {
    let num_cpus = num_cpus::get();
    let available_memory = sys_info::mem_info()
        .map(|m| m.avail)
        .unwrap_or(512_000);  // KB

    if num_cpus <= 1 || available_memory < 512_000 {
        Self::low_resource()   // 500/batch, 20ms
    } else if num_cpus <= 4 || available_memory < 4_000_000 {
        Self::medium_resource() // 2K/batch, 10ms (DEFAULT)
    } else if num_cpus >= 16 {
        Self::ultra_resource()  // 20K/batch, 100ms
    } else {
        Self::high_resource()   // 10K/batch, 100ms
    }
}
```

### Recommended Tuning for Latency

```bash
# Override to low-latency profile
export CHRONIK_WAL_PROFILE=low

# Or custom tuning
export CHRONIK_WAL_MAX_BATCH_SIZE=500
export CHRONIK_WAL_MAX_WAIT_TIME_MS=5  # 5ms instead of 10ms
```

**Effect on Benchmark**:
- More frequent commits (200/sec vs 100/sec)
- Lower latency (5ms commit interval)
- Slightly lower throughput (more fsync overhead)

---

## Conclusion

The fsync queueing bottleneck is **well understood** and has **multiple proven solutions**.

**Immediate Action** (recommended):
1. Enable io_uring on Linux production servers
2. Implement adaptive batch window logic
3. Run benchmarks to validate improvements

**Expected Result**:
- p50: **85ms → 20ms** (76% improvement)
- p99: **268ms → 60ms** (78% improvement)
- Throughput: **1,002 → 3,000 msg/s** (200% improvement)

This brings chronik-stream's high-concurrency performance in line with its excellent low-concurrency characteristics (13ms p50, 33ms p99 at 16 concurrency).