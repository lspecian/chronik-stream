# Session 1: WAL Group Commit Deep Dive

**File**: `crates/chronik-wal/src/group_commit.rs` (1,182 lines)
**Status**: ✅ **COMPLETE**
**Started**: 2025-11-18
**Completed**: 2025-11-18

---

## Executive Summary

**🚨 CRITICAL FINDING**: The WAL group commit implementation has **NO TIMEOUTS** on file I/O operations, which can cause indefinite blocking and system-wide deadlock if disk I/O stalls (slow NFS, disk failure, full disk, etc.). This is the **most likely root cause** of the Node 1 deadlock that lasted 3+ hours.

Additionally, the dual-worker architecture with 100ms intervals causes severe thrashing: 23 partitions × 10 ticks/sec = 230 timer fires/sec, most triggering empty commits.

---

## Architecture Overview

### Group Commit Design (PostgreSQL-style)

```
┌─────────────────────────────────────────────────────────────┐
│                  Group Commit WAL Flow                      │
├─────────────────────────────────────────────────────────────┤
│                                                             │
│  Producer Thread 1 ──┐                                      │
│  Producer Thread 2 ──┼─→ enqueue_and_wait()                 │
│  Producer Thread 3 ──┘      ↓                               │
│                        Create oneshot channel               │
│                        Enqueue PendingWrite                 │
│                        Notify worker: write_notify.notify() │
│                        Wait on rx.await ← BLOCKS HERE       │
│                             ↓                               │
│  Background Worker:    tokio::select! {                     │
│    - write_notify.notified() ← Producer signals            │
│    - interval.tick() ← Timer (100ms for ultra profile)     │
│    }                                                        │
│         ↓                                                   │
│    commit_batch():                                          │
│      1. Drain queue (up to max_batch_size)                 │
│      2. Write all records to file                          │
│      3. ⭐ Single fsync for entire batch                    │
│      4. Notify all waiters via oneshot channels            │
│                                                             │
│  Producer threads unblock and return success               │
└─────────────────────────────────────────────────────────────┘
```

**Key Insight**: All producers for a partition share a single worker thread. The worker batches writes and performs a single fsync, amortizing disk I/O cost across multiple requests.

---

## Profile Configurations

### Ultra Profile (Current - CHRONIK_WAL_PROFILE=ultra)

```rust
// Lines 264-275
fn ultra_resource() -> Self {
    Self {
        max_batch_size: 20_000,         // 20K writes per batch
        max_batch_bytes: 100_000_000,   // 100MB per batch
        max_wait_time_ms: 100,          // ⚠️ 100ms interval
        max_queue_depth: 100_000,       // 100K queue depth
        enable_metrics: true,
        enable_rotation: true,
        rotation_size_bytes: 512 * 1024 * 1024,  // 512MB
        rotation_age_secs: 30 * 60,     // 30 minutes
    }
}
```

**Problem**: `max_wait_time_ms: 100` → Worker wakes up every 100ms
**Impact**: 23 partitions × 10 ticks/sec = **230 timer fires/second**
**Result**: Most trigger empty commits (queue empty) → Log spam + CPU waste

---

## Data Structures

### PartitionCommitQueue (Lines 306-340)

```rust
struct PartitionCommitQueue {
    pending: Mutex<VecDeque<PendingWrite>>,       // ⚠️ Mutex 1
    total_queued_bytes: Mutex<usize>,             // ⚠️ Mutex 2
    file: Arc<Mutex<WalWriter>>,                  // ⚠️ Mutex 3 (CRITICAL)
    last_fsync: Mutex<Instant>,                   // ⚠️ Mutex 4
    write_notify: Arc<Notify>,
    metrics: Arc<CommitMetrics>,
    segment_id: Arc<AtomicU64>,
    segment_created_at: Arc<Mutex<Instant>>,      // ⚠️ Mutex 5
    segment_size_bytes: Arc<AtomicU64>,
    topic: String,
    partition: i32,
}
```

**Lock Count**: 5 Mutexes per partition
**Critical Lock**: `file: Arc<Mutex<WalWriter>>` - held during write + fsync
**Deadlock Risk**: If file I/O blocks indefinitely, this lock is held forever

---

## Critical Code Paths

### 1. Enqueue and Wait (Lines 564-632)

```rust
async fn enqueue_and_wait(...) -> Result<()> {
    // Skip backpressure for Raft topics (⚠️ Can OOM)
    let is_raft_topic = queue.topic.starts_with("__raft")
                     || queue.topic.starts_with("__chronik_metadata");

    if !is_raft_topic {
        let pending = queue.pending.lock().await;
        if pending.len() >= self.config.max_queue_depth {
            return Err(WalError::Backpressure(...));
        }
    }

    let (tx, rx) = oneshot::channel();

    // Enqueue write
    {
        let mut pending = queue.pending.lock().await;
        pending.push_back(PendingWrite { data, response_tx: Some(tx) });
    }

    // Notify worker
    queue.write_notify.notify_one();

    // ⭐ WAIT FOR FSYNC CONFIRMATION
    let result = rx.await  // ← Producer blocks until fsync completes
        .map_err(|_| WalError::CommitFailed("Response channel closed".into()))?;
    result
}
```

**Key**: Producer blocks on `rx.await` until fsync completes. If fsync never completes → Producer blocked forever.

