# Raft Message Exchange Issue - Deep Analysis

**Date**: 2025-10-18
**Issue**: Node 1 (bootstrap leader) generates 0 messages despite having all peers in voter set
**TiKV Raft Version**: 0.7.x

## Current Observations

### What's Working
1. ✅ Cluster config loaded correctly from TOML
2. ✅ Peer addresses added to RaftClient BEFORE replica creation
3. ✅ gRPC connections established successfully to all peers
4. ✅ ConfState now includes all voters: `{1, 2, 3}`

### What's Broken
1. ❌ Node 1 generates **0 messages** in `ready().messages()`
2. ❌ Term increments rapidly (2→3→4→5...) indicating repeated failed elections
3. ❌ Nodes 2 & 3 timeout and become independent leaders
4. ❌ No RequestVote or AppendEntries messages sent between nodes

## Log Evidence

### Node 1 Logs
```
newRaft, raft_id: 1, peers: Configuration { incoming: Configuration { voters: {1, 2, 3} } }
ready() EXTRACTING for __meta-0: messages=0, entries=0, committed=0, hard_state=Some(HardState { term: 2, vote: 1, commit: 0 })
ready() EXTRACTING for __meta-0: messages=0, entries=0, committed=0, hard_state=Some(HardState { term: 3, vote: 1, commit: 0 })
ready() EXTRACTING for __meta-0: messages=0, entries=0, committed=0, hard_state=Some(HardState { term: 4, vote: 1, commit: 0 })
```

**Analysis**:
- Raft knows about voters {1, 2, 3}
- Node 1 is calling `campaign()` and voting for itself (vote: 1)
- Term increments on each failed election
- **BUT**: No messages generated to request votes from peers 2 & 3

## TiKV Raft v0.7 Constraints

### Key Constraint: Progress Tracker Initialization

In TiKV Raft, the `Progress` tracker for each peer is NOT automatically initialized just by including peers in the `ConfState`. The Progress tracker is what determines whether Raft will send messages to a peer.

**Source**: TiKV Raft internals - `RawNode::new()` does NOT populate the Progress map from the initial ConfState voters. It only creates Progress entries for peers that are added via `ConfChange` operations AFTER the node becomes leader.

### Correct Bootstrap Pattern for TiKV Raft 0.7

#### Option A: Single-Node Bootstrap + ConfChange (Standard)
```rust
// Step 1: ALL nodes start as single-voter clusters
let conf_state = ConfState::from((vec![self_node_id], vec![]));
storage.initialize_with_conf_state(conf_state);
let raw_node = RawNode::new(&config, storage, logger)?;

// Step 2: ONLY lowest node_id campaigns to become leader
if is_bootstrap_leader {
    raw_node.campaign()?;
}

// Step 3: After leader elected, propose ConfChange for each peer
if is_leader {
    for peer_id in other_peers {
        let cc = ConfChange {
            node_id: peer_id,
            change_type: ConfChangeType::AddNode,
            ...
        };
        raw_node.propose_conf_change(context, cc)?;
    }
}

// Step 4: Process ready() to send ConfChange via AppendEntries
let ready = raw_node.ready();
send_messages(ready.messages());  // Now messages will be generated!
```

**Why this works**:
- `propose_conf_change()` creates log entries
- Leader sends AppendEntries with ConfChange entries to followers
- When ConfChange is committed, Raft adds peer to Progress tracker
- **THEN** Raft starts sending heartbeats to that peer

#### Option B: Pre-populated Progress (Complex, Not Recommended)
Some Raft implementations allow pre-populating the Progress tracker, but TiKV Raft 0.7 does NOT expose this in the public API. It's internal-only.

## Why Current Implementation Fails

### Current Code (Broken)
```rust
// replica.rs:151
let conf_state = ConfState::from((all_node_ids.clone(), vec![]));  // voters: {1, 2, 3}
storage.initialize_with_conf_state(conf_state);
let mut raw_node = RawNode::new(&raft_config, storage, &tracing_logger())?;

// Node 1 campaigns
raw_node.campaign()?;  // Becomes leader, votes for self
```

**Problem**:
- ConfState has voters {1, 2, 3}
- But RawNode's Progress tracker is EMPTY for peers 2 & 3
- When leader tries to send RequestVote, it checks Progress tracker
- Peers 2 & 3 not in Progress → No messages sent
- Election fails, term increments, repeat

### Why Nodes 2 & 3 Also Fail
```rust
// Nodes 2 & 3 also initialize with voters {1, 2, 3}
// They don't campaign (only node 1 does)
// They wait for RequestVote from node 1
// Never receive it (node 1 sends 0 messages)
// Election timeout expires
// They call campaign() themselves
// Same problem: Progress tracker empty, send 0 messages
// All 3 nodes are independent single-node "leaders"
```

## Root Cause

**TiKV Raft 0.7 does NOT support static multi-node initialization via ConfState**

The `ConfState` is used for:
1. Snapshot restoration (when rejoining an existing cluster)
2. Displaying current configuration
3. **NOT** for bootstrap initialization of peer communication

For bootstrap, you MUST:
1. Start as single voter
2. Use ConfChange to add peers dynamically

## Correct Fix

### Implementation Plan

1. **Revert `replica.rs` to single-voter ConfState**
   ```rust
   let conf_state = ConfState::from((vec![config.node_id], vec![]));
   ```

2. **Bootstrap leader proposes ConfChange for each peer** (in `integrated_server.rs`)
   - Already implemented at lines 222-228
   - But timing is wrong - happens BEFORE leader election completes

3. **Fix timing issue**: Wait for leader election BEFORE proposing ConfChanges
   - Check `replica.is_leader()` in a loop with timeout
   - Only then call `add_peer_to_replica()`

4. **Ensure tick driver processes ConfChanges**
   - Already running (`raft_integration.rs:485`)
   - Should work once ConfChanges are proposed at right time

### Expected Behavior After Fix

```
Node 1: Initialize with voters: {1}
Node 1: campaign() → becomes leader of single-node cluster
Node 1: Check is_leader() → true
Node 1: propose_conf_change(AddNode 2)
Node 1: process ready() → generates AppendEntries with ConfChange
Node 1: send AppendEntries to peer 2 (now in Progress tracker!)
Node 2: receives AppendEntries, joins cluster as follower
Node 1: propose_conf_change(AddNode 3)
Node 1: send AppendEntries to peers 2 & 3
Node 3: receives AppendEntries, joins cluster as follower
Cluster: 3 nodes, 1 leader, 2 followers, fully connected
```

## Action Items

1. Revert `ConfState::from(all_node_ids)` to `ConfState::from(vec![config.node_id])`
2. Add leader readiness check in `integrated_server.rs` before ConfChange proposals
3. Test with 3-node cluster
4. Verify messages are generated and cluster forms correctly
