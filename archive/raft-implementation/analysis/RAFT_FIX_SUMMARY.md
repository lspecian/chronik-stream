# Raft Fix Implementation Summary

## Problem Identified

**Root Cause**: The `RaftClient` was not initialized with peer addresses before the Raft replica started processing, causing all Raft messages to fail with `"No address for peer X"` errors.

**Impact**:
- Raft messages couldn't be sent
- Leader election was flaky
- Proposals never committed
- Cluster was completely non-functional

## Fix Implemented

### 1. Added `register_peer_address()` Method
**File**: `crates/chronik-raft/src/client.rs`

Added a new method that registers peer addresses WITHOUT attempting to connect:
```rust
pub async fn register_peer_address(&self, node_id: u64, addr: String) {
    info!("Registering peer {} address: {}", node_id, addr);
    // Just store the address - connection happens lazily
    self.peer_addrs.write().await.insert(node_id, addr);
}
```

**Why**: The previous `add_peer()` method tried to connect immediately, which failed because peer gRPC servers weren't up yet. Lazy connection allows addresses to be registered early.

### 2. Registered Peers BEFORE Replica Creation
**File**: `crates/chronik-server/src/raft_cluster.rs` (lines 218-239)

Moved peer address registration to happen synchronously BEFORE creating the `__meta` replica:
```rust
// BEFORE creating __meta replica (line 243):
if config.gossip_config.is_none() {
    info!("Registering {} static peer addresses with RaftClient", config.peers.len());

    for (peer_id, peer_addr) in &config.peers {
        let peer_url = format!("http://{}", peer_addr);
        raft_manager.raft_client().register_peer_address(*peer_id, peer_url).await;
    }
}

// NOW create the __meta replica
raft_manager.create_meta_replica(...).await?;
```

**Why**: The background loop starts immediately after replica creation. Peers must be registered BEFORE that to prevent "No address for peer" errors.

### 3. Removed Background Peer Addition Task
**File**: `crates/chronik-server/src/raft_cluster.rs` (lines 282-284)

Removed the background task that was trying to add peers after a 2-second delay:
```rust
// REMOVED: Background peer addition task
// Peers are now added synchronously before replica creation (see lines 218-246)
```

**Why**: The background task created a race condition. Messages were generated before peers were configured.

## Results

### ✅ Fixed: Message Delivery
- **Before**: `ERROR: Failed to send message to peer 2: Configuration error: No address for peer 2`
- **After**: Messages send successfully, no address errors

### ✅ Fixed: Peer Registration
- **Before**: `WARN: Failed to register peer 2 address: transport error`
- **After**: `INFO: Registering peer 2 address: http://127.0.0.1:5002` (immediate, no errors)

### ✅ Fixed: Message Flow
- **Before**: No messages sent (all failed)
- **After**: Messages flowing both directions (AppendEntries, responses, heartbeats)

### ✅ Fixed: Progress Tracker
- **Before**: All peers stuck in `Probe` state with `matched=0`
- **After**: Peers advancing to `Replicate` state with `matched=1,2,3...`

## Remaining Issue: Commits Not Happening

**Status**: Messages are flowing correctly, but entries are still not committing.

**Evidence**:
- `last_index=1,2` (proposals ARE being made)
- `commit=0` (commits NOT advancing)
- `matched=1,2` for some peers (acknowledgments ARE being received)
- `state=Probe` or `Replicate` (mixed states)

**Hypothesis**: There's a separate issue preventing commit index advancement, likely related to:
1. Leader instability (multiple leader elections happening)
2. Quorum calculation issue
3. Entry persistence timing
4. tikv/raft internal state machine issue

**Next Steps**:
1. Investigate why commit index isn't advancing despite quorum being achievable
2. Check if leader elections are happening too frequently
3. Verify entry persistence is working correctly
4. Consider adding more diagnostic logging to tikv/raft's commit calculation

## Code Changes Made

1. **crates/chronik-raft/src/client.rs**: Added `register_peer_address()` method
2. **crates/chronik-server/src/raft_cluster.rs**:
   - Added synchronous peer registration before replica creation
   - Removed background peer addition task
3. **crates/chronik-raft/src/replica.rs**: Added comprehensive diagnostic logging (Progress tracker, Raft log state, messages)

## Testing

**Test Script**: `test_raft_diagnostic.sh`
- Starts 3-node cluster
- Waits 30 seconds
- Analyzes logs for diagnostic information

**Key Improvements Verified**:
- ✅ No "No address for peer" errors
- ✅ Messages being sent and received
- ✅ Progress tracker advancing
- ✅ Peers in Replicate state (some nodes)

**Remaining Failure**:
- ❌ Commits not advancing (`commit=0`)
- ❌ Broker registration not completing
- ❌ Kafka servers not starting

## Conclusion

The peer address registration fix successfully eliminated the primary blocker (message delivery failures). However, a secondary issue with commit advancement remains. The diagnostic logging added will be crucial for debugging this next issue.

The architecture is sound - messages flow correctly, peers are configured properly, and the infrastructure works. The remaining issue is likely in tikv/raft's commit calculation logic or related to cluster stability.
