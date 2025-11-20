# Raft vs WAL Metadata Redundancy - Root Cause Analysis

**Date**: 2025-11-20
**Priority**: P0 - CRITICAL
**Status**: Architectural redundancy discovered
**Related**: [WAL_REPLICATION_FAILURE_ROOT_CAUSE.md](WAL_REPLICATION_FAILURE_ROOT_CAUSE.md), [METADATA_REPLICATION_INCOMPLETE_ANALYSIS.md](METADATA_REPLICATION_INCOMPLETE_ANALYSIS.md)

---

## Executive Summary

**Discovery**: Chronik has **TWO SEPARATE metadata replication systems** operating concurrently, causing architectural redundancy and race conditions:

1. **Raft Consensus** (legacy) - Slow (100-200ms)
2. **`__chronik_metadata` WAL Replication** (new) - Fast (microseconds)

Both update the **SAME** `MetadataStateMachine.partition_assignments` HashMap, but partition data validation checks Raft (which may not be synced yet) even though WAL metadata IS being replicated.

**Impact**: The WAL replication bug (data rejected due to missing replica info) is caused by checking the wrong metadata source (Raft) when the correct source (`__chronik_metadata` WAL) exists but arrives in a race with partition data.

---

## Architecture Discovery

### The Two Metadata Paths

**Path 1: Raft Consensus (Legacy)**
Location: `crates/chronik-server/src/raft_cluster.rs`

```rust
// Leader proposes metadata command
pub async fn propose(&self, cmd: MetadataCommand) -> Result<()> {
    let data = bincode::serialize(&cmd)?;
    raft.propose(vec![], data)?;  // Raft consensus (100-200ms)
    Ok(())
}

// State machine updated after Raft commit
fn apply(entry: Entry) {
    let cmd: MetadataCommand = bincode::deserialize(&entry.data)?;
    state_machine.apply(cmd)?;  // Updates partition_assignments HashMap
}
```

**Latency**: 100-200ms for 3-node cluster (Raft consensus)
**Purpose**: Originally designed for cluster metadata coordination

---

**Path 2: `__chronik_metadata` WAL Replication (New)**
Location: `crates/chronik-server/src/wal_replication.rs:1825-1872`

```rust
// Leader sends metadata via WAL replication
async fn replicate(&self, cmd: &MetadataCommand, offset: i64) -> Result<()> {
    let data = bincode::serialize(cmd)?;

    self.replication_mgr.replicate_partition(
        "__chronik_metadata",  // Special topic for metadata
        0,
        offset,
        offset,
        data,
    ).await;
    Ok(())
}

// Follower receives metadata WAL record
async fn handle_metadata_wal_record(
    raft_cluster: &Arc<RaftCluster>,
    record: &WalReplicationRecord,
) -> Result<()> {
    let cmd: MetadataCommand = bincode::deserialize(&record.data)?;

    // Apply directly to state machine (BYPASSES Raft!)
    raft_cluster.apply_metadata_command_direct(cmd.clone())?;
    // ↑ Updates THE SAME partition_assignments HashMap

    Ok(())
}
```

**Latency**: Microseconds (TCP + bincode deserialization)
**Purpose**: Fast metadata replication via WAL streaming
**Key Insight**: Calls `apply_metadata_command_direct()` which updates the SAME state machine as Raft

---

### The Shared State Machine

**File**: `crates/chronik-server/src/raft_metadata.rs:373-377`

```rust
pub struct MetadataStateMachine {
    /// Partition assignments: (topic, partition) → replica node IDs
    pub partition_assignments: HashMap<PartitionKey, Vec<u64>>,
    // ... other fields
}

impl MetadataStateMachine {
    /// Get partition replicas (used by BOTH paths)
    pub fn get_partition_replicas(&self, topic: &str, partition: i32) -> Option<Vec<u64>> {
        self.partition_assignments
            .get(&(topic.to_string(), partition))
            .cloned()
    }
}
```

**Critical Insight**: `get_partition_replicas()` reads from `partition_assignments` HashMap that is updated by BOTH Raft and WAL metadata replication.

---

### The apply_metadata_command_direct() Implementation

**File**: `crates/chronik-server/src/raft_cluster.rs:504-512`

```rust
pub fn apply_metadata_command_direct(&self, cmd: MetadataCommand) -> Result<()> {
    let current = self.state_machine.load();  // Lock-free atomic load
    let mut new_state = (**current).clone();  // Clone current state

    new_state.apply(cmd)?;  // Apply metadata command (updates partition_assignments!)

    self.state_machine.store(Arc::new(new_state));  // Atomic swap
    Ok(())
}
```

