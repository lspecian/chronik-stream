# Hybrid Implementation Plan: Best of Both Worlds

## Objective
Combine the best parts from two branches to create a high-performance, Kafka-compatible cluster:

**From `feat/v2.2.0-clean-wal-replication` (THIS branch)**:
- ✅ Zero-copy WAL replication (no double-parsing)
- ✅ Lock-free queue (SegQueue) for async replication
- ✅ Clean produce hook (after WAL write, non-blocking)
- ✅ `replicate_serialized()` method (direct bincode bytes)

**From `archive/failed-raft-data-replication-v2.2-v2.3` (RAFT branch)**:
- ✅ ISR tracking per partition
- ✅ Partition-level replication (not whole-node)
- ✅ Leader election per partition
- ✅ Cluster coordination
- ✅ Quorum-based acks=-1 support

**NEW Ideas to Add**:
- ✅ Per-partition WAL files (`data/wal/{topic}/{partition}/segment.wal`)
- ✅ Separate fast path (acks=0/1) from slow path (acks=-1)

---

## Architecture: Raft for Metadata + WAL for Data

```
┌──────────────────────────────────────────────────────────────┐
│              Hybrid Architecture (v2.5.0)                     │
├──────────────────────────────────────────────────────────────┤
│                                                               │
│  CONTROL PLANE (Raft - Metadata Only):                       │
│    - Cluster membership (which nodes are alive)              │
│    - Partition assignments (partition 0 → [node1, node2])    │
│    - ISR tracking (which replicas are caught up)             │
│    - Leader election PER PARTITION                            │
│                                                               │
│  DATA PLANE (WAL Streaming - Fast):                          │
│    - Per-partition WAL files                                 │
│    - Zero-copy replication (this branch's innovation)        │
│    - Lock-free queues                                        │
│    - Fire-and-forget for acks=0/1                           │
│    - Quorum wait for acks=-1                                 │
│                                                               │
│  Producer Path:                                              │
│    acks=0/1: Write WAL → Replicate async → Return (FAST)    │
│    acks=-1: Write WAL → Wait ISR quorum → Return (SLOW)     │
│                                                               │
└──────────────────────────────────────────────────────────────┘
```

---

## Implementation Steps (5-7 Days)

### Day 1: Branch Setup & Analysis
**Goal**: Understand both codebases and create merge strategy

1. Checkout Raft archive branch
2. Run benchmarks to get REAL performance numbers (not claimed)
3. Identify actual bottlenecks (RwLock? Missing batching? Something else?)
4. Document what works vs what's broken

**Deliverable**: Performance analysis document

### Day 2: Per-Partition WAL Files
**Goal**: Refactor WAL to support partition-level replication

**Current (both branches)**: One giant WAL per node
```
data/wal/__meta/
data/wal/topic-name/partition-0/  (all in one WAL)
```

**Target**: One WAL per partition
```
data/wal/{topic}/{partition}/
  ├─ segment_000.wal  (sealed, immutable)
  ├─ segment_001.wal  (active)
  └─ index.db         (offset → segment mapping)
```

**Changes**:
- Modify `WalManager` to support partition-scoped WAL
- Each partition gets its own `GroupCommitWal` instance
- Partition assignment = which WAL files this node owns

**Deliverable**: Per-partition WAL working in standalone mode

### Day 3: Fix Raft Performance Issues
**Goal**: Make Raft control plane lightweight

**Issue 1: RwLock in Hot Path**
```rust
// BEFORE (Raft branch - BROKEN):
pub struct ProduceHandler {
    raft_manager: Arc<RwLock<Option<Arc<RaftGroupManager>>>>,  // ❌ KILLS PERF
}

// AFTER (fixed):
pub struct ProduceHandler {
    raft_manager: Option<Arc<RaftGroupManager>>,  // ✅ Set during init, never changes
}
```

**Issue 2: Raft Used for Data Replication**
```rust
// BEFORE (Raft branch - SLOW):
Every produce → Raft propose() → Consensus → Apply

// AFTER (hybrid):
acks=0/1: WAL write → Async replicate (THIS branch's approach)
acks=-1: WAL write → Wait for ISR acks (Raft tracks ISR, WAL does transfer)
```

**Deliverable**: Raft only for metadata, not data path

### Day 4: Port Zero-Copy Replication
**Goal**: Use this branch's zero-copy approach for partition replication

**Port from THIS branch**:
1. `replicate_serialized()` method (no parsing overhead)
2. Lock-free SegQueue
3. Clean produce hook (after WAL, non-blocking)

