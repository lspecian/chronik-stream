# Implementation Gap Analysis: What's Missing vs. Clean Plan

**Date**: 2025-11-01
**Current Branch**: `feat/v2.5.0-kafka-cluster`
**Reference**: `docs/CLEAN_RAFT_IMPLEMENTATION.md`

---

## Executive Summary

We've completed **Phase 2 (partial)** of the 5-phase plan. Here's the status:

| Phase | Status | Completion | Critical Gaps |
|-------|--------|------------|---------------|
| **Phase 1**: Per-Partition WAL | ❌ **NOT DONE** | 0% | WAL still topic-scoped, not partition-scoped |
| **Phase 2**: Raft Metadata | ⚠️ **PARTIAL** | 60% | Leader election works, but metadata proposals DON'T |
| **Phase 3**: Partition Replication + ISR | ❌ **NOT DONE** | 0% | No ISR tracking, no partition replication |
| **Phase 4**: acks=-1 Quorum | ❌ **NOT DONE** | 0% | No ISR ack tracking, produces timeout |
| **Phase 5**: Partition Leader Election | ❌ **NOT DONE** | 0% | Only cluster leader election works |

**Overall Progress**: ~12% complete (Phase 2 partial only)

---

## Phase 1: Per-Partition WAL Files ❌ NOT IMPLEMENTED

### What's Missing

The WAL is still **topic-scoped**, not **partition-scoped**. This blocks partition-level operations.

#### Current Architecture (WRONG)
```
data/wal/
  ├─ __meta/
  └─ {topic}/
      └─ partition-{N}/  ← All partitions share same topic-level WAL
```

#### Required Architecture
```
data/wal/
  ├─ __meta/
  └─ partitions/
      └─ {topic}-{partition}/  ← Each partition has its own WAL
          ├─ segment_000.wal
          ├─ segment_001.wal
          └─ offset_index
```

### Impact

**Blocking Issues**:
- ❌ Can't replicate individual partitions (need partition-scoped WAL)
- ❌ Can't assign partitions to different nodes (WAL is topic-wide)
- ❌ Can't implement partition leader election (no partition isolation)
- ❌ Can't track ISR per partition (need partition-level offsets)

### Files That Need Changes

| File | Current State | Required Change |
|------|---------------|-----------------|
| `chronik-wal/src/manager.rs` | Topic-scoped append | Partition-scoped `get_partition_wal()` |
| `chronik-wal/src/group_commit.rs` | No partition context | Add `partition: i32` to constructor |
| `chronik-server/src/produce_handler.rs` | Calls topic-level WAL | Call partition-level WAL |

### Effort Estimate
**2-3 days** (as per original plan)

---

## Phase 2: Raft Metadata ⚠️ PARTIAL (60% complete)

### What's Implemented ✅

1. **Raft cluster bootstrap** - Working
   - RaftCluster struct with state machine
   - Voter configuration (nodes 1, 2, 3)
   - MemStorage initialization

2. **Network messaging** - Working (NEW: just completed!)
   - TCP send/receive for Raft messages
   - Protobuf serialization
   - Proper Ready handling (unpersisted + persisted messages)
   - Background message loop

3. **Cluster leader election** - Working
   - Nodes elect a cluster leader
   - Leader sends heartbeats
   - Followers acknowledge leader

### What's Missing ❌

1. **Metadata proposals via Raft** - NOT WORKING
   ```rust
   // Currently: Direct in-memory updates (NO consensus!)
   state_machine.partition_assignments.insert(...)  // ❌ NOT replicated!

   // Required: Raft proposals
   raft_cluster.propose(MetadataCommand::AssignPartition { ... }).await;  // ✅ Replicated via Raft
   ```

2. **Metadata initialization on topic creation** - NOT WORKING
   ```rust
   // When topic is created, should propose to Raft:
   raft_cluster.propose(MetadataCommand::AssignPartition {
       topic: "test-topic",
       partition: 0,
       replicas: vec![1, 2, 3],  // All cluster nodes
   }).await;
   ```

3. **Querying metadata from state machine** - NOT WIRED UP
   ```rust
   // ProduceHandler should query Raft state machine for partition assignments:
   let replicas = raft_cluster.get_partition_replicas(topic, partition);
   let leader = raft_cluster.get_partition_leader(topic, partition);
   let isr = raft_cluster.get_isr(topic, partition);
   ```

