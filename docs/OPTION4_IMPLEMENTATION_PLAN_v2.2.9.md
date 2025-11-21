# Option 4: WAL-Only Metadata Implementation Plan - v2.2.9

**Version**: 2.2.9
**Target Release Date**: TBD
**Status**: 🔴 NOT STARTED
**Overall Progress**: 0% (0/7 phases complete)

---

## Executive Summary

**Goal**: Remove Raft metadata redundancy, use `__chronik_metadata` WAL as the single source of truth for partition assignments, leaders, and ISR tracking. Keep Raft ONLY for cluster membership and leader election.

**Current State**: v2.2.8 has hybrid approach (both Raft + WAL metadata)
**Target State**: v2.2.9 uses WAL-only metadata (Raft only for leadership)

**Key Insight from Analysis**: Codebase is **80%+ ready**. The infrastructure (MetadataWal, WalMetadataStore, replication) is complete. We only need to wire it up by:
- Switching 3 Raft queries to metadata_store queries
- Adding 3 write methods to ProduceHandler
- Updating initialization

**Expected Performance Improvement**:
- Topic creation: 20+ seconds → <100ms (**200x faster**)
- Partition operations: ~500ms → ~2ms (**250x faster**)
- Metadata throughput: ~1,500 ops/s → 10,000+ ops/s (**6-7x**)

---

## Implementation Phases

### Phase 0: Preparation & Documentation ⏳ NOT STARTED

**Goal**: Create tracking infrastructure and verify current state

**Tasks**:
- [ ] Create this implementation plan document
- [ ] Create implementation tracker (separate file)
- [ ] Review all analysis findings from planning agent
- [ ] Verify current cluster is working (baseline test)
- [ ] Document current Raft metadata behavior
- [ ] Create rollback plan

**Verification**:
```bash
# Baseline test - should pass before starting
./tests/cluster/start.sh
python3 -c "
from kafka import KafkaProducer, KafkaConsumer
from kafka import TopicPartition
import time

# Produce 1000 messages
producer = KafkaProducer(bootstrap_servers=['localhost:9092'], acks='all')
for i in range(1000):
    producer.send('baseline-test', f'msg-{i}'.encode())
producer.flush()
producer.close()

time.sleep(2)

# Verify on all 3 nodes
for port in [9092, 9093, 9094]:
    consumer = KafkaConsumer(bootstrap_servers=[f'localhost:{port}'])
    tp = TopicPartition('baseline-test', 0)
    offsets = consumer.end_offsets([tp])
    print(f'Node {port}: watermark={offsets[tp]}')
    assert offsets[tp] == 1000, f'Node {port} missing data!'
    consumer.close()

print('✅ Baseline test passed - all nodes have 1000 messages')
"
```

**Files**:
- `docs/OPTION4_IMPLEMENTATION_PLAN_v2.2.9.md` (this file)
- `docs/OPTION4_IMPLEMENTATION_TRACKER.md` (progress tracker)
- `docs/OPTION4_ROLLBACK_PLAN.md` (if things go wrong)

**Duration**: 30 minutes

---

### Phase 1: Add MetadataStore Methods to ProduceHandler 🔴 NOT STARTED

**Goal**: Give ProduceHandler the ability to write partition metadata directly to metadata_store (bypassing Raft)

**Current State**:
- ProduceHandler has: `update_high_watermark()`, `auto_create_topic()`
- ProduceHandler queries: Raft for partition leader, ISR, nodes

**Target State**:
- ProduceHandler has: 3 new methods for partition metadata writes
- ProduceHandler queries: metadata_store for all partition info

**New Methods to Add**:

```rust
impl ProduceHandler {
    /// Assign a partition to replica nodes (writes to metadata WAL)
    pub async fn assign_partition(
        &self,
        topic: &str,
        partition: i32,
        replicas: Vec<u64>,
    ) -> Result<()> {
        // Write AssignPartition event to metadata_store
        // metadata_store will persist to __chronik_metadata WAL
        // WAL replication will propagate to followers

        self.metadata_store
            .assign_partition(topic, partition, replicas)
            .await
    }

    /// Set partition leader (writes to metadata WAL)
    pub async fn set_partition_leader(
        &self,
        topic: &str,
        partition: i32,
        leader: u64,
    ) -> Result<()> {
        self.metadata_store
            .set_partition_leader(topic, partition, leader)
            .await
    }

    /// Update in-sync replica set (writes to metadata WAL)
    pub async fn update_isr(
        &self,
        topic: &str,
        partition: i32,
        isr: Vec<u64>,
    ) -> Result<()> {
        self.metadata_store
            .update_isr(topic, partition, isr)
            .await
    }
}
```

