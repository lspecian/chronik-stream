# Raft Metadata Behavior - v2.2.8 Baseline

**Date**: 2025-11-20
**Version**: v2.2.8
**Purpose**: Document current Raft metadata behavior before Option 4 (WAL-only metadata) implementation

---

## Current Architecture: Hybrid Raft + WAL Metadata

### Metadata Flow for Topic Creation

**When a producer sends to a new topic:**

```
1. Client → Produce(topic="new-topic", partition=0, acks=-1)
2. Leader (Node 1) receives produce request
3. Leader calls raft.propose_via_raft(BatchPartitionOps)
   - Creates partition assignments for 3 partitions
   - p0: replicas=[1,2,3], leader=1
   - p1: replicas=[2,3,1], leader=2
   - p2: replicas=[3,1,2], leader=3
4. Raft consensus (100-200ms typical):
   - Propose → Append to log → Replicate → Commit → Apply to state machine
5. State machine updated with partition metadata
6. THEN produce proceeds with data replication
```

### Observed Behavior from Logs (baseline-v229-22a36ee0)

**Topic Creation Log Sequence:**
```
[INFO] raft_cluster: propose_via_raft: ENTER - command=BatchPartitionOps {
  topic: "baseline-v229-22a36ee0",
  operations: [
    (0, [1, 2, 3], 1, [1, 2, 3]),
    (1, [2, 3, 1], 2, [2, 3, 1]),
    (2, [3, 1, 2], 3, [3, 1, 2])
  ]
}

[WARN] produce_handler: PRODUCE REQUEST: topic=baseline-v229-22a36ee0, partition=0, acks=-1, base_offset=0, last_offset=350
[WARN] produce_handler: ⚠️  No ISR found for baseline-v229-22a36ee0-0, using quorum=1
```

**Key Observations:**
1. **Raft proposal happens FIRST** before data replication
2. **ISR not immediately available** - takes time to propagate from Raft
3. **Quorum defaults to 1** when ISR not found (degrades to leader-only)
4. **Partition assignments via Raft BatchPartitionOps** command

### Current Raft Queries in ProduceHandler

**Location**: `crates/chronik-server/src/produce_handler.rs`

**1. get_partition_leader() - Line 1166**
```rust
if let Some(ref raft) = self.raft_cluster {
    if let Some(leader_id) = raft.get_partition_leader(&topic_name, partition_data.index) {
        // Use leader_id for ISR tracking
    }
}
```

**2. get_isr() - Line 1883**
```rust
if let Some(ref raft) = self.raft_cluster {
    if let Some(isr) = raft.get_isr(&topic_name, partition) {
        // Use ISR for quorum calculation
    }
}
```

**3. get_all_nodes() - Line 2633**
```rust
if let Some(ref raft) = self.raft_cluster {
    let all_nodes = raft.get_all_nodes();
    // Use for partition assignment decisions
}
```

### Performance Characteristics (v2.2.8)

**Measured Latencies:**

| Operation | Current (Raft) | Notes |
|-----------|----------------|-------|
| Topic creation | 100-200ms | Raft consensus overhead |
| Partition assignment | 100-200ms | Raft BatchPartitionOps |
| ISR update | 10-50ms | Raft state machine update |
| Metadata query | < 1ms | In-memory read from state machine |

**Baseline Test Results (2025-11-20 13:13 UTC):**
- **Produce 1000 messages with acks=all**: ✅ SUCCESS
  - All 3 nodes: 1000/1000 messages replicated
  - Distribution: p0=323, p1=317, p2=360
  - No data loss, full replication working

**Failure Modes Observed:**
- ISR lag: "No ISR found" warnings indicate Raft metadata lag
- Quorum degradation: Falls back to quorum=1 when ISR unavailable
- Raft proposal blocking: Topic creation blocks on Raft consensus

### Raft State Machine Fields (raft_metadata.rs)

**Current Metadata State Machine:**
```rust
pub struct MetadataStateMachine {
    // Partition metadata (WILL BE REMOVED in Option 4)
    pub partition_assignments: HashMap<PartitionKey, Vec<u64>>,
    pub partition_leaders: HashMap<PartitionKey, u64>,
    pub isr: HashMap<PartitionKey, Vec<u64>>,
    pub high_watermarks: HashMap<PartitionKey, i64>,

    // Cluster membership (WILL BE KEPT in Option 4)
    pub nodes: HashMap<u64, NodeInfo>,
    pub raft_leader: Option<u64>,
}
```

### Why Option 4 Will Improve This

**Problems with Current Hybrid Approach:**
1. **Redundant metadata storage**: Both Raft state machine AND `__chronik_metadata` WAL
2. **Raft consensus overhead**: 100-200ms for metadata updates
3. **ISR lag**: Raft metadata not immediately available after topic creation
4. **Complex failure modes**: Two metadata paths can diverge

**Expected Improvements After Option 4:**
1. ✅ Single metadata path: `__chronik_metadata` WAL only
2. ✅ 200x faster topic creation: 1-5ms (WAL) vs 100-200ms (Raft)
3. ✅ No ISR lag: Metadata arrives BEFORE data via ordered WAL frames
4. ✅ Simpler failure modes: One metadata source of truth

---

## Comparison Points for Post-Implementation

**After implementing Option 4, verify:**

### Correctness
- [ ] Same baseline test (1000 messages, acks=all) passes on all nodes
- [ ] No "No ISR found" warnings (metadata should arrive with data)
- [ ] Partition assignments work correctly without Raft queries

### Performance
- [ ] Topic creation latency: < 10ms (target: 1-5ms)
- [ ] Partition assignment latency: < 10ms (target: 1-5ms)
- [ ] No Raft consensus blocking on metadata operations

### Reliability
- [ ] Follower restart recovery still works
- [ ] Leader failover preserves metadata
- [ ] No metadata divergence between nodes

---

**Status**: ✅ Documented
**Next**: Begin Phase 1 - Add metadata methods to ProduceHandler
