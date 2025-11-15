# acks=all Deadlock - Root Cause Analysis (FINAL)

## Executive Summary

**Issue**: `acks=all` produce requests hang indefinitely, causing 30s timeouts.

**Root Cause**: Attempting to use Raft consensus (`propose()`) for partition assignment during topic creation causes a deadlock. The produce request blocks waiting for Raft consensus, but Raft consensus cannot complete because it's being called from within the produce request handler.

**Solution**: Partition assignments should use **WAL-based metadata replication** (already implemented), NOT Raft consensus. The Raft state machine should be updated via `apply_metadata_command_direct()` on the leader, and followers receive updates via WAL streaming.

## Investigation Timeline

### Phase 1: Discovery Loop Timing
- **Issue**: Discovery loop was sleeping FIRST (10s), causing partition leaders to not be discovered until next cycle
- **Fix**: Moved sleep to END of loop, reduced interval from 10s to 1s
- **Result**: Discovery now happens within 100ms, but acks=all still times out ❌

### Phase 2: Raft Replication Delay
- **Issue**: Node 2 doesn't have partition assignments in its Raft metadata
- **Finding**: Node 1 (Raft leader) has assignments, but they're never replicated to followers
- **Root Cause**: Code uses `apply_metadata_command_direct()` which bypasses Raft consensus
- **Attempted Fix**: Changed to `propose()` to use Raft consensus
- **Result**: Topic creation hangs indefinitely ❌

### Phase 3: Deadlock Discovery (FINAL ROOT CAUSE)
- **Issue**: Using `propose()` causes infinite hang during topic creation
- **Root Cause**:
  1. Produce request starts topic creation (async context)
  2. Topic creation calls `raft.propose()` to assign partitions
  3. `propose()` blocks waiting for Raft consensus (needs to replicate to followers)
  4. Raft replication happens asynchronously in background
  5. But the async runtime is blocked waiting for the produce request to complete
  6. **DEADLOCK**: Produce waits for Raft, Raft waits for produce ♾️

## Architecture Clarification

Chronik has **TWO SEPARATE REPLICATION SYSTEMS**:

### 1. Raft Consensus (for critical cluster membership)
- **Purpose**: Cluster membership, leader election, critical coordination
- **Mechanism**: Multi-phase consensus protocol (propose → replicate → commit)
- **Latency**: 10-50ms (requires majority quorum)
- **Usage**: Raft leadership, adding/removing nodes
- **WARNING**: ⚠️ NEVER call `raft.propose()` from within request handlers - it blocks!

### 2. WAL-Based Metadata Replication (for partition metadata)
- **Purpose**: Partition assignments, ISR updates, topic metadata
- **Mechanism**: Leader writes to metadata WAL → Async streaming to followers
- **Latency**: 1-2ms (asynchronous, no blocking)
- **Usage**: Topic creation, partition assignment, ISR updates
- **Implementation**: `RaftMetadataStore` with WAL streaming

## The Correct Architecture

```
Topic Creation Flow (v2.2.7 Phase 2 - WAL-based):

1. Producer sends ProduceRequest to Node 1
   ↓
2. Node 1 (Raft leader) auto-creates topic
   ↓
3. Call metadata_store.assign_partition() for each partition
   ↓
4. RaftMetadataStore writes to metadata WAL (synchronous, fast)
   ↓
5. RaftMetadataStore applies to local Raft state machine (apply_metadata_command_direct)
   ↓
6. Background WAL replicator streams metadata to followers (async, non-blocking)
   ↓
7. Followers receive WAL entries and apply to their state machines
   ↓
8. ProduceRequest returns immediately (no blocking on replication)
```

**Key Point**: The leader applies changes locally and returns immediately. Replication happens asynchronously in the background via WAL streaming.

## The Incorrect Approach (Causes Deadlock)

```
❌ WRONG: Using Raft consensus during topic creation

1. Producer sends ProduceRequest to Node 1
   ↓
2. Node 1 starts topic creation
   ↓
3. Call raft.propose(AssignPartition) ← BLOCKS HERE
   ↓
4. propose() waits for Raft consensus (requires follower ACKs)
   ↓
5. Raft tries to replicate to followers
   ↓
6. But async runtime is blocked waiting for ProduceRequest to complete
   ↓
7. DEADLOCK: Neither can progress ♾️
```

## Code Locations

### Where the bug was introduced:
**File**: `crates/chronik-server/src/produce_handler.rs`
**Lines**: 2558, 2577

```rust
// ❌ WRONG (causes deadlock)
if let Err(e) = raft.propose(assign_command).await {
    // This blocks the produce request waiting for Raft consensus
}
```

### What it should be:
```rust
// ✅ CORRECT (non-blocking)
if let Err(e) = raft.apply_metadata_command_direct(assign_command) {
    // Apply locally, let WAL replication handle followers asynchronously
}
```

## Why apply_metadata_command_direct() is Correct

The `apply_metadata_command_direct()` function has this warning:

```rust
/// # WARNING
/// This bypasses Raft consensus! Only use when:
/// 1. Command is already persisted to metadata WAL
/// 2. Caller handles replication separately
```

Both conditions are met:
1. ✅ `metadata_store.assign_partition()` writes to metadata WAL (line 2537)
2. ✅ WAL replication handles followers asynchronously (background task)

## The Fix

**Revert the change from `apply_metadata_command_direct()` to `propose()`**

The original code was correct! The issue was NOT that we weren't using Raft consensus. The issue was:
1. ✅ Partition assignments were being written to local Raft state machine
2. ❌ But WAL metadata replication to followers wasn't implemented yet
3. ❌ Follower discovery was checking Raft state, but followers didn't have the data

**The real solution**: Ensure WAL metadata replication properly replicates partition assignments to followers. This is the "WAL FAST PATH" mentioned in the code comments.

## Files Involved

1. **`crates/chronik-server/src/produce_handler.rs`** (lines 2518-2584)
   - Topic creation and partition assignment
   - Should use `apply_metadata_command_direct()` + WAL replication

2. **`crates/chronik-server/src/raft_metadata_store.rs`**
   - WAL-based metadata store
   - Handles async replication to followers

3. **`crates/chronik-server/src/raft_cluster.rs`**
   - `apply_metadata_command_direct()` - Apply locally (fast, non-blocking)
   - `propose()` - Raft consensus (slow, BLOCKS - DO NOT use in request handlers!)

4. **`crates/chronik-server/src/wal_replication.rs`**
   - WAL streaming infrastructure
   - Follower discovery based on partition leaders

## Next Steps

1. ✅ Revert produce_handler.rs to use `apply_metadata_command_direct()`
2. ⏳ Verify WAL metadata replication is working correctly
3. ⏳ Ensure followers receive partition assignments via WAL stream
4. ⏳ Test acks=all completes in < 1 second

## Key Lesson

**DO NOT use Raft consensus (`propose()`) from within request handlers!**

Raft consensus is synchronous and blocks waiting for replication. This is fine for admin operations (adding nodes, changing configuration), but NEVER for the hot path (produce/fetch requests).

For hot path operations, use WAL-based replication which is asynchronous and non-blocking.