### 2. Worker Loop (Lines 720-748)

```rust
loop {
    tokio::select! {
        _ = queue.write_notify.notified() => {},
        _ = interval.tick() => {},  // ← Fires every 100ms
        _ = shutdown.notified() => break,
    }

    if let Err(e) = Self::commit_batch(...).await {
        error!("Commit batch failed: {}", e);
    }
}
```

**Problem**: `commit_batch()` called on EVERY timer tick, even if queue empty.

### 3. Commit Batch (Lines 803-918) - **CRITICAL**

```rust
async fn commit_batch(...) -> Result<()> {
    // Drain queue
    let batch = {
        let mut pending = queue.pending.lock().await;
        if pending.is_empty() {
            return Ok(());  // Empty commit - wasted work
        }
        // ... drain up to max_batch_size
    };

    // Write to file
    let mut file = queue.file.lock().await;  // ⚠️ CRITICAL LOCK
    for write in &batch {
        file.write_all(&write.data).await?;  // ⚠️ NO TIMEOUT
    }

    // ⭐⭐⭐ SINGLE FSYNC FOR ENTIRE BATCH ⭐⭐⭐
    file.sync_all().await?;  // ⚠️⚠️⚠️ NO TIMEOUT - CAN BLOCK FOREVER
    drop(file);

    // Notify all waiters
    for write in batch {
        if let Some(tx) = write.response_tx {
            let _ = tx.send(Ok(()));  // Unblock producer
        }
    }

    Ok(())
}
```

**🚨 CRITICAL ISSUE**: Line 852 `file.sync_all().await?` - **NO TIMEOUT**

### 4. WalWriter Implementation (Lines 82-107)

```rust
async fn sync_all(&self) -> Result<()> {
    #[cfg(all(target_os = "linux", feature = "async-io"))]
    if let Some(handle) = &self.io_uring_handle {
        return handle.sync_all(self.partition_key.clone()).await;
    }

    // Fallback to standard I/O
    self.file.sync_all().await?;  // ⚠️⚠️⚠️ NO TIMEOUT
    Ok(())
}
```

---

## Lock Hierarchy Analysis

### Locks Acquired During Commit

1. **queue.pending** (line 816) - Drain writes from queue
2. **queue.file** (line 844) - Write data + fsync ← **HELD LONGEST**
3. **queue.last_fsync** (line 858) - Update timestamp
4. **queue.total_queued_bytes** (line 864) - Update counter
5. **queue.segment_created_at** (line 998) - Check rotation

**Lock Ordering**: Always `pending → file → last_fsync → total_queued_bytes → segment_created_at`

---

## Critical Issues Found

### 1. 🚨🚨🚨 NO TIMEOUTS ON FILE I/O (CRITICAL)

**Location**: Lines 82-107, 846, 852, 1024
**Severity**: **CRITICAL**
**Impact**: System-wide deadlock if disk I/O stalls

**Root Cause of Node 1 Deadlock**:
```
20:04 - Node 1 receives produce request
20:04 - WAL worker calls file.sync_all().await
20:04 - Disk I/O stalls (NFS timeout? Hardware issue?)
20:04 - file.sync_all() blocks indefinitely
20:04 - file lock held forever
20:04 - All producers waiting on this partition blocked
20:04 - Metadata partition blocked → Raft frozen
20:04-23:45 - Node 1 completely frozen for 3 hours
23:45 - Disk I/O unstalls (network recovered?)
23:45 - file.sync_all() completes
23:45 - System recovers
```

**Fix Required**:
```rust
async fn sync_all(&self) -> Result<()> {
    tokio::time::timeout(
        Duration::from_secs(30),
        self.file.sync_all()
    ).await
    .map_err(|_| WalError::FsyncTimeout)?
}
```

### 2. ⚠️ Worker Loop Thrashing

**Location**: Lines 714-748
**Severity**: HIGH
**Impact**: 230 empty commits/sec, log spam, CPU waste

**Problem**: Interval ticks every 100ms regardless of queue state

**Fix**: Only commit if queue non-empty

### 3. ⚠️ Dual Worker Redundancy

**Location**: Lines 696 (per-partition), 756 (global)
**Severity**: MEDIUM
**Impact**: Wasted CPU, double timer fires

**Fix**: Remove global background worker

### 4. ⚠️ No Backpressure for Raft Topics

**Location**: Lines 573-591
**Severity**: MEDIUM
**Impact**: Raft topics can grow unbounded, OOM risk

---

## Recommendations

### Immediate (v2.2.9)

1. ✅ Add fsync timeout (30 seconds)
2. ✅ Fix worker loop to skip commit if queue empty
3. ✅ Remove global background committer

### Short-term (v2.3.0)

4. Add backpressure for Raft topics (high limit)
5. Audit all file I/O operations for missing timeouts
6. Document lock hierarchy across all components

---

## Conclusion

**Root Cause Hypothesis**: Node 1 deadlock caused by `file.sync_all().await` blocking indefinitely due to disk I/O stall.

**Fix Priority**: Add timeouts to all file I/O operations immediately (v2.2.9)

**Performance Issue**: Worker loop thrashing causes log spam, but the 5ms latency per request likely comes from Raft consensus or metadata operations (to be investigated in Session 2-4).
