# Raft Root Cause FOUND: Peer Addresses Not Registered

## Executive Summary

**ROOT CAUSE IDENTIFIED**: The `RaftClient` is not being initialized with peer addresses, causing all Raft message sends to fail with "No address for peer X" errors.

## Diagnostic Evidence

### 1. Symptoms Observed

✅ **Leader election happens** (nodes become leaders)
❌ **Messages fail to send** ("No address for peer X")
❌ **Proposals never commit** (`last_index=0`, `commit=0`)
❌ **Progress tracker stuck in Probe state** (no acknowledgments)

### 2. Critical Log Evidence

**Error from `/tmp/raft-node1.log`:**
```
ERROR chronik_server::raft_integration: Failed to send message to peer 2: Configuration error: No address for peer 2
ERROR chronik_server::raft_integration: Failed to send message to peer 3: Configuration error: No address for peer 3
```

**Error from `/tmp/raft-node2.log`:**
```
WARN chronik_server::integrated_server: Failed to register broker 2 (attempt 1/30): StorageError("Forward to leader failed: Configuration error: No address for peer 0"), retrying in 2s...
```

**Progress Tracker (Node 2 - Leader):**
```
Peer 1: matched=1, next_idx=1, state=Probe, paused=false, pending_snapshot=0, recent_active=true
Peer 2: matched=0, next_idx=1, state=Replicate, paused=false, pending_snapshot=0, recent_active=false
Peer 3: matched=1, next_idx=1, state=Probe, paused=false, pending_snapshot=0, recent_active=true
```

- **Probe state** means followers are not acknowledging (because messages never arrive!)
- **matched=1** should advance, but stays at 1 (initial value)

**Raft Log State (Node 2 - Leader):**
```
Raft log for __meta-0: last_index=0, committed=0, applied=0, unstable_entries=0
```

- **last_index=0** means NO proposals have been made to the log
- This contradicts the "Successfully registered broker 2" message
- The registration "succeeded" locally but was never replicated

### 3. Message Flow Analysis

**Outgoing Messages Generated** (Node 2 - Leader):
```
type=8 (MsgHeartbeat), from=2, to=1, term=16
type=8 (MsgHeartbeat), from=2, to=3, term=16
```

- tikv/raft IS generating messages correctly
- Heartbeats are being sent

**Messages Received** (Node 3 - Follower):
```
Received Raft message for __meta-0: type=8, from=2, to=3, term=16, log_term=0, index=0, commit=0, entries=0, reject=false
```

- Node 3 receives SOME messages (occasionally)
- This explains why Progress shows `recent_active=true` for some peers

**Send Failures**:
```
Failed to send message to peer 2: Configuration error: No address for peer 2
```

- Most messages fail to send
- Only occasionally do messages get through (race condition?)

## Root Cause Analysis

### Where Peer Addresses Should Be Added

In `raft_cluster.rs`, peers are added to the `RaftClient` in a background task (lines 259-295):

```rust
if config.gossip_config.is_none() {
    let raft_manager_for_peers = raft_manager.clone();
    let peers_to_add = config.peers;
    tokio::spawn(async move {
        // CRITICAL: Wait 2 seconds before connecting to peers
        // Without this, all nodes try to connect immediately before servers are ready
        tokio::time::sleep(Duration::from_secs(2)).await;
        info!("Starting peer connections after initial startup delay");

        for (peer_id, meta) in peer_metadata {
            // ... retry loop ...
            match raft_manager_for_peers.add_peer(peer_id, peer_url.clone()).await {
                Ok(_) => { info!("Added static peer {} to Raft client", peer_id); break; }
                Err(e) => { /* retry */ }
            }
        }
    });
}
```

### The Problem

1. **Race Condition**: The peer addition happens in a background task with a 2-second delay
2. **Replica Created Before Peers Added**: The `__meta` replica is created at line 237-243, BEFORE the peer addition task starts
3. **RaftReplicaManager Background Loop Starts Immediately**: As soon as the replica is created, the background loop starts ticking and trying to send messages
4. **Messages Generated Before Peers Configured**: tikv/raft generates messages, but RaftClient has no peer addresses yet
5. **All Sends Fail**: Every `client.send_message()` call fails with "No address for peer X"

### Timeline

```
TIME 0: raft_cluster.rs creates __meta replica (line 237)
TIME 0+1ms: RaftReplicaManager starts background loop for __meta
TIME 0+2ms: tikv/raft generates RequestVote messages
TIME 0+3ms: RaftReplicaManager tries to send messages
TIME 0+4ms: ERROR: No address for peer 1/2/3 (RaftClient is empty!)

TIME 2000ms: Background task wakes up and starts adding peers
TIME 2100ms: First peer address added to RaftClient
TIME 2200ms: Second peer address added
TIME 2300ms: Third peer address added

TIME 2301ms: Messages can now be sent!
```

**But by this time:**
- Proposals have already been attempted and failed
- Leader election may have succeeded by luck (if a few messages got through)
- Cluster is in an inconsistent state

## The Fix

### Option 1: Add Peers BEFORE Creating Replica (Recommended)

Move the peer addition logic to happen synchronously BEFORE creating the `__meta` replica:

```rust
// In raft_cluster.rs, BEFORE line 237:

// Add all peers to RaftClient synchronously
for (peer_id, peer_addr) in &config.peers {
    let peer_url = if peer_addr.starts_with("http://") || peer_addr.starts_with("https://") {
        peer_addr.clone()
    } else {
        format!("http://{}", peer_addr)
    };

    info!("Adding peer {} ({}) to Raft client", peer_id, peer_url);
    raft_manager.add_peer(*peer_id, peer_url).await?;
}

info!("All {} peers added to Raft client, now creating __meta replica", config.peers.len());

// NOW create the __meta replica (line 237-243)
raft_manager.create_meta_replica(...).await?;
```

### Option 2: Delay Background Loop Start

Modify `RaftReplicaManager::start_replica_processing` to wait until peers are configured:

```rust
// Add a flag to RaftReplicaManager indicating peers are ready
let peers_ready = Arc::new(AtomicBool::new(false));

// In background task, check flag before sending messages
tokio::spawn(async move {
    loop {
        ticker.tick().await;
        replica.tick()?;
        let (messages, committed) = replica.ready().await?;

        // Wait until peers are configured
        if !peers_ready.load(Ordering::SeqCst) {
            debug!("Peers not ready yet, skipping message send");
            continue;
        }

        // Send messages...
    }
});
```

### Option 3: Make add_peer Non-Blocking

Change `add_peer` to not fail if the peer is not yet reachable:

```rust
// In RaftClient::add_peer, don't try to connect immediately
// Just store the address
self.peers.write().unwrap().insert(peer_id, peer_url);
info!("Peer {} address registered: {}", peer_id, peer_url);
// Connection will happen lazily when first message is sent
```

## Recommended Fix: Option 1

**Why Option 1 is Best:**
- ✅ Simple and straightforward
- ✅ No race conditions (peers added before replica starts)
- ✅ No need for complex synchronization primitives
- ✅ Fail-fast if peers are unreachable (better than silent failures)
- ✅ Matches the expected initialization order

**Implementation:**
1. Move peer addition code from background task to synchronous code
2. Add peers to RaftClient BEFORE calling `create_meta_replica`
3. Remove the 2-second sleep (no longer needed)
4. Keep retry logic for robustness

## Next Step

Implement Option 1 fix in `raft_cluster.rs`.
