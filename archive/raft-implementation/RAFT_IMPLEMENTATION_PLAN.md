# Raft Clustering Implementation Plan

**Date**: 2025-10-16
**Status**: IN PROGRESS
**Target**: Production-ready Raft consensus for Chronik partitions

---

## Current State Assessment

### ✅ Already Implemented

1. **Basic Infrastructure**:
   - `PartitionReplica` - Raft state machine per partition (using TiKV raft-rs)
   - `RaftGroupManager` - Multi-partition replica management
   - `RaftServiceImpl` - gRPC service skeleton
   - ISR manager stub
   - Lease-based reads framework
   - Graceful shutdown handling

2. **ProduceHandler Integration Points**:
   - Inline WAL writes (working, durable)
   - Raft feature flag check (#[cfg(feature = "raft")])
   - Placeholder for Raft propose (lines 1244-1280)

### ❌ Critical Gaps (Production Blockers)

1. **Network Layer (RPC)**:
   - `AppendEntries` RPC - returns placeholder
   - `RequestVote` RPC - returns placeholder
   - `InstallSnapshot` RPC - returns placeholder
   - No message routing between nodes

2. **State Machine Integration**:
   - `TODO: Apply committed entries to state machine`
   - No integration with WAL/Storage

3. **Raft-based Produce Path**:
   - ProduceHandler has Raft check but doesn't actually propose
   - Need to propose writes through Raft log

4. **Leader Election**:
   - Tick loop exists but messages aren't sent
   - No peer communication

5. **ISR Tracking**:
   - ISR manager exists but not connected

---

## Implementation Phases

### Phase 1: Complete Network Layer (HIGH PRIORITY)

**Goal**: Enable Raft nodes to communicate with each other

**Tasks**:
1. ✅ Implement `AppendEntries` RPC handler
   - Extract topic/partition from request
   - Route to correct PartitionReplica
   - Call `replica.step(msg)` with Raft message
   - Return proper response

2. ✅ Implement `RequestVote` RPC handler
   - Same routing as AppendEntries
   - Handle vote requests properly

3. ✅ Implement `InstallSnapshot` RPC handler
   - Handle snapshot transfer
   - Apply snapshot to state machine

4. ✅ Implement message sending in group_manager
   - Replace `TODO: Send messages to peers via RPC`
   - Use tonic gRPC client to send messages
   - Handle connection pooling

**Acceptance Criteria**:
- Leader election completes successfully
- Heartbeats are sent and received
- Log replication starts (even if state machine doesn't apply yet)

---

### Phase 2: State Machine Integration

**Goal**: Apply committed Raft entries to WAL/Storage

**Tasks**:
1. ✅ Create `RaftStateMachine` trait
   - `apply(entry)` - apply committed log entry
   - `snapshot()` - create snapshot
   - `restore(snapshot)` - restore from snapshot

2. ✅ Implement `WalStateMachine`
   - Integrates with WalManager
   - Applies entries as WAL writes
   - Snapshots = WAL segments

3. ✅ Connect in group_manager
   - Replace `TODO: Apply committed entries to state machine`
   - Call state_machine.apply() for each committed entry

4. ✅ Add to PartitionReplica
   - Pass state_machine to replica
   - Call on commit index advance

**Acceptance Criteria**:
- Raft-committed entries are applied to WAL
- Data is durable and replicated
- Snapshots work correctly

---

### Phase 3: Raft-based Produce Path

**Goal**: Route produce requests through Raft consensus

**Tasks**:
1. ✅ Implement `propose_write()` in PartitionReplica
   - Serialize CanonicalRecord
   - Call `raw_node.propose(data)`
   - Wait for commit (via oneshot channel)

2. ✅ Update ProduceHandler
   - Replace placeholder Raft code
   - Call `raft_manager.propose()` instead of direct WAL write
   - Keep WAL write as fallback for non-Raft partitions

3. ✅ Add write acknowledgment
   - Track pending proposals
   - Complete when entry is committed
   - Return to producer

4. ✅ Handle leader redirection
   - If not leader, return NOT_LEADER_FOR_PARTITION error
   - Include current leader ID in error

**Acceptance Criteria**:
- Produce requests go through Raft log
- Only committed writes are acknowledged
- Leader election triggers producer retry
- No data loss during failover

---

### Phase 4: ISR Management

**Goal**: Track in-sync replicas for partition health

**Tasks**:
1. ✅ Connect ISR manager to PartitionReplica
   - Track follower lag (commit_index vs applied_index)
   - Mark replica as in-sync if lag < threshold

2. ✅ Add ISR health checks
   - Periodically check follower progress
   - Remove slow followers from ISR
   - Add caught-up followers to ISR

3. ✅ Expose ISR via metadata
   - Update partition metadata with ISR list
   - Return ISR in Metadata API

4. ✅ Add min ISR check
   - Reject writes if ISR < min_isr (configurable)
   - Protect against data loss

**Acceptance Criteria**:
- ISR reflects actual replication state
- Slow followers are detected
- Caught-up followers rejoin ISR
- Metadata API shows current ISR

---

### Phase 5: Comprehensive Testing

**Goal**: Chaos testing and production readiness

**Tasks**:
1. ✅ Leader election test
   - Kill leader, verify new leader elected
   - Verify no data loss

2. ✅ Network partition test
   - Split cluster, verify quorum behavior
   - Verify minority can't commit

3. ✅ Crash recovery test
   - Kill node, restart, verify rejoins cluster
   - Verify data is recovered

4. ✅ Slow follower test
   - Add artificial lag
   - Verify removed from ISR
   - Verify catches up and rejoins

5. ✅ Load test with failover
   - High throughput writes
   - Kill leader mid-test
   - Verify zero data loss
   - Measure recovery time

**Acceptance Criteria**:
- All tests pass consistently
- No data loss in any scenario
- Recovery time < 5 seconds
- Performance degradation < 20% vs standalone

---

## Technical Design Decisions

### 1. One Raft Group Per Partition (✅ DECIDED)

**Why**:
- Kafka semantics require partition-level consistency
- Allows independent scaling of partitions
- Matches Kafka's design

**How**:
- RaftGroupManager creates one PartitionReplica per (topic, partition)
- Each replica has its own Raft log
- Messages are routed by (topic, partition) key

### 2. TiKV raft-rs (✅ DECIDED)

**Why**:
- Production-proven (used in TiKV, PingCAP)
- Mature, stable API
- Good performance
- OpenRaft is pre-1.0 (not production-ready yet)

**Trade-offs**:
- Tick-based (not fully async)
- More manual integration
- But: More control, better understood

### 3. WAL as State Machine (✅ DECIDED)

**Why**:
- WAL is already the source of truth
- Raft log + WAL = double durability
- Snapshots map naturally to WAL segments

**How**:
- RaftStateMachine wraps WalManager
- Committed entries → WAL writes
- Snapshots = sealed WAL segments

### 4. gRPC for RPC (✅ DECIDED)

**Why**:
- Industry standard
- Good performance
- Protobuf already used by raft-rs
- Tonic integration is mature

**How**:
- proto/raft.proto defines service
- RaftServiceImpl implements server
- RaftClient wraps tonic client
- Connection pooling per peer

---

## Implementation Order (Prioritized)

1. **Week 1**: Network Layer (Phase 1)
   - Implement all RPC handlers
   - Add message sending in group_manager
   - Test: Leader election works

2. **Week 2**: State Machine Integration (Phase 2)
   - Implement WalStateMachine
   - Connect to group_manager
   - Test: Log replication + apply works

3. **Week 3**: Raft Produce Path (Phase 3)
   - Update ProduceHandler
   - Add propose + wait logic
   - Test: End-to-end produce through Raft

4. **Week 4**: ISR + Polish (Phase 4)
   - ISR tracking
   - Health checks
   - Metadata integration
   - Test: ISR reflects reality

5. **Week 5**: Chaos Testing (Phase 5)
   - All 5 test scenarios
   - Performance benchmarks
   - Production readiness assessment

---

## Success Metrics

| Metric | Target | Current |
|--------|--------|---------|
| Leader election time | < 5s | N/A |
| Failover time | < 5s | N/A |
| Data loss events | 0 | N/A |
| Performance overhead | < 20% | N/A |
| ISR accuracy | 100% | N/A |
| Test pass rate | 100% | N/A |

---

## Next Immediate Actions

1. ✅ Create this plan document
2. ⏳ Implement AppendEntries RPC handler (30 min)
3. ⏳ Implement RequestVote RPC handler (20 min)
4. ⏳ Implement message sending in group_manager (45 min)
5. ⏳ Test: Can elect a leader? (15 min)

**Estimated time to Phase 1 complete**: 2 hours
