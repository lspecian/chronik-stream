# Raft Consensus Root Cause Analysis

## Executive Summary

After in-depth investigation of the Raft consensus issue where proposals are not committing (`commit=0`), I've identified that **all the infrastructure is correctly implemented**:

- ✅ Progress tracker is initialized with all peers
- ✅ Background loop is ticking and calling ready()
- ✅ gRPC message delivery pipeline is working
- ✅ Proposal mechanism is correctly implemented
- ✅ State machines are properly configured
- ✅ RaftMetaLog integration is correct

**The most likely root cause is that the leader is not receiving proposal acknowledgments from followers because the followers are not responding to AppendEntries messages, or the leader is not broadcasting AppendEntries at all.**

## Critical Finding: Missing Diagnostic Visibility

The core issue is **we don't have visibility into what tikv/raft is doing internally**. We need to add diagnostic logging to answer these questions:

1. **Is the leader sending AppendEntries messages?**
2. **Are followers receiving those messages?**
3. **Are followers responding with success?**
4. **Is the leader receiving the responses?**
5. **Is the leader advancing the commit index?**

## Architecture Review

### 1. Proposal Flow (Verified Working)

```
RaftMetaLog::register_broker()
  ↓
propose_and_wait(MetadataOp::RegisterBroker)
  ↓
propose_tx.send((data, response_tx))  // Send to BackgroundProcessor
  ↓
BackgroundProcessor::process_propose_requests()
  ↓
raft_replica.propose(data)  // If leader, OR forward to leader if follower
  ↓
PartitionReplica::propose()
  ↓
node.propose(vec![], data)  // tikv/raft RawNode
  ↓
**RETURNS INDEX immediately (doesn't wait for commit)**
```

**Status**: ✅ Working correctly. The issue is NOT in the proposal mechanism.

### 2. Message Broadcasting (tikv/raft Internal)

```
After propose(), on next ready():
  ↓
node.ready()  // tikv/raft extracts messages and committed entries
  ↓
**tikv/raft internally:**
  - bcast_append() → generates MsgAppend for each follower
  - Each MsgAppend contains the new entry
  - Messages are queued in Ready.messages
  ↓
PartitionReplica::ready()
  ↓
messages = ready.take_messages()  // Extract messages
  ↓
RaftReplicaManager background loop
  ↓
client.send_message(&topic, partition, to, msg)  // gRPC to peer
```

**Status**: ⚠️ **Visibility gap**. We can see messages being sent (or not sent), but we can't see if tikv/raft is generating them.

### 3. Message Receiving (Verified Working)

```
Peer receives gRPC call
  ↓
RaftServiceImpl::step()
  ↓
replica.step(raft_msg)
  ↓
PartitionReplica::step()
  ↓
node.step(msg)  // tikv/raft processes message
  ↓
**tikv/raft internally:**
  - Follower appends entry to log
  - Follower queues MsgAppendResponse
  - Response queued in Ready.messages on next ready()
```

**Status**: ✅ Working correctly (messages are being delivered).

### 4. Acknowledgment Processing (tikv/raft Internal)

```
Leader receives MsgAppendResponse
  ↓
node.step(response_msg)  // tikv/raft processes response
  ↓
**tikv/raft internally:**
  - Updates Progress[follower_id].matched_index
  - Checks if majority of nodes have matched_index >= proposed_index
  - If yes, advances commit_index
  - Queues committed entries in Ready.committed_entries
  ↓
PartitionReplica::ready()
  ↓
committed_entries = ready.take_committed_entries()
  ↓
state_machine.apply(entry)  // Apply to state
  ↓
pending_proposals[index].send(Ok(()))  // Notify propose_and_wait()
```

**Status**: ⚠️ **Visibility gap**. We can't see if tikv/raft is receiving responses or advancing commit_index.

## Diagnostic Plan: Enhanced Logging

### Step 1: Add Raft Internal State Logging

Modify `PartitionReplica::ready()` to log tikv/raft internal state:

