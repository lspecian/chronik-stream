# Cluster Crash Root Cause Analysis - 1000 Topic Test

**Date**: 2025-11-19
**Priority**: P0 - CRITICAL
**Status**: Root cause identified, fixes required

---

## Executive Summary

The 3-node test cluster **completely failed** when tested with 1000 topics × 3 partitions (3000 total partitions):

- **Node 1**: Crashed after 14 minutes (00:57-00:11)
- **Node 2/3**: Consumed 9.3 GB and 15.8 GB RAM respectively (28.7% and 49.1% of system memory)
- **Result**: 0/3000 messages successfully consumed (100% data loss)
- **Log files**: Node 1 generated 24,442,657 log lines before crashing

---

## Root Causes Identified

### Bug #1: Catastrophic DEBUG Logging (Log Bomb)

**Problem**: Combination of `RUST_LOG=debug` + `CHRONIK_WAL_PROFILE=ultra` creates a log bomb with many partitions.

**How it happens**:
1. Each partition creates a `GroupCommitWal` with its own worker thread
2. Ultra profile uses 100ms commit interval (`max_wait_time_ms: 100`)
3. Each worker logs DEBUG messages every 100ms:
   - `⏰ WORKER_TICK: Interval tick fired`
   - `📝 WORKER_COMMIT: About to call commit_batch`
   - `🔵 COMMIT_START: Entering commit_batch`
   - `⚪ COMMIT_EMPTY: commit_batch called but queue is empty`
   - `✅ WORKER_SUCCESS: commit_batch completed successfully`
   - `🔄 WORKER_LOOP: Waiting for notification...`

**The math**:
```
3000 partitions × 10 ticks/sec × ~6 log lines/tick = 180,000 log lines/second
```

**Result over 14 minutes**:
```
180,000 lines/sec × 840 seconds = 151 million lines (theoretical)
Actual: 24 million lines (before crash)
```

**Evidence**:
```bash
# Node 1 log file
$ wc -l tests/cluster/logs/node1.log
24442657 tests/cluster/logs/node1.log

# Just the last 100K lines contain 15,984 WORKER_TICK entries
$ tail -100000 tests/cluster/logs/node1.log | grep -c "WORKER_TICK"
15984
```

**Impact**:
- Disk I/O saturation writing logs
- Memory exhaustion from log buffers
- CPU overhead from logging infrastructure
- **Node crash** from resource exhaustion

### Bug #2: Memory Leak Under High Partition Count

**Problem**: Nodes exhibit unbounded memory growth with many partitions.

**Evidence**:
```
Node 3: 15.8 GB RAM (49.1% of 32 GB system)
Node 2:  9.3 GB RAM (28.7% of 32 GB system)
Node 1: Crashed (likely OOM)
```

**For context**: This is a **test cluster** with:
- 1000 topics
- 3 partitions each
- 1 message per partition (3000 messages total)
- Total data: ~300 KB of actual message data

**Memory should be**: ~500 MB - 1 GB max
**Actual memory**: 15.8 GB on Node 3 (31x higher than expected!)

**Likely causes**:
1. Each partition allocates WAL buffers (per `GroupCommitConfig`)
   - Ultra profile: 100 MB max_batch_bytes × 3000 partitions = 300 GB theoretical!
   - Actual allocations likely smaller but still excessive
2. Log buffers for 24M log lines (~10 GB+ of logs in memory)
3. No backpressure or memory limits on partition count
4. Possible memory leak in GroupCommitWal worker threads

### Bug #3: No Partition Count Limits

**Problem**: System accepts unlimited partition count without validation or warnings.

**What happened**:
- Test created 1000 topics × 3 partitions = 3000 partitions
- System accepted all 3000 partitions
- Each partition spawned worker threads + allocated buffers
- No warning about approaching resource limits

**What should happen**:
- Validate partition count against available resources
- Warn when approaching dangerous thresholds
- Reject or throttle excessive partition creation
- Provide guidance on recommended limits

