# Phase 2 Progress: Wire RaftCluster to ProduceHandler

## Date: 2025-11-01

## Summary

Successfully implemented Phase 2 of the Raft cluster integration, wiring `RaftCluster` to `ProduceHandler` for partition leadership checks. However, discovered a critical bug in the Raft message loop that causes crashes during leader election.

## Changes Implemented

### 1. Fixed Leadership Check in ProduceHandler

**File:** `crates/chronik-server/src/produce_handler.rs`

**Problem:** Code was checking for `self.raft_manager` which didn't exist. The correct field is `self.raft_cluster`.

**Fix** (lines 982-1011):
```rust
// Check leadership: RaftCluster takes precedence over metadata store
// v2.5.0 Phase 2: Use RaftCluster for partition leadership checks
let (is_leader, leader_hint) = {
    if let Some(ref raft_cluster) = self.raft_cluster {
        // Get partition leader from Raft metadata state machine
        let leader_id = raft_cluster.get_partition_leader(&topic_data.name, partition_data.index);

        if let Some(leader) = leader_id {
            // Partition is managed by Raft
            let is_raft_leader = leader == raft_cluster.node_id();

            debug!(
                "Raft leadership check for {}-{}: node_id={}, leader={}, is_leader={}",
                topic_data.name, partition_data.index, raft_cluster.node_id(), leader, is_raft_leader
            );

            (is_raft_leader, Some(leader))
        } else {
            // Partition not yet assigned in Raft, fall back to metadata store
            debug!(
                "Partition {}-{} not assigned in Raft, using metadata store for leadership",
                topic_data.name, partition_data.index
            );
            self.check_metadata_leadership(&topic_data.name, partition_data.index).await?
        }
    } else {
        // No Raft cluster, use metadata store (standalone mode)
        self.check_metadata_leadership(&topic_data.name, partition_data.index).await?
    }
};
```

**Status:** ✅ Complete and correct

### 2. Verified RaftCluster Wiring

**File:** `crates/chronik-server/src/integrated_server.rs`

**Found:** The wiring was already correct (lines 406-409):
```rust
// v2.5.0 Phase 3: Wire RaftCluster to ProduceHandler for partition metadata
if let Some(ref cluster) = raft_cluster {
    info!("Setting RaftCluster for ProduceHandler");
    produce_handler_inner.set_raft_cluster(Arc::clone(cluster));
}
```

**Status:** ✅ Already implemented correctly

## Critical Bug Discovered

### Raft Message Loop Panic

**Error:**
```
thread 'tokio-runtime-worker' panicked at /home/ubuntu/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/raft-0.7.0/src/raw_node.rs:674:13:
not leader but has new msg after advance, raft_id: 1
```

**Root Cause:** The Raft message loop in `crates/chronik-server/src/raft_cluster.rs` modifies the `Ready` struct by calling `ready.take_persisted_messages()` before passing it to `advance()`. This violates Raft's protocol.

**Location:** `crates/chronik-server/src/raft_cluster.rs:516-537`

**Current (Buggy) Code:**
```rust
// Step 6: Send persisted messages (AFTER persistence)
if !ready.persisted_messages().is_empty() {
    let persisted_msgs = ready.take_persisted_messages();  // ❌ Modifies Ready!
    // ... send messages ...
}

// Step 7: Advance Raft (marks Ready as processed)
{
    let mut raft = match self.raft_node.write() {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("Failed to acquire Raft lock for advance: {}", e);
            continue;
        }
    };
    raft.advance(ready);  // ❌ Passing modified Ready causes panic!
}
```

**Required Fix:**
According to Raft documentation and the TiKV Raft examples, we should NOT modify the Ready struct. Instead, we should:
1. Iterate over `ready.persisted_messages()` WITHOUT taking them
2. Clone messages before sending in spawned tasks
3. Pass the unmodified `ready` to `advance()`

**Correct Pattern:**
```rust
// Step 6: Send persisted messages (AFTER persistence)
// CRITICAL: Do NOT call take_persisted_messages() - just iterate!
for msg in ready.persisted_messages() {
    let peer_id = msg.to;
    let msg_type = msg.msg_type;
    let msg_clone = msg.clone();  // Clone for spawned task
    let self_clone = self.clone();

    tokio::spawn(async move {
        match self_clone.send_raft_message(peer_id, msg_clone).await {
            Ok(_) => { /* ... */ }
            Err(e) => { /* ... */ }
        }
    });
}

// Step 7: Advance Raft (with UNMODIFIED ready)
{
    let mut raft = self.raft_node.write().unwrap();
    raft.advance(ready);  // ✅ Unmodified Ready - no panic!
}
```

## Testing Status

### What Worked
- ✅ Code compiles successfully
- ✅ Leadership check logic is correct
- ✅ RaftCluster is properly wired to ProduceHandler

### What Failed
- ❌ 3-node cluster crashes during leader election due to Raft message loop bug
- ❌ Cannot test produce to leader/follower until cluster stability is fixed

## Next Steps

### Immediate (Critical)
1. **Fix Raft message loop** in `crates/chronik-server/src/raft_cluster.rs`:
   - Remove `ready.take_persisted_messages()` call
   - Clone messages instead of taking them
   - Pass unmodified `ready` to `advance()`
   - Apply same fix to `ready.take_messages()` for unpersisted messages

2. **Test cluster stability**:
   - Start 3-node cluster
   - Verify all nodes stay running for 2+ minutes
   - Verify leader election completes successfully
   - Check logs for "not leader but has new msg" panic

### Phase 2 Completion (After Fix)
3. **Test partition leadership**:
   - Create topic on leader node
   - Produce to leader (should succeed)
   - Produce to follower (should return NOT_LEADER_FOR_PARTITION)
   - Verify error includes correct leader_id hint

4. **Document results**:
   - Update NEXT_SESSION_PROMPT.md with completion status
   - Create integration test for leadership checks
   - Update PRODUCTION_READINESS_ANALYSIS.md

## Files Modified

- ✅ `crates/chronik-server/src/produce_handler.rs` (lines 982-1011)
- ⏳ `crates/chronik-server/src/raft_cluster.rs` (needs fix at lines 433-455, 516-537)

## Build Status

✅ **Builds successfully** with warnings (no errors)

```bash
cargo build --release --bin chronik-server
```

## Known Issues

1. **Raft message loop panic** - Must be fixed before any testing
2. **Message handling during leadership transition** - The panic occurs when Node 1 becomes a follower but still has messages to send
3. **Ready struct mutation** - The TiKV Raft library does NOT allow modifying Ready before advance()

## References

- TiKV Raft single_mem_node example: https://github.com/tikv/raft-rs/blob/master/examples/single_mem_node/main.rs
- Raft Ready handling: https://docs.rs/raft/latest/raft/raw_node/struct.Ready.html
- Previous fix commit: "feat(v2.5.0): Fix 4 critical Raft cluster bugs"

---

**Session Status:** Phase 2 implementation complete, but blocked by Raft message loop bug that needs immediate fix.
