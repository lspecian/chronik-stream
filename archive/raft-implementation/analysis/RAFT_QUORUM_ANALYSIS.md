# Raft Consensus Quorum Achievement Analysis

## Problem Statement

The Raft cluster is showing:
- ✅ Leader election succeeds (node 2 becomes leader)
- ✅ gRPC connectivity works (nodes can communicate)
- ✅ Some broker registrations succeed (node 2 as leader)
- ❌ **Proposals not committing** (`commit=0` persists)
- ❌ **Leader shows `entries=0`** (no proposals being made)
- ❌ All brokers not registering (nodes 1, 3 fail)

## Raft Consensus Flow Analysis

### 1. Proposal Path (Working)

```rust
// In RaftMetaLog::propose_and_wait (raft_meta_log.rs:513-543)
pub async fn propose_and_wait(&self, op: MetadataOp, ...) -> Result<...> {
    // 1. Serialize operation
    let data = bincode::serialize(&op)?;

    // 2. Send to background processor
    let (tx, rx) = oneshot::channel();
    self.propose_tx.send((data, tx)).await?;

    // 3. Wait up to 5 seconds for result
    match timeout(Duration::from_secs(5), rx).await {
        Ok(Ok(index)) => { ... }  // Success
        Err(_) => { ... }  // Timeout - THIS IS HAPPENING
    }
}

// In BackgroundProcessor::process_propose_requests (raft_meta_log.rs:612-652)
async fn process_propose_requests(&self) {
    while let Ok((data, response_tx)) = rx.try_recv() {
        // Try to propose locally first
        let result = self.raft_replica.propose(data.clone()).await;

        // If "Not leader", forward to leader
        if error.contains("Not leader") {
            client.propose_metadata_to_leader(leader_id, data).await
        }
    }
}

// In PartitionReplica::propose (replica.rs:281-317)
pub async fn propose(&self, data: Vec<u8>) -> Result<u64> {
    // Check if we're the leader
    if node.raft.state != StateRole::Leader {
        return Err("Not leader (current leader: {})");  // Followers return this
    }

    // Leader proposes
    node.propose(vec![], data)?;  // ← Adds to Raft log
    let index = node.raft.raft_log.last_index();

    Ok(index)  // Returns immediately, doesn't wait for commit!
}
```

**Key Insight**: The `propose()` method returns an index immediately after adding to the local log. It does NOT wait for quorum acknowledgment or commit.

### 2. Message Sending (Working)

```rust
// In RaftReplicaManager background loop (raft_integration.rs:583-627)
loop {
    ticker.tick().await;
    replica.tick()?;  // Advance election/heartbeat timers

    let (messages, committed) = replica.ready().await?;

    // Send messages to peers via gRPC
    for msg in messages {
        tokio::spawn(async move {
            client.send_message(&topic, partition, to, msg).await
        });
    }
}

// In PartitionReplica::ready (replica.rs:499-728)
pub async fn ready(&self) -> Result<(Vec<Message>, Vec<Entry>)> {
    if !node.has_ready() {
        return Ok((Vec::new(), Vec::new()));  // No work to do
    }

    let mut ready = node.ready();

    // Extract messages (AppendEntries, RequestVote, etc.)
    let immediate_messages = ready.take_messages();  // Leader messages
    let persisted_messages = ready.take_persisted_messages();  // Follower votes
    let committed_entries = ready.take_committed_entries();  // ← THIS IS THE KEY

    // Advance Raft state machine
    node.advance(ready);

    // Return messages + committed entries
    Ok((all_messages, committed_entries))
}
```

**Key Insight**: The `ready()` method is responsible for extracting committed entries. If `committed_entries` is empty, nothing gets committed.

### 3. Message Receiving (Working)

```rust
// In RaftServiceImpl::step (rpc.rs:187-237)
async fn step(&self, request: Request<RaftMessage>) -> Result<Response<StepResponse>> {
    let req = request.into_inner();

    // Get replica for the partition
    let replica = self.get_replica(&req.topic, req.partition)?;

    // Deserialize Raft message
    let raft_msg = decode_raft_message(&req.message)?;

    // Step the message through Raft
    replica.step(raft_msg).await?;  // ← Feeds message into RawNode

    Ok(StepResponse { success: true })
}

// In PartitionReplica::step (replica.rs:472-488)
pub async fn step(&self, msg: Message) -> Result<()> {
    let mut node = self.raw_node.write();
    node.step(msg)?;  // ← tikv/raft processes the message
    Ok(())
}
```