4. **Persisting hard state and entries** - Using MemStorage (NOT DURABLE!)
   ```rust
   // TODO: Persist to disk, not just MemStorage
   if let Some(hs) = ready.hs() {
       storage.set_hardstate(hs);  // ❌ MemStorage = lost on restart!
   }
   ```

### Impact

**Current Limitations**:
- ❌ Metadata not replicated (each node has independent state)
- ❌ Metadata lost on restart (MemStorage only)
- ❌ Can't query partition assignments from Raft
- ❌ No integration with ProduceHandler/FetchHandler

### Files That Need Changes

| File | Current State | Required Change |
|------|---------------|-----------------|
| `raft_cluster.rs` | propose() exists but unused | Wire to topic creation, partition assignment |
| `integrated_server.rs` | Has RaftCluster reference | Use it for metadata queries |
| `produce_handler.rs` | Uses local metadata | Query RaftCluster for assignments |
| `raft_metadata.rs` | Has MetadataStateMachine | Wire apply() to committed entries |

### Effort Estimate
**1-2 days** (remaining 40% of Phase 2)

---

## Phase 3: Partition Replication + ISR ❌ NOT IMPLEMENTED

### What's Missing

**No ISR tracking at all**:
- No `IsrTracker` component
- No follower offset tracking
- No lag calculation
- No ISR set maintenance

**No partition-level replication**:
- WAL replication is still topic-level (Phase 1 blocker)
- Can't replicate specific partitions
- Can't query replica assignments from Raft

### Required Components

1. **IsrTracker** (`crates/chronik-server/src/isr_tracker.rs`) - NEW FILE
   ```rust
   pub struct IsrTracker {
       follower_offsets: DashMap<(u64, PartitionKey), i64>,
       max_lag_ms: u64,
       max_lag_entries: u64,
   }
   ```

2. **Partition Replication** (modify `wal_replication.rs`)
   ```rust
   pub async fn replicate_partition(
       &self,
       topic: String,
       partition: i32,  // NEW: partition-specific
       offset: i64,
       data: Vec<u8>,
   )
   ```

3. **Integration with ProduceHandler**
   ```rust
   // After WAL write, replicate to ISR members only
   if let Some(replicas) = raft_cluster.get_partition_replicas(topic, partition) {
       for node_id in replicas {
           if isr_tracker.is_in_sync(node_id, topic, partition) {
               repl_mgr.replicate_partition(topic, partition, offset, data).await;
           }
       }
   }
   ```

### Impact

**Current Limitations**:
- ❌ No replication to followers
- ❌ Can't track which replicas are in-sync
- ❌ Can't determine ISR membership
- ❌ acks=-1 has no ISR to wait for (causes timeouts)

### Files That Need Changes

| File | Status | Required Change |
|------|--------|-----------------|
| `isr_tracker.rs` | ❌ Doesn't exist | Create new file with IsrTracker |
| `wal_replication.rs` | Topic-level only | Add partition-level `replicate_partition()` |
| `produce_handler.rs` | No ISR awareness | Check ISR before replicating |
| `integrated_server.rs` | No IsrTracker | Add IsrTracker component |

### Blockers
- **Phase 1 required**: Per-partition WAL must exist first
- **Phase 2 completion required**: Raft metadata queries must work

### Effort Estimate
**3 days** (as per original plan)

---

## Phase 4: acks=-1 Quorum Support ❌ NOT IMPLEMENTED

### What's Missing

**No quorum waiting mechanism**:
```rust
// Currently: acks=-1 immediately returns after local WAL write
match acks {
    -1 => {
        wal_write(data).await;
        Ok(offset)  // ❌ Returns immediately, no ISR wait!
    }
}
```

**Required: Wait for ISR quorum**:
```rust
match acks {
    -1 => {
        let offset = wal_write(data).await;

        // Register oneshot channel for this offset
        let (tx, rx) = oneshot::channel();
        isr_ack_tracker.register_wait(topic, partition, offset, tx);

        // Wait for ISR quorum (with 30s timeout)
        tokio::time::timeout(Duration::from_secs(30), rx).await??;

        Ok(offset)
    }
}
```

### Required Components