**Adapt for partition-level**:
```rust
// Instead of replicating ALL partitions:
replicate_serialized(topic, partition, serialized_data);

// Only replicate if this partition has replicas:
if let Some(replicas) = partition_assignments.get(topic, partition) {
    for replica_node in replicas {
        if is_in_isr(replica_node) {
            send_wal_stream(replica_node, topic, partition, data);
        }
    }
}
```

**Deliverable**: Zero-copy replication working for individual partitions

### Day 5: Separate Fast/Slow Paths
**Goal**: Implement your brilliant idea of separate acks=-1 buffer

```rust
pub async fn produce_to_partition(
    &self,
    topic: &str,
    partition: i32,
    batch: &[u8],
    acks: i16,
) -> Result<i64> {
    // Write to local WAL (always, for durability)
    let offset = self.wal_write(topic, partition, batch).await?;
    
    match acks {
        0 => {
            // FAST PATH: Return immediately (fire-and-forget)
            // Replication happens in background
            Ok(offset)
        }
        1 => {
            // FAST PATH: Already fsynced by wal_write
            // Replication happens in background
            Ok(offset)
        }
        -1 => {
            // SLOW PATH: Wait for ISR quorum
            // Create oneshot channel for ack
            let (tx, rx) = oneshot::channel();
            
            // Send to ISR quorum buffer (separate queue)
            self.isr_quorum_buffer.push(IsrWaitRequest {
                topic: topic.to_string(),
                partition,
                offset,
                responder: tx,
            });
            
            // Wait for quorum (with timeout)
            rx.await?
        }
    }
}
```

**Deliverable**: Fast path for acks=0/1, slow path for acks=-1

### Day 6: Integration & Testing
**Goal**: Wire everything together and test

1. 3-node Raft cluster setup
2. Partition assignments (topic-0 → [node1, node2, node3])
3. ISR tracking working
4. Leader election working
5. Produce with acks=0/1/−1 all working

**Benchmark Targets**:
- Standalone (no replication): 70K+ msg/s (match v2.1.0 baseline)
- acks=0/1 (async replication): 60K+ msg/s (within 15% of baseline)
- acks=-1 (quorum): 40K+ msg/s (acceptable for strong consistency)

**Deliverable**: Working 3-node cluster with all acks modes

### Day 7: Documentation & Polish
**Goal**: Production-ready release

1. Update CLAUDE.md with hybrid architecture
2. Add performance benchmarks to docs
3. Create migration guide from v2.1.0
4. Tag as v2.5.0

---

## Success Criteria

**MUST HAVE**:
- ✅ Standalone: >= 70K msg/s (within 10% of v2.1.0 baseline)
- ✅ acks=0/1: >= 60K msg/s (within 15% overhead)
- ✅ acks=-1: >= 40K msg/s (acceptable for quorum writes)
- ✅ Partition-level replication working
- ✅ ISR tracking working
- ✅ Leader election per partition working
- ✅ No RwLock in produce hot path

**NICE TO HAVE**:
- ✅ Partition reassignment (move partition between nodes)
- ✅ Rebalancing on node join/leave
- ✅ Cross-DC replication

---

## Risk Mitigation

**Risk 1: Raft still too slow**
- Mitigation: Use Raft ONLY for metadata (not data path)
- Fallback: Replace Raft with etcd/Consul for coordination

**Risk 2: Integration complexity**
- Mitigation: Incremental integration (one piece at a time)
- Fallback: Keep branches separate, merge tested components only

**Risk 3: Performance regression**
- Mitigation: Benchmark at EVERY step
- Fallback: Revert specific changes that cause regression

---

## Why This Will Work

**This branch proves**:
- Zero-copy replication CAN be fast (52K baseline)
- Clean hook design doesn't hurt performance
- Lock-free queues work well

**Raft branch proves**:
- ISR tracking is implemented
- Partition-level replication architecture exists
- Cluster coordination works

**Combination gives**:
- Fast data plane (WAL streaming)
- Smart control plane (Raft metadata)
- Best of both worlds

**Your two ideas are CRITICAL**:
1. Per-partition WAL → enables partition reassignment
2. Separate acks=-1 buffer → keeps fast path fast

---

## Next Steps

**Decision Point**: Do we proceed with this hybrid approach?

If YES:
1. I'll checkout the Raft archive branch
2. Run real benchmarks (not claimed numbers)
3. Start Day 1: Branch analysis
4. Implement Days 2-7 systematically

If NO:
1. Continue with PostgreSQL-style replication (current branch)
2. Accept limitations (no Kafka compatibility)
3. Ship as master-slave DR solution

**My recommendation**: GO FOR IT. This is the right architecture.
