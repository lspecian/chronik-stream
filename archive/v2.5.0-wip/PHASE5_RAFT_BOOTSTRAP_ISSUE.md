# Phase 5: Raft Bootstrap Issue - Empty Voter Configuration

**Date**: 2025-11-01
**Status**: ROOT CAUSE IDENTIFIED
**Priority**: CRITICAL - Blocks all leader election functionality

---

## Problem Summary

**Raft clusters are not electing leaders because the voter configuration is empty.**

Evidence from logs:
```
INFO switched to configuration, config: Configuration { voters: Configuration { incoming: Configuration { voters: {} }, outgoing: Configuration { voters: {} } }
INFO newRaft, peers: Configuration { incoming: Configuration { voters: {} }, outgoing: Configuration { voters: {} } }
```

After 30+ seconds of waiting:
```
WARN Raft leader election did not complete within 30 seconds - metadata proposals may fail
INFO no leader at term 0; dropping proposal
```

---

## Root Cause Analysis

### Current Bootstrap Code

**File**: `crates/chronik-server/src/raft_cluster.rs` (lines 49-83)

```rust
pub async fn bootstrap(node_id: u64, peers: Vec<(u64, String)>) -> Result<Self> {
    info!("Bootstrapping Raft cluster: node_id={}, peers={:?}", node_id, peers);

    // Create Raft configuration
    let config = Config {
        id: node_id,
        election_tick: 10,
        heartbeat_tick: 3,
        max_size_per_msg: 1024 * 1024,
        max_inflight_msgs: 256,
        ..Default::default()
    };

    // Create storage
    let storage = MemStorage::new();

    // Create Raft node
    let raft_node = RawNode::new(&config, storage, &raft::default_logger())?;

    // ❌ PROBLEM: Never calls raft_node.add_peer() or similar!
    // The peers parameter is received but NOT used to configure Raft voters

    Ok(Self {
        node_id,
        state_machine,
        raft_node: Arc::new(RwLock::new(raft_node)),
    })
}
```

### What's Wrong

1. **Peers are passed but ignored** - The `peers` parameter is logged but never used
2. **No voter configuration** - `RawNode::new()` creates an empty Raft cluster
3. **No bootstrap process** - We don't call `bootstrap()` on the Raft node to init the cluster
4. **No leader election** - With zero voters, Raft can't elect a leader

### Why This Wasn't Caught Earlier