1. **IsrAckTracker** (NEW component)
   ```rust
   pub struct IsrAckTracker {
       pending_acks: DashMap<(String, i32, i64), PendingAck>,  // (topic, partition, offset)
   }

   struct PendingAck {
       required_quorum: usize,
       acks_received: HashSet<u64>,  // node IDs
       notifier: Option<oneshot::Sender<()>>,
   }
   ```

2. **Follower ACK Handler** (NEW RPC)
   ```rust
   // Followers send ACK after successful replication
   pub async fn handle_follower_ack(&self, topic: &str, partition: i32, offset: i64, node_id: u64) {
       isr_ack_tracker.record_ack(topic, partition, offset, node_id);

       if isr_ack_tracker.has_quorum(topic, partition, offset) {
           isr_ack_tracker.notify(topic, partition, offset);  // Wakes waiting producer
       }
   }
   ```

### Impact

**Current Behavior**:
- ❌ **acks=-1 produces TIMEOUT** (no ISR to wait for)
- ❌ No quorum guarantee (data only on leader)
- ❌ Not Kafka-compatible (acks=-1 should wait for replicas)

**Test Evidence**:
```
ERROR acks=-1: ISR quorum timeout for test-leader-election-0 offset 155 after 30s
```

### Files That Need Changes

| File | Status | Required Change |
|------|--------|-----------------|
| `isr_ack_tracker.rs` | ❌ Doesn't exist | Create new quorum tracking component |
| `produce_handler.rs` | Returns immediately | Add quorum wait for acks=-1 |
| `wal_replication.rs` | No ACK mechanism | Followers send ACKs after replication |

### Blockers
- **Phase 3 required**: ISR tracking must exist first

### Effort Estimate
**1 day** (as per original plan)

---

## Phase 5: Partition Leader Election ❌ NOT IMPLEMENTED

### What's Implemented ✅

**Cluster leader election** - Working
- 3-node cluster elects a cluster leader
- Leader maintains heartbeats
- Followers acknowledge leadership

### What's Missing ❌

**Partition leader election** - NOT IMPLEMENTED

This is different from cluster leader election!

#### Cluster Leader vs Partition Leader

| Type | Purpose | Current Status |
|------|---------|----------------|
| **Cluster Leader** | Coordinates Raft consensus for metadata | ✅ Working |
| **Partition Leader** | Handles produce/fetch for specific partition | ❌ Not implemented |

**Example**:
```
Cluster has 3 nodes: [1, 2, 3]
Topic "orders" has 3 partitions: [0, 1, 2]

Cluster Leader: Node 1 (coordinates Raft)

Partition Leaders (should be):
- orders-0: Node 1 (handles produce/fetch for partition 0)
- orders-1: Node 2 (handles produce/fetch for partition 1)
- orders-2: Node 3 (handles produce/fetch for partition 2)
```

### Required Implementation

1. **Metadata Proposal on Topic Creation**
   ```rust
   // When topic is created, propose partition assignments
   for partition in 0..num_partitions {
       raft_cluster.propose(MetadataCommand::AssignPartition {
           topic: topic.clone(),
           partition,
           replicas: vec![1, 2, 3],  // All nodes
       }).await;

       raft_cluster.propose(MetadataCommand::SetPartitionLeader {
           topic: topic.clone(),
           partition,
           leader: assign_leader_for_partition(partition),  // Round-robin
       }).await;
   }
   ```

2. **Leader Election on Failure**
   ```rust
   impl MetadataStateMachine {
       pub fn elect_partition_leader(&mut self, topic: &str, partition: i32) -> u64 {
           let isr = self.isr_sets.get(&(topic, partition))?;

           // Pick first ISR member as new leader
           let new_leader = isr.first()?;
           self.partition_leaders.insert((topic, partition), *new_leader);

           new_leader
       }
   }
   ```

3. **Heartbeat Monitoring**
   ```rust
   // Monitor partition leader heartbeats
   // If leader stops sending produce/fetch heartbeats, trigger election
   ```

### Impact

**Current Limitations**:
- ❌ All partitions go to same node (no load balancing)
- ❌ No failover if partition leader dies
- ❌ Can't redistribute partitions across cluster

### Files That Need Changes

