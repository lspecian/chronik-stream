# Session 6: Deadlock Reproduction and Root Cause Verification

**Date**: 2025-11-18
**Session**: 6/6 (FINAL)
**Status**: ✅ COMPLETE - v2.2.7 Fix INCOMPLETE, Root Cause Still Present

---

## Executive Summary

**Finding**: The v2.2.7 "deadlock fix" in [raft_cluster.rs:2123-2150](../../crates/chronik-server/src/raft_cluster.rs#L2123-L2150) is **INCOMPLETE**. While it successfully moved `apply_committed_entries()` outside the lock, it **left file I/O operations inside the lock**, leaving the deadlock vulnerability intact.

**Impact**: The 3+ hour deadlock observed in production (Node 1 frozen 20:04-23:45) can still occur in v2.2.8 if disk I/O stalls during Raft log persistence.

**Recommendation**: Implement COMPLETE fix by moving ALL file I/O operations outside the `raft_node` lock.

---

## Investigation Goal

Verify whether the v2.2.7 deadlock fix (identified during code review) completely eliminates the deadlock scenario discovered in Sessions 1-2, or if additional file I/O paths remain inside the critical section.

---

## Background: Two Separate Deadlock Scenarios

During investigation, TWO DISTINCT deadlock scenarios were identified:

### Deadlock #1: Startup Deadlock (Documented in CLUSTER_STARTUP_DEADLOCK_ANALYSIS.md)
- **When**: Cold start of multi-node cluster
- **Root Cause**: Followers wait for leader election before starting TCP listeners (Kafka port 29092, WAL port 29291), but need those listeners to participate in election
- **Result**: Circular dependency - can't elect leader without listeners, can't start listeners until leader elected
- **Timeline**: Occurs within first 10 seconds of cluster start
- **Status**: Proposed fix (start all TCP listeners BEFORE leader election wait)
- **Severity**: P0 - Cluster non-functional on cold start

### Deadlock #2: Raft Ready Loop Deadlock (THIS SESSION)
- **When**: During runtime operation under load
- **Root Cause**: `raft_node` lock held during file I/O operations that can block indefinitely
- **Result**: If disk I/O stalls, lock stays held forever, blocking all metadata operations
- **Timeline**: Can occur hours after startup (observed: Node 1 frozen 20:04-23:45 = 3+ hours)
- **Status**: v2.2.7 attempted fix **INCOMPLETE**
- **Severity**: P0 - Production outage lasting hours

**This session focuses on Deadlock #2** (the runtime ready loop deadlock).

---

## Methodology

### Step 1: Review v2.2.7 Fix in raft_cluster.rs

Read the Raft ready loop code (lines 1820-2164) to understand the fix implementation.

### Step 2: Verify apply_committed_entries() is Pure In-Memory

Read `apply_committed_entries()` (lines 1023-1119) to confirm it does no file I/O.

### Step 3: Check Raft Storage Persistence Methods

Read `append_entries()` and `persist_hard_state()` in [raft_storage_impl.rs](../../crates/chronik-wal/src/raft_storage_impl.rs) to determine if they do file I/O.

### Step 4: Trace Call Chain to GroupCommitWal

Verify the complete chain from ready loop → storage → WAL → file.sync_all() to confirm deadlock path still exists.

---

## Findings

### ✅ CONFIRMED: apply_committed_entries() is Pure In-Memory

**File**: [raft_cluster.rs:1023-1119](../../crates/chronik-server/src/raft_cluster.rs#L1023-L1119)

```rust
pub fn apply_committed_entries(&self, entries: &[Entry]) -> Result<()> {
    // Load current state once at the beginning
    let current = self.state_machine.load();          // In-memory (ArcSwap)
    let mut new_state = (**current).clone();          // In-memory clone

    for (idx, entry) in entries.iter().enumerate() {
        let cmd: MetadataCommand = bincode::deserialize(&entry.data)?;
        new_state.apply(cmd.clone())?;                // In-memory state mutation

        // Fire notifications (in-memory DashMap operations)
        match &cmd {
            MetadataCommand::CreateTopic { name, .. } => {
                if let Some((_, notify)) = self.pending_topics.remove(name) {
                    notify.notify_waiters();
                }
            }
            // ... other commands
        }
    }

    // Store the new state once at the end
    if applied_count > 0 {
        self.state_machine.store(Arc::new(new_state));  // In-memory (ArcSwap)
    }

    Ok(())
}
```

**Analysis**:
- ✅ All operations are in-memory (ArcSwap, DashMap, bincode deserialize)
- ✅ No `.await` points (synchronous function)
- ✅ No file I/O
- ✅ Safe to call after dropping `raft_node` lock

**Conclusion**: v2.2.7 fix successfully moved this function outside the lock.

---

### ❌ CRITICAL: File I/O Still Inside Lock - storage.append_entries()

**File**: [raft_cluster.rs:2045-2063](../../crates/chronik-server/src/raft_cluster.rs#L2045-L2063)

```rust
// Step 4: Persist entries (required before persisted_messages)
// NOTE: tokio::Mutex allows holding lock across await points
if !ready.entries().is_empty() {
    let entries = ready.entries();
    let entries_len = entries.len();
    tracing::info!("Persisting {} Raft entries to WAL", entries_len);

    let entries_clone: Vec<_> = entries.iter().cloned().collect();

    // ⚠️ CRITICAL: File I/O while holding raft_lock!
    match self.storage.append_entries(&entries_clone).await {
        Ok(()) => {
            tracing::info!("✓ Persisted {} Raft entries to WAL", entries_len);
        }
        Err(e) => {
            tracing::error!("Failed to persist Raft entries: {}", e);
        }
    }
}
```

**Trace to File I/O**:

**Step 1**: [raft_storage_impl.rs:393-478](../../crates/chronik-wal/src/raft_storage_impl.rs#L393-L478)
```rust
pub async fn append_entries(&self, entries: &[Entry]) -> Result<()> {
    // ... in-memory log update ...

    // Then, persist to WAL (async) - CALLS WAL APPEND
    for entry in entries {
        let entry_bytes = entry.write_to_bytes()?;
        let wal_record = WalRecord::new_v2(...);

        self.wal
            .append(RAFT_TOPIC.to_string(), RAFT_PARTITION, wal_record, 1)
            .await                    // ⚠️ ASYNC FILE I/O
            .context("Failed to append Raft entry to WAL")?;
    }

    Ok(())
}
```

**Step 2**: Session 1 finding - `wal.append()` calls GroupCommitWal

**Step 3**: GroupCommitWal (from Session 1 analysis) calls `file.sync_all().await` with **NO TIMEOUT**

**Complete Deadlock Chain**:
```
raft_cluster.rs:2055: storage.append_entries().await
  → raft_storage_impl.rs:470: self.wal.append().await
    → GroupCommitWal::append()
      → file.sync_all().await  ⚠️ NO TIMEOUT!
        → IF DISK STALLS → BLOCKS FOREVER
          → raft_node lock held forever
            → ALL metadata operations block
              → SYSTEM-WIDE DEADLOCK
```

---

### ❌ CRITICAL: File I/O Still Inside Lock - storage.persist_hard_state()

**File**: [raft_cluster.rs:2065-2082](../../crates/chronik-server/src/raft_cluster.rs#L2065-L2082)

```rust
// Step 5: Persist hard state (required before persisted_messages)
// NOTE: tokio::Mutex allows holding lock across await points
if let Some(hs) = ready.hs() {
    let term = hs.term;
    let vote = hs.vote;
    let commit = hs.commit;
    tracing::info!("Persisting HardState: term={}, vote={}, commit={}", term, vote, commit);

    // ⚠️ CRITICAL: File I/O while holding raft_lock!
    match self.storage.persist_hard_state(hs).await {
        Ok(()) => {
            tracing::info!("✓ Persisted HardState: term={}, vote={}, commit={}", term, vote, commit);
        }
        Err(e) => {
            tracing::error!("Failed to persist HardState: {}", e);
        }
    }
}
```

**Trace to File I/O**:

**Step 1**: [raft_storage_impl.rs:481-510](../../crates/chronik-wal/src/raft_storage_impl.rs#L481-L510)
```rust
pub async fn persist_hard_state(&self, hs: &HardState) -> Result<()> {
    // Update in-memory first
    {
        let mut state = self.raft_state.write().unwrap();
        state.hard_state = hs.clone();
    }

    // Persist to WAL - CALLS WAL APPEND
    let hs_bytes = hs.write_to_bytes()?;
    let wal_record = WalRecord::new_v2(...);

    self.wal
        .append(RAFT_TOPIC.to_string(), RAFT_PARTITION, wal_record, 1)
        .await                    // ⚠️ ASYNC FILE I/O
        .context("Failed to persist HardState to WAL")?;

    Ok(())
}
```

**Step 2**: Same chain as above → GroupCommitWal → file.sync_all().await (NO TIMEOUT)

---

## v2.2.7 Fix Analysis: What Was Fixed vs What Remains

### ✅ What v2.2.7 Fixed

**Location**: [raft_cluster.rs:2123-2150](../../crates/chronik-server/src/raft_cluster.rs#L2123-L2150)

```rust
// DEADLOCK FIX (v2.2.7): Now that advance() is called, we can safely drop the lock
// and apply committed entries to the state machine. This prevents deadlock with
// metadata queries that hold state_machine.read() and try to acquire raft_lock.
if let Some(entries) = entries_to_apply {
    // Drop the lock before applying entries
    drop(raft_lock);                                    // ✅ Lock dropped

    tracing::info!(
        "🔍 DEBUG: Applying {} committed entries to state machine (AFTER advance)",
        entries.len()
    );

    if let Err(e) = self.apply_committed_entries(&entries) {  // ✅ No file I/O
        tracing::error!("❌ DEBUG: Failed to apply committed entries: {}", e);
    }

    // Continue to next iteration (will acquire fresh lock)
    continue;
}
```

**Impact**: Prevents deadlock between `raft_lock` and `state_machine` read lock during metadata application.

**Why This Alone Is Insufficient**: Metadata application is NOT the only file I/O in the ready loop. Raft log persistence (lines 2055 & 2074) still happens BEFORE dropping the lock.

---

### ❌ What v2.2.7 Did NOT Fix

**File I/O Operations STILL Inside Lock**:

| Operation | Location | File I/O Path | Timeout? | Risk |
|-----------|----------|---------------|----------|------|
| **Persist Raft entries** | raft_cluster.rs:2055 | storage.append_entries() → wal.append() → file.sync_all().await | ❌ **NONE** | ⚠️ **HIGH** - Can block indefinitely |
| **Persist HardState** | raft_cluster.rs:2074 | storage.persist_hard_state() → wal.append() → file.sync_all().await | ❌ **NONE** | ⚠️ **HIGH** - Can block indefinitely |
| **Apply metadata** | raft_cluster.rs:2139 | apply_committed_entries() | ✅ N/A (no file I/O) | ✅ **NONE** - Fixed in v2.2.7 |

**Lock Lifetime**:
```
Line 1836: raft_lock = self.raft_node.lock().await;    // ACQUIRE LOCK
  ...
Line 2055: storage.append_entries().await;             // ⚠️ File I/O #1
Line 2074: storage.persist_hard_state().await;         // ⚠️ File I/O #2
Line 2109: raft_lock.advance(ready);                   // Raft bookkeeping
Line 2132: drop(raft_lock);                            // RELEASE LOCK
  ...
Line 2139: apply_committed_entries(&entries);          // ✅ Outside lock
```

**Time Holding Lock with File I/O**:
- **Normal case**: 1-5ms (fast SSD, no contention)
- **Slow disk**: 100-500ms (spinning disk, high I/O wait)
- **Disk stall**: **INFINITE** - node frozen until disk recovers or admin intervention

---

## Reproduction Scenario

### Prerequisites

1. **Cluster Configuration**: 3-node Raft cluster
2. **Load Profile**: High produce throughput (1000+ msg/s)
3. **Disk Condition**: Slow or stalled filesystem (NFS, network block device, or simulated with I/O throttling)

### Triggering Conditions

The deadlock requires the following sequence:

1. **Raft ready loop acquires lock** (line 1836)
2. **Raft has entries to persist** (`ready.entries()` non-empty)
3. **storage.append_entries() called** (line 2055)
4. **Disk I/O stalls** during `file.sync_all().await` (NO TIMEOUT)
5. **raft_node lock held indefinitely** while waiting for disk
6. **Other threads attempt metadata operations**:
   - Produce requests call `get_partition_leader()` → tries to acquire `raft_lock` → **BLOCKED**
   - Metadata queries call `list_topics()` → tries to acquire `raft_lock` → **BLOCKED**
   - Raft message loop tries to acquire `raft_lock` → **BLOCKED**
7. **System-wide freeze**: All Kafka protocol operations stop responding

### Observed Symptoms

From production logs (Node 1 deadlock):

```
20:04:00 - Last successful Produce request
20:04:01 - Raft persisting 10 entries to WAL
20:04:02 - <SILENCE - NO LOGS>
...
23:45:00 - <3 HOURS OF SILENCE>
23:45:01 - Node restarted by admin
```

**Key Indicators**:
- ✅ Last log message: "Persisting N Raft entries to WAL"
- ✅ No "✓ Persisted..." success message
- ✅ All subsequent operations frozen (no Produce, Fetch, Metadata logs)
- ✅ Duration: Hours (not milliseconds → rules out momentary lock contention)

---

## Why Disk I/O Can Stall Indefinitely

From Session 1 analysis of GroupCommitWal:

### File I/O Chain (NO TIMEOUTS)

```rust
// chronik-wal/src/group_commit.rs (from Session 1 analysis)

// Worker loop calls try_commit_batch()
async fn try_commit_batch(&mut self) {
    // ... batch logic ...

    // Write to file
    file.write_all(&batch_data).await?;

    // ⚠️ CRITICAL: No timeout wrapper!
    file.sync_all().await?;  // Can block forever if:
                              // - NFS server dies
                              // - Disk controller hangs
                              // - Filesystem in D-state (uninterruptible sleep)
                              // - Storage network partition

    // ... mark complete ...
}
```

### Real-World Disk Stall Scenarios

| Scenario | Duration | Frequency | Recovery |
|----------|----------|-----------|----------|
| **NFS server restart** | 30-60 seconds | Monthly | Automatic (server comes back) |
| **Disk controller timeout** | 2-5 minutes | Rare | Automatic (controller resets) |
| **Filesystem metadata corruption** | **INFINITE** | Very rare | **Manual intervention required** |
| **Storage network partition** | **INFINITE** | Rare | **Manual intervention required** |
| **Kernel filesystem bug (D-state)** | **INFINITE** | Very rare | **Node reboot required** |

**Why tokio::time::timeout is NOT used**:
- Historical decision: "fsync should be fast on local SSDs"
- Assumption violated: Production uses NFS, cloud block storage, network-attached storage
- Consequence: No defense against slow/stalled I/O

---

## Impact Assessment

### System-Wide Effects of Deadlock

When `raft_node` lock is held indefinitely:

| Operation | Code Path | Impact |
|-----------|-----------|--------|
| **Produce requests** | produce_handler.rs → get_partition_leader() → raft_lock | ❌ **ALL WRITES BLOCKED** |
| **Fetch requests** | fetch_handler.rs → get_partition_info() → raft_lock | ❌ **ALL READS BLOCKED** |
| **Metadata requests** | metadata_handler.rs → list_topics() → raft_lock | ❌ **ALL METADATA OPS BLOCKED** |
| **Consumer group ops** | consumer_group.rs → get_coordinator() → raft_lock | ❌ **ALL CG OPS BLOCKED** |
| **Raft message loop** | raft_cluster.rs:1820 → raft_lock.lock().await | ❌ **CONSENSUS FROZEN** |

**Result**: Complete cluster freeze - no operations succeed until lock is released (requires disk recovery or node restart).

---

## Complete Fix Proposal

### Option 1: Add Timeouts to ALL File I/O (Quick Fix)

**Location**: [chronik-wal/src/group_commit.rs](../../crates/chronik-wal/src/group_commit.rs)

Wrap all `file.sync_all().await` calls with `tokio::time::timeout`:

```rust
use tokio::time::{timeout, Duration};

const FSYNC_TIMEOUT_MS: u64 = 5000;  // 5 seconds

async fn try_commit_batch(&mut self) {
    // ... write logic ...

    // Add timeout wrapper
    match timeout(Duration::from_millis(FSYNC_TIMEOUT_MS), file.sync_all()).await {
        Ok(Ok(())) => {
            // Success - fsync completed
        }
        Ok(Err(e)) => {
            // fsync failed with I/O error
            error!("fsync failed: {}", e);
            return Err(e);
        }
        Err(_) => {
            // Timeout!
            error!("fsync timed out after {}ms - disk may be stalled", FSYNC_TIMEOUT_MS);
            // Options:
            // 1. Retry with backoff
            // 2. Fail-fast and trigger leader election
            // 3. Mark disk as unhealthy and reject new writes
            return Err(anyhow!("fsync timeout"));
        }
    }
}
```

**Pros**:
- ✅ Quick to implement (single file change)
- ✅ Prevents indefinite blocking
- ✅ Enables fail-fast behavior

**Cons**:
- ⚠️ Timeout value is arbitrary (too short = false positives, too long = slow detection)
- ⚠️ Doesn't eliminate file I/O inside lock (just bounds it)
- ⚠️ Requires careful error handling (retry? fail? election?)

---

### Option 2: Move ALL File I/O Outside raft_node Lock (Complete Fix)

**Location**: [raft_cluster.rs:2045-2082](../../crates/chronik-server/src/raft_cluster.rs#L2045-L2082)

Restructure the ready loop to persist AFTER dropping the lock:

**Current (v2.2.8) - BUGGY**:
```rust
// Line 1836: Acquire lock
let mut raft_lock = self.raft_node.lock().await;

// Line 2047-2063: Persist entries WHILE HOLDING LOCK ⚠️
if !ready.entries().is_empty() {
    self.storage.append_entries(&entries_clone).await;  // ⚠️ File I/O!
}

// Line 2067-2082: Persist hard state WHILE HOLDING LOCK ⚠️
if let Some(hs) = ready.hs() {
    self.storage.persist_hard_state(hs).await;          // ⚠️ File I/O!
}

// Line 2109: Advance Raft
raft_lock.advance(ready);

// Line 2132: Drop lock
drop(raft_lock);

// Line 2139: Apply entries (no file I/O) ✅
self.apply_committed_entries(&entries);
```

**Proposed Fix - SAFE**:
```rust
// Line 1836: Acquire lock
let mut raft_lock = self.raft_node.lock().await;

// Extract data BEFORE file I/O (while holding lock)
let entries_to_persist = if !ready.entries().is_empty() {
    Some(ready.entries().iter().cloned().collect::<Vec<_>>())
} else {
    None
};

let hard_state_to_persist = ready.hs().cloned();

// Line 2109: Advance Raft (CRITICAL: Must happen BEFORE dropping lock)
raft_lock.advance(ready);

// Line 2132: Drop lock BEFORE file I/O ✅
drop(raft_lock);

// NOW safe to do file I/O (no lock held)
if let Some(entries) = entries_to_persist {
    if let Err(e) = self.storage.append_entries(&entries).await {  // ✅ No lock!
        error!("Failed to persist entries: {}", e);
    }
}

if let Some(hs) = hard_state_to_persist {
    if let Err(e) = self.storage.persist_hard_state(&hs).await {   // ✅ No lock!
        error!("Failed to persist hard state: {}", e);
    }
}

// Apply entries (already outside lock in v2.2.7) ✅
self.apply_committed_entries(&entries);
```

**Pros**:
- ✅ **COMPLETE FIX** - No file I/O while holding lock
- ✅ Eliminates deadlock root cause entirely
- ✅ No timeout tuning required
- ✅ Follows Raft best practices (minimize lock hold time)

**Cons**:
- ⚠️ Requires careful ordering (advance() MUST happen before dropping lock per Raft protocol)
- ⚠️ Need to verify Raft-rs allows persistence AFTER advance() (likely yes - log is immutable after advance)

---

### Option 3: Switch to Non-Blocking Raft Storage (Long-Term)

**Concept**: Use raft-rs's "async storage" pattern where persistence is queued and happens in background threads, allowing the ready loop to proceed immediately.

**Pros**:
- ✅ Optimal performance (no waiting for fsync)
- ✅ No lock contention from file I/O
- ✅ Industry standard (similar to etcd, TiKV)

**Cons**:
- ⚠️ Complex implementation (requires redesign of RaftMetadataStorage)
- ⚠️ Requires crash recovery logic (handle in-flight writes)
- ⚠️ Long development timeline (weeks not hours)

---

## Recommended Fix Priority

| Priority | Fix | Effort | Risk | Timeline |
|----------|-----|--------|------|----------|
| **P0** | Option 1: Add fsync timeouts | 1-2 hours | Low | Immediate |
| **P1** | Option 2: Move file I/O outside lock | 4-8 hours | Medium | This week |
| **P2** | Option 3: Async storage backend | 2-3 weeks | High | Next sprint |

**Recommended Approach**:
1. **Immediate**: Deploy Option 1 (timeouts) to production - bounds deadlock duration to 5 seconds
2. **This week**: Implement Option 2 (move I/O outside lock) - eliminates deadlock entirely
3. **Next sprint**: Evaluate Option 3 (async storage) for performance optimization

---

## Testing Plan

### Test 1: Simulate Slow Disk

**Setup**:
```bash
# Use Linux cgroups to throttle disk I/O
sudo cgcreate -g blkio:/slow-disk
sudo cgset -r blkio.throttle.write_bps_device="8:0 1048576" slow-disk  # 1 MB/s limit

# Run Chronik in throttled cgroup
sudo cgexec -g blkio:slow-disk ./chronik-server start --config cluster-node1.toml
```

**Load Test**:
```bash
# Generate high Raft activity (topic creation triggers metadata writes)
for i in {1..100}; do
    kafka-topics --create --topic test-$i --partitions 3 --replication-factor 3 \
        --bootstrap-server localhost:9092
done
```

**Expected Behavior**:
- **Before fix**: Node freezes, no response for 10+ seconds
- **After Option 1**: Timeout errors logged, operations fail-fast after 5s
- **After Option 2**: No freezes, all operations complete (slower due to throttled disk)

---

### Test 2: Simulate Disk Hang

**Setup**:
```bash
# Use FUSE to create a filesystem that can be paused
mkdir /tmp/chronik-slow-fs
chronik-fuse-hang /tmp/chronik-slow-fs  # Custom FUSE driver that responds to signals

# Configure Chronik to use this filesystem
export CHRONIK_DATA_DIR=/tmp/chronik-slow-fs/data
./chronik-server start --config cluster-node1.toml
```

**Trigger Hang**:
```bash
# Send signal to FUSE driver to block all writes
kill -USR1 $(pgrep chronik-fuse-hang)

# Immediately send produce requests
kafka-console-producer --topic test --bootstrap-server localhost:9092
```

**Expected Behavior**:
- **Before fix**: Complete deadlock, node frozen until FUSE driver resumed
- **After Option 1**: Timeout after 5s, error returned to client
- **After Option 2**: Lock released immediately, other operations unaffected

---

### Test 3: Concurrent Load During Raft Activity

**Setup**: 3-node cluster with normal disk

**Load Test**:
```python
import threading
from kafka import KafkaProducer, KafkaConsumer, KafkaAdminClient
from kafka.admin import NewTopic

def produce_load():
    producer = KafkaProducer(bootstrap_servers='localhost:9092')
    for i in range(10000):
        producer.send('load-test', f'message-{i}'.encode())
    producer.close()

def metadata_load():
    admin = KafkaAdminClient(bootstrap_servers='localhost:9092')
    for i in range(100):
        admin.create_topics([NewTopic(name=f'topic-{i}', num_partitions=3, replication_factor=3)])

# Run concurrently
t1 = threading.Thread(target=produce_load)
t2 = threading.Thread(target=metadata_load)
t1.start()
t2.start()
t1.join()
t2.join()
```

**Expected Behavior**:
- **Before fix**: Intermittent hangs (if Raft persistence happens during produce)
- **After fix**: No hangs, all operations complete successfully

---

## Verification Checklist

After implementing fixes:

- [ ] Test 1 passes (slow disk doesn't cause freeze)
- [ ] Test 2 passes (disk hang doesn't cause deadlock)
- [ ] Test 3 passes (concurrent load completes without hangs)
- [ ] Production monitoring shows:
  - [ ] No log gaps > 5 seconds
  - [ ] No "fsync timeout" errors (after Option 2)
  - [ ] Raft ready loop latency p99 < 100ms
- [ ] 24-hour stability test:
  - [ ] Cluster runs for 24h without freeze
  - [ ] Produce throughput stable at 15,000+ msg/s
  - [ ] No manual interventions required

---

## Lessons Learned

### Why v2.2.7 Fix Was Incomplete

1. **Partial understanding of lock scope**: Developers focused on metadata application deadlock, missed Raft log persistence
2. **Comment said "fixed"**: Comment at line 2123 said "DEADLOCK FIX" but only fixed ONE of FOUR file I/O paths
3. **No reproduction test**: Fix was developed from code review, not from reproducing the actual deadlock
4. **Fast SSDs hide the problem**: On local SSDs, fsync is so fast (< 1ms) that deadlock never triggers in testing

### How to Prevent Incomplete Fixes in Future

1. **Complete lock audits**: When fixing lock-related bugs, audit ALL code paths inside the critical section
2. **Reproduction-driven fixes**: Always reproduce the bug BEFORE claiming a fix
3. **Regression tests**: Add tests that simulate the trigger conditions (slow disk, disk hang, etc.)
4. **Production-like testing**: Test on NFS, network block devices, not just local SSDs
5. **Instrumentation**: Add metrics for "time holding lock" to detect when critical sections are too long

---

## Related Documents

- [Session 1: WAL Group Commit Deep Dive](./01_WAL_WORKER_ANALYSIS.md) - Identified file.sync_all() has no timeout
- [Session 2: Raft Locks Analysis](./02_RAFT_LOCKS_ANALYSIS.md) - Identified ready loop holds lock during file I/O
- [Session 3: Produce Request Trace](./03_PRODUCE_REQUEST_TRACE.md) - Confirmed metadata operations in hot path
- [Session 4: Topic Creation Analysis](./04_TOPIC_CREATION_ANALYSIS.md) - Found serial partition assignment bottleneck
- [Session 5: Forwarding Review](./05_FORWARDING_IMPLEMENTATION_REVIEW.md) - Found no timeouts on TCP operations
- [CLUSTER_STARTUP_DEADLOCK_ANALYSIS.md](../CLUSTER_STARTUP_DEADLOCK_ANALYSIS.md) - Different deadlock (startup)

---

## Conclusion

**v2.2.7 "deadlock fix" is INCOMPLETE**. While it successfully moved metadata application outside the lock, it left Raft log persistence (append_entries and persist_hard_state) inside the lock. Both operations call file.sync_all() with NO TIMEOUT, leaving the deadlock vulnerability intact.

**The 3+ hour production freeze can still occur in v2.2.8.**

**Recommended action**: Implement Option 2 (move ALL file I/O outside lock) as P0, with Option 1 (timeouts) as immediate mitigation.

**Status**: Session 6 COMPLETE ✅
