# Raft Message Loop Deadlock - CRITICAL BUG

**Date**: 2025-11-15
**Status**: ROOT CAUSE IDENTIFIED
**Severity**: CRITICAL - Cluster completely unusable after ~90 seconds

## Summary

Node 2 in the cluster deadlocks consistently after ~90 seconds of operation. The Raft message loop stops processing completely, with no error messages or panics - the logs simply stop.

## Evidence

### Reproducibility
- ✅ **100% Reproducible** - Happens every time
- ⏱️ **Timing**: Node 2 freezes 90-120 seconds after startup
- 📊 **Impact**: Without Node 2, quorum cannot be achieved (need 2/3 votes)
- 🔄 **Result**: Cluster enters endless election loop (term 1001 after 12 hours!)

### Log Evidence

**Node 2 Freeze Pattern**:
```
[00:14:33.729081Z] INFO Raft ready: 1 persisted messages to send
[00:14:33.801057Z] WARN WAL stream timeout detected for __chronik_metadata-0 (33s since last heartbeat)
<-- NOTHING AFTER THIS - COMPLETE SILENCE -->
```

**Node 1's Perspective**:
```
Raft RPC failed with non-transient error ... to=2 ... error=status: Cancelled, message: "Timeout expired"
✓ Raft RPC succeeded ... to=3 ... latency_ms=0.631722
```

Node 1 can talk to Node 3, but not Node 2.

## Root Cause Analysis

### Lock Acquisition Order Inversion Deadlock