**Tasks**:
- [ ] Add `assign_partition()` method to ProduceHandler
- [ ] Add `set_partition_leader()` method to ProduceHandler
- [ ] Add `update_isr()` method to ProduceHandler
- [ ] Verify metadata_store trait has these methods (it should via WalMetadataStore)
- [ ] Add unit tests for each new method
- [ ] Build and verify compilation

**Files to Modify**:
- `crates/chronik-server/src/produce_handler.rs` (+60 lines)

**Verification**:
```bash
cargo test --lib -p chronik-server produce_handler::tests::test_assign_partition
cargo test --lib -p chronik-server produce_handler::tests::test_set_partition_leader
cargo test --lib -p chronik-server produce_handler::tests::test_update_isr
```

**Duration**: 1-2 hours

---

### Phase 2: Replace Raft Queries with MetadataStore Queries 🔴 NOT STARTED

**Goal**: Remove Raft dependency from partition metadata lookups

**Current Code** (3 locations to fix):

**Location 1**: `produce_handler.rs:1166` - Get partition leader
```rust
// BEFORE
if let Some(ref raft) = self.raft_cluster {
    if let Some(leader_id) = raft.get_partition_leader(&topic_name, partition_data.index) {
        // ... use leader_id
    }
}

// AFTER
if let Some(leader_id) = self.metadata_store
    .get_partition_leader(&topic_name, partition_data.index)
    .await?
{
    // ... use leader_id
}
```

**Location 2**: `produce_handler.rs:1883` - Get ISR
```rust
// BEFORE
if let Some(ref raft) = self.raft_cluster {
    if let Some(isr) = raft.get_isr(topic, partition) {
        // ... use isr
    }
}

// AFTER
if let Some(isr) = self.metadata_store
    .get_isr(topic, partition)
    .await?
{
    // ... use isr
}
```

**Location 3**: `produce_handler.rs:2633` - Get all nodes
```rust
// BEFORE
let nodes = if let Some(ref raft) = self.raft_cluster {
    raft.get_all_nodes().await
} else {
    vec![]
};

// AFTER
let nodes = self.metadata_store
    .get_all_nodes()
    .await?;
```

**Tasks**:
- [ ] Replace `raft.get_partition_leader()` with `metadata_store.get_partition_leader()`
- [ ] Replace `raft.get_isr()` with `metadata_store.get_isr()`
- [ ] Replace `raft.get_all_nodes()` with `metadata_store.get_all_nodes()`
- [ ] Remove `if let Some(ref raft)` conditionals (metadata_store is always present)
- [ ] Update error handling (metadata_store returns Result)
- [ ] Build and verify compilation

**Files to Modify**:
- `crates/chronik-server/src/produce_handler.rs` (3 locations)

**Verification**:
```bash
cargo build --release --bin chronik-server
# Should compile without warnings about unused raft_cluster
```

**Duration**: 1 hour

---

### Phase 3: Update Topic Creation to Use WAL Metadata 🔴 NOT STARTED

**Goal**: Remove Raft consensus from topic creation, use WAL metadata instead

**Current Code**: `produce_handler.rs:2674` - Auto-create topic via Raft
```rust
// BEFORE
if let Some(ref raft) = self.raft_cluster {
    // Create topic via Raft consensus
    let cmd = MetadataCommand::CreateTopic { ... };
    raft.propose(vec![], bincode::serialize(&cmd)?)?;
}
```

**After**: Topic creation via metadata_store
```rust
// Create topic via metadata_store (writes to __chronik_metadata WAL)
self.metadata_store
    .create_topic(topic_name, topic_config)
    .await?;

// Assign partitions to replicas
for partition in 0..num_partitions {
    self.assign_partition(topic_name, partition, replicas.clone()).await?;
}

// Set partition leaders
for partition in 0..num_partitions {
    self.set_partition_leader(topic_name, partition, leader_id).await?;
}
```

**Tasks**:
- [ ] Identify all topic creation call sites (auto-create + admin API)
- [ ] Replace Raft-based creation with metadata_store writes
- [ ] Ensure partition assignment happens via new `assign_partition()` method
- [ ] Ensure leader assignment happens via new `set_partition_leader()` method
- [ ] Remove Raft proposal calls
- [ ] Build and verify compilation

**Files to Modify**:
- `crates/chronik-server/src/produce_handler.rs` (auto-create topic)
- `crates/chronik-server/src/admin_api.rs` (admin create topic, if applicable)

