# Raft to WAL Replication Migration Plan

**Date**: 2025-11-01
**Current Branch**: `feat/v2.5.0-kafka-cluster`
**Target Branch**: `feat/v2.2.0-clean-wal-replication`
**Goal**: Complete PostgreSQL-style WAL replication for multi-node clustering

---

## Executive Summary

We have **two conflicting implementations** of multi-node clustering:

### Current State (v2.5.0-kafka-cluster)
- ✅ Raft consensus implementation (tikv/raft-rs)
- ✅ ISR tracking and metadata
- ✅ Leader election infrastructure
- ❌ **Network messaging NOT implemented** (nodes can't communicate)
- ❌ **Blocked on TODO at [raft_cluster.rs:295-303](crates/chronik-server/src/raft_cluster.rs#L295-L303)**
- ❌ 36 untracked test/doc files cluttering the repo

### Target State (v2.2.0-clean-wal-replication)
- ✅ **Clean WAL streaming** (PostgreSQL-style, TCP-based)
- ✅ **Working network code** (actual send/receive implemented)
- ✅ Fire-and-forget leader → follower streaming
- ✅ Lock-free queue (crossbeam)
- ✅ Zero-copy optimizations
- ✅ **Already tested and working**

**Decision**: **Abandon Raft, adopt WAL replication**

### Why WAL Replication Wins

| Aspect | Raft (v2.5.0) | WAL Replication (v2.2.0) |
|--------|---------------|--------------------------|
| **Implementation Status** | 70% (network TODO) | 100% (working) |
| **Network Layer** | ❌ Not implemented | ✅ TCP streaming working |
| **Complexity** | High (consensus + network) | Low (streaming only) |
| **Latency** | Higher (quorum writes) | Lower (async streaming) |
| **Consistency** | Strong (linearizable) | Eventual (acceptable for logs) |
| **Kafka Model Match** | Poor (Kafka uses ISR, not Raft) | **Excellent** (mirrors Kafka ISR) |
| **Code Cleanliness** | Messy (36 test files) | Clean |
| **Time to Production** | 2-4 weeks | **1-2 days** |

**Key Insight**: Kafka doesn't use Raft. Kafka uses **ISR-based replication with leader/follower streaming**. We've been solving the wrong problem!

---

## What feat/v2.2.0-clean-wal-replication Has (The Good Stuff)

### 1. Working WAL Streaming (`wal_replication.rs`)

**Protocol** (PostgreSQL-inspired):
```
┌──────────────────────────────────────────────────────────────┐
│                  WAL Streaming Protocol v1                    │
├──────────────────────────────────────────────────────────────┤
│                                                               │
│  Leader (Primary)              Network              Follower  │
│       │                                                  │    │
│       │─────── WalReplicationRecord ──────────────────→ │    │
│       │  [magic=0x5741|version|topic|partition|data]   │    │
│       │                                                  │    │
│       │←────────── ACK (optional) ─────────────────────│    │
│       │  [offset confirmed]                             │    │
│       │                                                  │    │
│       │─────── Heartbeat (every 10s) ─────────────────→│    │
│       │  [magic=0x4842|timestamp]                       │    │
│       │                                                  │    │
│       │  (Follower detects leader death if no HB >30s)  │    │
│       │  (Leader reconnects if connection drops)        │    │
│                                                               │
└──────────────────────────────────────────────────────────────┘
```

**Key Features**:
- Fire-and-forget from leader (never blocks produce)
- Lock-free queue (crossbeam SegQueue for MPMC)
- Automatic reconnection
- Heartbeat-based failure detection
- Zero-copy with `Bytes` and `BytesMut`

### 2. Connection Management

**Implemented**:
- TCP connection pool (DashMap)
- Automatic reconnection on failure
- Heartbeat monitoring
- Graceful shutdown

### 3. Integration with ProduceHandler

**Already wired**:
```rust
// In ProduceHandler::handle_produce_internal
if let Some(wal_repl) = &self.wal_replication {
    wal_repl.enqueue_record(topic, partition, offset, &canonical_record);
}
```

### 4. Performance Optimizations

**Already implemented**:
- Batch sending (reduces syscalls)
- Buffer pooling
- Zero-copy serialization
- Async I/O (Tokio)

---

## What We Need to Remove (The Raft Mess)

### Files to Delete

**Core Raft Code**:
- `crates/chronik-server/src/raft_cluster.rs` (281 lines)
- `crates/chronik-server/src/raft_metadata.rs` (191 lines)
- `crates/chronik-server/src/isr_tracker.rs` (220 lines)
- `crates/chronik-server/src/isr_ack_tracker.rs` (338 lines)
- `crates/chronik-server/src/leader_election.rs` (new, untracked)

**Dependencies in Cargo.toml**:
```toml
[dependencies]
raft = "0.7"
raft-proto = "0.7"
prost = "0.12"
protobuf = "3.3"
```

**Test Files (36 untracked)**:
- `chaos_test.py`
- `test_leader_election.py`
- `test_phase5_manual.sh`
- `verify_phase5.py`
- `PHASE4_*.md` (9 files)
- `PHASE5_*.md` (10 files)
- `NEXT_SESSION_PROMPT.md`
- `CHAOS_TEST_SUCCESS.md`
- `IMPLEMENTATION_GAP_ANALYSIS.md`
- etc.

### Code Changes Required

**`integrated_server.rs`**:
- Remove RaftCluster initialization
- Remove ISR tracking
- Simplify to just WAL replication

**`produce_handler.rs`**:
- Remove IsrAckTracker
- Remove Raft metadata proposals
- Keep simple WAL replication enqueue

**`fetch_handler.rs`**:
- Remove ISR-based lag checks
- Simplify to basic offset tracking

**`main.rs`**:
- Remove `raft-cluster` subcommand
- Remove Raft configuration
- Keep only `standalone` + `wal-replication` mode

---

## Migration Steps (Detailed)

### Phase 1: Archive the Mess (30 minutes)

**Goal**: Clean workspace without losing work

```bash
# Create archive directory
mkdir -p archive/v2.5.0-raft-attempt

# Move all untracked test/doc files
mv PHASE*.md archive/v2.5.0-raft-attempt/
mv chaos_test.py test_leader_election.py test_phase5_manual.sh archive/v2.5.0-raft-attempt/
mv verify_phase5.py archive/v2.5.0-raft-attempt/
mv CHAOS_TEST_SUCCESS.md IMPLEMENTATION_GAP_ANALYSIS.md NEXT_SESSION_PROMPT.md archive/v2.5.0-raft-attempt/
mv FINAL_*.md archive/v2.5.0-raft-attempt/

# Archive Raft source code (for reference)
mkdir -p archive/v2.5.0-raft-attempt/src
cp crates/chronik-server/src/raft_cluster.rs archive/v2.5.0-raft-attempt/src/
cp crates/chronik-server/src/raft_metadata.rs archive/v2.5.0-raft-attempt/src/
cp crates/chronik-server/src/isr_tracker.rs archive/v2.5.0-raft-attempt/src/
cp crates/chronik-server/src/isr_ack_tracker.rs archive/v2.5.0-raft-attempt/src/
cp crates/chronik-server/src/leader_election.rs archive/v2.5.0-raft-attempt/src/

# Create archive README
cat > archive/v2.5.0-raft-attempt/README.md << 'EOF'
# v2.5.0 Raft Implementation Attempt

**Date**: 2025-10-31 to 2025-11-01
**Status**: Abandoned
**Reason**: Network messaging not implemented, Raft doesn't match Kafka model

## What Was Attempted
- Raft consensus (tikv/raft-rs)
- ISR tracking
- Leader election
- Metadata proposals

## Why Abandoned
- Network layer blocked on TODO (raft_cluster.rs:295-303)
- Raft adds complexity without benefit (Kafka uses ISR, not Raft)
- WAL replication is simpler, faster, and already working

## Lessons Learned
- Don't add dependencies without full implementation plan
- Match the reference architecture (Kafka = ISR, not Raft)
- Simpler is better (streaming > consensus for logs)
EOF

git add archive/
git commit -m "archive(v2.5.0): Archive abandoned Raft implementation attempt"
```

### Phase 2: Remove Raft Code (1 hour)

**Step 1: Remove Raft modules from `main.rs`**

```rust
// Remove these module declarations
// mod raft_cluster;
// mod raft_metadata;
// mod isr_tracker;
// mod isr_ack_tracker;
// mod leader_election;
```

**Step 2: Remove `raft-cluster` subcommand**

Delete entire `ServerMode::RaftCluster` variant and handler.

**Step 3: Update `Cargo.toml`**

```toml
# Remove these dependencies
[dependencies]
# raft = "0.7"  # REMOVED
# raft-proto = "0.7"  # REMOVED
# prost = "0.12"  # REMOVED (unless used elsewhere)
# protobuf = "3.3"  # REMOVED
```

**Step 4: Restore clean WAL replication**

```bash
# Cherry-pick WAL replication from v2.2.0
git checkout feat/v2.2.0-clean-wal-replication -- crates/chronik-server/src/wal_replication.rs

# Verify it compiles
cargo check --bin chronik-server
```

**Step 5: Simplify `integrated_server.rs`**

Remove all Raft/ISR code, keep only:
```rust
pub struct IntegratedKafkaServer {
    // Core components
    metadata_store: Arc<dyn MetadataStore>,
    segment_manager: Arc<SegmentManager>,

    // WAL replication (optional)
    wal_replication: Option<Arc<WalReplicationManager>>,

    // Handlers
    produce_handler: Arc<ProduceHandler>,
    fetch_handler: Arc<FetchHandler>,
    // ...
}
```

**Step 6: Simplify `produce_handler.rs`**

Keep only:
```rust
pub async fn handle_produce_internal(...) {
    // 1. Write to WAL
    // 2. Write to segment
    // 3. Enqueue to WAL replication (if enabled)
    if let Some(wal_repl) = &self.wal_replication {
        wal_repl.enqueue_record(topic, partition, offset, &record);
    }
    // 4. Return success
}
```

**Step 7: Delete Raft files**

```bash
rm crates/chronik-server/src/raft_cluster.rs
rm crates/chronik-server/src/raft_metadata.rs
rm crates/chronik-server/src/isr_tracker.rs
rm crates/chronik-server/src/isr_ack_tracker.rs
rm crates/chronik-server/src/leader_election.rs

cargo check --bin chronik-server
```

### Phase 3: Test WAL Replication (2 hours)

**Test 1: Single Leader, Two Followers**

**Setup**:
```bash
# Terminal 1: Leader
cargo run --bin chronik-server -- \
  --kafka-port 9092 \
  --advertised-addr localhost \
  standalone \
  --wal-followers "localhost:9192,localhost:9193"

# Terminal 2: Follower 1
cargo run --bin chronik-server -- \
  --kafka-port 9093 \
  --advertised-addr localhost \
  standalone \
  --wal-receiver 9192

# Terminal 3: Follower 2
cargo run --bin chronik-server -- \
  --kafka-port 9094 \
  --advertised-addr localhost \
  standalone \
  --wal-receiver 9193
```

**Verify**:
```python
from kafka import KafkaProducer, KafkaConsumer

# Produce to leader
producer = KafkaProducer(bootstrap_servers='localhost:9092')
for i in range(100):
    producer.send('test-topic', f'message-{i}'.encode())
producer.flush()

# Consume from follower 1 (should have all 100 messages)
consumer1 = KafkaConsumer('test-topic', bootstrap_servers='localhost:9093', auto_offset_reset='earliest')
messages1 = [msg.value for msg in consumer1]
print(f"Follower 1: {len(messages1)} messages")

# Consume from follower 2 (should have all 100 messages)
consumer2 = KafkaConsumer('test-topic', bootstrap_servers='localhost:9094', auto_offset_reset='earliest')
messages2 = [msg.value for msg in consumer2]
print(f"Follower 2: {len(messages2)} messages")

assert len(messages1) == 100
assert len(messages2) == 100
print("✅ WAL replication working!")
```

**Test 2: Leader Failover (Manual)**

**Goal**: Verify followers can be promoted to leader

```bash
# Step 1: Produce 50 messages to Node 1 (leader)
python3 -c "
from kafka import KafkaProducer
p = KafkaProducer(bootstrap_servers='localhost:9092')
for i in range(50):
    p.send('test-topic', f'msg-{i}'.encode())
p.flush()
print('✅ 50 messages produced to Node 1')
"

# Step 2: Kill Node 1
pkill -f "kafka-port 9092"

# Step 3: Promote Node 2 to leader (manual restart with followers)
cargo run --bin chronik-server -- \
  --kafka-port 9093 \
  --advertised-addr localhost \
  standalone \
  --wal-followers "localhost:9194"

# Step 4: Produce 50 more messages to Node 2 (new leader)
python3 -c "
from kafka import KafkaProducer
p = KafkaProducer(bootstrap_servers='localhost:9093')
for i in range(50, 100):
    p.send('test-topic', f'msg-{i}'.encode())
p.flush()
print('✅ 50 messages produced to Node 2')
"

# Step 5: Verify all 100 messages on Node 3 (follower)
python3 -c "
from kafka import KafkaConsumer
c = KafkaConsumer('test-topic', bootstrap_servers='localhost:9094', auto_offset_reset='earliest')
messages = [msg.value for msg in c]
print(f'✅ Node 3 has {len(messages)} messages')
assert len(messages) == 100
"
```

**Test 3: Automatic Reconnection**

**Goal**: Verify follower reconnects after network blip

```bash
# Step 1: Start leader + follower
# Step 2: Produce 50 messages
# Step 3: Kill follower for 10 seconds
# Step 4: Restart follower
# Step 5: Produce 50 more messages
# Step 6: Verify follower has all 100 messages
```

### Phase 4: Document and Commit (1 hour)

**Update CLAUDE.md**:

```markdown
## Multi-Node Replication (v2.2.0+)

Chronik supports PostgreSQL-style WAL streaming replication for multi-node deployments.

### Architecture

**Leader-Follower Model**:
- One leader accepts writes
- Multiple followers receive WAL stream
- Fire-and-forget streaming (never blocks leader)
- Automatic reconnection on failure

**NOT Raft**: Chronik uses ISR-based replication like Kafka, not consensus protocols.

### Setup

**Leader Node**:
```bash
cargo run --bin chronik-server -- \
  --kafka-port 9092 \
  standalone \
  --wal-followers "follower1:9192,follower2:9193"
```

**Follower Nodes**:
```bash
# Follower 1
cargo run --bin chronik-server -- \
  --kafka-port 9093 \
  standalone \
  --wal-receiver 9192

# Follower 2
cargo run --bin chronik-server -- \
  --kafka-port 9094 \
  standalone \
  --wal-receiver 9193
```

### Failover

**Manual Promotion**:
1. Detect leader failure (heartbeat timeout)
2. Promote follower by restarting with `--wal-followers`
3. Update DNS/load balancer to new leader
4. Clients reconnect automatically

**Automatic (Future)**:
- Implement leader election via ZooKeeper/etcd
- Automatic ISR management
- Read-your-writes consistency
```

**Commit Plan**:
```bash
# Commit 1: Archive Raft attempt
git add archive/
git commit -m "archive(v2.5.0): Archive abandoned Raft implementation"

# Commit 2: Remove Raft code
git rm crates/chronik-server/src/raft_cluster.rs
git rm crates/chronik-server/src/raft_metadata.rs
git rm crates/chronik-server/src/isr_tracker.rs
git rm crates/chronik-server/src/isr_ack_tracker.rs
git add crates/chronik-server/Cargo.toml
git add crates/chronik-server/src/main.rs
git add crates/chronik-server/src/integrated_server.rs
git commit -m "refactor(v2.2.0): Remove Raft dependencies, restore WAL replication"

# Commit 3: Restore clean WAL replication
git checkout feat/v2.2.0-clean-wal-replication -- crates/chronik-server/src/wal_replication.rs
git add crates/chronik-server/src/wal_replication.rs
git add crates/chronik-server/src/produce_handler.rs
git commit -m "feat(v2.2.0): Restore PostgreSQL-style WAL replication"

# Commit 4: Update docs
git add CLAUDE.md
git add RAFT_TO_WAL_MIGRATION.md
git commit -m "docs(v2.2.0): Document WAL replication architecture"

# Merge to main (after testing)
git checkout main
git merge feat/v2.5.0-kafka-cluster --no-ff -m "feat(v2.2.0): PostgreSQL-style WAL replication"
git tag v2.2.0
git push origin main --tags
```

---

## Success Criteria

**Definition of Done**:
1. ✅ All Raft code removed from codebase
2. ✅ WAL replication compiles and runs
3. ✅ 3-node cluster (1 leader + 2 followers) successfully replicates 1000 messages
4. ✅ Follower survives leader producing 100 msg → follower restart → leader producing 100 msg
5. ✅ Manual failover works (promote follower to leader)
6. ✅ Documentation updated (CLAUDE.md)
7. ✅ Workspace clean (no untracked files)

**Time Estimate**: 4-5 hours total
- 0.5h: Archive
- 1h: Remove Raft
- 2h: Test WAL replication
- 1h: Document and commit

---

## Why This Is The Right Call

### Technical Reasons
1. **Kafka doesn't use Raft** - We were implementing the wrong architecture
2. **WAL replication is simpler** - Fewer moving parts, easier to debug
3. **WAL replication is faster** - No quorum waits, fire-and-forget
4. **Already working** - v2.2.0 branch has proven code

### Practical Reasons
1. **Network layer complete** - No TODOs blocking progress
2. **Time to production** - Days, not weeks
3. **Code quality** - Clean, tested, documented
4. **Maintainability** - Simpler = fewer bugs

### Strategic Reasons
1. **Match Kafka architecture** - Easier to explain, understand, adopt
2. **Future ISR** - Can add ISR tracking later without Raft
3. **Operational simplicity** - Easier to deploy, monitor, debug

---

## Next Steps (After Migration)

### v2.3.0: ISR Tracking (No Raft)
Add In-Sync Replica tracking to WAL replication:
- Track follower lag
- Implement `min.insync.replicas`
- Add ISR to metadata

### v2.4.0: Automatic Failover
Implement leader election without Raft:
- ZooKeeper integration (or etcd)
- Automatic ISR-based leader election
- Client-side leader discovery

### v3.0.0: Read Replicas
Add read-from-follower support:
- Route reads to followers
- Stale read tolerance
- Read-your-writes consistency

---

## References

**Working Code**:
- [v2.2.0 WAL replication](https://github.com/chronik-stream/chronik-stream/blob/feat/v2.2.0-clean-wal-replication/crates/chronik-server/src/wal_replication.rs)
- [PostgreSQL WAL streaming](https://www.postgresql.org/docs/current/warm-standby.html)
- [Kafka ISR design](https://kafka.apache.org/documentation/#replication)

**Abandoned Code**:
- [v2.5.0 Raft attempt](archive/v2.5.0-raft-attempt/)

---

## Approval Checklist

Before proceeding, confirm:
- [ ] I understand we're abandoning Raft for WAL replication
- [ ] I understand this matches Kafka's architecture better
- [ ] I'm okay with archiving (not deleting) the Raft work
- [ ] I want a clean, working multi-node cluster in 1-2 days
- [ ] I agree to follow the migration steps exactly

**Ready to proceed? Let's clean up and ship WAL replication!**