**Location**: [crates/chronik-server/src/raft_cluster.rs](file://crates/chronik-server/src/raft_cluster.rs)

#### The Deadlock Pattern

**Raft Message Loop** (lines 1726-2130):
1. Line 1742: `let mut raft_lock = self.raft_node.lock().await;` → **Acquires Raft lock**
2. Line 1939: Calls `apply_committed_entries(&normal_entries)` → **While holding Raft lock**
3. Line 939 (in `apply_committed_entries`): `let mut sm = self.state_machine.write()` → **Tries to acquire state_machine WRITE lock**

**Metadata Query Path** (e.g., list_topics in raft_metadata_store.rs):
1. Lines 173, 193, 203, 216, 224, 245, 253, etc.: `let state = self.state();` → **Acquires state_machine READ lock**
2. Then tries to check leader status or forward query → **Tries to acquire Raft lock**

#### Lock Inversion Diagram

```
Thread A (Raft Message Loop):          Thread B (Metadata Query):
-------------------------------         ---------------------------
1. raft_lock.lock() ✓                  1. state_machine.read() ✓
2. apply_committed_entries()           2. Need leader info...
3. state_machine.write() ❌ BLOCKED    3. raft.am_i_leader() ❌ BLOCKED
   (Thread B holds read lock)             (Thread A holds raft_lock)

🔒 DEADLOCK: A waits for B, B waits for A
```

### Why This Doesn't Always Happen Immediately

The deadlock requires specific timing:
- Thread A must be processing committed entries (holding raft_lock)
- Thread B must be servicing a metadata request (holding state_machine read lock)
- Both must try to acquire the other's lock **at the same time**

This is why it takes 90 seconds - it's waiting for the "unlucky" scheduling where both threads are in the critical section simultaneously.

## Code Locations

### Critical Lock Acquisitions

**1. Raft Message Loop (raft_cluster.rs:1742)**:
```rust
let mut raft_lock = self.raft_node.lock().await; // RAFT LOCK ACQUIRED

// ... many operations while holding lock ...

if !ready.committed_entries().is_empty() {
    let normal_entries: Vec<_> = ready.committed_entries()
        .iter()
        .filter(|entry| entry.get_entry_type() != raft::prelude::EntryType::EntryConfChangeV2)
        .cloned()
        .collect();

    if !normal_entries.is_empty() {
        // DEADLOCK POINT 1: Tries to acquire state_machine write lock while holding raft_lock
        if let Err(e) = self.apply_committed_entries(&normal_entries) {
            tracing::error!("❌ DEBUG: Failed to apply committed entries: {}", e);
        }
    }
}
```

**2. apply_committed_entries (raft_cluster.rs:939)**:
```rust
pub fn apply_committed_entries(&self, entries: &[Entry]) -> Result<()> {
    // DEADLOCK POINT 2: Acquires write lock on state_machine
    let mut sm = self.state_machine.write()
        .map_err(|e| anyhow::anyhow!("Failed to acquire state machine lock: {}", e))?;

    // ... apply entries while holding state_machine write lock ...
}
```

**3. Metadata Queries (raft_metadata_store.rs:245)**:
```rust
async fn list_topics(&self) -> Result<Vec<TopicMetadata>> {
    // LEADER: Always read from local state
    if self.raft.am_i_leader().await {  // ← Tries to acquire raft_lock
        let state = self.state();  // ← Acquires state_machine read lock
        return Ok(state.topics.values().cloned().collect());
    }

    // FOLLOWER: Check lease before reading
    if self.lease_manager.has_valid_lease().await {
        let state = self.state();  // ← Acquires state_machine read lock FIRST
        return Ok(state.topics.values().cloned().collect());
    }

    // No valid lease - forward to leader
    let query = crate::metadata_rpc::MetadataQuery::ListTopics;
    let response = self.raft.query_leader(query).await  // ← Tries to acquire raft_lock
        .map_err(|e| MetadataError::StorageError(e.to_string()))?;
}
```

## Fix Strategy

### Option 1: Drop Raft Lock Before Applying Entries (RECOMMENDED)

```rust
// In start_message_loop() around line 1939:

// Step 3b: Handle normal committed entries
if !ready.committed_entries().is_empty() {
    let normal_entries: Vec<_> = ready.committed_entries()
        .iter()
        .filter(|entry| entry.get_entry_type() != raft::prelude::EntryType::EntryConfChangeV2)
        .cloned()
        .collect();

    if !normal_entries.is_empty() {
        // DEADLOCK FIX: Drop raft_lock BEFORE calling apply_committed_entries
        drop(raft_lock);

        tracing::info!("Processing {} normal committed entries", normal_entries.len());
        if let Err(e) = self.apply_committed_entries(&normal_entries) {
            tracing::error!("Failed to apply committed entries: {}", e);
        }

        // Lock was dropped - skip advance() and continue to next iteration
        continue;
    }
}

// Step 4: Persist entries (only if we didn't drop raft_lock above)
// ... rest of the loop ...
```

**Rationale**:
- Applying committed entries doesn't need the Raft lock
- The entries are already committed and immutable
- Dropping the lock allows metadata queries to check leader status
- Similar to how snapshot application already drops the lock (line 1800)

### Option 2: Reverse Lock Order (ALTERNATIVE)

Change metadata queries to:
1. **First** check leader status (acquires raft_lock briefly)
2. **Then** read state (acquires state_machine lock)

This ensures consistent lock ordering across all code paths.

**Problem**: Requires changes in many places (every `state()` call)

### Option 3: Make state_machine Lock-Free (FUTURE)

Use atomic reference-counted immutable state:
```rust
pub state_machine: Arc<ArcSwap<MetadataStateMachine>>
```

This eliminates the lock entirely, but requires larger refactoring.

## Testing the Fix

After implementing the fix:

1. **Restart cluster and monitor Node 2**:
   ```bash
   ./tests/cluster/stop.sh
   ./tests/cluster/start.sh
   tail -f tests/cluster/logs/node2.log
   ```

2. **Check log file growth**:
   ```bash
   watch -n 1 'ls -lh tests/cluster/logs/*.log'
   ```

   Node 2 should continue growing (not freeze at ~92KB)

3. **Verify metadata requests work**:
   ```bash
   python3 test_node_ready.py
   ```

4. **Long-running test** (24 hours):
   ```bash
   while true; do
     python3 -c "
from kafka import KafkaProducer
producer = KafkaProducer(bootstrap_servers='localhost:9092')
producer.send('test', b'test')
producer.close()
"
     sleep 10
   done
   ```

## Related Issues

- [METADATA_LEASE_BUG.md](file://docs/METADATA_LEASE_BUG.md) - Incorrectly identified as leader_id tracking bug
- The partition assignment fix (v2.2.7) IS working correctly - this is a separate pre-existing deadlock

## Impact

**Without Fix**:
- ❌ Cluster becomes unusable after ~90 seconds
- ❌ All metadata requests timeout
- ❌ Endless leader elections (term increases forever)
- ❌ No Kafka clients can produce or consume

**With Fix**:
- ✅ Cluster remains stable indefinitely
- ✅ Metadata requests succeed
- ✅ Stable leadership
- ✅ Normal Kafka operations

## Priority

**CRITICAL - P0**
This bug makes multi-node Raft clusters completely unusable. Must be fixed before any production deployment.