**Verification**:
```bash
# Test auto-create topic
python3 -c "
from kafka import KafkaProducer
import uuid
topic = f'test-{uuid.uuid4().hex[:8]}'
producer = KafkaProducer(bootstrap_servers=['localhost:9092'], acks=1)
producer.send(topic, b'test')
producer.flush()
print(f'✅ Created topic: {topic}')
"
```

**Duration**: 2 hours

---

### Phase 4: Update Partition Rebalancer 🔴 NOT STARTED

**Goal**: Get node list from metadata_store instead of Raft

**Current Code**: `partition_rebalancer.rs:71, 102` - Get nodes from Raft
```rust
// BEFORE
if let Some(ref raft) = self.raft_cluster {
    let nodes = raft.get_all_nodes().await;
    // ... rebalance
}
```

**After**: Get nodes from metadata_store
```rust
let nodes = self.metadata_store
    .get_all_nodes()
    .await?;
// ... rebalance
```

**Tasks**:
- [ ] Update partition_rebalancer to accept metadata_store instead of raft_cluster
- [ ] Replace `raft.get_all_nodes()` with `metadata_store.get_all_nodes()`
- [ ] Verify rebalancer still triggers correctly
- [ ] Build and verify compilation

**Files to Modify**:
- `crates/chronik-server/src/partition_rebalancer.rs`
- `crates/chronik-server/src/integrated_server.rs` (pass metadata_store to rebalancer)

**Verification**:
```bash
# Test node addition (should trigger rebalance)
./target/release/chronik-server cluster add-node 4 \
  --kafka localhost:9095 \
  --wal localhost:9294 \
  --raft localhost:5004 \
  --config tests/cluster/node1.toml

# Verify partitions were rebalanced
./target/release/chronik-server cluster status --config tests/cluster/node1.toml
```

**Duration**: 1-2 hours

---

### Phase 5: Update Server Initialization 🔴 NOT STARTED

**Goal**: Ensure WalMetadataStore is used instead of Raft metadata

**Current State**:
- `main.rs` may initialize Raft-based metadata
- Need to verify WalMetadataStore is used

**Tasks**:
- [ ] Review `main.rs` initialization code
- [ ] Verify `WalMetadataStore` is instantiated (should already be)
- [ ] Remove any Raft metadata store initialization
- [ ] Ensure metadata_store is passed to ProduceHandler
- [ ] Ensure metadata_store is passed to all components that need it
- [ ] Build and verify compilation

**Files to Modify**:
- `crates/chronik-server/src/main.rs`
- `crates/chronik-server/src/integrated_server.rs`

**Verification**:
```bash
cargo build --release --bin chronik-server
./tests/cluster/stop.sh
./tests/cluster/start.sh
# Should start without errors
```

**Duration**: 1 hour

---

### Phase 6: Clean Up Raft Metadata State Machine 🔴 NOT STARTED

**Goal**: Remove partition metadata from Raft state machine, keep only cluster membership

**Current State**: `raft_metadata.rs:MetadataStateMachine` has:
- Partition assignments
- Partition leaders
- ISR tracking
- Cluster membership
- Broker registry

**Target State**: Keep only:
- Cluster membership (nodes)
- Raft leader ID