**What this does**:
- Loads current MetadataStateMachine
- Clones it
- Applies the MetadataCommand (e.g., `AssignPartition` → inserts into `partition_assignments`)
- Atomically swaps the new state

**Key Point**: This is the SAME state machine that Raft updates. Both paths converge to the same in-memory HashMap.

---

## The Race Condition Explained

### Timeline of Concurrent Replication

```
T+0ms:   Leader creates topic "my-topic"
         - Proposes to Raft (async, doesn't wait)
         - Sends metadata via __chronik_metadata WAL (async)
         - Sends partition data via WAL (async)

T+1ms:   ALL THREE replication streams sent concurrently:
         1. Raft AppendEntries (metadata)
         2. __chronik_metadata WAL frame (metadata)
         3. Partition data WAL frame (data)

T+2ms:   Follower receives frames in RANDOM ORDER (race condition!)

Case A: __chronik_metadata arrives FIRST
   T+2ms:  Metadata WAL frame arrives
   T+3ms:  handle_metadata_wal_record() → apply_metadata_command_direct()
   T+4ms:  partition_assignments updated: {"my-topic-0" → [1,2,3]}
   T+5ms:  Partition data WAL frame arrives
   T+6ms:  Checks get_partition_replicas("my-topic", 0) → Some([1,2,3]) ✅
   T+7ms:  Data ACCEPTED and written to local WAL ✅

Case B: Partition data arrives FIRST (THE BUG!)
   T+2ms:  Partition data WAL frame arrives
   T+3ms:  Checks get_partition_replicas("my-topic", 0) → None ❌
   T+4ms:  Line 1718: "NO REPLICA INFO yet, REJECTING replication" ❌
   T+5ms:  Data DROPPED ❌
   T+100ms: __chronik_metadata WAL frame arrives (too late)
   T+101ms: partition_assignments updated: {"my-topic-0" → [1,2,3]}
   T+∞:    Data already dropped, can never be recovered ❌
```

---

## The Redundant Raft Check

**File**: `crates/chronik-server/src/wal_replication.rs:1698-1722`

```rust
// CRITICAL FIX (Phase 3 CORRECTED): Only accept replication for partitions where we're a REPLICA
if let Some(ref raft) = raft_cluster {
    // Query partition replicas from Raft metadata
    if let Some(replicas) = raft.get_partition_replicas(&wal_record.topic, wal_record.partition) {
        if !replicas.contains(&node_id) {
            info!("⏭️  Skipping replication for {}-{}: this node ({}) is NOT in replica set {:?}",
                wal_record.topic, wal_record.partition, node_id, replicas);
            continue; // Skip - we're not a replica for this partition
        }

        info!("✅ Accepting replication for {}-{}: this node ({}) IS in replica set {:?}",
            wal_record.topic, wal_record.partition, node_id, replicas);
    } else {
        // No replica info yet - REJECT to prevent duplication
        warn!("⛔ NO REPLICA INFO for {}-{} yet, REJECTING replication (waiting for metadata sync)",
            wal_record.topic, wal_record.partition);
        continue; // Skip - no replica info available  ← THIS IS THE BUG
    }
}
```

**Why This Is Redundant**:
1. `get_partition_replicas()` queries `state_machine.partition_assignments`
2. `state_machine.partition_assignments` is updated by BOTH Raft AND `__chronik_metadata` WAL
3. The check doesn't distinguish between "no metadata at all" vs "metadata being replicated via WAL"
4. Metadata IS being sent via `__chronik_metadata`, but might arrive microseconds after partition data

**Original Intent**: Prevent message duplication by ensuring followers only accept partitions they own.

**Actual Behavior**: Rejects data when metadata hasn't synced yet, even though metadata IS being replicated (just in a race).

---

## Evidence: Logs Confirming Both Paths Are Active

### Leader Logs (Node 3)

**Raft consensus for metadata**:
```
[Node 3] INFO RaftCluster: Proposed MetadataCommand::AssignPartition to Raft
```

**WAL metadata replication**:
```
[Node 3] INFO WalReplicationManager: ✅ Sent and flushed WAL record for __chronik_metadata-0 to follower: localhost:9291 (102 bytes)
[Node 3] INFO WalReplicationManager: ✅ Sent and flushed WAL record for __chronik_metadata-0 to follower: localhost:9292 (102 bytes)
```

**Partition data replication**:
```
[Node 3] INFO WalReplicationManager: ✅ Sent and flushed WAL record for perf-acks-all-0 to follower: localhost:9291 (1990 bytes)
[Node 3] INFO WalReplicationManager: ✅ Sent and flushed WAL record for perf-acks-all-0 to follower: localhost:9292 (1990 bytes)
```

