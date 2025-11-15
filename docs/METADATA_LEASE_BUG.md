# Metadata Lease Bug - Investigation Summary

**Date**: 2025-11-15
**Status**: ROOT CAUSE IDENTIFIED

## Summary

Kafka metadata requests are failing with "No Raft leader elected after 4 retries" even though:
1. ✅ The Raft leader WAS elected successfully (leader_id=1)
2. ✅ The partition assignment fix IS working correctly
3. ❌ BUT: The `RaftCluster::get_leader_id()` method cannot find the leader

## Evidence from Logs

**Leader Election Succeeded** ([node1.log:76](file://tests/cluster/logs/node1.log#L76)):
```
[2025-11-14T23:54:15.261609Z] ✓ Raft leader elected: leader_id=1, this_node=1, is_leader=true
```

**Broker Registration Succeeded** ([node1.log:78-80](file://tests/cluster/logs/node1.log#L78)):
```
[2025-11-14T23:54:15.262871Z] ✅ Successfully registered broker 1 via Raft
[2025-11-14T23:54:15.263653Z] ✅ Successfully registered broker 2 via Raft
[2025-11-14T23:54:15.264337Z] ✅ Successfully registered broker 3 via Raft
```

**Partition Assignment Triggered** ([node1.log:82](file://tests/cluster/logs/node1.log#L82)):
```
[2025-11-14T23:54:15.264347Z] Broker registration completed - starting partition assignment
```

**But Metadata Requests Fail** (90 seconds later, [node1.log:600-620](file://tests/cluster/logs/node1.log#L600)):
```
[2025-11-14T23:55:46.258540Z] No Raft leader elected (attempt 1/4), retrying in 50ms
[2025-11-14T23:55:46.309254Z] No Raft leader elected (attempt 2/4), retrying in 100ms
[2025-11-14T23:55:46.410824Z] No Raft leader elected (attempt 3/4), retrying in 200ms
[2025-11-14T23:55:46.612245Z] Failed to auto-create topics on metadata: "No Raft leader elected after 4 retries"
```

## Root Cause

The problem is in [RaftCluster::get_leader_id()](file://crates/chronik-server/src/raft_cluster.rs#L1144-L1153):

```rust
pub async fn get_leader_id(&self) -> Option<u64> {
    let raft_node = self.raft_node.lock().await;
    let leader_id = raft_node.raft.leader_id;

    if leader_id == raft::INVALID_ID {
        None  // ← Returns None even though leader was elected!
    } else {
        Some(leader_id)
    }
}
```

**The Issue**: Even though the Raft leader is elected at startup, the `raft_node.raft.leader_id` field is not being properly updated/maintained, so it remains `raft::INVALID_ID`.

## Call Stack

1. Client sends Kafka Metadata request
2. → `handle_metadata()` in kafka_handler.rs
3. → `list_topics()` in raft_metadata_store.rs:258
4. → Checks `lease_manager.has_valid_lease()` → returns `false` (lease expired)
5. → Forwards to leader via `raft.query_leader()` in raft_cluster.rs:1168
6. → Calls `get_leader_id()` in raft_cluster.rs:1144
7. → Returns `None` because `raft_node.raft.leader_id == raft::INVALID_ID`
8. → Retries 4 times with exponential backoff
9. → Returns error: "No Raft leader elected after 4 retries"

## Why This is NOT Caused by Partition Assignment Fix

The partition assignment fix (v2.2.7) correctly places partition assignment AFTER:
- Raft message loop starts
- Raft leader is elected
- Brokers are registered

The logs confirm this sequence is working correctly:
```
[23:54:15.261] ✓ Raft leader elected
[23:54:15.262-264] ✅ All brokers registered
[23:54:15.264] Partition assignment started
```

The metadata request failure happens 90 seconds LATER (23:55:46), indicating this is a **separate pre-existing bug** in the Raft leader tracking mechanism.

## Impact

- ❌ All Kafka metadata requests fail
- ❌ Topic auto-creation fails
- ❌ Producers cannot send messages (need metadata)
- ❌ Consumers cannot fetch messages (need metadata)

**Result**: Cluster starts successfully but cannot serve any Kafka requests.

## Next Steps

1. ✅ **COMPLETED**: Partition assignment architectural fix (v2.2.7)
2. **REQUIRED**: Fix Raft leader tracking bug (this issue)
   - Investigate why `raft_node.raft.leader_id` is not updated after election
   - May need to check Raft message loop or leader election handling
3. **REQUIRED**: Fix lease manager or implement bypass for leader node
   - Leader node should not need to forward queries to itself
   - Check `am_i_leader()` logic in query_leader()

## Related Files

- [crates/chronik-server/src/raft_cluster.rs:1144](file://crates/chronik-server/src/raft_cluster.rs#L1144) - `get_leader_id()` method
- [crates/chronik-server/src/raft_cluster.rs:1168](file://crates/chronik-server/src/raft_cluster.rs#L1168) - `query_leader()` method
- [crates/chronik-server/src/raft_metadata_store.rs:250](file://crates/chronik-server/src/raft_metadata_store.rs#L250) - Lease check logic
- [tests/cluster/logs/node1.log](file://tests/cluster/logs/node1.log) - Evidence logs
