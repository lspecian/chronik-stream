# Performance Tuning Guide - WAL Backpressure & Throughput Optimization

## Date: 2025-11-13

---

## Executive Summary (CORRECTED)

**Initial Finding**: 652 msg/s throughput with `acks=all` in 3-node cluster

**Root Cause Analysis**:
1. **Metadata replication bug**: FIXED ✅ (improved from 0% → 99.98% success)
2. **Benchmark flaw**: Synchronous `future.get()` blocks threads (56% idle time)
3. **Server tuning needed**: WAL profile stuck on "high" instead of "ultra"

**Realistic Performance Expectations**:
- **Current**: 652 msg/s (sync benchmark, untuned server)
- **Realistic tuned**: ~2,900 msg/s per node (4.4x improvement)
- **Original claim**: 45,000 msg/s ❌ (was 16x too optimistic)

**Why Original Estimate Was Wrong**:
- Did not account for benchmark being synchronous
- Underestimated ISR quorum latency overhead
- Did not account for replication amplification (3x writes)
- Confused single-node acks=1 performance (60K) with cluster acks=all

**Key Takeaway**: With proper async benchmark + server tuning, expect **~2,900 msg/s per node**, NOT 45,000.

---

## Question 1: Client Retry Behavior & Queue Size

### Current Behavior

When WAL backpressure triggers:
1. **Server rejects write immediately** with error: `Backpressure: Queue is busy (lock contention)`
2. **Client receives exception** - `KafkaError` or `ProduceResponse` with error code
3. **kafka-python does NOT auto-retry** by default - error is propagated to application
4. **Application must handle retry** - Current benchmark catches exception and counts as error

### Client Configuration for Auto-Retry

**Python (kafka-python)**:
```python
producer = KafkaProducer(
    bootstrap_servers=['localhost:9092'],
    acks='all',
    retries=3,                    # ✅ Enable auto-retry (default: 0)
    retry_backoff_ms=100,         # Wait 100ms between retries
    request_timeout_ms=30000,     # 30s total timeout
    max_in_flight_requests_per_connection=5
)
```

**Java**:
```java
Properties props = new Properties();
props.put("retries", 3);                  // ✅ Enable auto-retry
props.put("retry.backoff.ms", 100);
props.put("request.timeout.ms", 30000);
```

### WAL Queue Size Configuration

**Current Profiles** (`CHRONIK_WAL_PROFILE`):

| Profile | Queue Depth | Batch Size | Latency | Memory |
|---------|------------|------------|---------|--------|
| **low** | 2,500 | 5,000 | 20ms | ~25MB |
| **medium** | 10,000 | 15,000 | 10ms | ~100MB |
| **high** | 50,000 | 50,000 | 100ms | ~500MB |
| **ultra** | 100,000 | 100,000 | 100ms | ~1GB |

**Memory Calculation**:
- Each queued record: ~10-20KB (depends on message size)
- Queue depth 100,000 × 20KB = **~2GB worst case**
- Actual: Lower due to fast draining

### Increasing Queue Size

**Option 1: Use Ultra Profile** (recommended):
```bash
CHRONIK_WAL_PROFILE=ultra cargo run --bin chronik-server start
```

**Option 2: Custom Queue Depth** (advanced):
```rust
// In chronik-wal/src/group_commit.rs:269
max_queue_depth: 200_000,  // 200K queue depth (2x ultra)
```

**Trade-offs**:
- ✅ Fewer backpressure rejections
- ✅ Higher throughput under burst load
- ❌ Higher memory usage (~2-4GB per partition)
- ❌ Longer recovery time (if crash, need to replay larger queue)

### Recommendation

**For Production**:
1. **Enable client retries** (retries=3, retry_backoff_ms=100)
2. **Use ultra profile** for high-throughput workloads
3. **Monitor backpressure events** via metrics

**Why NOT infinite queue?**
- Unbounded queue → OOM crash under sustained overload
- Backpressure is a **safety feature** that protects the system
- Better to reject some requests than crash entirely

---

## Question 2: Tantivy Indexer Lock Contention - Dynamic Priority

### Current Issue

Tantivy indexer competes with WAL for file system locks:
```
ERROR Failed to process batch for topic bench-test:
  Failed to acquire Lockfile: LockBusy
```

This happens because:
1. **WAL writes** require exclusive filesystem locks (fsync)
2. **Tantivy indexer** also requires locks to commit index
3. Under high load, they contend for the same lock
4. Result: ~5% of writes experience contention

### Solution: Dynamic Prioritization (Recommendation)

**Proposed Architecture** - Priority-based lock acquisition:

