# What Actually Works vs. What Doesn't

**Date**: 2025-11-01
**After careful code verification**

---

## Summary: What's Actually Implemented

| Component | Status | Evidence | Notes |
|-----------|--------|----------|-------|
| **Raft Cluster** | ✅ EXISTS | raft_cluster.rs | Leader election works |
| **Metadata State Machine** | ✅ EXISTS | raft_metadata.rs | Apply logic exists |
| **Metadata Proposals** | ✅ CALLED | integrated_server.rs:525,536,545 | AssignPartition, SetPartitionLeader, UpdateISR |
| **Apply Committed Entries** | ✅ CALLED | raft_cluster.rs:469 | In message loop |
| **ISR Tracker** | ✅ EXISTS | isr_tracker.rs | Follower offset tracking |
| **ISR Ack Tracker** | ✅ EXISTS | isr_ack_tracker.rs | Quorum waiting |
| **Leader Election** | ✅ EXISTS | leader_election.rs | Partition leader election logic |
| **Per-Partition WAL** | ❌ DOESN'T EXIST | manager.rs:34 | Single GroupCommitWal for all |
| **Raft Persistence** | ❌ MemStorage | raft_cluster.rs:87 | Lost on restart |

---

## Critical Questions to Answer

### Q1: Why are acks=-1 produces timing out?

**Possible Causes**:
1. ❓ ISR not being initialized for new partitions
2. ❓ Quorum wait logic not actually called in produce path
3. ❓ Follower ACKs not being sent
4. ❓ IsrAckTracker not being used

**Verification Needed**:
- Check if ISR is initialized when topic is created
- Check if acks=-1 actually calls quorum wait
- Check if followers send ACKs after replication

### Q2: Are metadata proposals actually being committed?

**What We Know**:
- ✅ Proposals ARE called (grep verified)
- ✅ `apply_committed_entries()` IS called in message loop
- ❓ Are entries actually being committed by Raft?
- ❓ Does the state machine have the data?

**Verification Needed**:
- Add debug logging to `MetadataStateMachine::apply()`
- Check if partition assignments exist in state machine
- Verify Raft consensus is working (quorum of nodes)

### Q3: Do we need per-partition WAL right now?

**Current WAL**:
```rust
// Single WAL for ALL partitions
group_commit_wal: Arc<GroupCommitWal>
```

**Can we test cluster WITHOUT per-partition WAL?**

**Answer**: YES! Here's why:

| Feature | Needs Per-Partition WAL? |
|---------|-------------------------|
| Raft metadata replication | ❌ NO - metadata is separate |
| Topic creation across cluster | ❌ NO - metadata only |
| acks=-1 quorum | ❌ NO - can work with topic-level WAL |
| Partition reassignment | ✅ YES - need to move partition WAL |
| Fine-grained replication | ✅ YES - need partition isolation |

**Conclusion**: Per-partition WAL is NOT blocking us from testing basic cluster functionality!

We can:
1. Test metadata replication (topic creation)
2. Test acks=-1 quorum (with topic-level WAL)
3. Test leader election (cluster + partition leaders)

We CANNOT (without per-partition WAL):
1. Reassign partitions between nodes
2. Isolate partition operations
3. Have different nodes lead different partitions

**Recommendation**: Skip per-partition WAL for now, implement later.

---

## Revised Implementation Gaps

Based on code verification, here's what's ACTUALLY missing:

### Gap 1: Raft Metadata Not Initialized on Topic Creation (Need to Verify)

**Hypothesis**: Topics are being created, but metadata proposals might not be committed.

**Check**:
```rust
// In integrated_server.rs or produce_handler.rs
// When topic is created, are these proposals successful?
raft.propose(MetadataCommand::AssignPartition { ... }).await;
raft.propose(MetadataCommand::SetPartitionLeader { ... }).await;
raft.propose(MetadataCommand::UpdateISR { ... }).await;
```

**Verification**:
- Check logs: Are proposals succeeding?
- Check state machine: Do partition assignments exist?
- Check Raft: Are entries being committed?