| File | Status | Required Change |
|------|--------|-----------------|
| `raft_metadata.rs` | No leader election logic | Add `elect_partition_leader()` |
| `integrated_server.rs` | No partition assignment | Propose assignments on topic create |
| `produce_handler.rs` | No leader routing | Route produce to partition leader |
| `leader_election.rs` | Cluster leader only | Add partition leader monitoring |

### Blockers
- **Phase 3 required**: ISR sets must exist for leader election

### Effort Estimate
**1-2 days** (as per original plan)

---

## Critical Path to Working Cluster

### Minimum Viable Implementation

To get a **minimally working Raft cluster** with proper failover:

#### Option A: Full Implementation (9-11 days)
Follow the 5-phase plan exactly:
1. Phase 1: Per-partition WAL (2-3 days)
2. Phase 2 completion: Metadata proposals (1-2 days)
3. Phase 3: ISR tracking + replication (3 days)
4. Phase 4: acks=-1 quorum (1 day)
5. Phase 5: Partition leader election (1-2 days)

#### Option B: Simplified Path (3-4 days)
Skip per-partition WAL, implement subset:

1. **Phase 2 completion**: Metadata proposals (1 day)
   - Wire topic creation to Raft proposals
   - Query assignments from state machine

2. **Phase 3 lite**: Basic ISR tracking (1 day)
   - Track follower offsets (no per-partition WAL needed)
   - Simple is_in_sync() check

3. **Phase 4 lite**: Basic acks=-1 (1 day)
   - Simple quorum counter
   - Timeout if quorum not reached

4. **Phase 5 lite**: Basic partition leader election (1 day)
   - Static assignment on topic creation
   - No dynamic rebalancing

**Tradeoff**: Simplified path gives working cluster but:
- ❌ Can't assign partitions to specific nodes
- ❌ Can't move partitions between nodes
- ❌ Performance may be lower (topic-level WAL contention)

---

## Recommendations

### Immediate Next Steps

**Recommendation**: Complete Phase 2 first (finish what we started)

1. **Wire metadata proposals** (4 hours)
   - Call `raft_cluster.propose()` on topic creation
   - Apply committed entries to state machine
   - Query state machine in produce/fetch handlers

2. **Test metadata replication** (2 hours)
   - Create topic on node 1
   - Verify metadata appears on nodes 2 and 3
   - Kill node 1, verify metadata persists

3. **Persist Raft state to disk** (2 hours)
   - Replace MemStorage with disk-backed storage
   - Verify metadata survives restarts

**Total**: 1 day to complete Phase 2

### Long-Term Path

After Phase 2 is complete:
- **Week 1**: Phase 1 (per-partition WAL)
- **Week 2**: Phase 3 (ISR + replication)
- **Week 3**: Phase 4 + 5 (acks=-1 + leader election)

**Total**: ~3 weeks for full Kafka-compatible cluster

---

## Testing Gaps

### What CAN'T Be Tested Yet

| Test Scenario | Blocker | Phase Required |
|---------------|---------|----------------|
| acks=-1 produces | No ISR tracking | Phase 3 + 4 |
| Leader failover | No partition leaders | Phase 5 |
| Partition reassignment | No per-partition WAL | Phase 1 |
| ISR shrink/grow | No ISR tracker | Phase 3 |
| Quorum loss handling | No acks=-1 support | Phase 4 |

### What CAN Be Tested Now

| Test Scenario | Status | Evidence |
|---------------|--------|----------|
| Cluster leader election | ✅ Working | Node 1 became leader at term 2 |
| Raft message send/receive | ✅ Working | TCP messages sent successfully |
| Heartbeat maintenance | ✅ Working | MsgAppend sent every 300ms |
| Follower acknowledgment | ✅ Working | Nodes 2,3 became followers |

---

## Summary

**What We Have**: Working Raft cluster leader election with TCP messaging

**What We Need**:
1. Complete Phase 2 (metadata proposals)
2. Implement Phases 1, 3, 4, 5 (per-partition WAL, ISR, acks=-1, partition leaders)

**Estimated Time to Full Cluster**:
- Fast path (simplified): 3-4 days
- Full path (production-ready): 9-11 days

**Immediate Blocker**: Phase 2 metadata proposals not wired up

**Recommendation**: Complete Phase 2 (1 day), then decide on full vs simplified path.