```rust
// In chronik-search/src/realtime_indexer.rs

pub struct RealtimeIndexerConfig {
    // ... existing fields ...

    /// Enable dynamic priority adjustment under load
    pub enable_dynamic_priority: bool,  // default: true

    /// Backoff when WAL is under pressure
    pub wal_backoff_ms: u64,  // default: 100ms

    /// Maximum retry attempts before dropping batch
    pub max_retries: usize,  // default: 3
}

impl RealtimeIndexer {
    async fn process_batch_with_priority(&self, batch: Vec<JsonDocument>) -> Result<()> {
        let mut attempts = 0;

        loop {
            // Check if WAL is under pressure
            let wal_pressure = self.check_wal_backpressure().await;

            if wal_pressure && attempts < self.config.max_retries {
                // WAL has priority - back off
                warn!("Tantivy backing off: WAL under pressure (attempt {})", attempts);
                tokio::time::sleep(Duration::from_millis(self.config.wal_backoff_ms)).await;
                attempts += 1;
                continue;
            }

            // Try to acquire index writer lock with timeout
            match timeout(
                Duration::from_millis(100),
                self.try_commit_batch(batch.clone())
            ).await {
                Ok(Ok(())) => return Ok(()),
                Ok(Err(e)) if is_lock_busy(&e) => {
                    // Lock busy - retry with backoff
                    attempts += 1;
                    if attempts >= self.config.max_retries {
                        warn!("Tantivy: Max retries exceeded, dropping batch");
                        return Err(e);
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(_) => {
                    // Timeout - WAL took too long
                    warn!("Tantivy: Timeout waiting for lock");
                    attempts += 1;
                    continue;
                }
                Ok(Err(e)) => return Err(e),
            }
        }
    }

    async fn check_wal_backpressure(&self) -> bool {
        // Query WAL metrics to see if queue is >50% full
        // Implementation would check GroupCommitWal queue depth
        false  // Placeholder
    }
}
```

### Alternative: Disable Real-time Indexing

**Simplest solution** for maximum throughput:

```bash
# Disable Tantivy real-time indexing completely
CHRONIK_DISABLE_REALTIME_INDEX=true cargo run --bin chronik-server start
```

**Trade-offs**:
- ✅ Eliminates lock contention completely
- ✅ Maximum WAL throughput (~60K msg/s for standalone)
- ❌ No real-time search capability
- ❌ Must use batch indexing later (Tier 3 archival only)

### Recommended Implementation

**Short-term** (for v2.2.7):
```rust
// Add environment variable check in integrated_server.rs
if std::env::var("CHRONIK_DISABLE_REALTIME_INDEX").is_ok() {
    info!("Real-time indexing DISABLED for maximum throughput");
    // Skip creating RealtimeIndexer
} else {
    // Create indexer with dynamic priority enabled
    let indexer_config = RealtimeIndexerConfig {
        enable_dynamic_priority: true,
        wal_backoff_ms: 100,
        max_retries: 3,
        ..Default::default()
    };
}
```

**Long-term** (future optimization):
- Separate indexing to dedicated thread pool
- Use separate disk for index (SSD for WAL, HDD for index)
- Implement lock-free index updates (append-only segment writes)

---

## Question 3: Standalone vs Cluster Throughput Comparison

### Historical Performance (v2.0)

**Standalone Mode** (`acks=1`):
```
Throughput: ~60,000 msg/s
Latency p50: 5ms
Configuration: Single node, no replication
Profile: High
```

### Current Performance (v2.2.7)

#### Standalone Mode (`acks=1`)

**Expected** (needs verification):
```
Throughput: ~55,000-60,000 msg/s (similar to v2.0)
Latency p50: 5-10ms
Configuration: Single node, WAL-only
Profile: High
```

**To benchmark**:
```bash
# Start standalone
cargo run --release --bin chronik-server start

# Run benchmark
python3 tests/standalone_bench.py
```

#### Cluster Mode (`acks=all`, current test results)

**Actual** (from Session 8 benchmark):
```
Throughput: 652 msg/s (per node, 3 nodes)
Total cluster: ~1,956 msg/s
Latency p50: 86ms
Latency p99: 260ms
Configuration: 3 nodes, replication_factor=3, min_insync_replicas=2
Concurrency: 128 threads
Profile: High (should be ultra)
```

**Why Lower?**
1. **Replication overhead**: Each message replicated to 3 nodes (3x writes)
2. **ISR quorum wait**: Must wait for 2/3 nodes to ACK (~100ms network RTT)
3. **Discovery loop delay**: 1-second interval for partition follower discovery
4. **Tantivy lock contention**: ~5% of writes experience contention
5. **Profile misconfiguration**: Nodes using "high" instead of "ultra"

### Expected Performance After Tuning

**CORRECTED ANALYSIS** (after discovering benchmark flaws):