### Gap 2: ISR Not Initialized for New Partitions (Need to Verify)

**Hypothesis**: ISR tracker exists, but ISR might not be populated.

**Check**:
```rust
// When partition is created, is ISR initialized?
isr_tracker.update_isr(topic, partition, vec![node1, node2, node3]);
```

**Verification**:
- Check logs: Is ISR being updated?
- Check state machine: Does `get_isr()` return nodes?
- Check tracker: Are follower offsets being tracked?

### Gap 3: acks=-1 Quorum Not Actually Called (Need to Verify)

**Hypothesis**: `IsrAckTracker` exists, but produce path might not use it.

**Check**:
```rust
// In produce_handler.rs
match acks {
    -1 => {
        // Is this actually calling quorum wait?
        isr_ack_tracker.wait_for_quorum(topic, partition, offset).await?;
    }
}
```

**Verification**:
- Check produce handler code
- Check if `IsrAckTracker` is constructed
- Check if wait_for_quorum is called

### Gap 4: Follower ACKs Not Being Sent (Need to Verify)

**Hypothesis**: Followers replicate data, but might not send ACKs back.

**Check**:
```rust
// In follower replication handler
// After successful WAL write, does follower send ACK?
send_ack_to_leader(topic, partition, offset, self.node_id).await;
```

**Verification**:
- Check replication handler
- Check if ACK message exists
- Check if leader receives ACKs

---

## Next Steps: Systematic Verification

### Step 1: Check Metadata Replication (30 min)

```bash
# Start 3-node cluster
# Create topic on node 1
# Check logs on nodes 2 and 3 for:
# - "Applied N committed entries to state machine"
# - Partition assignments in state machine

# If this works: ✅ Raft metadata is working
# If this fails: ❌ Raft consensus is broken
```

### Step 2: Check ISR Initialization (15 min)

```bash
# After creating topic, check logs for:
# - "ISR initialized for partition X"
# - "ISR contains nodes: [1, 2, 3]"

# Query state machine:
# raft_cluster.get_isr("test-topic", 0)

# If this works: ✅ ISR is being initialized
# If this fails: ❌ ISR not wired up to topic creation
```

### Step 3: Check acks=-1 Code Path (15 min)

```bash
# Read produce_handler.rs
# Search for "acks == -1" or "acks = -1"
# Verify quorum wait is called

# Check if IsrAckTracker is:
# 1. Constructed in IntegratedKafkaServer
# 2. Passed to ProduceHandler
# 3. Actually used in acks=-1 path

# If wired up: ✅ Logic exists
# If not: ❌ Need to wire it up
```

### Step 4: Check Follower ACK Handler (15 min)

```bash
# Check wal_replication.rs or follower handler
# Look for ACK message after successful replication

# If exists: ✅ ACKs are being sent
# If not: ❌ Need to implement ACK response
```

---

## Expected Outcome

After these 4 verification steps (1.5 hours total), we'll know:

1. Is Raft metadata replication working?
   - YES: Move to ISR testing
   - NO: Fix Raft consensus first

2. Is ISR initialized for partitions?
   - YES: Move to acks=-1 testing
   - NO: Wire ISR initialization to topic creation

3. Is acks=-1 quorum wait wired up?
   - YES: Check why it's timing out
   - NO: Wire it up

4. Are followers sending ACKs?
   - YES: Check why leader isn't receiving them
   - NO: Implement ACK response

**Then we'll know EXACTLY what to implement.**

---

## Conclusion

**What I Got Wrong**:
- Claimed metadata proposals weren't called ❌ (they are!)
- Claimed ISR tracking doesn't exist ❌ (it does!)
- Claimed committed entries aren't applied ❌ (they are!)

**What's Actually Unclear**:
- ❓ Are Raft entries actually being committed?
- ❓ Is ISR being initialized?
- ❓ Is acks=-1 quorum wait actually called?
- ❓ Are follower ACKs being sent?

**Next Action**: VERIFY, don't assume. Run the 4 verification steps above.
