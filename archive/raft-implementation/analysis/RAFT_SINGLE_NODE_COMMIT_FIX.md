# Raft Single-Node Commit Fix

## Problem

Single-node Raft clusters were not committing proposals:
- `propose_and_wait()` would timeout after 30 seconds
- `has_ready()` always returned `false` in the background tick loop
- The Raft log remained empty (`last_index=0`) even after proposals
- Leader election worked, but commits never happened

## Root Cause

TiKV Raft's design requires explicit `tick()` calls to trigger commit logic, even in single-node clusters where there's no quorum to wait for. The background tick loop was running independently and often missed the critical moment after a proposal when a `tick()` call would trigger the commit.

## Solution

Implemented **synchronous commits for single-node clusters**:

### 1. Constructor Changes (`replica.rs` lines 128-179)
- Added explicit `campaign()` call for single-node clusters
- Process the campaign's `ready()` state immediately with `advance()`
- Directly update `ReplicaState` to reflect leadership

### 2. Propose Method Fix (`replica.rs` lines 280-325)
After calling `node.propose()`:
- Detect if this is a single-node cluster by checking `ConfState`
- If single-node:
  - Call `tick()` to trigger commit logic
  - Process `ready()` state synchronously
  - Call `advance()` to finalize the commit
- This makes proposals commit immediately without waiting for the background loop

### 3. Propose-and-Wait Fix (`replica.rs` lines 342-358)
- Check if single-node cluster at the start
- If single-node, delegate to `propose()` which handles synchronous commit
- If multi-node, use the original async path with channel notification

## Code Changes

**File**: `crates/chronik-raft/src/replica.rs`

**Key sections**:
1. Lines 128-179: Constructor with campaign() processing
2. Lines 280-325: Single-node synchronous commit in propose()
3. Lines 342-358: Single-node fast path in propose_and_wait()

## Behavior

**Single-Node Clusters**:
- Proposals commit synchronously within the propose() call
- No background loop dependency
- Immediate commit guarantee
- No timeout issues

**Multi-Node Clusters** (unchanged):
- Proposals remain async
- Background tick/ready loop handles replication
- Channel notifications for commit events
- Proper quorum-based commits

## Testing

To test single-node commits:

```rust
let replica = PartitionReplica::new(
    "test".to_string(),
    0,
    config,
    storage,
    vec![], // Empty peers = single-node
)?;

// Should commit immediately
let index = replica.propose_and_wait(data).await?;
```

Expected behavior:
- Leader election completes in constructor
- `is_leader()` returns true immediately
- `propose()` and `propose_and_wait()` succeed without timeout
- Commits happen synchronously

## Related Files

- `crates/chronik-raft/src/replica.rs` - Main implementation
- `crates/chronik-server/src/raft_integration.rs` - Background loop (unchanged)
- `tests/integration/raft_single_node_debug.rs` - Test file

## Migration Notes

No breaking changes. Existing multi-node clusters work identically. Single-node clusters now work correctly where they previously timed out.
