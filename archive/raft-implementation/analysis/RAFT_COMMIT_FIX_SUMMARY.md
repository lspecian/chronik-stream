# Raft Commit Advancement Fix - Summary

## Problem
Raft consensus was failing to commit proposals despite successful leader election and message exchange. Symptoms:
- `commit_index` stuck at 0
- Leader's own `matched_index` remaining at 0
- Raft log `last_index` resetting to 0 after entries were added
- No committed entries despite quorum responses

## Root Cause
**tikv/raft's MemStorage was not being updated synchronously before `advance()` was called.**

### Technical Details
The issue was in `crates/chronik-raft/src/replica.rs::ready()`:

**Before (Broken)**:
```rust
// 1. Extract entries from ready
let entries_to_persist = ready.entries().iter().cloned().collect();

// 2. Call advance() FIRST (tells tikv/raft we've persisted)
let mut light_rd = node.advance(ready);

// 3. LATER: Persist to custom storage
self.persist_entries(&entries_to_persist).await?;
```

**Problem**: tikv/raft expects its Storage trait to be updated BEFORE `advance()` is called. When we called `advance()` without updating MemStorage first, tikv/raft assumed the entries were lost and reset the log.

**After (Fixed)**:
```rust
// 1. Extract entries and hard state from ready
let entries_to_persist = ready.entries().iter().cloned().collect();
let hard_state_to_persist = ready.hs().cloned();

// 2. Persist to tikv/raft's MemStorage FIRST
if let Some(hs) = hard_state_to_persist.as_ref() {
    let storage = node.mut_store();
    storage.wl().set_hardstate(hs.clone());
}
if !entries_to_persist.is_empty() {
    let storage = node.mut_store();
    storage.wl().append(&entries_to_persist)?;
}

// 3. NOW it's safe to call advance() - storage is in sync
let mut light_rd = node.advance(ready);

// 4. Also persist to our custom durable storage (for recovery)
self.persist_entries_to_custom_storage(&entries_to_persist).await?;
```

## Changes Made

### File: `crates/chronik-raft/src/replica.rs`

1. **Lines 662-670**: Added synchronous persistence to tikv/raft's MemStorage BEFORE `advance()`
   - First persist hard_state via `storage.wl().set_hardstate()`
   - Then persist entries via `storage.wl().append()`
   - Order matters: hard state must be persisted before entries

2. **Lines 925-947**: Renamed `persist_entries()` to `persist_entries_to_custom_storage()`
   - Removed the duplicate MemStorage append that was in the wrong place
   - This method now only persists to our custom durable storage
   - tikv/raft's MemStorage is updated synchronously in `ready()` before this is called

## Test Results

After the fix, the 3-node Raft cluster shows:
- ✅ Node 3 elected as leader successfully
- ✅ Peers 1 and 2 show `matched=6` (replication working)
- ✅ Leader (Peer 3) shows `matched=6` (leader's own log persisting)
- ✅ All peers in `Replicate` state
- ✅ Committed entries up to index 6
- ✅ Commit index (`commit=6`) propagating correctly

## Key Learnings

1. **tikv/raft's Storage contract**: The Storage trait MUST be updated synchronously before calling `advance()`. This is a hard requirement of the tikv/raft library.

2. **Two-level storage**: We maintain TWO storage layers:
   - tikv/raft's MemStorage (in-memory, for Raft state machine consistency)
   - Our custom RaftLogStorage (durable, for disaster recovery)
   Both must be kept in sync, but MemStorage must be updated FIRST.

3. **Order matters**: hard_state must be persisted before entries. This ensures term/vote/commit are durable before log entries.

4. **Never call `advance()` prematurely**: Calling `advance()` tells tikv/raft "I've persisted everything". If storage is not actually updated, Raft assumes data loss and resets state.

## Related Files
- `crates/chronik-raft/src/replica.rs` - Core fix
- `crates/chronik-raft/src/storage.rs` - Custom durable storage (unchanged)
- `test_raft_diagnostic.sh` - Test script for verification

## Verification
```bash
# Build with raft feature
cargo build --release --bin chronik-server --features=raft

# Run diagnostic test
bash test_raft_diagnostic.sh

# Check committed entries in logs
grep "Committed entries" /tmp/raft-node*.log
```

Expected output:
- Committed entries from index 1 to 6 on all nodes
- Leader's Progress tracker showing `matched=6` for all peers
- No "No address for peer" errors
- Stable leader without re-elections
