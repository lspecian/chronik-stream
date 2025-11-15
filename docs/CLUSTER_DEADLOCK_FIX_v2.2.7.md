# Cluster Deadlock Fix - v2.2.7

## Date
2025-11-15

## Summary
Fixed critical cluster deadlocks by:
1. Implementing channel-based election triggers (prevents raft_node lock contention)
2. Adding missing heartbeat sending functionality
3. **CRITICAL: Fixing DashMap iterator deadlock (remove while iterating)**

## Issues Fixed

### 1. ✅ WAL Timeout Monitor Deadlock (FIXED)

**Symptoms:**
- Cluster would freeze after 20-90 seconds
- No logs after "WAL stream timeout detected"
- All Kafka protocol handling stopped (ApiVersions, Produce, Fetch all hung)

**Root Cause:**
The WAL timeout monitor directly called `leader_elector.trigger_election_on_timeout()`, which tried to lock `raft_node`:
```
monitor_timeouts()
  → trigger_election_on_timeout()
    → am_i_leader() → lock(raft_node)
    → propose_via_raft() → lock(raft_node) AGAIN
```

Meanwhile, Raft message loop continuously holds `raft_node` lock → **DEADLOCK**

**Fix:**
Implemented channel-based election triggers (Option A):
- Timeout monitor sends election requests to an `mpsc::unbounded_channel` (never blocks)
- Separate election worker task receives from channel and can safely lock `raft_node`
- No more blocking in the monitor loop

**Files Changed:**
- `crates/chronik-server/src/wal_replication.rs`
  - Added `ElectionTriggerMessage` struct
  - Added `run_election_worker()` async task
  - Modified `monitor_timeouts()` to send to channel instead of calling directly
  - Added `election_tx` channel to spawn election worker in `handle_connection()`
  - **CRITICAL FIX:** Changed `monitor_timeouts()` to collect keys before removing (prevents DashMap iterator deadlock)

### 2. ✅ Missing Heartbeat Sending (FIXED)

**Symptoms:**
- WAL timeout errors every 32 seconds
- Followers never received heartbeats from leader
- Timeout-based elections would trigger unnecessarily

**Root Cause:**
The WAL replication sender (`run_sender_worker`) only sent data records, never heartbeats.
Followers expect heartbeats every 10 seconds to know the leader is alive.

**Fix:**
- Added heartbeat tracking to `run_sender_worker()`
- Send heartbeat frame every 10 seconds when queue is empty
- Added `send_heartbeat_to_followers()` function

**Code:**
```rust
async fn run_sender_worker(&self) {
    let mut last_heartbeat = std::time::Instant::now();

    while !self.shutdown.load(Ordering::Relaxed) {
        if let Some(record) = self.queue.pop() {
            self.send_to_followers(&record).await;
            last_heartbeat = std::time::Instant::now();
        } else {
            // Queue empty - send heartbeat if needed
            if last_heartbeat.elapsed() >= HEARTBEAT_INTERVAL {
                self.send_heartbeat_to_followers().await;
                last_heartbeat = std::time::Instant::now();
            }
            sleep(Duration::from_millis(100)).await;
        }
    }
}
```

### 3. ✅ DashMap Iterator Deadlock (FIXED)

**Symptoms:**
- Node completely freezes after "WAL stream timeout detected" log
- Process shows 0:00 CPU time (completely hung)
- No panic or error logs - just silent freeze
- Raft message loop stops processing

**Root Cause:**
The `monitor_timeouts()` function was calling `last_heartbeat.remove(entry.key())` WHILE actively iterating over the DashMap:
```rust
for entry in last_heartbeat.iter() {  // Holds read lock on DashMap shard
    if timeout_detected {
        last_heartbeat.remove(entry.key());  // Tries to acquire write lock → DEADLOCK!
    }
}
```

DashMap uses internal locking per shard. When `iter()` is active, it holds a read lock. Calling `remove()` during iteration tries to acquire a write lock on the same shard, causing a deadlock.

**Fix:**
Collect keys to remove FIRST, then remove them AFTER iteration completes:
```rust
let mut to_remove = Vec::new();

for entry in last_heartbeat.iter() {
    if timeout_detected {
        to_remove.push((topic.clone(), *partition));  // Collect, don't remove yet
    }
}

// Remove AFTER iteration completes (no lock contention)
for key in to_remove {
    last_heartbeat.remove(&key);
}
```

**Why This Works:**
- Iteration completes and releases the read lock
- Then `remove()` calls can safely acquire write locks
- No lock contention between `iter()` and `remove()`

## Test Results

### Before Fix
- ❌ Cluster freezes after 20-90 seconds
- ❌ chronik-bench cannot connect (no ApiVersions response)
- ❌ Python clients hang waiting for metadata

### After All Fixes
- ✅ Cluster stable indefinitely (no deadlock)
- ✅ chronik-bench can connect (ApiVersions works)
- ✅ Python kafka-python clients work perfectly
- ✅ Topic creation succeeds on all nodes
- ✅ Messages produce and replicate correctly
- ✅ Heartbeats sent every ~10 seconds
- ✅ Election worker processes triggers without blocking
- ✅ WAL timeout detection works without causing freeze
- ✅ All nodes show healthy CPU usage (no 0:00 hangs)

## All Issues Resolved! ✅

All three deadlock issues have been completely fixed:
1. ✅ Channel-based election triggers (no more raft_node lock contention)
2. ✅ Heartbeat sending (followers stay connected)
3. ✅ DashMap iterator deadlock (collect-then-remove pattern)

The cluster is now fully functional and stable.

## Files Modified

1. `crates/chronik-server/src/wal_replication.rs`
   - Added `tokio::sync::mpsc` import
   - Added `ElectionTriggerMessage` struct (line 111-122)
   - Added `election_tx` field to `WalReplicationManager` (line 178-180)
   - Modified `run_sender_worker()` to send heartbeats (line 444-474)
   - Added `send_heartbeat_to_followers()` function (line 552-579)
   - Added `run_election_worker()` task (line 1874-1915)
   - Modified `monitor_timeouts()` to use channel (line 1917-1942)
   - Modified `handle_connection()` to spawn election worker (line 1532-1557)

## Performance Impact

- **No impact on write path** - heartbeats only sent when queue is empty
- **Election triggers are async** - monitor never blocks
- **Channel overhead** - negligible (unbounded channel with minimal messages)

## Lessons Learned

1. **Never call blocking operations from monitoring loops** - Use channels for async communication
2. **Heartbeats are critical** - Without them, followers can't distinguish between leader failure and network issues
3. **Lock contention is subtle** - The same lock being acquired multiple times in a call chain can cause deadlocks even with `await`
4. **Test with realistic workloads** - Single-threaded tests won't expose concurrency bugs
5. **CRITICAL: Never modify a collection while iterating over it** - Even with DashMap's interior mutability, calling `remove()` during `iter()` causes shard lock deadlocks. Always collect keys first, then remove after iteration.

## Deployment Notes

This fix should be deployed to all cluster nodes simultaneously to ensure heartbeat protocol compatibility.

## Related Documents

- [docs/RAFT_MESSAGE_LOOP_DEADLOCK.md](RAFT_MESSAGE_LOOP_DEADLOCK.md) - Original deadlock investigation
- [docs/ACKS_ALL_DEADLOCK_FIX.md](ACKS_ALL_DEADLOCK_FIX.md) - Previous ArcSwap fix