---

### Follower Logs (Node 1/2)

**WAL metadata received and applied**:
```
[Node 1] INFO WalReceiver: METADATA✓ Replicated: __chronik_metadata-0 offset 0 (45 bytes)
[Node 1] DEBUG handle_metadata_wal_record: Applied MetadataCommand::AssignPartition directly to state machine
```

**Partition data rejected**:
```
[Node 1] WARN WalReceiver: ⛔ NO REPLICA INFO for perf-acks-all-0 yet, REJECTING replication (waiting for metadata sync)
```

**Conclusion**: `__chronik_metadata` IS arriving and being applied, but partition data arrives first in the race.

---

## Why Two Metadata Paths Exist

### Historical Context

**Phase 1: Raft-Only Metadata** (v2.0.0 - v2.2.6)
- Metadata coordinated via Raft consensus
- Slow (100-200ms) but strong consistency
- No WAL metadata replication

**Phase 2: WAL Metadata Replication Added** (v2.2.7+)
- Added `__chronik_metadata` WAL topic for fast metadata sync
- Bypasses Raft via `apply_metadata_command_direct()`
- Intended to be faster than Raft for follower updates

**Current State (v2.2.8)**:
- BOTH systems running concurrently
- Raft still used for leader election and cluster membership
- WAL metadata used for partition assignments
- **NO CLEAR MIGRATION PATH** - hybrid state

---

## The Question: Can We Remove Raft Metadata?

### What Raft Currently Does

1. **Cluster membership** - Which nodes are in the cluster
2. **Leader election** - Which node is the Raft leader
3. **Partition assignments** - Which nodes replicate which partitions (❓ redundant)
4. **Partition leaders** - Which node is the leader for each partition (❓ redundant)
5. **ISR tracking** - In-sync replica sets (❓ redundant)

### What `__chronik_metadata` WAL Does

**Same as Raft for #3-5**:
- Partition assignments → `MetadataCommand::AssignPartition`
- Partition leaders → `MetadataCommand::SetPartitionLeader`
- ISR tracking → `MetadataCommand::UpdateISR`

**Key Difference**: WAL is FASTER (microseconds) but NO consensus (eventual consistency)

---

## Proposed Solutions

### Option 1: Remove Raft Check (Simple, Unsafe)

**Change**: Delete lines 1698-1722 in `wal_replication.rs`

**Pros**:
- Eliminates race condition
- Simpler code

**Cons**:
- ❌ **UNSAFE**: Nodes will accept partition data even if they're NOT supposed to be replicas
- ❌ Can cause duplication if partition reassignment happens
- ❌ Defeats the original purpose of the check

**Recommendation**: ❌ **DO NOT DO THIS** without additional validation

---

### Option 2: Buffer Partition Data Until Metadata Arrives (Correct Fix)

**Change**: In `WalReceiver::handle_connection()`, buffer received partition data if no metadata yet

**Implementation**:
```rust
// In handle_connection() at line 1698
if let Some(ref raft) = raft_cluster {
    if let Some(replicas) = raft.get_partition_replicas(&wal_record.topic, wal_record.partition) {
        if !replicas.contains(&node_id) {
            info!("⏭️  Skipping: not a replica");
            continue;
        }
    } else {
        // NO METADATA YET - BUFFER instead of reject
        warn!("📦 BUFFERING WAL data for {}-{} until metadata syncs (buffer size: {})",
              wal_record.topic, wal_record.partition, pending_buffer.len());

        // Add to pending buffer
        pending_buffer.insert((wal_record.topic, wal_record.partition), wal_record);

        // Spawn task to retry after __chronik_metadata WAL arrives (poll every 100ms for 10s)
        tokio::spawn(async move {
            for _ in 0..100 {
                sleep(Duration::from_millis(100)).await;
                if let Some(replicas) = raft.get_partition_replicas(&topic, partition) {
                    if replicas.contains(&node_id) {
                        // Metadata arrived! Flush buffer to WAL
                        for record in pending_buffer.drain() {
                            Self::write_to_wal(&wal_manager, &record).await?;
                        }
                        return Ok(());
                    }
                }
            }
            // Timeout after 10s - drop buffer and warn
            warn!("⚠️ Metadata never arrived for {}-{}, dropping {} buffered records",
                  topic, partition, pending_buffer.len());
        });

        continue;  // Skip write_to_wal for now, will be done by retry task
    }
}
```

