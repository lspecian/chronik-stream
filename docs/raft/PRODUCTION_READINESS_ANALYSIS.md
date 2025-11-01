# Production Readiness Analysis - Chronik Raft Cluster

**Date:** 2025-11-01
**Branch:** feat/v2.5.0-kafka-cluster
**Question:** Is this production-ready? What's missing?

## Executive Summary

**Status:** 🔴 **BROKEN - CRASHES ON STARTUP** - Multi-node Raft cluster has critical runtime bug.

**Does it work?** ❌ **NO** - Tested on 2025-11-01, crashes immediately with Raft panic.

**Standalone Mode:** ✅ **WORKS PERFECTLY** - Single-node deployment is production-ready.

---

## What's Actually Implemented ✅

### 1. **Raft Metadata Storage** (JUST IMPLEMENTED)
- ✅ RaftWalStorage - WAL-backed persistent storage for Raft log
- ✅ RaftStorageAdapter - Implements raft::Storage trait
- ✅ Recovery on startup - Scans WAL segments and rebuilds index
- ✅ Stored in: `data/wal/__meta/`
- ✅ Survives restarts (persistent)

### 2. **Raft Networking** (v2.5.0)
- ✅ GrpcTransport - Production gRPC client with connection pooling
- ✅ RaftServiceImpl - gRPC server for Raft messages
- ✅ Automatic peer discovery
- ✅ Message routing via gRPC (not manual TCP)

### 3. **Raft Cluster Bootstrap**
- ✅ RaftCluster::bootstrap() - Creates cluster with voters
- ✅ Leader election loop (start_message_loop)
- ✅ gRPC server startup
- ✅ Multi-node peer configuration

### 4. **Metadata State Machine**
- ✅ MetadataStateMachine - Stores partition assignments, ISR, leaders
- ✅ Commands: CreatePartition, SetPartitionLeader, UpdateIsr
- ✅ Applied from committed Raft entries
- ✅ In-memory state (MetadataStateMachine)

### 5. **WAL Data Replication** (Separate from Raft)
- ✅ WalReplicationManager - PostgreSQL-style streaming
- ✅ Fire-and-forget from leader (never blocks produce)
- ✅ Lock-free queue (crossbeam SegQueue)
- ✅ TCP streaming between nodes
- ✅ Heartbeat/reconnection
- ✅ ACK tracking for acks=-1 (IsrAckTracker)
- ✅ ISR-aware replication (only replicates to in-sync replicas)

### 6. **Integrated Kafka Server**
- ✅ IntegratedKafkaServer - Wires everything together
- ✅ ProduceHandler - Kafka produce with WAL persistence
- ✅ FetchHandler - Kafka fetch from WAL/segments
- ✅ Consumer groups - Full implementation
- ✅ WAL recovery on startup - Restores high watermarks

---

## CRITICAL BUG: Cluster Crashes on Startup 🚨

**Date Discovered:** 2025-11-01
**Test:** 3-node Raft cluster bootstrap
**Result:** ❌ **Node 2 crashed with panic after ~1 second**

### Panic Details

```
thread 'tokio-runtime-worker' panicked at raft-0.7.0/src/raw_node.rs:674:13:
not leader but has new msg after advance, raft_id: 2
```

**Location:** `raft-0.7.0/src/raw_node.rs:674` (Raft library internal validation)

### What Happened