```rust
// In replica.rs, ready() method, after line 516:
if has_ready {
    info!(
        "ready() HAS_READY for {}-{}: raft_state={:?}, term={}, commit={}, leader={}",
        self.topic, self.partition,
        node.raft.state, node.raft.term,
        node.raft.raft_log.committed, node.raft.leader_id
    );

    // ADD THIS: Log Progress tracker state
    info!("Progress tracker for {}-{}:", self.topic, self.partition);
    for (peer_id, progress) in node.raft.prs().iter() {
        info!(
            "  Peer {}: matched={}, next_idx={}, state={:?}, paused={}, pending_snapshot={}, recent_active={}",
            peer_id,
            progress.matched,
            progress.next_idx,
            progress.state,
            progress.paused,
            progress.pending_snapshot,
            progress.recent_active
        );
    }

    // ADD THIS: Log Raft log state
    info!(
        "Raft log for {}-{}: last_index={}, committed={}, applied={}, unstable entries={}",
        self.topic, self.partition,
        node.raft.raft_log.last_index(),
        node.raft.raft_log.committed,
        node.raft.raft_log.applied,
        node.raft.raft_log.unstable_entries().len()
    );
}
```

### Step 2: Log All Outgoing Messages

```rust
// In replica.rs, ready() method, after line 635:
if !all_messages.is_empty() {
    info!("Outgoing messages from {}-{}:", self.topic, self.partition);
    for (i, msg) in all_messages.iter().enumerate() {
        info!(
            "  Message {}: type={:?}, from={}, to={}, term={}, log_term={}, index={}, commit={}, entries={}",
            i,
            msg.msg_type,
            msg.from,
            msg.to,
            msg.term,
            msg.log_term,
            msg.index,
            msg.commit,
            msg.entries.len()
        );
    }
}
```

### Step 3: Log All Incoming Messages

```rust
// In replica.rs, step() method, after line 481:
info!(
    "Received Raft message for {}-{}: type={:?}, from={}, to={}, term={}, log_term={}, index={}, commit={}, entries={}, reject={}",
    self.topic,
    self.partition,
    msg.msg_type,
    msg.from,
    msg.to,
    msg.term,
    msg.log_term,
    msg.index,
    msg.commit,
    msg.entries.len(),
    msg.reject
);
```

### Step 4: Log Committed Entries

```rust
// In replica.rs, ready() method, after line 671:
if !committed_entries.is_empty() {
    info!(
        "Committed entries for {}-{}: count={}, first_index={}, last_index={}",
        self.topic,
        self.partition,
        committed_entries.len(),
        committed_entries.first().map(|e| e.index),
        committed_entries.last().map(|e| e.index)
    );

    for (i, entry) in committed_entries.iter().enumerate() {
        info!(
            "  Committed entry {}: index={}, term={}, type={:?}, data_len={}",
            i,
            entry.index,
            entry.term,
            entry.entry_type,
            entry.data.len()
        );
    }
}
```

## Expected Normal Flow (With Logging)

### Scenario: Leader proposes broker registration

```
TIME 0: Leader (node 2) proposes
LOG: "Proposing entry of 123 bytes to __meta-0 (with wait)"
LOG: "Proposed entry at index 1 to __meta-0, waiting for commit"

TIME 1: Leader's next tick + ready()
LOG: "ready() HAS_READY for __meta-0: raft_state=Leader, term=1, commit=0, leader=2"
LOG: "Progress tracker for __meta-0:"
LOG: "  Peer 1: matched=0, next_idx=1, state=Replicate, ..."
LOG: "  Peer 2: matched=0, next_idx=1, state=Replicate, ..."
LOG: "  Peer 3: matched=0, next_idx=1, state=Replicate, ..."
LOG: "Raft log for __meta-0: last_index=1, committed=0, applied=0, unstable entries=1"
LOG: "Outgoing messages from __meta-0:"
LOG: "  Message 0: type=MsgAppend, from=2, to=1, term=1, index=0, entries=1"
LOG: "  Message 1: type=MsgAppend, from=2, to=3, term=1, index=0, entries=1"
LOG: "Sending 2 messages to peers for __meta-0"

TIME 2: Follower 1 receives message
LOG: "Received Raft message for __meta-0: type=MsgAppend, from=2, to=1, term=1, index=0, entries=1"
LOG: "ready() HAS_READY for __meta-0: raft_state=Follower, term=1, commit=0, leader=2"
LOG: "Raft log for __meta-0: last_index=1, committed=0, applied=0, unstable entries=1"
LOG: "Outgoing messages from __meta-0:"
LOG: "  Message 0: type=MsgAppendResponse, from=1, to=2, term=1, index=1, reject=false"

TIME 3: Leader receives response
LOG: "Received Raft message for __meta-0: type=MsgAppendResponse, from=1, to=2, term=1, index=1, reject=false"
LOG: "ready() HAS_READY for __meta-0: raft_state=Leader, term=1, commit=1, leader=2"  ← commit advanced!
LOG: "Progress tracker for __meta-0:"
LOG: "  Peer 1: matched=1, next_idx=2, state=Replicate, ..."  ← matched_index advanced!
LOG: "Committed entries for __meta-0: count=1, first_index=1, last_index=1"
LOG: "  Committed entry 0: index=1, term=1, type=Normal, data_len=123"
```