**Tasks**:
- [ ] Comment out (don't delete yet) partition-related fields in MetadataStateMachine
- [ ] Comment out partition-related command handling in `apply()`
- [ ] Keep cluster membership fields (nodes, raft_leader)
- [ ] Update state machine serialization/deserialization
- [ ] Build and verify compilation

**Files to Modify**:
- `crates/chronik-server/src/raft_metadata.rs`

**Why Comment Instead of Delete**:
- Easier rollback if needed
- Can clean up in v2.2.10 after v2.2.9 is stable
- Reduces risk

**Verification**:
```bash
cargo build --release --bin chronik-server
# Should compile without errors
```

**Duration**: 1 hour

---

### Phase 7: Integration Testing & Validation 🔴 NOT STARTED

**Goal**: Comprehensive testing to ensure WAL-only metadata works correctly

**Test Suite**:

#### Test 1: Basic Replication (acks=all)
```bash
./tests/cluster/stop.sh
rm -rf tests/cluster/data/*
./tests/cluster/start.sh

python3 -c "
from kafka import KafkaProducer, KafkaConsumer
from kafka import TopicPartition
import uuid

topic = f'replication-test-{uuid.uuid4().hex[:8]}'
producer = KafkaProducer(bootstrap_servers=['localhost:9092'], acks='all')

# Produce 1000 messages
for i in range(1000):
    producer.send(topic, f'msg-{i}'.encode())
producer.flush()
producer.close()

import time
time.sleep(2)

# Verify on all 3 nodes
for port in [9092, 9093, 9094]:
    consumer = KafkaConsumer(bootstrap_servers=[f'localhost:{port}'])
    tp = TopicPartition(topic, 0)
    offsets = consumer.end_offsets([tp])
    print(f'Node {port}: watermark={offsets[tp]}')
    assert offsets[tp] == 1000, f'Node {port} missing data!'
    consumer.close()

print('✅ PASS: All nodes have 1000 messages with acks=all')
"
```

#### Test 2: Topic Auto-Creation Speed
```bash
python3 -c "
from kafka import KafkaProducer
import time
import uuid

topic = f'speed-test-{uuid.uuid4().hex[:8]}'
producer = KafkaProducer(bootstrap_servers=['localhost:9092'], acks=1, request_timeout_ms=10000)

start = time.time()
producer.send(topic, b'test').get(timeout=10)
elapsed = time.time() - start

print(f'Topic creation took: {elapsed:.2f}s')
assert elapsed < 1.0, f'Too slow: {elapsed:.2f}s (expected <1s)'
print('✅ PASS: Topic creation <1s (WAL metadata is fast!)')
"
```

#### Test 3: Leader Failover
```bash
# Start cluster
./tests/cluster/start.sh

# Identify leader
LEADER_PID=$(ps aux | grep chronik-server | grep node3 | awk '{print $2}' | head -1)

# Kill leader
kill $LEADER_PID

# Wait for election
sleep 5

# Produce to new leader
python3 -c "
from kafka import KafkaProducer
producer = KafkaProducer(bootstrap_servers=['localhost:9092,localhost:9093'], acks=1)
producer.send('failover-test', b'after-failover').get(timeout=10)
print('✅ PASS: Produce succeeded after leader failover')
"
```

#### Test 4: Follower Restart with Metadata Recovery
```bash
# Stop Node 1
./tests/cluster/stop.sh node1

# Create topic on remaining nodes
python3 -c "
from kafka import KafkaProducer
producer = KafkaProducer(bootstrap_servers=['localhost:9093'], acks=1)
producer.send('recovery-test', b'msg1')
producer.flush()
print('Created topic while Node 1 down')
"

# Restart Node 1
./tests/cluster/start.sh node1

# Wait for metadata sync
sleep 5

# Verify Node 1 knows about topic
python3 -c "
from kafka import KafkaConsumer
from kafka import TopicPartition
consumer = KafkaConsumer(bootstrap_servers=['localhost:9092'])
tp = TopicPartition('recovery-test', 0)
offsets = consumer.end_offsets([tp])
print(f'Node 1 watermark: {offsets[tp]}')
assert offsets[tp] >= 0, 'Node 1 does not know about topic!'
print('✅ PASS: Node 1 recovered metadata after restart')
"
```

#### Test 5: High Metadata Churn (1000 Topics)
```bash
python3 -c "
from kafka import KafkaProducer
import time

producer = KafkaProducer(bootstrap_servers=['localhost:9092'], acks=1)

start = time.time()
for i in range(1000):
    topic = f'churn-test-{i}'
    producer.send(topic, b'test')
    if i % 100 == 0:
        print(f'Created {i} topics...')

producer.flush()
elapsed = time.time() - start

print(f'Created 1000 topics in {elapsed:.2f}s')
print(f'Average: {elapsed/1000:.3f}s per topic')
assert elapsed < 120, f'Too slow: {elapsed:.2f}s (expected <120s)'
print('✅ PASS: 1000 topics created (WAL metadata handles churn)')
"
```

#### Test 6: Benchmark Performance
```bash
./target/release/chronik-bench \
  --bootstrap-servers localhost:9092,localhost:9093,localhost:9094 \
  --topic perf-test \
  --mode produce \
  --concurrency 128 \
  --message-size 1024 \
  --duration 30s \
  --acks all \
  --partitions 3 \
  --replication-factor 3 \
  --create-topic

# Expected: >50K msg/s with acks=all
```

**Tasks**:
- [ ] Run Test 1: Basic Replication
- [ ] Run Test 2: Topic Creation Speed
- [ ] Run Test 3: Leader Failover
- [ ] Run Test 4: Follower Restart
- [ ] Run Test 5: High Metadata Churn
- [ ] Run Test 6: Benchmark Performance
- [ ] Document any failures and fix
- [ ] Re-run failed tests until all pass

**Verification**:
All 6 tests must pass before v2.2.9 can be released.

**Duration**: 3-4 hours

---

## Implementation Tracker

See `docs/OPTION4_IMPLEMENTATION_TRACKER.md` for session-by-session progress tracking.

---

## Rollback Plan

If Option 4 causes issues in v2.2.9:

**Immediate Rollback** (revert to v2.2.8):
```bash
git tag -d v2.2.9
git reset --hard v2.2.8
cargo build --release --bin chronik-server
./tests/cluster/stop.sh
./tests/cluster/start.sh
```

**Partial Rollback** (keep infrastructure, re-enable Raft queries):
1. Uncomment Raft metadata state machine fields
2. Revert produce_handler.rs Raft query removals
3. Keep new ProduceHandler methods (they're additive, safe)
4. Test hybrid approach again

**Data Safety**: No data loss during rollback
- `__chronik_metadata` WAL is persistent
- Partition data unaffected
- Raft state machine can be rebuilt from WAL events

---

## Success Criteria for v2.2.9 Release

**Must Pass**:
- ✅ All 6 integration tests pass
- ✅ No data loss with acks=all
- ✅ Topic creation <1s (vs 20+ seconds in v2.2.8)
- ✅ Cluster survives leader failover
- ✅ Follower restart recovers metadata
- ✅ 1000 topics can be created without timeout
- ✅ Benchmark shows >50K msg/s with acks=all

**Performance Targets**:
- Topic creation: <100ms (currently 20+ seconds)
- Partition assignment: <10ms (currently ~500ms)
- Metadata ops/sec: >10,000 (currently ~1,500)

**Documentation**:
- Update CLAUDE.md with v2.2.9 changes
- Update README.md with performance improvements
- Create OPTION4_COMPLETE.md summary document

---

## Timeline Estimate

**Total Time**: 10-15 hours across multiple sessions

| Phase | Duration | Complexity |
|-------|----------|------------|
| Phase 0: Preparation | 30 min | Low |
| Phase 1: Add Methods | 1-2 hours | Low |
| Phase 2: Replace Queries | 1 hour | Low |
| Phase 3: Topic Creation | 2 hours | Medium |
| Phase 4: Rebalancer | 1-2 hours | Low |
| Phase 5: Initialization | 1 hour | Low |
| Phase 6: Clean Up | 1 hour | Low |
| Phase 7: Testing | 3-4 hours | High |

**Recommended Sessions**:
- Session 1: Phases 0-1 (2 hours)
- Session 2: Phases 2-3 (3 hours)
- Session 3: Phases 4-5 (2 hours)
- Session 4: Phase 6 (1 hour)
- Session 5: Phase 7 (4 hours)

---

## Risk Assessment

**Low Risk**:
- Infrastructure already exists (MetadataWal, WalMetadataStore)
- MetadataStore trait provides clean abstraction
- Phased approach with testing at each step
- Easy rollback (comment/uncomment code)

**Medium Risk**:
- Topic creation speed improvement (expect 200x faster)
- Need to ensure metadata arrives before data (ordering)
- Partition rebalancer node discovery

**Mitigation**:
- Test after each phase
- Keep Raft code commented (not deleted) for easy rollback
- Run comprehensive test suite before release

---

## Questions & Decisions

### Q1: Should we delete Raft metadata immediately or keep commented?
**Decision**: Keep commented in v2.2.9, delete in v2.2.10 after stability proven.

### Q2: What if metadata_store doesn't have all needed methods?
**Answer**: WalMetadataStore implements full MetadataStore trait (verified by agent). Methods exist.

### Q3: How do we handle leader election without Raft metadata?
**Answer**: Keep Raft for leader election (cluster membership). Only remove partition metadata.

### Q4: What about cluster add/remove node?
**Answer**: Already uses Admin API + Raft cluster management. No changes needed.

---

## References

- [OPTION4_WAL_ONLY_METADATA_DESIGN.md](OPTION4_WAL_ONLY_METADATA_DESIGN.md) - Design document
- [RAFT_VS_WAL_METADATA_REDUNDANCY_ANALYSIS.md](RAFT_VS_WAL_METADATA_REDUNDANCY_ANALYSIS.md) - Root cause analysis
- [WAL_REPLICATION_FAILURE_ROOT_CAUSE.md](WAL_REPLICATION_FAILURE_ROOT_CAUSE.md) - Original bug
- [METADATA_REPLICATION_INCOMPLETE_ANALYSIS.md](METADATA_REPLICATION_INCOMPLETE_ANALYSIS.md) - Metadata lag analysis

---

**Status Legend**:
- 🔴 NOT STARTED
- 🟡 IN PROGRESS
- 🟢 COMPLETE
- ⏸️ BLOCKED
- ⏭️ SKIPPED

**Last Updated**: 2025-11-20
**Next Session**: Phase 0 - Preparation & Documentation