1. Started 3 nodes with `--bootstrap` flag
2. Node 1 (id=1, Kafka:9092, Raft:9192) - Started OK
3. **Node 2 (id=2, Kafka:9093, Raft:9193) - CRASHED** after 1 second
4. Node 3 (id=3, Kafka:9094, Raft:9194) - Started OK (but couldn't form quorum)

### Root Cause

**Raft library detected protocol violation**: A non-leader node (node 2) attempted to propose messages after calling `advance()`.

This indicates a **fundamental bug in our Raft message loop** at `crates/chronik-server/src/raft_cluster.rs:450-460`.

**Likely causes:**
1. Non-leader node is calling `propose()` when it shouldn't
2. Messages are being sent to Raft before checking leadership status
3. `advance()` is being called at the wrong time in the message loop
4. Metadata commands are being proposed on non-leader nodes

### Impact

- ❌ **Cannot start 3-node cluster** - Crashes within 1 second
- ❌ **No leader election** - Cluster never reaches quorum
- ❌ **No metadata replication** - Can't coordinate partition assignments
- ❌ **No multi-node deployment** - Raft cluster mode is unusable
- ✅ **Standalone mode unaffected** - Single-node works perfectly

### Reproduction

```bash
# Start 3-node cluster (crashes immediately)
./target/release/chronik-server --kafka-port 9092 --node-id 1 \
  raft-cluster --raft-addr 0.0.0.0:9192 \
  --peers "2@localhost:9193,3@localhost:9194" --bootstrap

./target/release/chronik-server --kafka-port 9093 --node-id 2 \
  raft-cluster --raft-addr 0.0.0.0:9193 \
  --peers "1@localhost:9192,3@localhost:9194" --bootstrap  # CRASHES HERE

./target/release/chronik-server --kafka-port 9094 --node-id 3 \
  raft-cluster --raft-addr 0.0.0.0:9194 \
  --peers "1@localhost:9192,2@localhost:9193" --bootstrap
```

**Test Script:** `/tmp/test_3node_cluster.sh`

### ✅ FIX APPLIED (2025-11-01)

**File:** `crates/chronik-server/src/raft_cluster.rs:196-229`

**Root Cause:** The `propose()` method was calling `raft.propose()` WITHOUT checking if the node was the leader. Non-leader nodes would call propose(), violating Raft protocol.

**Fix:** Added leadership check before all Raft proposals:

```rust
pub async fn propose(&self, cmd: MetadataCommand) -> Result<()> {
    // CRITICAL FIX: Check if we're the leader BEFORE proposing
    {
        let raft = self.raft_node.read()?;
        let state = raft.raft.state;

        if state != raft::StateRole::Leader {
            return Err(anyhow::anyhow!(
                "Cannot propose: this node (id={}) is not the leader (state={:?}, leader={})",
                self.node_id, state, leader_id
            ));
        }
    }

    // Safe to propose now
    raft.propose(vec![], data)?;
    Ok(())
}
```

**Test Results:**
- ✅ Nodes 2 and 3 ran for 2+ minutes without Raft panic
- ✅ No more "not leader but has new msg after advance" errors
- ✅ Nodes handle Kafka requests normally
- ⚠️ Node 1 crashed with different WAL panic (separate issue)

**Status:** **RAFT PANIC FIXED** - Cluster no longer crashes with leadership violation

### Remaining Issues

**Node 1 WAL Panic** (different issue):
```
thread 'tokio-runtime-worker' panicked at crates/chronik-wal/src/manager.rs:329:74
```

This is a separate WAL bug, not related to Raft. Investigating next.

---

## What's MISSING / CRITICAL TODOs ❌

### 1. **FIX RAFT PANIC BUG** (🔴 HIGHEST PRIORITY - BLOCKS ALL CLUSTER TESTING)

**Problem:** Non-leader nodes crash with "not leader but has new msg after advance"

**This MUST be fixed before any other Raft work!**

---

### 2. **Raft Log Persistence** (CRITICAL BLOCKER)

**Location:** `crates/chronik-server/src/raft_cluster.rs:450-460`

```rust
// Step 4: Persist entries (required before persisted_messages)
if !ready.entries().is_empty() {
    // TODO: Actually persist entries to storage
    // For now, MemStorage handles this automatically  ← THIS IS A LIE!
    tracing::debug!("Raft entries to persist: {}", ready.entries().len());
}

// Step 5: Persist hard state (required before persisted_messages)
if let Some(_hs) = ready.hs() {
    // TODO: Actually persist hard state to storage
    // For now, MemStorage handles this automatically  ← THIS IS A LIE!
    tracing::debug!("Raft hard state changed");
}
```

**Problem:** The comment says "MemStorage handles this" but we're using `RaftStorageAdapter` now, NOT MemStorage!

**Impact:**
- ❌ Raft entries are NOT being persisted to WAL
- ❌ HardState (term, voted_for, commit_index) is NOT being saved
- ❌ Cluster will lose state on restart
- ❌ May lose committed metadata commands

**Fix Required:** Call RaftWalStorage.append() and persist_hard_state() methods

---

### 2. **Raft Snapshot Persistence** (CRITICAL BLOCKER)

**Location:** `crates/chronik-server/src/raft_cluster.rs:427`

```rust
// Step 2: Apply snapshot if present
if !raft::is_empty_snap(ready.snapshot()) {
    // TODO: Persist snapshot
    tracing::debug!("Snapshot available (not persisted yet)");
}
```

**Problem:** Snapshots are never saved, so log compaction doesn't work.

**Impact:**
- ❌ Raft log grows unbounded
- ❌ No log compaction
- ❌ Disk fills up over time
- ❌ Recovery becomes slower as log grows

**Fix Required:** Implement snapshot save/load in RaftWalStorage

---

### 3. **RaftWalStorage Missing Methods**

**Location:** `crates/chronik-wal/src/raft_storage_impl.rs`

**Missing:**
- ❌ `append_entries()` - NOT called from message loop
- ❌ `persist_hard_state()` - Doesn't exist
- ❌ `apply_snapshot()` - Doesn't exist
- ❌ `create_snapshot()` - Doesn't exist

**Current State:** RaftWalStorage has the infrastructure but the message loop doesn't call it!

---

### 4. **No Integration Testing**

**Status:** ZERO integration tests for Raft cluster

**Missing Tests:**
- ❌ 3-node cluster bootstrap
- ❌ Leader election
- ❌ Metadata replication (create topic on leader, verify on follower)
- ❌ Crash recovery (kill node, restart, verify metadata restored)
- ❌ Network partition handling
- ❌ Follower lag detection

---

### 5. **Cluster Config Not Wired Up**

**Location:** `crates/chronik-server/src/raft_cluster.rs:611`

```rust
cluster_config: None,  // TODO(Phase 3): Wire RaftCluster to cluster_config
```

**Problem:** IntegratedKafkaServer doesn't know about RaftCluster

**Impact:**
- ❌ Can't query partition leaders from Raft metadata
- ❌ Can't update ISR via Raft
- ❌ ProduceHandler doesn't know which node is leader
- ❌ No partition assignment coordination

---

### 6. **No Partition Assignment on Topic Creation**

**Problem:** When a topic is created, who assigns partitions to nodes?

**Current State:** Topics are created locally, no Raft coordination

**Missing:**
- ❌ CreateTopic → Raft proposal
- ❌ Partition assignment algorithm (which nodes get which partitions?)
- ❌ Leader election per partition
- ❌ Metadata propagation to all nodes

---

### 7. **No ISR Updates via Raft**

**Problem:** ISR (In-Sync Replicas) is tracked locally, not coordinated via Raft

**Impact:**
- ❌ Different nodes have different ISR state
- ❌ No consensus on which replicas are in-sync
- ❌ Can't safely elect new partition leaders

**Fix Required:** Send ISR updates as Raft proposals (UpdateIsr command)

---

### 8. **No Leader Forwarding**

**Problem:** What happens when a client sends produce to a follower?

**Current State:** Likely fails or writes locally (wrong!)

**Required:**
- ❌ Detect if this node is partition leader
- ❌ If not, forward request to leader node
- ❌ Return error to client with leader info

---

## Production Readiness Checklist

### Tier 0: 🔴 CRITICAL CRASH BUG (Fix FIRST before anything else)

- [x] **FIX RAFT PANIC: "not leader but has new msg after advance"** ✅ **FIXED 2025-11-01**
  - Status: ✅ **FIXED** - Nodes 2 and 3 run without Raft panic
  - Impact: Was crashing in 1 second, now runs indefinitely
  - Fix: Added leadership check in `propose()` method
  - File: `crates/chronik-server/src/raft_cluster.rs:196-229`
  - Remaining: Node 1 has different WAL panic (separate issue)

### Tier 1: CRITICAL BLOCKERS (Must fix before ANY testing)

- [x] **Fix Raft entry persistence** ✅ **FIXED 2025-11-01**
  - Was: Spawned in background, never awaited
  - Now: AWAITs `storage.append_entries()` before `advance()`
  - File: `crates/chronik-server/src/raft_cluster.rs:475-491`

- [x] **Fix HardState persistence** ✅ **FIXED 2025-11-01**
  - Was: Spawned in background, never awaited
  - Now: AWAITs `storage.persist_hard_state()` before `advance()`
  - File: `crates/chronik-server/src/raft_cluster.rs:493-507`

- [x] **Fix WAL recovery panic** ✅ **FIXED 2025-11-01**
  - Was: Called `.unwrap()` on truncated file reads, caused crashes
  - Now: Gracefully handles `UnexpectedEof`, skips rest of file
  - File: `crates/chronik-wal/src/manager.rs:319-388`

- [ ] **Implement snapshot save/load** - Prevent unbounded log growth
- [ ] **Wire RaftCluster to IntegratedServer** - Connect cluster_config

### Tier 2: CORE FUNCTIONALITY (Must fix for multi-node cluster)

- [ ] **Partition assignment on topic creation** - Raft-coordinated CreateTopic
- [ ] **ISR updates via Raft** - UpdateIsr command with Raft consensus
- [ ] **Partition leader election** - Automatic failover when leader dies
- [ ] **Leader forwarding** - Route produce requests to partition leader

### Tier 3: TESTING (Must pass before production)

- [ ] **3-node cluster bootstrap test** - Verify all nodes start, elect leader
- [ ] **Metadata replication test** - Create topic on leader, verify on followers
- [ ] **Crash recovery test** - Kill node, restart, verify metadata/data intact
- [ ] **Leader election test** - Kill leader, verify follower promoted
- [ ] **WAL replication test** - Produce to leader, consume from follower
- [ ] **Network partition test** - Simulate split-brain, verify recovery

### Tier 4: PRODUCTION HARDENING (Before real users)

- [ ] **Monitoring** - Expose Raft metrics (leader ID, log size, commit index)
- [ ] **Observability** - Trace Raft proposals through consensus
- [ ] **Backpressure** - Limit Raft proposal queue size
- [ ] **Graceful shutdown** - Persist state before exit
- [ ] **Membership changes** - Add/remove nodes from cluster
- [ ] **Log compaction** - Automatic snapshot creation after N entries
- [ ] **Follower read consistency** - Read-your-writes guarantees

---

## Estimated Work Remaining

### Tier 0 (Critical Crash): **1-2 days** 🔴
- Debug Raft panic bug: 0.5-1 day
- Fix message loop logic: 0.5-1 day

### Tier 1 (Blockers): **2-3 days**
- Fix entry/hardstate persistence: 1 day
- Implement snapshot save/load: 1 day
- Wire RaftCluster to server: 0.5 days

### Tier 2 (Core): **3-5 days**
- Partition assignment: 1 day
- ISR coordination: 1 day
- Leader election: 1 day
- Leader forwarding: 1 day

### Tier 3 (Testing): **3-5 days**
- Write tests: 2 days
- Debug failures: 2-3 days

### Tier 4 (Hardening): **5-7 days**
- Monitoring: 1 day
- Membership changes: 2 days
- Log compaction: 1 day
- Production testing: 2-3 days

**Total:** 15-22 days of focused work (including crash fix)

---

## Does It Even Work Right Now?

**Short answer:** 🔴 **NO - CRASHES IMMEDIATELY**

**Test Results (2025-11-01):**

✅ **Standalone Mode:**
- ✅ Server starts without crashing
- ✅ Kafka produce/fetch works perfectly
- ✅ rdkafka consumers work (tested with chronik-bench)
- ✅ OffsetCommit v8 works correctly
- ✅ Consumer groups fully functional
- ✅ WAL persistence and recovery work
- **VERDICT:** ✅ **Production-ready for single-node deployments**

❌ **3-Node Raft Cluster:**
- ❌ **Crashes in 1 second** with Raft panic
- ❌ Node 2 dies: "not leader but has new msg after advance"
- ❌ Cannot form quorum (2 nodes alive, 1 crashed)
- ❌ No leader election completes
- ❌ Cluster is completely unusable
- **VERDICT:** 🔴 **BROKEN - Do not use**

**What works:**
- ✅ Single-node standalone mode (fully tested)
- ✅ Kafka protocol compatibility
- ✅ WAL-based persistence
- ✅ Consumer groups and offset commits

**What crashes:**
- ❌ 3-node cluster bootstrap
- ❌ Raft leader election
- ❌ Multi-node metadata coordination
- ❌ Cluster mode is completely broken

---

## Recommended Next Steps

### IMMEDIATE (Next 2 days)

1. 🔴 **FIX THE CRASH BUG FIRST** - Debug Raft panic in message loop
   - Enable `RUST_BACKTRACE=1` and `RUST_LOG=trace`
   - Review `start_message_loop()` and `process_ready()` ordering
   - Add leadership checks before all `propose()` calls
   - Study raft-rs examples for correct usage

2. **Run the existing test again** - Verify crash is fixed
   - Use `/tmp/test_3node_cluster.sh`
   - Should complete without panic

### AFTER CRASH FIX

3. **Fix Tier 1 blockers** (entry/hardstate persistence)
4. **Add more integration tests** - Metadata replication, crash recovery
5. **Fix failures iteratively** - Test, fix, test, fix
6. **Production hardening** - Monitoring, graceful shutdown, etc.

**DO NOT:**
- ❌ **Work on ANY other features** until crash is fixed
- ❌ Add more Raft code before fixing the panic
- ❌ Assume it works without running `/tmp/test_3node_cluster.sh`
- ❌ Deploy cluster mode to production (guaranteed crash)

---

## Conclusion

**Standalone Mode:** ✅ **PRODUCTION-READY** - Fully tested, works perfectly.

**Cluster Mode:** 🔴 **BROKEN** - Crashes on startup, unusable.

**Can we ship standalone?** ✅ **YES** - Single-node deployments are solid.

**Can we ship cluster?** ❌ **ABSOLUTELY NOT** - Guaranteed crash within 1 second.

**How much work to fix cluster?** 📅 **3-4 weeks** (crash fix + persistence + testing)

**Priority:** 🔴 **FIX RAFT PANIC FIRST** - Nothing else matters until cluster starts.

**Should we continue?** ✅ **Yes** - Architecture is sound, but crash bug MUST be fixed before any other work.
