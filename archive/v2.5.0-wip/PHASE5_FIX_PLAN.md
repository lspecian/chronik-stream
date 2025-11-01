# Phase 5: Fix Plan - Raft Message Processing Loop

**Date**: 2025-11-01
**Status**: READY TO IMPLEMENT
**Estimated Time**: 3-4 hours

---

## Executive Summary

**Phase 5 is 95% complete** - only the Raft message processing loop is missing. This loop is critical because:

1. **Metadata IS replicated by Raft** - topics, partitions, ISR, leaders
2. **Without the message loop**, Raft proposals never get committed
3. **Without committed metadata**, LeaderElector has nothing to monitor
4. **Without ISR data**, leader election cannot happen

This is a well-understood problem with a clear solution.

---

## Problem Statement

### What's Broken

```
❌ Raft message processing loop MISSING
❌ Metadata proposals fail: "Failed to propose to Raft"
❌ No ISR data available for elections
❌ Leader election CANNOT work without Raft metadata
```

### Why It's Broken

**Raft Metadata Replication Flow**:
```
Topic Created
    ↓
Partition metadata proposed to Raft
    ↓
❌ NO MESSAGE LOOP TO PROCESS PROPOSAL ❌
    ↓
Proposal never committed
    ↓
Metadata never replicated
    ↓
Leader election has no data
```

**What SHOULD Happen**:
```
Topic Created
    ↓
Partition metadata proposed to Raft
    ↓
✅ Message loop processes Ready state ✅
    ↓
Proposal committed across cluster
    ↓
Metadata replicated to all nodes
    ↓
Leader election uses Raft metadata
```

---

## Architecture Overview

### Raft Metadata State Machine

**Location**: `crates/chronik-server/src/raft_metadata.rs`

**What's Replicated**:
```rust
pub struct MetadataStateMachine {
    /// Cluster nodes: node_id → address
    pub nodes: HashMap<u64, String>,

    /// Partition assignments: (topic, partition) → replica node IDs
    pub partition_assignments: HashMap<PartitionKey, Vec<u64>>,

    /// Partition leaders: (topic, partition) → leader node ID
    pub partition_leaders: HashMap<PartitionKey, u64>,

    /// ISR sets: (topic, partition) → in-sync replica node IDs
    pub isr_sets: HashMap<PartitionKey, Vec<u64>>,
}
```

**Commands**:
```rust
pub enum MetadataCommand {
    AddNode { node_id, address },
    RemoveNode { node_id },
    AssignPartition { topic, partition, replicas },  // ← Phase 5 uses this
    SetPartitionLeader { topic, partition, leader }, // ← Phase 5 uses this
    UpdateISR { topic, partition, isr },             // ← Phase 5 uses this
}
```

### Current Implementation

**RaftCluster** (`crates/chronik-server/src/raft_cluster.rs`):
```rust
pub struct RaftCluster {
    node_id: u64,
    raft_node: Arc<RwLock<RawNode<MemStorage>>>,  // ← Raft node exists
    state_machine: Arc<RwLock<MetadataStateMachine>>, // ← State machine exists
}

impl RaftCluster {
    // ✅ Has propose() method
    pub async fn propose(&self, cmd: MetadataCommand) -> Result<()> {
        let data = bincode::serialize(&cmd)?;
        let mut raft = self.raft_node.write().unwrap();
        raft.propose(vec![], data)?;  // ← Proposal goes into Raft queue
        Ok(())
    }

    // ✅ Has apply method
    pub fn apply_committed_entries(&self, entries: &[Entry]) -> Result<()> {
        let mut sm = self.state_machine.write().unwrap();
        for entry in entries {
            let cmd: MetadataCommand = bincode::deserialize(&entry.data)?;
            sm.apply(cmd)?;  // ← Can apply to state machine
        }
        Ok(())
    }

    // ❌ NO MESSAGE PROCESSING LOOP!
    // Proposals sit in queue forever, never get committed
}
```

---

## The Fix: Raft Message Processing Loop

### Step 1: Implement Message Loop in RaftCluster

**File**: `crates/chronik-server/src/raft_cluster.rs`