- Raft message loop runs fine (it's just ticking an empty cluster)
- Server starts successfully (no crashes)
- Only fails when trying to use Raft (proposals, metadata, etc.)

---

## Solution: Proper Raft Bootstrap

### Option 1: Single-Node Bootstrap (Recommended for Testing)

For development/testing with a single node that should immediately become leader:

```rust
pub async fn bootstrap(node_id: u64, peers: Vec<(u64, String)>) -> Result<Self> {
    info!("Bootstrapping Raft cluster: node_id={}, peers={:?}", node_id, peers);

    // Create Raft configuration
    let config = Config {
        id: node_id,
        election_tick: 10,
        heartbeat_tick: 3,
        max_size_per_msg: 1024 * 1024,
        max_inflight_msgs: 256,
        ..Default::default()
    };

    // Create storage
    let mut storage = MemStorage::new();

    // ✅ FIX: Bootstrap the Raft cluster with all node IDs
    let mut all_nodes = vec![node_id];
    for (peer_id, _) in &peers {
        all_nodes.push(*peer_id);
    }

    // Create initial Raft configuration with all voters
    storage.initialize_with_conf_state(ConfState {
        voters: all_nodes.clone(),
        ..Default::default()
    })?;

    // Create Raft node with initialized storage
    let raft_node = RawNode::new(&config, storage, &raft::default_logger())?;

    info!("✓ Raft cluster bootstrapped with voters: {:?}", all_nodes);

    Ok(Self {
        node_id,
        state_machine: Arc::new(RwLock::new(MetadataStateMachine::new())),
        raft_node: Arc::new(RwLock::new(raft_node)),
    })
}
```

### Option 2: Multi-Step Bootstrap (Production)

For production multi-node clusters:

1. **Leader node** calls `raft.bootstrap()` with all node IDs
2. **Follower nodes** connect and join via `add_node` messages
3. Requires network communication between nodes

**Current limitation**: We don't have network messaging implemented yet (message loop just logs messages).

---

## Testing Plan

### Test 1: Single Node Bootstrap

**Setup**: Start 1 node with empty peers list

**Expected**:
```
INFO Raft cluster bootstrapped with voters: [1]
INFO ✓ This node is Raft leader (state=Leader)
INFO ✓ Applied 3 committed entries to state machine
```

**Command**:
```bash
./target/release/chronik-server \
  --node-id 1 \
  --kafka-port 9092 \
  raft-cluster \
  --raft-addr 0.0.0.0:9192 \
  --peers ""  # Empty peers for single-node
  --bootstrap
```

### Test 2: Three Node Bootstrap (Without Network)

**Setup**: Start 3 nodes, all with same voter list

**Expected**:
- All 3 nodes bootstrap with voters=[1,2,3]
- One node becomes leader (likely node 1)
- Leader can accept proposals
- Metadata gets committed

**Note**: Without network messaging, followers won't sync. But at least proposals will work on the leader.

### Test 3: Chaos Test After Fix

**Expected**:
```
INFO ✓ This node is Raft leader (state=Leader), proceeding with metadata initialization
INFO ✓ Proposed Raft metadata for chaos-test-0: replicas=[1, 2, 3], leader=1, ISR=[1, 2, 3]
INFO ✓ Applied 3 committed entries to state machine
```

---

## Implementation Steps

### Step 1: Fix RaftCluster::bootstrap() (30 minutes)

**File**: `crates/chronik-server/src/raft_cluster.rs`

Add voter initialization before creating RawNode:

```rust
// Build list of all nodes (self + peers)
let mut all_nodes = vec![node_id];
for (peer_id, _) in &peers {
    all_nodes.push(*peer_id);
}

// Initialize storage with voter configuration
let mut storage = MemStorage::new();
storage.initialize_with_conf_state(ConfState {
    voters: all_nodes.clone(),
    ..Default::default()
})?;

info!("✓ Initializing Raft with voters: {:?}", all_nodes);
```

### Step 2: Test Single Node (15 minutes)

Start single node, verify:
- Leader elected within 1 second
- Metadata proposals succeed
- Committed entries logged

### Step 3: Test Three Nodes (15 minutes)

Start 3 nodes, verify:
- At least one becomes leader
- Leader can accept proposals
- Metadata initialization succeeds

### Step 4: Re-run Chaos Test (30 minutes)

Full chaos test should now show:
- Metadata proposals succeed
- ISR data available
- Leader election can work (if we implement failover logic)

---

## Timeline

| Task | Duration | Deliverable |
|------|----------|-------------|
| Fix bootstrap | 30 min | Voters configured |
| Test single node | 15 min | Leader elected |
| Test multi-node | 15 min | Cluster forms |
| Chaos test | 30 min | End-to-end working |
| **TOTAL** | **90 minutes** | **Raft fully functional** |

---

## Expected Outcomes

### Before Fix
- ❌ No voters configured
- ❌ No leader election
- ❌ All proposals fail
- ❌ Metadata never commits
- ❌ Leader election blocked

### After Fix
- ✅ Voters configured: [1, 2, 3]
- ✅ Leader elected within 1s
- ✅ Proposals succeed
- ✅ Metadata commits
- ✅ ISR data available
- ✅ Leader election unblocked

---

## Additional Considerations

### Network Messaging

Currently the message loop logs messages but doesn't send them:

```rust
// Send messages to peers
for msg in ready.messages() {
    // TODO: Send message to peer via network
    tracing::debug!("Message to send to peer {}: {:?}", msg.to, msg.msg_type);
}
```

**For single-node testing**: Not needed, leader can commit immediately.

**For multi-node production**: Will need to implement network transport (gRPC, TCP, etc.).

### Failover Logic

Even with Raft metadata working, we'll need to implement:
1. Detecting partition leader failures
2. Proposing new leader to Raft
3. Redirecting clients to new leader

This is Phase 5's original goal, currently blocked on Raft bootstrap.

---

## Conclusion

**Root cause**: Raft voter configuration is empty, preventing leader election.

**Fix**: Initialize storage with voter list before creating RawNode.

**Impact**: Unlocks all Raft functionality - proposals, commits, metadata, leader election.

**Time**: ~90 minutes to fully working Raft cluster.

**Next step**: Implement the bootstrap fix in `raft_cluster.rs`.