**Key Insight**: Incoming messages are correctly fed into the `RawNode`. The issue is NOT in message delivery.

### 4. Commit Application (POTENTIAL ISSUE)

```rust
// In PartitionReplica::ready (replica.rs:661-728)
if !committed_entries.is_empty() {
    for entry in &committed_entries {
        // Apply conf changes
        if entry.get_entry_type() == EntryType::EntryConfChange {
            let conf_change = decode_conf_change(&entry.data)?;
            node.apply_conf_change(&conf_change)?;
        } else {
            // Apply data entry to state machine
            state_machine.apply(&raft_entry).await?;
        }

        // Notify pending proposals
        if let Some(tx) = pending_proposals.remove(&entry.index) {
            let _ = tx.send(Ok(()));  // ← Unblocks propose_and_wait()
        }
    }
}
```

**Key Insight**: Proposals only get acknowledged when `committed_entries` is non-empty. The issue is that `ready()` is not returning committed entries.

## Root Cause Hypotheses

### Hypothesis 1: Quorum Not Achieved ⚠️

**Symptom**: Leader has `entries=0`, `commit=0`

**Theory**: The leader is not receiving acknowledgments from followers, so it cannot commit.

**Raft Commit Rule**:
```
A log entry is committed when:
1. It's stored on the leader
2. It's replicated to a majority of servers (quorum)
3. The leader advances its commit index
```

**For 3-node cluster**:
- Quorum = 2 nodes (including leader)
- Leader needs 1 follower to acknowledge

**Check**:
```rust
// In tikv/raft RawNode internals (not visible in our code):
// 1. Leader proposes entry → adds to log
// 2. Leader sends AppendEntries to followers
// 3. Followers respond with success/failure
// 4. Leader updates Progress[follower_id].matched_index
// 5. Leader calculates new commit_index = majority matched_index
// 6. Leader advances commit_index
```

**Potential Issues**:
1. **AppendEntries not being sent**: Check if leader is sending heartbeats/entries
2. **AppendEntries responses not received**: Check if followers are replying
3. **Progress tracker misconfigured**: Check if followers are in the Progress map
4. **Quorum calculation wrong**: Check if quorum size is correct (2 for 3 nodes)

### Hypothesis 2: Initial Peer List Incomplete ❌ FIXED

**Previously**: Peer list excluded current node, breaking quorum calculation.

**Fix Applied** (raft_cluster.rs:57-58):
```rust
// OLD (broken):
initial_peers = other_nodes  // Missing self

// NEW (fixed):
initial_peers.push(config.node_id);  // Include self
```

**Status**: FIXED in previous session.

### Hypothesis 3: Background Loop Race Condition ❌ FIXED

**Previously**: Two background loops (`RaftReplicaManager` + `RaftMetaLog::BackgroundProcessor`) were both calling `tick()` and `process_ready()` on the `__meta` replica.

**Fix Applied** (raft_meta_log.rs:597-604):
```rust
// REMOVED from BackgroundProcessor::run():
// self.raft_replica.tick()?;  // ← Duplicate tick
// self.process_ready().await?;  // ← Duplicate ready processing
```

**Reason**: `RaftReplicaManager` already runs a background loop for ALL replicas (including `__meta`). Having a second loop caused race conditions.

**Status**: FIXED in previous session.

### Hypothesis 4: Progress Tracker Not Initialized ⚠️ INVESTIGATE

**Theory**: When the Raft replica is created, the Progress tracker might not include all peers.

**Raft Progress Tracker**:
```rust
// tikv/raft internals (not directly visible):
pub struct Progress {
    matched_index: u64,  // Highest log index known to be replicated on this peer
    next_index: u64,     // Next log index to send to this peer
    state: ProgressState,  // Probe, Replicate, Snapshot
}

pub struct Raft {
    prs: ProgressTracker,  // Map of peer_id -> Progress
}
```

**Critical**: If a follower is not in the Progress tracker, the leader will not count its acknowledgments toward the quorum.

