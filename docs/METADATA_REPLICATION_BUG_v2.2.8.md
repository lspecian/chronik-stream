# Metadata Replication Bug - FIXED (v2.2.7)

## Problem Summary

**Issue**: Follower nodes (2 & 3) did not receive committed Raft entries containing metadata commands (broker registrations, partition assignments, etc.)

**Impact**: Followers had empty metadata stores → clients got "read underflow" errors when connecting to followers

**Scope**: Cluster mode only (standalone mode unaffected)

**Status**: ✅ **FIXED** with two critical patches (2025-11-09)

## Root Cause (TWO BUGS)

### Bug 1: Message Sender Dropped Failed Messages

**Location**: `crates/chronik-server/src/raft_cluster.rs:184-206`

**Problem**:
When the leader tried to send AppendEntries to followers via gRPC, if the connection wasn't ready yet (peer not connected), the message sender would:
1. Attempt to send the message
2. Get a "transport error: Failed to connect" error
3. **Log a warning and DROP the message** (no retry!)

**Timeline of Failure**:
```
19:07:13.480 - Leader persists 2 Raft entries (broker registrations)
19:07:13.480 - Leader tries to send AppendEntries to peer 2 → FAILED (not connected)
19:07:13.684 - Leader tries to send AppendEntries to peer 3 → FAILED (not connected)
19:07:13.286 - Connected to peer 2 ✅
19:07:14.880 - Connected to peer 3 ✅
19:07:15+    - Leader ONLY sends heartbeats (no entries to replicate anymore - already dropped!)
```

**Evidence**:
```bash
# Node 1 tried to send entries but failed
grep "failed to send.*to peer" tests/cluster/logs/node1.log
# Output: Failed to connect to http://localhost:5002: transport error

# Node 3 NEVER received entries
grep "Persisting.*Raft entries" tests/cluster/logs/node3.log
# Output: (empty)

# Node 3 NEVER applied committed entries
grep "Applied.*committed entries" tests/cluster/logs/node3.log
# Output: (empty)
```

**The Fix**: Exponential backoff retry in message sender (lines 193-235)

```rust
// CRITICAL FIX (v2.2.7): Retry failed messages with exponential backoff
let mut retry_count = 0;
let max_retries = 10;
let mut backoff_ms = 50;

loop {
    match transport_for_sender.send_message(...).await {
        Ok(_) => {
            tracing::debug!("✓ Message sender: sent to peer {} (retries: {})", peer_id, retry_count);
            break; // Success
        }
        Err(e) => {
            retry_count += 1;
            if retry_count >= max_retries {
                tracing::error!("FAILED after {} retries - MESSAGE LOST", max_retries);
                break;
            }

            // Exponential backoff: 50ms, 100ms, 200ms, 400ms, ..., max 1s
            tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
            backoff_ms = (backoff_ms * 2).min(1000);
        }
    }
}
```

**Result**: Messages are now retried up to 10 times with exponential backoff, giving gRPC transport time to establish connections.

### Bug 2: Duplicate Raft Entries Caused log.len() Mismatch

**Location**: `crates/chronik-wal/src/raft_storage_impl.rs:214-287`

**Problem**:
During Raft conflict resolution, multiple entries can be written at the same index (different terms). All versions get persisted to WAL. On recovery:
1. WAL recovery loads ALL entries (including duplicates)
2. Example: 1745 entries loaded, but 241 duplicates (same indices, different terms)
3. `log.len() = 1745` (counts duplicates)
4. `last_index = first_index + log.len() - 1 = 724 + 1745 - 1 = 2468`
5. But actual last entry index = 2227 (only 1504 unique indices)
6. When Raft asks for entries near index 2468, storage returns `Compacted` error
7. **Panic**: `unexpected error getting unapplied entries [724, 724): Store(Compacted)`

**Evidence**:
```bash
# Recovery log showed mismatch
grep "Recovered.*Raft entries" tests/cluster/logs/node3.log
# Output: ✓ Recovered 1745 Raft entries (index 724-2227)
#         ↑ 1745 entries           ↑ last index 2227
#         But 2227 - 724 + 1 = 1504 (should be 1504 entries if continuous)

# Raft thought last_index was different
grep "newRaft.*last index" tests/cluster/logs/node3.log
# Output: newRaft, last index: 2468
#                              ↑ Mismatch! Should be 2227
```

**The Fix**: Deduplicate entries during recovery (lines 233-264)

```rust
// CRITICAL FIX (v2.2.7): Deduplicate entries by index
// Keep only the latest (highest term) for each index
let mut deduped = Vec::new();
let mut last_index = 0u64;

for entry in recovered_entries {
    if entry.index != last_index {
        // New index - add it
        deduped.push(entry.clone());
        last_index = entry.index;
    } else {
        // Duplicate index - replace with higher term
        if entry.term > deduped.last().unwrap().term {
            deduped.pop();
            deduped.push(entry.clone());
        }
    }
}

recovered_entries = deduped;
```

**Result**: Only one entry per index is kept (the one with highest term), ensuring `log.len()` matches actual index range.

## Verification

### Test Results (2025-11-09, 20:15 UTC)

**All 3 nodes started successfully** ✅

```bash
# Node status
✓ Node 1: Running (PID 3159437, port 9092) - LEADER
✓ Node 2: Running (PID 3159539, port 9093) - FOLLOWER
✓ Node 3: Running (PID 3159559, port 9094) - FOLLOWER
```

**Metadata replication working** ✅

```bash
# Committed entries applied on all nodes
grep "Applied.*committed" tests/cluster/logs/node1.log | wc -l  # 22 entries
grep "Applied.*committed" tests/cluster/logs/node2.log | wc -l  # 22 entries
grep "Applied.*committed" tests/cluster/logs/node3.log | wc -l  # 23 entries
```

**No panics or crashes** ✅

```bash
# Check for errors
grep -i "panic\|error" tests/cluster/logs/node3.log
# Output: (clean - only expected "no topics found" warnings)
```

## Files Changed

### Fix 1: Message Sender Retry
- **File**: `crates/chronik-server/src/raft_cluster.rs`
- **Lines**: 182-238 (message sender background task)
- **Change**: Added exponential backoff retry loop (max 10 retries, 50ms-1s backoff)

### Fix 2: Entry Deduplication
- **File**: `crates/chronik-wal/src/raft_storage_impl.rs`
- **Lines**: 233-264 (WAL recovery deduplication)
- **Change**: Deduplicate recovered entries, keeping highest term for each index

## Lessons Learned

1. **Network Failures Are Not Optional**: Always retry failed network operations in distributed systems
2. **Raft Invariants Must Be Enforced**: Storage layer MUST guarantee continuous log indices
3. **Recovery != Runtime**: Code paths for WAL recovery and runtime append can have different constraints
4. **Test Early Startup Races**: The "leader tries to send before followers connect" race only happens during cluster bootstrap
5. **Log What Matters**: Without retry logs, we couldn't have debugged the message loss issue

## Related Bugs Fixed

This fix also resolves:
- `RAFT_LOG_UNSTABLE_PANIC_v2.2.7.md` - Panic during conflict resolution (related to duplicates)
- `BROKER_REGISTRATION_BUG_v2.2.7.md` - Broker registration deadlock (separate fix already applied)

## Version

**Fixed in**: v2.2.7
**Date**: 2025-11-09
**Commits**:
- Message sender retry: `raft_cluster.rs` lines 193-235
- Entry deduplication: `raft_storage_impl.rs` lines 233-264