**Pros**:
- ✅ Solves race condition completely
- ✅ Preserves duplication prevention
- ✅ Works with BOTH Raft and WAL metadata
- ✅ Graceful degradation (10s timeout)

**Cons**:
- Adds memory overhead for buffering
- Complex async retry logic

**Recommendation**: ⭐ **BEST SOLUTION** - fixes the bug correctly

---

### Option 3: Guarantee `__chronik_metadata` Arrives BEFORE Partition Data

**Change**: On leader side, don't send partition data until `__chronik_metadata` WAL is sent

**Implementation**:
```rust
// In ProduceHandler or topic creation logic
// Step 1: Send metadata first
self.metadata_replicator.replicate(
    &MetadataCommand::AssignPartition { topic, partition, replicas }
).await?;

// Step 2: Small delay to ensure metadata arrives first (e.g., 10ms)
tokio::time::sleep(Duration::from_millis(10)).await;

// Step 3: NOW send partition data
self.wal_replication_mgr.replicate_partition(...).await;
```

**Pros**:
- ✅ Eliminates race condition by guaranteeing ordering
- ✅ No buffering needed on follower

**Cons**:
- ⚠️ Adds latency to produce path (10ms delay)
- ⚠️ Fragile - depends on timing assumptions
- ⚠️ Doesn't scale if network is slow

**Recommendation**: 🔶 **OKAY** as a quick fix, but Option 2 is more robust

---

### Option 4: Remove Raft Metadata Entirely, Use WAL-Only

**Change**: Migrate all metadata queries to use `__chronik_metadata` WAL instead of Raft

**Steps**:
1. Remove Raft-based metadata queries (`get_partition_replicas`, etc.)
2. Keep Raft ONLY for cluster membership and leader election
3. Use WAL replication for ALL partition metadata
4. Add ordering guarantees (metadata before data)

**Pros**:
- ✅ Eliminates architectural redundancy
- ✅ Simpler mental model
- ✅ Faster metadata propagation (microseconds vs 100ms)

**Cons**:
- ⚠️ Large refactor (touch many files)
- ⚠️ Lose strong consistency (Raft → eventual consistency)
- ⚠️ Need to design new failure modes

**Recommendation**: 🔶 **GOOD LONG-TERM** but risky for immediate fix

---

## Recommended Fix Plan

### Immediate Fix (Option 2 - Buffering)

**Target**: v2.2.10

1. **Add pending buffer** to `WalReceiver`:
   ```rust
   struct PendingWalBuffer {
       records: Arc<DashMap<(String, i32), VecDeque<WalReplicationRecord>>>,
       max_per_partition: usize,  // 10,000 records
   }
   ```

2. **Modify `handle_connection()`** (lines 1698-1722):
   - If replica info missing: buffer instead of drop
   - Spawn retry task to check `get_partition_replicas()` every 100ms
   - Apply buffered records once metadata arrives (from EITHER Raft OR `__chronik_metadata`)
   - Timeout after 10 seconds with warning

3. **Add metrics**:
   - `wal_replication_buffered_records` - count of buffered records
   - `wal_replication_buffer_timeouts` - count of timeouts
   - `wal_replication_buffer_applied` - count of successful applications

4. **Test**:
   - Create new topic with acks=all
   - Verify followers buffer WAL data
   - Verify followers apply buffered data after metadata sync
   - Verify all nodes have data (100% replication)

---

### Long-Term Cleanup (Option 4 - Remove Raft Metadata)

**Target**: v2.3.0

1. **Audit all `get_partition_replicas()` calls** in codebase
2. **Decide**: Keep Raft for cluster membership only, or remove entirely?
3. **Migrate metadata queries** to WAL-based state
4. **Add ordering guarantees** (metadata before data)
5. **Test failure modes** (network partitions, node crashes)

---

## Conclusion

Chronik has **architectural redundancy** - TWO metadata systems updating the SAME state machine:

1. **Raft consensus** (slow, strong consistency)
2. **`__chronik_metadata` WAL** (fast, eventual consistency)

The WAL replication bug is caused by checking Raft (which might not be synced) when WAL metadata IS being replicated but arrives in a race with partition data.

**Fix Priority**: Implement Option 2 (buffering) immediately for v2.2.10.

**Long-Term**: Decide whether to keep both systems or migrate fully to WAL metadata (v2.3.0).

**Estimated Fix Time**: 4-6 hours (implementation + testing)
**Estimated Architecture Cleanup Time**: 1-2 weeks (audit + refactor + test)

**CRITICAL**: This must be fixed before any production deployment. Current cluster configuration is NOT safe for acks=all workloads.