**Check in PartitionReplica::new** (replica.rs:100-200):
```rust
impl PartitionReplica {
    pub fn new(
        topic: String,
        partition: i32,
        config: RaftConfig,
        log_storage: Arc<dyn RaftLogStorage>,
        state_machine: Arc<TokioRwLock<dyn StateMachine>>,
        peers: Vec<u64>,  // ← THIS is the initial peer list
    ) -> Result<Self> {
        // Create Raft config
        let raft_config = RaftCoreConfig {
            id: config.node_id,
            election_tick: config.election_tick,
            heartbeat_tick: config.heartbeat_tick,
            // ...
        };

        // Validate config
        raft_config.validate()?;

        // Create storage
        let storage = MemStorage::new();

        // Create RawNode
        let raw_node = RawNode::new(&raft_config, storage, &NullLogger)?;

        // ??? Are peers added to the Progress tracker here ???
    }
}
```

**MISSING**: There's no visible code that adds the `peers` to the Raft Progress tracker!

**Expected**:
```rust
// After creating RawNode, we should:
for peer_id in peers {
    if peer_id != config.node_id {
        // Add peer to Raft cluster (via ConfChange)
        let conf_change = ConfChange {
            change_type: ConfChangeType::AddNode,
            node_id: peer_id,
        };
        raw_node.propose_conf_change(vec![], conf_change)?;
    }
}
```

### Hypothesis 5: No Initial Bootstrap Configuration ⚠️ CRITICAL

**Theory**: The Raft cluster is not properly bootstrapped with the initial peer configuration.

**Raft Bootstrap Process**:
1. **Single-node start**: Node 1 starts alone, becomes leader of itself
2. **Add peers via ConfChange**: Node 1 proposes `AddNode` for nodes 2, 3
3. **Replicate configuration**: Nodes 2, 3 join, configuration is replicated
4. **Joint consensus**: Raft uses joint consensus for configuration changes

**Alternative (recommended)**:
1. **Multi-node bootstrap**: All nodes start with the same initial configuration
2. **Use `initial_peers` in RaftCoreConfig**: tikv/raft supports initial configuration
3. **Automatic quorum calculation**: Raft knows about all peers from the start

**Check** (replica.rs:~120-150):
```rust
// When creating RawNode, are we using ConfState?
let conf_state = ConfState {
    voters: peers.clone(),  // All voting members
    learners: vec![],       // Non-voting members
    ..Default::default()
};

let storage = MemStorage::new();
// ??? Is conf_state applied to storage ???
```

**Expected**:
```rust
// Apply initial configuration to storage BEFORE creating RawNode
storage.initialize_with_conf_state(conf_state)?;
let raw_node = RawNode::new(&raft_config, storage, &NullLogger)?;
```

## Diagnostic Plan

### Step 1: Check Raft Progress Tracker

Add debug logging to see if the leader knows about its followers:

```rust
// In PartitionReplica::ready (replica.rs:~510)
if has_ready {
    info!(
        "ready() HAS_READY for {}-{}: raft_state={:?}, term={}, commit={}, leader={}",
        self.topic, self.partition,
        node.raft.state, node.raft.term,
        node.raft.raft_log.committed, node.raft.leader_id
    );

    // ADD THIS:
    info!(
        "Progress tracker: voters={:?}, learners={:?}",
        node.raft.prs.conf().voters, node.raft.prs.conf().learners
    );

    for (peer_id, progress) in node.raft.prs.iter() {
        info!(
            "Peer {}: matched={}, next={}, state={:?}",
            peer_id, progress.matched, progress.next_idx, progress.state
        );
    }
}
```

**What to look for**:
- Are all 3 peers (1, 2, 3) in the Progress tracker?
- Do followers have `matched_index` increasing?
- Are followers in `Replicate` state (not `Probe`)?

### Step 2: Check AppendEntries Messages

Add logging to see if leader is sending entries:

```rust
// In PartitionReplica::ready (replica.rs:~525)
info!(
    "ready() EXTRACTING for {}-{}: immediate_msgs={}, persisted_msgs={}, entries={}, committed={}",
    self.topic, self.partition,
    ready.messages().len(), ready.persisted_messages().len(),
    ready.entries().len(), ready.committed_entries().len()
);

// ADD THIS:
for msg in ready.messages() {
    info!(
        "Outgoing message: type={:?}, from={}, to={}, term={}, log_term={}, index={}",
        msg.msg_type, msg.from, msg.to, msg.term, msg.log_term, msg.index
    );
}
```

