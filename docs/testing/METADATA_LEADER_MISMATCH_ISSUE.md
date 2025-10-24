# Metadata Leader Mismatch Issue - Root Cause Analysis

## Problem Statement

When producing messages to a Raft-replicated topic, clients receive `NOT_LEADER_OR_FOLLOWER` errors even though:
- ✅ Raft replicas are created correctly
- ✅ Leaders are elected successfully
- ✅ Replication is working

## Root Cause

**There is a mismatch between the partition assignment leader and the actual Raft leader.**

### How It Happens

1. **Topic Creation** (`kafka-topics --create`):
   - Creates topic metadata
   - `ProduceHandler` creates Raft replicas for all partitions
   - `ProduceHandler` uses `PartitionAssignment` manager to assign leaders
   - Example: `PartitionAssignment` might assign Node 1 as leader for partition 0

2. **Raft Leader Election**:
   - Raft replicas elect leaders independently
   - Raft doesn't care about partition assignment
   - Example: Raft might elect Node 2 as leader for partition 0

3. **Metadata Request** (client asks "who is leader?"):
   - Calls `metadata_store.get_partition_leader(topic, partition)`
   - This queries the `PartitionAssignment` table
   - Returns Node 1 (from step 1)

4. **Produce Request**:
   - Client sends to Node 1
   - Node 1's Raft replica knows Node 2 is the actual leader
   - Returns `NOT_LEADER_OR_FOLLOWER` error

## Evidence

**From logs** (`node2.log`):
```
[INFO] Raft state transition node_id=2 topic=raft-test partition=0 old_role=Candidate new_role=Leader leader_id=2 term=1
[INFO] Raft state transition node_id=2 topic=raft-test partition=2 old_role=Candidate new_role=Leader leader_id=2 term=2
```

**From logs** (`node3.log`):
```
[INFO] Raft state transition node_id=3 topic=raft-test partition=1 old_role=Candidate new_role=Leader leader_id=3 term=1
```

**Actual Raft Leaders**:
- `raft-test-0`: Node 2 ✅
- `raft-test-1`: Node 3 ✅
- `raft-test-2`: Node 2 ✅

**Partition Assignments** (likely):
- Based on round-robin in `ProduceHandler.rs:2057`
- Probably Node 1, Node 2, Node 3 (or similar)
- **Does NOT match Raft leaders!**

## Solution

**Update partition assignment when a Raft replica becomes leader.**

### Implementation Plan

**Location**: `crates/chronik-raft/src/replica.rs:651-659`

When a replica transitions to Leader state:
```rust
if old_role != StateRole::Leader && state.role == StateRole::Leader {
    info!(
        node_id = self.config.node_id,
        topic = %self.topic,
        partition = self.partition,
        term = state.term,
        "Became Raft leader"
    );

    // NEW: Update partition assignment in metadata store
    // Option 1: Call callback to server layer
    // Option 2: Send message to RaftReplicaManager
    // Option 3: Update via shared metadata store reference
}
```

### Design Options

**Option 1: Callback Function** (Cleanest)
- Add `on_became_leader` callback to `RaftReplica`
- Server layer provides callback that updates partition assignment
- Pros: Clean separation, no circular dependencies
- Cons: Requires threading callback through construction

**Option 2: Metadata Store Reference** (Simplest)
- Pass metadata store to `RaftReplica`
- Directly call `metadata_store.assign_partition()` when becoming leader
- Pros: Direct, simple
- Cons: Adds dependency from raft crate to metadata

**Option 3: Event Channel** (Most Flexible)
- `RaftReplica` sends "BecameLeader" events to channel
- Server layer listens and updates partition assignments
- Pros: Decoupled, extensible
- Cons: More complex

## Recommended Approach

**Use Option 1 (Callback)** for v1.3.67.

### Code Changes Required

1. **`chronik-raft/src/replica.rs`**:
   - Add `on_became_leader: Option<Box<dyn Fn(String, u32, u64) + Send + Sync>>` to `RaftReplicaConfig`
   - Call callback when `state.role` becomes `Leader`

2. **`chronik-server/src/raft_integration.rs`**:
   - Provide callback that calls `metadata_store.assign_partition()`
   - Set `is_leader = true` for new leader
   - Set `is_leader = false` for all other nodes (optional cleanup)

3. **Testing**:
   - Verify partition assignment updates after leader election
   - Verify metadata API returns correct leader
   - Verify produce requests succeed

## Alternative Quick Fix (Workaround)

For immediate testing, we could:
1. **Disable partition assignment pre-allocation** in `ProduceHandler`
2. **Only create assignments when Raft leaders are known**
3. This delays assignment until after election, ensuring correctness

But this is a workaround - the proper fix is to update assignments on leader change.

## Impact

**High Priority** - This blocks all produce operations in Raft cluster mode.

Without this fix:
- ❌ Clients cannot produce to Raft-replicated topics
- ❌ Metadata API returns incorrect leader information
- ✅ Raft replication works (but unreachable)
- ✅ Consume might work if clients guess the right leader

With this fix:
- ✅ Metadata API returns correct Raft leader
- ✅ Clients route produce requests correctly
- ✅ Full Raft cluster functionality

## Files Involved

- `crates/chronik-raft/src/replica.rs:651-659` - Detect leader transition
- `crates/chronik-server/src/raft_integration.rs` - Provide callback
- `crates/chronik-server/src/produce_handler.rs:2050-2075` - Current assignment logic
- `crates/chronik-common/src/metadata/memory.rs` - Partition assignment storage
- `crates/chronik-protocol/src/handler.rs:6747` - Metadata response logic

## Timeline

- **Discovery**: 2025-10-23 (today)
- **Root Cause**: Identified via log analysis
- **Proposed Fix**: Callback on leader transition
- **ETA**: Can be implemented in ~1-2 hours
- **Testing**: Use existing 3-node cluster

## Related Issues

- Topic creation callback (FIXED in previous session)
- Multi-node validation blocking (FIXED today)
- This completes the "Topic Creation → Raft Replicas → Metadata Sync" chain