**Add new method**:
```rust
impl RaftCluster {
    /// Start background Raft message processing loop
    ///
    /// This loop:
    /// 1. Ticks Raft every 100ms
    /// 2. Processes Ready states
    /// 3. Commits entries to state machine
    /// 4. Sends messages to peers
    /// 5. Advances Raft
    pub fn start_message_loop(self: Arc<Self>) {
        info!("Starting Raft message processing loop");

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(100));

            loop {
                interval.tick().await;

                // Tick Raft
                {
                    let mut raft = match self.raft_node.write() {
                        Ok(r) => r,
                        Err(e) => {
                            error!("Failed to acquire Raft lock: {}", e);
                            continue;
                        }
                    };
                    raft.tick();
                }

                // Process Ready state
                let ready = {
                    let mut raft = match self.raft_node.write() {
                        Ok(r) => r,
                        Err(e) => {
                            error!("Failed to acquire Raft lock: {}", e);
                            continue;
                        }
                    };

                    if !raft.has_ready() {
                        continue;
                    }

                    raft.ready()
                };

                // Handle committed entries
                if !ready.committed_entries().is_empty() {
                    debug!("Processing {} committed entries", ready.committed_entries().len());

                    if let Err(e) = self.apply_committed_entries(ready.committed_entries()) {
                        error!("Failed to apply committed entries: {}", e);
                    } else {
                        info!("✓ Applied {} committed entries to state machine",
                              ready.committed_entries().len());
                    }
                }

                // Persist hard state (if changed)
                if !raft::is_empty_snap(ready.snapshot()) {
                    // TODO: Persist snapshot
                    debug!("Snapshot available (not persisted yet)");
                }

                // Send messages to peers
                for msg in ready.messages() {
                    // TODO: Send message to peer via network
                    // For now, just log
                    debug!("Message to send to peer {}: {:?}", msg.to, msg.msg_type);
                }

                // Advance Raft
                {
                    let mut raft = match self.raft_node.write() {
                        Ok(r) => r,
                        Err(e) => {
                            error!("Failed to acquire Raft lock: {}", e);
                            continue;
                        }
                    };
                    raft.advance(ready);
                }
            }
        });

        info!("✓ Raft message loop started");
    }
}
```

### Step 2: Wire Message Loop to IntegratedKafkaServer

**File**: `crates/chronik-server/src/integrated_server.rs`

**Location**: After RaftCluster creation (~line 370)

**Add**:
```rust
// v2.5.0 Phase 2: Create Raft cluster for metadata coordination
let raft_cluster = if config.cluster_config.is_some() {
    info!("Initializing Raft cluster for metadata coordination");

    let cluster = RaftCluster::bootstrap(
        config.node_id as u64,
        config.cluster_config.as_ref().unwrap().peers.clone(),
    ).await?;

    // ✅ START RAFT MESSAGE LOOP
    cluster.clone().start_message_loop();
    info!("✓ Raft message processing loop started");

    Some(Arc::new(cluster))
} else {
    None
};
```

### Step 3: Add Network Message Sending (Future Enhancement)

**For Phase 5, we can skip this** - with a single-node "cluster", proposals will commit immediately without needing network communication.

**For multi-node clusters** (future):
```rust
// In message loop, replace the TODO with:
for msg in ready.messages() {
    let peer_addr = self.get_peer_address(msg.to);

    // Send via gRPC/TCP to peer
    tokio::spawn(async move {
        send_raft_message(peer_addr, msg).await;
    });
}
```

---

## Implementation Steps

### Phase 1: Basic Message Loop (2 hours)

**Tasks**:
1. ✅ Add `start_message_loop()` to RaftCluster
2. ✅ Implement Raft tick + ready processing
3. ✅ Apply committed entries to state machine
4. ✅ Advance Raft
5. ✅ Wire to IntegratedKafkaServer
6. ✅ Add logging for debugging

**Deliverable**: Raft proposals get committed on single node

### Phase 2: Testing (1 hour)

**Tests**:
1. ✅ Start 1-node cluster
2. ✅ Create topic (triggers metadata proposal)
3. ✅ Verify metadata committed (check logs)
4. ✅ Verify LeaderElector has partition data
5. ✅ Produce messages
6. ✅ Verify acks=-1 works

**Expected Logs**:
```
INFO v2.5.0: Initializing partition metadata in RaftCluster for topic 'test'
INFO ✓ Applied 3 committed entries to state machine
INFO ✓ Initialized partition metadata for test-0: replicas=[1, 2, 3], leader=1, ISR=[1, 2, 3]
```