**What to look for**:
- Are `MsgAppend` (AppendEntries) messages being sent?
- Are they being sent to the correct peers (nodes 1, 3)?
- Do they contain log entries?

### Step 3: Check AppendEntries Responses

Add logging to see if followers are replying:

```rust
// In PartitionReplica::step (replica.rs:~473)
debug!(
    node_id = self.config.node_id,
    topic = %self.topic,
    partition = self.partition,
    msg_type = ?msg.get_msg_type(),
    from = msg.from, to = msg.to, term = msg.term,
    "Received Raft message"
);

// ADD THIS:
if msg.msg_type == MessageType::MsgAppendResponse {
    info!(
        "AppendEntries response from {}: term={}, index={}, commit={}, reject={}",
        msg.from, msg.term, msg.index, msg.commit, msg.reject
    );
}
```

**What to look for**:
- Are followers sending `MsgAppendResponse` back to leader?
- Are they accepting entries (`reject=false`)?
- Are they advancing their `index`?

### Step 4: Check Initial Configuration

Examine how PartitionReplica is initialized:

```rust
// In PartitionReplica::new (replica.rs:~100-200)
// Need to verify:
// 1. Is MemStorage initialized with ConfState?
// 2. Are peers added to Progress tracker?
// 3. Is initial term/commit correct?
```

## Expected Raft Message Flow (Normal Case)

### Scenario: Leader proposes entry

```
TIME 0: Leader (node 2) proposes entry
│
├─ node.propose(vec![], data)
├─ Raft log: [Entry { index: 1, term: 1, data }]
├─ commit_index: 0 (not yet committed)
│
TIME 1: ready() extracts messages
│
├─ MsgAppend to node 1: entries=[Entry 1], commit=0
├─ MsgAppend to node 3: entries=[Entry 1], commit=0
│
TIME 2: Followers receive AppendEntries
│
├─ Node 1: step(MsgAppend) → appends entry to log
├─ Node 3: step(MsgAppend) → appends entry to log
│
TIME 3: Followers respond
│
├─ Node 1: MsgAppendResponse to node 2: index=1, reject=false
├─ Node 3: MsgAppendResponse to node 2: index=1, reject=false
│
TIME 4: Leader receives responses
│
├─ Node 2: step(MsgAppendResponse from 1) → Progress[1].matched = 1
├─ Node 2: step(MsgAppendResponse from 3) → Progress[3].matched = 1
│
TIME 5: Leader calculates quorum
│
├─ Progress: {1: matched=1, 2: matched=1, 3: matched=1}
├─ Majority matched = 1 (quorum achieved!)
├─ commit_index: 0 → 1 (advanced)
│
TIME 6: ready() extracts committed entries
│
├─ committed_entries = [Entry { index: 1 }]
├─ Leader applies entry to state machine
├─ Leader notifies pending proposal (propose_and_wait unblocks)
│
TIME 7: Leader sends heartbeat with updated commit
│
├─ MsgAppend to node 1: entries=[], commit=1
├─ MsgAppend to node 3: entries=[], commit=1
│
TIME 8: Followers update commit and apply
│
├─ Node 1: commit_index: 0 → 1, applies Entry 1
├─ Node 3: commit_index: 0 → 1, applies Entry 1
```

## Next Steps

1. **Run test with enhanced logging** to capture Progress tracker state
2. **Check if followers are in Progress tracker** (Step 1)
3. **Verify AppendEntries messages are sent** (Step 2)
4. **Verify AppendEntries responses are received** (Step 3)
5. **Fix initial configuration if broken** (Step 4)

## Likely Root Cause

Based on the symptom (`commit=0`, `entries=0` from leader), the most likely issue is:

**The Raft cluster is not properly bootstrapped with the initial peer configuration.**

The leader doesn't know about its followers, so it cannot send AppendEntries to them, and thus cannot achieve quorum.

**Fix**: Ensure `MemStorage` is initialized with `ConfState` containing all peers BEFORE creating `RawNode`.