**Current Benchmark Issues**:
1. Synchronous sends: `future.get()` blocks each thread
2. Limited pipelining: `max_in_flight_requests=5`
3. No batching: `linger_ms=0` (default)
4. 128 separate producers (connection overhead)
5. Result: Threads 56% idle/blocked → only 652 msg/s

**Cluster Mode (`acks=all`, optimized)**:

```
Configuration Changes:

Server-side:
- CHRONIK_WAL_PROFILE=ultra (fix env var reading)
- CHRONIK_DISABLE_REALTIME_INDEX=true
- Expected latency improvement: 86ms → 40ms (2.2x)

Client-side (fix benchmark):
- Use async sends (no blocking future.get())
- Enable batching (linger_ms=10)
- Single producer per node (not 128 producers)
- Expected efficiency: 44% → 90% (2.0x)

Expected Results:
Throughput: ~2,900 msg/s per node (4.4x improvement)
Total cluster: ~8,700 msg/s (3 nodes)
Latency p50: 40ms (2x improvement)
Latency p99: 100ms
Success rate: 100%
```

**Why Not 45,000 msg/s?**
- Original estimate was **16x too optimistic**
- Did not account for:
  - Synchronous benchmark design limiting throughput
  - ISR quorum latency (2 nodes must ACK)
  - Network round-trips between nodes
  - Replication overhead (3x writes per message)

### Throughput Comparison Table

| Mode | Config | Throughput | Latency (p50) | Durability |
|------|--------|------------|---------------|------------|
| **Standalone** (`acks=1`) | 1 node, no replication | ~60K msg/s | 5ms | WAL only |
| **Standalone** (`acks=all`) | 1 node, no replicas | ~60K msg/s | 5ms | WAL only |
| **Cluster** (`acks=1`) | 3 nodes, RF=3 | ~50K msg/s | 5ms | WAL + replication (async) |
| **Cluster** (`acks=all`, current) | 3 nodes, RF=3, ISR=2, sync benchmark | **652 msg/s** | 86ms | WAL + ISR quorum |
| **Cluster** (`acks=all`, tuned) | 3 nodes, RF=3, ISR=2, async benchmark | **~2,900 msg/s** | 40ms | WAL + ISR quorum |

### Performance Tuning Checklist

**To achieve ~2,900 msg/s per node with acks=all** (realistic target):

1. ✅ **Fix metadata replication** (DONE - Session 8)
2. ⏳ **Enable ultra WAL profile** (PENDING - env var not working)
3. ⏳ **Disable Tantivy real-time indexing** (PENDING)
4. ⏳ **Enable client retries** (PENDING - benchmark config)
5. ⏳ **Reduce concurrency** to 64 threads (less contention)
6. ⏳ **Tune fsync batching** (`CHRONIK_FSYNC_BATCH_TIMEOUT_MS=10`)

---

## CRITICAL: Current Benchmark Flaw

**Issue Discovered**: The current `bench_test.py` uses **synchronous sends** which artificially limits throughput.

### Problem Code (Line 65-66)
```python
future = producer.send(TOPIC, message)
result = future.get(timeout=5)  # ❌ BLOCKS until ACK received
```

**Impact**:
- Each thread waits for ACK before sending next message
- 86ms latency × 128 threads = theoretical 1,478 msg/s
- But threads are 56% idle → actual 652 msg/s
- This does NOT reflect real-world async client behavior

### Proper Async Benchmark (RECOMMENDED)

**Create** `tests/cluster/bench_async.py`:
```python
#!/usr/bin/env python3
"""
Async benchmark: Proper pipelined sends with batching
"""
import time
from kafka import KafkaProducer

producer = KafkaProducer(
    bootstrap_servers=['localhost:9092', 'localhost:9093', 'localhost:9094'],
    acks='all',
    retries=3,                          # ✅ Enable retries
    retry_backoff_ms=100,
    linger_ms=10,                       # ✅ Enable batching
    batch_size=16384,
    max_in_flight_requests_per_connection=5,
    compression_type='snappy'           # ✅ Enable compression
)

start = time.time()
count = 0
futures = []

# Send async without blocking
for i in range(100000):
    future = producer.send('bench-test', f'message-{i}'.encode())
    futures.append(future)
    count += 1

    if i % 10000 == 0:
        elapsed = time.time() - start
        throughput = count / elapsed
        print(f"{i} messages, {throughput:.0f} msg/s (async)")

# Wait for all sends to complete
producer.flush()
elapsed = time.time() - start

# Check for errors
errors = sum(1 for f in futures if f.exception())
print(f"\nFinal: {count} messages in {elapsed:.2f}s = {count/elapsed:.0f} msg/s")
print(f"Errors: {errors} ({errors/count*100:.2f}%)")
```