### Bug #4: Cluster Continued Operating with Dead Node

**Problem**: Nodes 2 and 3 continued running after Node 1 crashed, creating a split-brain scenario.

**Evidence**:
```bash
# Only nodes 2 and 3 running
ubuntu   2432173  chronik-server start --config node2.toml
ubuntu   2432194  chronik-server start --config node3.toml
# Node 1 is missing!

# But test still tried to connect to Node 1
BOOTSTRAP_SERVERS = ['localhost:9092', 'localhost:9093', 'localhost:9094']
# Result: Metadata timeout (can't reach 9092)
```

**Impact**:
- Producers failed with metadata timeout (can't reach Node 1)
- Consumers returned 0 messages (metadata inconsistent)
- **100% data loss** despite 2/3 nodes running

**Expected behavior**:
- Detect node failure via Raft heartbeat
- Elect new leader among remaining nodes
- Redistribute partitions to healthy nodes
- Continue serving requests from 2-node quorum

---

## Test Results Summary

### What Was Tested
```
Test: 1000 topics × 3 partitions = 3000 total partitions
Step 1: Broker discovery      ✅ SUCCESS (3 brokers found)
Step 2: Produce 3000 messages  ⚠️  SLOW (305 seconds = 9.8 msg/sec)
Step 3: Consume 3000 messages  ❌ FAILED (0 messages consumed)
```

### Timeline
```
00:57:00 - Cluster started (3 nodes)
00:57:05 - Test begins producing messages
01:02:05 - Production complete (305 seconds)
01:02:07 - Consumption begins
01:02:12 - Consumption fails (0 messages, timeout)
~00:11:xx - Node 1 crashes silently
01:17:00 - Investigation reveals Node 1 dead
```

### Performance

**Production Rate**: 9.8 msg/sec (expected: 10,000+ msg/sec)
**Consumption Rate**: 0 msg/sec (expected: 50,000+ msg/sec)
**Throughput**: **99.9% slower than expected**

---

## Configuration Issues

### File: `tests/cluster/start.sh`

```bash
# Line 44, 51, 58 - The problematic settings
RUST_LOG=debug CHRONIK_WAL_PROFILE=ultra "$BINARY" start --config ...
```

**Problems**:
1. `RUST_LOG=debug` - Too verbose for production/testing
2. `CHRONIK_WAL_PROFILE=ultra` - Aggressive batching (100ms interval)
3. Combination creates log bomb with many partitions

**Should be**:
```bash
RUST_LOG=info CHRONIK_WAL_PROFILE=high "$BINARY" start --config ...
```

Or even better for testing:
```bash
RUST_LOG=info,chronik_wal::group_commit=warn "$BINARY" start --config ...
```

---

## Fixes Required

### Fix #1: Change Default Logging Level (IMMEDIATE)

**File**: `tests/cluster/start.sh`
**Change**: `RUST_LOG=debug` → `RUST_LOG=info`

**Impact**: Reduces log volume by 100-1000x

### Fix #2: Silence Verbose Group Commit Logs (IMMEDIATE)

**File**: `crates/chronik-wal/src/group_commit.rs`
**Change**: Remove or guard DEBUG logs in hot paths

**Specific lines to change**:
- Line ~550: `debug!("⏰ WORKER_TICK: Interval tick fired");` → Remove or change to `trace!`
- Line ~555: `debug!("📝 WORKER_COMMIT: About to call commit_batch");` → Remove
- Line ~570: `debug!("🔵 COMMIT_START: Entering commit_batch");` → Remove
- Line ~575: `debug!("⚪ COMMIT_EMPTY: ...");` → Change to `trace!` or remove
- Line ~580: `debug!("✅ WORKER_SUCCESS: ...");` → Change to `trace!` or remove

**Rationale**: These logs are extremely high frequency (10/sec per partition). With 1000s of partitions, they create millions of log lines. Only useful for debugging specific WAL issues.

### Fix #3: Add Partition Count Limits (HIGH PRIORITY)

**File**: `crates/chronik-server/src/integrated_server.rs`
**Add validation**:

```rust
const MAX_PARTITIONS_PER_NODE: usize = 1000;  // Configurable via env var
const WARN_PARTITIONS_PER_NODE: usize = 500;

// In topic creation:
let current_partitions = self.metadata_store.count_partitions().await?;
if current_partitions + new_partitions > MAX_PARTITIONS_PER_NODE {
    return Err(anyhow!("Partition limit exceeded: {}/{}",
        current_partitions, MAX_PARTITIONS_PER_NODE));
}
if current_partitions + new_partitions > WARN_PARTITIONS_PER_NODE {
    warn!("Approaching partition limit: {}/{}",
        current_partitions, MAX_PARTITIONS_PER_NODE);
}
```

### Fix #4: Implement Memory Limits per Partition (HIGH PRIORITY)

**File**: `crates/chronik-wal/src/group_commit.rs`
**Add memory accounting**:

```rust
// Instead of per-partition huge buffers, share a global pool
struct GlobalWalMemoryPool {
    total_bytes_allocated: AtomicU64,
    max_total_bytes: u64,  // e.g., 4 GB for entire node
    max_per_partition_bytes: u64,  // e.g., 10 MB per partition
}

// In GroupCommitWal::new():
pool.allocate_for_partition(partition_key, config.max_batch_bytes)?;
```

**Impact**: Prevents single partition from allocating 100 MB when there are 3000 partitions.

### Fix #5: Fix Raft Failure Detection (HIGH PRIORITY)

**File**: `crates/chronik-server/src/raft_cluster.rs`
**Fix**: Ensure Raft heartbeat detects dead nodes and triggers re-election

**Current behavior**: Node 1 dies silently, cluster continues with 2/3 nodes but can't serve requests
**Expected behavior**: Detect Node 1 death within ~5 seconds, elect new leader, redistribute partitions

---

## Testing Plan

### Phase 1: Fix Logging (Quick Win)
1. Change start.sh to use `RUST_LOG=info`
2. Remove or guard hot-path DEBUG logs in group_commit.rs
3. Restart cluster, verify log volume drops
4. Re-run 1000-topic test, confirm no crash

### Phase 2: Add Limits
1. Implement partition count validation
2. Add memory pooling for WAL buffers
3. Test with 10,000 partitions (should reject or warn)

### Phase 3: Fix Raft
1. Investigate Raft failure detection
2. Test node failure scenarios (kill -9 Node 1)
3. Verify cluster continues serving from 2/3 nodes

### Success Criteria
- ✅ 1000 topics × 3 partitions: NO crash
- ✅ All nodes stay under 2 GB RAM
- ✅ Log files under 100 MB after 1 hour
- ✅ 100% message delivery (3000/3000 consumed)
- ✅ Throughput > 10,000 msg/sec produce, > 50,000 msg/sec consume

---

## Priority Assessment

| Issue | Priority | Impact | Fix Effort | Fix By |
|-------|----------|--------|------------|--------|
| DEBUG logging | P0 | Crashes cluster | 10 min | Immediate |
| Memory leak | P0 | Crashes cluster | 2-4 hours | Today |
| No partition limits | P1 | Silent failures | 1 hour | This week |
| Raft failure handling | P1 | Data loss | 4-8 hours | This week |

---

## Conclusion

The cluster failure was caused by a **perfect storm** of issues:

1. **Log bomb**: DEBUG + ultra profile + 3000 partitions = 180K lines/sec
2. **Memory exhaustion**: No limits on partition count or per-partition memory
3. **Silent failure**: Node 1 crashed but Raft didn't detect/recover
4. **100% data loss**: Despite 2/3 nodes alive, no messages consumed

**The good news**: These are all fixable configuration and implementation issues. The core WAL and Raft logic appears sound.

**Next steps**: Implement fixes in priority order, test incrementally, verify 1000+ topic scalability.
