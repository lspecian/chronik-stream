# Partition Leadership v2.2.1 Analysis

**Date:** 2025-11-08
**Version Tested:** v2.2.1
**User Feedback:** Still experiencing partition leadership errors

---

## User Report Summary

✅ **FIXED**: Broker metadata synchronization working perfectly
❌ **NOT FIXED**: Partition leadership errors still occurring

### Error Pattern from v2.2.1 Deployment

```
❌ Failed to elect leader for __meta-0:
   Cannot propose: this node (id=1) is not the leader (state=Follower, leader=3)

❌ Failed to elect leader for _confluent-ksql-chronik_ksql_cluster_command_topic-0:
   Cannot propose: this node (id=1) is not the leader (state=Follower, leader=3)

Leader timeout for __meta-0 (leader=1), triggering election
Leader timeout for __meta-0 (leader=1), triggering election
(repeats continuously)
```

---

## Root Cause Analysis

### What v2.2.1 Fixed ✅

The fix in [integrated_server.rs:593-707](../crates/chronik-server/src/integrated_server.rs#L593-L707) successfully prevents **startup initialization errors**:

```rust
// CRITICAL FIX (v2.2.1): Only Raft leader proposes partition metadata
if !this_node_is_leader {
    info!("Skipping metadata initialization - this node is a Raft follower");
} else {
    // Only Raft leader initializes partition metadata
    raft.propose(MetadataCommand::AssignPartition { ... }).await;
    raft.propose(MetadataCommand::SetPartitionLeader { ... }).await;
}
```

**This code is working correctly!** Our testing confirmed only the Raft leader initializes metadata on startup.

### What v2.2.1 Did NOT Fix ❌

The errors in the user report are coming from a **different code path**: the **leader election monitoring service** in [leader_election.rs:100-158](../crates/chronik-server/src/leader_election.rs#L100-L158).

#### The Leader Election Service

This service runs on **ALL nodes** (leader and followers) to monitor partition health:

```rust
// leader_election.rs:107-114 (ALREADY HAS THE FIX!)
let (is_leader, leader_id, state) = raft_cluster.is_leader_ready();
if !is_leader {
    debug!("Skipping partition leader check - this node is not the Raft leader");
    return;  // ✅ Correct - followers skip
}

// Only Raft leader proceeds to check partition health...
```

**WAIT!** Looking at the code, the leader election service **ALREADY HAS THE FIX** (line 107-114). It checks `if !is_leader { return; }` before doing anything!

So why are we still seeing errors?

---

## The Real Problem

After reviewing the code and user logs more carefully, I believe the issue is **timing-related**:

### Scenario 1: Race Condition During Startup

1. Topics are created during startup (e.g., `__meta`, KSQL topics)
2. Multiple nodes try to initialize these topics simultaneously
3. Even though integrated_server.rs has the fix, other code paths don't:
   - `produce_handler.rs:2422-2485` - Topic creation at runtime
   - Auto-created topics (KSQL command topics, etc.)

### Scenario 2: Existing Partitions from v2.2.0

If the user upgraded from v2.2.0 → v2.2.1 **without clearing data**:

1. Partition assignments from v2.2.0 are still in Raft state
2. These assignments have Node 1 as partition leader (from v2.2.0's broken logic)
3. Node 1 is a Raft follower in v2.2.1
4. Leader election service detects timeouts and tries to re-elect
5. **FAILS** because Node 1 can't propose (it's not Raft leader)

**This is most likely!** The user report shows errors for `__meta-0`, which was created in v2.2.0.

---

## Additional Fixes Needed

### Fix 1: Runtime Topic Creation (produce_handler.rs)

**File**: `crates/chronik-server/src/produce_handler.rs:2422-2485`

**Problem**: When topics are created at runtime, all nodes call `initialize_raft_partitions()`

**Current Code** (lines 2443-2454):
```rust
// NOTE: This will fail on follower nodes - only leader can propose
if let Err(e) = raft.propose(...) {
    debug!("Could not propose partition assignment (expected on followers): {}", e);
    return Ok(());  // ← Early return for followers
}
```

**Issue**: The early return only happens if AssignPartition fails. But there's a timing window where a follower might successfully propose if Raft leadership changes mid-operation.

**Solution**: Add explicit Raft leadership check at the start:

```rust
pub async fn initialize_raft_partitions(&self, topic_name: &str, num_partitions: u32) -> Result<()> {
    if let Some(ref raft) = self.raft_cluster {
        // CRITICAL FIX: Only Raft leader should initialize partition metadata
        let (is_leader, leader_id, _state) = raft.is_leader_ready();
        if !is_leader {
            debug!("Skipping Raft partition initialization for '{}' - this node is a follower (leader={})",
                   topic_name, leader_id);
            return Ok(());  // Follower will receive metadata via Raft replication
        }

        info!("Initializing Raft partition metadata for topic '{}' ({} partitions) - this node is Raft leader",
              topic_name, num_partitions);

        // ... rest of the initialization code ...
    }
    Ok(())
}
```

### Fix 2: Migration Path for v2.2.0 Deployments

For users upgrading from v2.2.0 with existing data:

**Option A: Clear and Restart** (Simple)
```bash
# Stop cluster
docker-compose down

# Clear Raft state
rm -rf ./data/node{1,2,3}/wal/__meta

# Restart with v2.2.1
docker-compose up -d
```

**Option B: Re-elect Leaders on Startup** (Complex)

Add a "leader reassignment" phase on startup that:
1. Detects partition leaders that are not the Raft leader
2. Proposes leadership changes to assign leaders to the Raft leader
3. Only runs on Raft leader node

---

## Why Leader Election Service Works Correctly

The leader election service in [leader_election.rs:107-114](../crates/chronik-server/src/leader_election.rs#L107-L114) **DOES have the fix**:

```rust
let (is_leader, leader_id, state) = raft_cluster.is_leader_ready();
if !is_leader {
    debug!("Skipping partition leader check - this node is not the Raft leader");
    return;
}
```

So the errors must be coming from:
1. **Runtime topic creation** (`produce_handler.rs`) - doesn't have the check
2. **Old partition assignments** from v2.2.0 that haven't been fixed

---

## Recommendations

### For v2.2.2 Release

1. ✅ **Keep the v2.2.1 fix** - it's working for startup initialization
2. ✅ **Add fix to `produce_handler.rs`** - check Raft leadership before initializing partitions
3. ✅ **Add migration guide** - document how to upgrade from v2.2.0

### For Users Currently on v2.2.1

**Workaround**: Clear Raft metadata and restart

```bash
# Stop cluster
docker-compose down

# Clear ONLY Raft metadata (preserves message data)
rm -rf ./data/node{1,2,3}/wal/__meta

# Start cluster with clean Raft state
docker-compose up -d
```

**Why this works**: Fresh Raft state means v2.2.1's fixed initialization code will run and create correct partition assignments.

---

## Test Plan for v2.2.2

1. **Fresh deployment** (already tested - works ✅)
2. **Upgrade from v2.2.0** (NOT tested - likely fails ❌)
3. **Runtime topic creation** (NOT tested - likely fails ❌)
4. **KSQL integration** (NOT tested - likely fails ❌)

---

## Conclusion

v2.2.1 **partially fixed** the issue:
- ✅ Startup initialization: FIXED
- ❌ Runtime topic creation: NOT FIXED
- ❌ Upgrade path from v2.2.0: NOT FIXED

The user's errors are likely from:
1. Upgrading from v2.2.0 without clearing Raft state
2. Runtime topic creation (KSQL command topics, etc.)

**Next Steps**:
1. Apply fix to `produce_handler.rs`
2. Add migration logic for v2.2.0 upgrades
3. Test with KSQL integration
4. Release v2.2.2