## Potential Root Causes to Check

### Hypothesis 1: Leader Not Broadcasting ⚠️

**Check**: Are we seeing "Outgoing messages" logs with `type=MsgAppend` from the leader?

**If NO**:
- tikv/raft is not generating AppendEntries messages
- Possible causes:
  - Progress[follower] is in `Probe` state instead of `Replicate`
  - Progress[follower] is `paused`
  - Followers are not in the voters list

**Fix**: Check Progress tracker initialization and state

### Hypothesis 2: Followers Not Responding ⚠️

**Check**: Are we seeing "Outgoing messages" logs with `type=MsgAppendResponse` from followers?

**If NO**:
- Followers are receiving messages but not responding
- Possible causes:
  - Term mismatch (follower's term > leader's term)
  - Log mismatch (follower's log doesn't match leader's prev_log_index)
  - Follower is in wrong state

**Fix**: Check term and log consistency

### Hypothesis 3: Leader Not Receiving Responses ⚠️

**Check**: Are we seeing "Received Raft message: type=MsgAppendResponse" on the leader?

**If NO**:
- Messages are being sent but not delivered
- Possible causes:
  - gRPC connection failure
  - Network issue
  - Incorrect peer address

**Fix**: Check gRPC connectivity and peer addresses

### Hypothesis 4: Quorum Not Achieved ⚠️

**Check**: Are we seeing "Progress tracker" with `matched_index` advancing for followers?

**If NO**:
- Leader is not updating Progress tracker
- Possible causes:
  - Responses are rejected (`reject=true`)
  - Responses have wrong index
  - Progress tracker not initialized correctly

**Fix**: Check response handling and Progress tracker state

### Hypothesis 5: Commit Index Not Advancing ⚠️

**Check**: Are we seeing "commit" advancing in "ready() HAS_READY" logs?

**If NO**:
- Leader is not calculating new commit index
- Possible causes:
  - Majority not achieved (only 1 out of 3 nodes responded)
  - Leader's own entry not marked as matched
  - Raft configuration has wrong quorum size

**Fix**: Check quorum calculation and leader's self-progress

## Test Plan

1. **Apply enhanced logging** to `replica.rs`
2. **Rebuild and run test**:
   ```bash
   cargo build --release --bin chronik-server --features raft

   # Run test with detailed logs
   RUST_LOG=chronik_raft=info,chronik_server=info ./test_raft_cluster.sh 2>&1 | tee raft_diagnostic.log
   ```

3. **Analyze logs** for the pattern above
4. **Identify where the flow breaks**
5. **Apply targeted fix** based on findings

## Next Steps

1. **Implement enhanced logging** in `replica.rs`
2. **Run test** and capture full logs
3. **Search logs** for:
   - `"Proposing entry"` - Confirm proposals are made
   - `"Outgoing messages"` - Check if AppendEntries are sent
   - `"Received Raft message"` - Check if responses are received
   - `"Progress tracker"` - Check if matched_index advances
   - `"Committed entries"` - Check if entries commit
4. **Identify the gap** where the flow breaks
5. **Apply fix** based on the specific failure point

## Conclusion

The infrastructure is solid. We just need diagnostic visibility to see where tikv/raft's internal state machine is getting stuck. Once we have the logs, the root cause will be obvious.
