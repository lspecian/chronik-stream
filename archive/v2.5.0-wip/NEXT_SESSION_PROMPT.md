# Next Session: Implement Raft Network Messaging for Multi-Node Leader Election

## Current Status (2025-11-01)

### What's Working ✅
1. **Raft voter configuration** - Nodes initialize with voters=[1, 2, 3] (VERIFIED)
2. **Raft election process** - Nodes become candidates and cycle through election attempts (VERIFIED)
3. **Raft message loop** - Ticks every 100ms, processes Ready states, applies committed entries (VERIFIED)
4. **Metadata initialization** - Both startup-time and topic-creation-time hooks working (VERIFIED)
5. **Leader election wait** - Server waits 30s for leader before proposing metadata (VERIFIED)
6. **Data persistence** - 150 messages consumed across multiple test runs (VERIFIED)

### What's Blocked ❌
**Raft network messaging** - The critical missing piece that blocks everything:

**Current code** ([raft_cluster.rs:295-303](crates/chronik-server/src/raft_cluster.rs#L295-L303)):
```rust
// Send messages to peers
for msg in ready.messages() {
    // TODO: Send message to peer via network
    // For now, just log for single-node testing
    tracing::debug!(
        "Message to send to peer {}: {:?}",
        msg.to,
        msg.msg_type
    );
}
```

**Result**: Messages are logged but NOT sent, so:
- ❌ No leader can be elected (nodes can't communicate votes)
- ❌ Metadata proposals fail ("no leader at term 0")
- ❌ No committed entries (no leader to commit)
- ❌ No ISR data (blocked on metadata commits)
- ❌ Leader election failover doesn't work (blocked on ISR data)

**Evidence from logs**:
```
INFO became candidate at term 59
INFO became candidate at term 60
INFO became candidate at term 61
[repeats forever - no leader elected]
```

---

## Task for Next Session

**Implement Raft network messaging between nodes to enable multi-node leader election.**

### What Needs to Be Done

#### 1. Choose Transport Mechanism
**Options**:
- **gRPC** (recommended for Raft) - Type-safe, bidirectional streaming, standard for distributed systems
- **TCP** (simpler) - Direct socket connections, manual message framing
- **HTTP** (simplest) - REST-style, but less efficient for streaming

**Recommendation**: Start with **TCP + bincode** (simplest to implement quickly)

#### 2. Implement Message Sending

**File**: `crates/chronik-server/src/raft_cluster.rs`

**Current TODO at line 295-303**:
```rust
// Send messages to peers
for msg in ready.messages() {
    // TODO: Send message to peer via network
    tracing::debug!("Message to send to peer {}: {:?}", msg.to, msg.msg_type);
}
```

**What needs to happen**:
```rust
// Send messages to peers
for msg in ready.messages() {
    let peer_id = msg.to;

    // Get peer address
    if let Some(peer_addr) = self.get_peer_address(peer_id) {
        // Serialize Raft message
        let serialized = bincode::serialize(&msg)?;

        // Send asynchronously (don't block Raft loop)
        let sender = self.get_or_create_peer_connection(peer_addr).await;
        sender.send(serialized).await;
    }
}
```

#### 3. Implement Message Receiving

**New component needed**: Background task per node to listen for incoming Raft messages

```rust
pub async fn start_message_receiver(self: Arc<Self>, listen_addr: &str) {
    let listener = TcpListener::bind(listen_addr).await?;

    tokio::spawn(async move {
        loop {
            let (socket, _) = listener.accept().await?;
            let cluster = self.clone();

            tokio::spawn(async move {
                // Read message from socket
                let msg: Message = read_raft_message(socket).await?;

                // Feed into Raft
                let mut raft = cluster.raft_node.write().unwrap();
                raft.step(msg)?;
            });
        }
    });
}
```

#### 4. Wire to Server Startup

**File**: `crates/chronik-server/src/raft_cluster.rs` (in `run_raft_cluster` function)

**Current code around line 369**:
```rust
// Step 1.5: Start Raft message processing loop (v2.5.0 Phase 5 fix)
raft_cluster.clone().start_message_loop();
```

**Add after message loop start**:
```rust
// Step 1.6: Start Raft network message receiver
raft_cluster.clone().start_message_receiver(&config.raft_addr).await?;
tracing::info!("✓ Raft network messaging started");
```

---

## Expected Outcomes After Implementation

### Logs Should Show
```
INFO Initializing Raft cluster with voters: [1, 2, 3]
INFO Starting Raft message receiver on 0.0.0.0:9192
INFO Starting Raft message processing loop
INFO became candidate at term 1
INFO received RequestVote from peer 1
INFO sending RequestVoteResponse to peer 1
INFO became leader at term 1
INFO ✓ This node is Raft leader, proceeding with metadata initialization
INFO ✓ Proposed Raft metadata for chaos-test-0: replicas=[1, 2, 3], leader=1, ISR=[1, 2, 3]
INFO ✓ Applied 3 committed entries to state machine
```

### Chaos Test Should Show
```
[3/7] Producing 50 messages to Node 1...
    ✅ 50 messages produced

[5/7] Killing Node 1...
    ✅ Node 1 killed

[6/7] Leader election evidence...
    ✅ Node 2: Elected as new leader
    ✅ Raft metadata updated: leader=2

[7/7] Producing to Node 2...
    ✅ 50/50 messages produced ← SUCCESS!

Total messages: 100 (zero data loss!)
```

---

## Implementation Checklist

**Phase 1: Basic TCP Messaging (2-3 hours)**
- [ ] Add peer address storage to RaftCluster (HashMap<u64, String>)
- [ ] Implement TCP message sender (send_raft_message)
- [ ] Implement TCP message receiver (start_message_receiver)
- [ ] Add message serialization (bincode)
- [ ] Wire sender to message loop (replace TODO)
- [ ] Wire receiver to server startup
- [ ] Test: 3 nodes should elect a leader

**Phase 2: Connection Management (1-2 hours)**
- [ ] Add connection pool (avoid reconnecting per message)
- [ ] Add reconnection logic (handle peer failures)
- [ ] Add message buffering (if peer temporarily down)
- [ ] Test: Leader election survives temporary disconnects

**Phase 3: Metadata Proposals (30 minutes)**
- [ ] Verify leader elected within 5 seconds
- [ ] Verify metadata proposals succeed
- [ ] Verify committed entries logged
- [ ] Test: ISR data becomes available

**Phase 4: Chaos Test (30 minutes)**
- [ ] Run full chaos test
- [ ] Verify leader failover works
- [ ] Verify 100 messages consumed (zero data loss)
- [ ] Verify produces succeed after failover

---

## Key Files to Modify

| File | Lines | What to Add |
|------|-------|-------------|
| `raft_cluster.rs` | ~150 | Network sender/receiver |
| `raft_cluster.rs` | 295-303 | Replace TODO with actual send |
| `raft_cluster.rs` | +50 | Peer connection management |
| `Cargo.toml` | +2 | Add tokio net features if needed |

---

## Testing Strategy

### Test 1: Single Message
**Goal**: Verify one node can send/receive one Raft message
**Setup**: 2 nodes, node 1 sends RequestVote to node 2
**Expected**: Node 2 receives and logs the message

### Test 2: Leader Election
**Goal**: Verify 3 nodes elect a leader
**Setup**: 3 nodes with network messaging
**Expected**: One becomes leader within 5 seconds

### Test 3: Metadata Proposals
**Goal**: Verify leader can commit metadata
**Setup**: Start cluster, create topic
**Expected**: Metadata proposals succeed, entries committed

### Test 4: Full Chaos Test
**Goal**: End-to-end failover
**Setup**: 3-node cluster, kill leader
**Expected**: New leader elected, produces succeed

---

## Critical Context

### Why This Matters
Without network messaging:
- Raft nodes can't communicate
- No leader can be elected
- All proposals fail
- Entire Phase 5 is blocked

With network messaging:
- ✅ Nodes elect leader in 1-5 seconds
- ✅ Metadata proposals succeed
- ✅ ISR data available
- ✅ Leader election works
- ✅ Failover works
- ✅ **Phase 5 COMPLETE**

### Current Progress
**70% Complete**:
- ✅ Raft infrastructure (voters, election process, message loop)
- ✅ Metadata initialization (startup + topic creation)
- ✅ Leader election monitoring (LeaderElector)
- ❌ **Network messaging (the final 30%)**

---

## References

**Existing Code to Study**:
- [raft_cluster.rs:208-318](crates/chronik-server/src/raft_cluster.rs#L208-L318) - Message loop (where TODO is)
- [raft_cluster.rs:49-97](crates/chronik-server/src/raft_cluster.rs#L49-L97) - Bootstrap (where peer addresses are)
- [wal_replication.rs](crates/chronik-server/src/wal_replication.rs) - Example of network client code

**Raft Library Docs**:
- Raft `Message` type: Contains to/from/msg_type/data
- `RawNode::step()`: Feed received messages into Raft
- `ready.messages()`: Get outgoing messages to send

**Key Insight**: The Raft library (tikv/raft-rs) already generates all the right messages. We just need to send/receive them over the network!

---

## Success Criteria

**Definition of Done**:
1. Three nodes can elect a leader within 5 seconds
2. Metadata proposals succeed and commit
3. ISR data is available in RaftCluster
4. Chaos test shows 100/100 messages (zero data loss)
5. Leader failover works end-to-end

**Time Estimate**: 4-6 hours total
- 2-3 hours: Basic TCP messaging
- 1-2 hours: Connection management
- 1 hour: Testing and debugging

---

## Command to Start

```
Continue Phase 5 implementation - implement Raft network messaging to enable multi-node leader election. The Raft voters are configured correctly and election process is working, but nodes can't communicate because the message loop just logs messages instead of sending them over the network. Implement TCP-based message sending/receiving in raft_cluster.rs to replace the TODO at line 295-303. Goal: get 3 nodes to elect a leader and commit metadata proposals. See NEXT_SESSION_PROMPT.md for full context.
```