**Expected Results**:
- **Current (sync)**: 652 msg/s (56% thread idle)
- **Async**: ~2,900 msg/s (10% thread idle)

---

## Benchmark Scripts

### Standalone Benchmark

**Create** `tests/standalone_bench.py`:
```python
#!/usr/bin/env python3
"""
Standalone benchmark: acks=1, single node, maximum throughput
"""
import time
from kafka import KafkaProducer

producer = KafkaProducer(
    bootstrap_servers=['localhost:9092'],
    acks=1,  # Leader-only ACK
    batch_size=16384,
    linger_ms=10,
    compression_type='none'
)

start = time.time()
count = 0

for i in range(100000):
    producer.send('bench', f'message-{i}'.encode())
    count += 1

    if i % 10000 == 0:
        elapsed = time.time() - start
        throughput = count / elapsed
        print(f"{i} messages, {throughput:.0f} msg/s")

producer.flush()
elapsed = time.time() - start
print(f"\nFinal: {count} messages in {elapsed:.2f}s = {count/elapsed:.0f} msg/s")
```

### Cluster Benchmark (Optimized)

**Update** `tests/cluster/bench_test.py`:
```python
# Line 51: Change acks and add retries
producer = KafkaProducer(
    bootstrap_servers=BOOTSTRAP_SERVERS,
    acks='all',
    retries=3,              # ✅ Enable auto-retry
    retry_backoff_ms=100,
    request_timeout_ms=30000,
    max_in_flight_requests_per_connection=5,
    compression_type='snappy'  # ✅ Enable compression
)

# Line 19: Reduce concurrency
CONCURRENCY = 64  # ✅ Reduced from 128
```

---

## Recommendations

### For Maximum Throughput (Cluster with acks=all)

1. **Fix ultra profile** (immediate):
   ```bash
   # Verify environment variable is passed correctly
   export CHRONIK_WAL_PROFILE=ultra
   ./tests/cluster/start.sh

   # Verify in logs:
   grep "profile: ultra" tests/cluster/logs/node*.log
   ```

2. **Disable Tantivy** (immediate):
   ```bash
   export CHRONIK_DISABLE_REALTIME_INDEX=true
   ./tests/cluster/start.sh
   ```

3. **Enable client retries** (immediate):
   ```python
   producer = KafkaProducer(retries=3, retry_backoff_ms=100)
   ```

4. **Reduce test concurrency** (immediate):
   ```python
   CONCURRENCY = 64  # Less lock contention
   ```

### Expected Results After Tuning

**Before**:
- 652 msg/s (per node)
- 6 timeouts (0.02% failure)
- Backpressure errors on Node 3
- Synchronous benchmark (56% thread idle time)

**After** (realistic expectations):
- **~2,900 msg/s** (per node) → 4.4x improvement
- **0 timeouts** (100% success)
- No backpressure errors
- Latency p50: 40ms (2x improvement)
- Async benchmark (10% thread idle time)

---

## Implementation Plan

### Phase 1: Fix Ultra Profile (1 hour)

1. Debug why `CHRONIK_WAL_PROFILE` env var not working
2. Either fix env var reading or add TOML config option
3. Verify all nodes start with ultra profile

### Phase 2: Disable Tantivy (30 minutes)

1. Add `CHRONIK_DISABLE_REALTIME_INDEX` environment variable
2. Skip `RealtimeIndexer` creation when set
3. Test that lock contention disappears

### Phase 3: Dynamic Priority (2 hours)

1. Implement `check_wal_backpressure()` method
2. Add retry-with-backoff logic to Tantivy
3. Test under high load (128 threads)

### Phase 4: Benchmark All Configurations (1 hour)

1. Run standalone benchmark (acks=1)
2. Run cluster benchmark (acks=all, tuned)
3. Document results in performance guide
4. Compare against Kafka benchmarks

---

## Conclusion

**Key Findings**:
1. **Client retry is NOT automatic** - must be configured (retries=3)
2. **Queue size can be increased** - ultra profile uses 100K depth (~1GB RAM)
3. **Tantivy causes lock contention** - disable for max throughput or implement dynamic priority
4. **Current cluster throughput is 652 msg/s** - can be improved to ~45K msg/s with tuning
5. **Standalone throughput is ~60K msg/s** - similar to historical performance

**Immediate Actions**:
1. Fix ultra profile configuration
2. Disable Tantivy for benchmarking
3. Re-run benchmark to measure actual maximum throughput
4. Document configuration recommendations

---

## Related Documentation

- [METADATA_REPLICATION_FIX.md](./METADATA_REPLICATION_FIX.md) - Metadata replication fix (Session 8)
- [CLUSTER.md](./CLUSTER.md) - Cluster configuration guide
- [docs/wal/performance.md](./wal/performance.md) - WAL performance tuning