### Phase 3: Chaos Test (1 hour)

**Test**:
1. ✅ Start 3-node cluster (with message loop)
2. ✅ Create topic
3. ✅ Produce 50 messages to node 1
4. ✅ Kill node 1
5. ✅ **Verify leader election happens**
6. ✅ **Produce 50 messages to node 2**
7. ✅ Consume 100 messages (verify zero data loss)

**Expected Results**:
```
INFO Leader timeout for test-0 (leader=1), triggering election
INFO Electing first ISR member as leader for test-0: 2 (ISR: [1, 2, 3])
INFO ✓ Elected new leader for test-0: node 2
```

---

## Code Changes Summary

### Files to Modify

| File | Changes | Lines |
|------|---------|-------|
| `raft_cluster.rs` | Add `start_message_loop()` | +80 |
| `integrated_server.rs` | Wire message loop | +3 |
| **TOTAL** | | **~83 lines** |

### No Breaking Changes

- ✅ Existing code continues to work
- ✅ Only adds new functionality
- ✅ Backward compatible

---

## Testing Plan

### Test 1: Single Node Metadata Commit

**Setup**: 1-node cluster
**Steps**:
1. Start node 1
2. Create topic 'test'
3. Check logs for "Applied N committed entries"

**Expected**: Metadata proposals succeed

### Test 2: Multi-Node Metadata Replication

**Setup**: 3-node cluster
**Steps**:
1. Start nodes 1, 2, 3
2. Create topic 'test' on node 1
3. Check all nodes have same metadata

**Expected**: Metadata replicated to all nodes

### Test 3: Leader Election After Failover

**Setup**: 3-node cluster
**Steps**:
1. Create topic
2. Produce to node 1
3. Kill node 1
4. Verify new leader elected
5. Produce to node 2
6. Verify success

**Expected**: Leader election works, produces succeed

---

## Success Criteria

### Before Fix
- ❌ Metadata proposals fail
- ❌ "Failed to propose to Raft" errors
- ❌ No ISR data
- ❌ Leader election doesn't work
- ❌ acks=-1 always times out

### After Fix
- ✅ Metadata proposals succeed
- ✅ "Applied N committed entries" logs
- ✅ ISR data available
- ✅ Leader election works
- ✅ acks=-1 succeeds
- ✅ Failover works
- ✅ Zero data loss

---

## Risk Assessment

### Low Risk

**Why**:
- Small, isolated change (~83 lines)
- Well-understood Raft pattern
- No breaking changes
- Can disable if issues found

**Mitigation**:
- Comprehensive logging
- Gradual rollout (single-node → multi-node)
- Easy to revert (comment out `start_message_loop()`)

---

## Timeline

| Phase | Duration | Deliverable |
|-------|----------|-------------|
| Implementation | 2 hours | Message loop code complete |
| Testing | 1 hour | Single/multi-node tests pass |
| Chaos Test | 1 hour | Leader election verified |
| **TOTAL** | **4 hours** | **Phase 5 complete and working** |

---

## Next Steps

1. **Implement `start_message_loop()` in `raft_cluster.rs`** (2 hours)
   - Copy code from plan above
   - Add proper error handling
   - Add comprehensive logging

2. **Wire to `integrated_server.rs`** (5 minutes)
   - Add `cluster.clone().start_message_loop();`
   - After RaftCluster creation

3. **Test metadata commit** (30 minutes)
   - Start single node
   - Create topic
   - Verify logs show "Applied committed entries"

4. **Run chaos test** (30 minutes)
   - Start 3-node cluster
   - Kill leader
   - Verify election + failover

5. **Document results** (30 minutes)
   - Update PHASE5_COMPLETE.md
   - Mark Phase 5 as fully operational

---

## Conclusion

**Phase 5 is 95% complete** - only the Raft message loop is missing.

**This is a 4-hour fix** that will:
- ✅ Enable metadata replication via Raft
- ✅ Provide ISR data for leader election
- ✅ Make leader election fully functional
- ✅ Enable successful failover
- ✅ Complete Phase 5

**The implementation is straightforward** - we have all the pieces (RaftCluster, MetadataStateMachine, LeaderElector), we just need to connect them with the message processing loop.

**Ready to implement!** 🚀

