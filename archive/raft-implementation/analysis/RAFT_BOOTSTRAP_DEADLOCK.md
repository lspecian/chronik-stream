# TiKV Raft Bootstrap Deadlock Analysis

**Date**: 2025-10-18
**Critical Finding**: Single-node + ConfChange bootstrap pattern CANNOT work with TiKV Raft 0.7

## The Fundamental Problem

### ConfChange Bootstrap Deadlock

**What We Tried**:
1. Node 1 starts as single-voter cluster (`voters: {1}`)
2. Node 1 calls `campaign()` → becomes leader of its single-node cluster
3. Node 1 proposes `ConfChange::AddNode(2)` and `ConfChange::AddNode(3)`
4. Raft creates log entries for these ConfChanges (`entries=2`)
5. Node 1 calls `ready()` to get messages to send...
6. **BUT**: `ready().messages() = []` (empty!)

**Why No Messages?**:
- TiKV Raft's `Progress` tracker determines which peers to send messages to
- Progress tracker is empty for peers 2 & 3 (they're not in the cluster yet)
- ConfChange entries are created but **cannot be replicated** to non-existent peers
- Leader can't send AppendEntries to peers not in Progress tracker
- ConfChange can't be committed without replication
- Peers can't be added to Progress tracker without committed ConfChange
- **DEADLOCK**: Chicken-and-egg problem!

### Log Evidence

```
Node 1:
  newRaft, voters: {1}  // Single voter
  campaign() → became leader
  propose_conf_change(AddNode 2)
  propose_conf_change(AddNode 3)
  ready() EXTRACTING: messages=0, entries=2  // Entries exist, no messages!

Node 2 & 3:
  newRaft, voters: {2}  // Independent single-node clusters
  timeout waiting for messages from node 1
  campaign() → became independent leaders
```

## Why This Pattern Doesn't Work

### TiKV Raft's Progress Tracker Design

The `Progress` struct tracks replication state for each peer:
- `match_index`: Last replicated log index
- `next_index`: Next log index to send
- `state`: Probe/Replicate/Snapshot

**Key Constraint**: Progress entries are ONLY created when:
1. ConfChange is **committed** (requires majority vote)
2. Snapshot is installed (for rejoining nodes)

**NOT created when**:
- Peer is in initial ConfState
- ConfChange is **proposed** but not committed

### The Bootstrap Scenario

```rust
// Node 1 (leader of single-node cluster)
Progress tracker: {
    // Empty! No peers!
}

// Try to send ConfChange:
for peer_id in ready().messages() {
    // Progress tracker doesn't contain peer_id
    // No messages generated
}

// Result: ConfChange stuck in log, never replicated
```

## Correct Patterns for TiKV Raft 0.7

### Pattern 1: Static Peer Configuration (Recommended)

**ALL nodes must start with knowledge of ALL peers:**

```rust
// Node 1, 2, AND 3 all initialize with:
let all_peers = vec![1, 2, 3];
let conf_state = ConfState::from((all_peers, vec![]));
storage.initialize_with_conf_state(conf_state);
let raw_node = RawNode::new(&config, storage, logger)?;

// Don't call campaign() on any node initially!
// Let pre-vote algorithm elect a leader naturally
```

**How it works**:
1. ALL nodes know about each other from ConfState
2. TiKV Raft populates Progress tracker from ConfState
3. Pre-vote messages exchanged between all peers
4. Majority agrees on a leader
5. Leader starts sending heartbeats
6. Cluster operational

**Requirements**:
- ALL nodes must be configured identically
- ALL nodes must start within election timeout window
- Network connectivity between all peers

### Pattern 2: External Cluster Coordinator (Complex)

Use an external service (like etcd) for peer discovery:
1. All nodes register with coordinator
2. Coordinator waits for quorum
3. Coordinator tells all nodes the full peer list
4. Nodes initialize with full peer list (Pattern 1)

### Pattern 3: Add Learners First (Doesn't Solve Bootstrap)

Some suggest using `AddLearnerNode` first, but this has the same problem:
- Learners must be added via ConfChange
- ConfChange can't be sent without existing peers in Progress tracker
- Same deadlock!

## What We Need to Fix

### Current (Broken) Code

```rust
// replica.rs:168
let conf_state = ConfState::from((vec![config.node_id], vec![]));  // Single voter
```

### Correct Fix

```rust
// replica.rs:168
// For NEW cluster bootstrap, include ALL peers
let conf_state = if is_new_cluster_bootstrap {
    ConfState::from((all_peer_ids, vec![]))  // All voters
} else {
    ConfState::from((vec![config.node_id], vec![]))  // Rejoining existing cluster
};
```

### How to Detect New Cluster Bootstrap

**Option A**: Check if local storage is empty
```rust
let is_new_bootstrap = storage.initial_state()?.hard_state.commit == 0;
```

**Option B**: Explicit flag in config
```rust
pub struct RaftConfig {
    pub bootstrap_new_cluster: bool,  // true for initial bootstrap
}
```

### Updated Bootstrap Flow

```
1. ALL nodes start with peers: {1, 2, 3} in ConfState
2. TiKV Raft populates Progress tracker from ConfState
3. Pre-vote phase begins (no leader yet)
4. Nodes exchange RequestPreVote messages
5. Node with lowest ID (or random) wins pre-vote
6. Winner calls campaign() for real election
7. Winner sends RequestVote to all peers
8. Majority votes for winner
9. Winner becomes leader, sends heartbeats
10. Cluster fully operational
```

## Action Items

1. **Modify `replica.rs`**: Initialize with all peers for new cluster bootstrap
2. **Add bootstrap detection**: Distinguish new cluster vs. rejoining
3. **Remove ConfChange-based bootstrap**: Delete the broken auto-add logic
4. **Update integration tests**: Test all-nodes-start-together scenario
5. **Document**: Add clear guidance on bootstrap requirements

## References

- TiKV Raft docs: https://github.com/tikv/raft-rs
- Raft paper: https://raft.github.io/raft.pdf (Section 6: Cluster membership changes)
- etcd bootstrap: https://etcd.io/docs/latest/op-guide/clustering/
