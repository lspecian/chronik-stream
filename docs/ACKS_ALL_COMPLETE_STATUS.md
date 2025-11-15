# acks=all Implementation - Complete Status Document

**Last Updated**: 2025-11-14
**Version**: v2.2.7 Phase 4
**Status**: 🟡 IN PROGRESS - Partition leadership bug identified and partially fixed

---

## Executive Summary

**Goal**: Make `acks=all` (min.insync.replicas) work correctly in Chronik cluster mode.

**Current Status**:
- ✅ WAL metadata replication working (followers receive partition assignments)
- ✅ WAL data replication working (followers receive produce data)
- ✅ ACK sending/receiving infrastructure working
- ✅ Single-partition topics with acks=all: **WORKING**
- ❌ Multi-partition topics with acks=all: **FAILING** (timeout)

**Root Cause Identified**: Partition leadership assignment bug - two different code paths assign partition leaders inconsistently.

---

## What Has Been Fixed ✅

### 1. Event-Driven Metadata Replication (Phase 1-2)
**Problem**: Followers didn't receive partition assignments from leader.
**Solution**: Implemented event-driven WAL metadata replication.
- Created `MetadataEventBus` for publishing partition assignment events
- `MetadataWalReplicator` listens to events and replicates via WAL
- Eliminates 10-second polling delay - followers updated within 100ms

**Files Modified**:
- `crates/chronik-server/src/metadata_events.rs` (new)
- `crates/chronik-server/src/metadata_wal_replication.rs`
- `crates/chronik-server/src/raft_metadata_store.rs`
- `crates/chronik-server/src/integrated_server.rs`

### 2. Connection Pre-Warming (Phase 3)
**Problem**: WAL connections not established before first produce request.
**Solution**: Pre-warm connections on server startup with 10-second delay.

**Files Modified**:
- `crates/chronik-server/src/integrated_server.rs` (lines 450-470)

### 3. Metadata Partition Follower Registration (Phase 4)
**Problem**: `__chronik_metadata-0` partition had no followers registered.
**Solution**: Register metadata partition followers on startup.

**Files Modified**:
- `crates/chronik-server/src/wal_replication.rs` (`register_metadata_partition_followers()`)
- `crates/chronik-server/src/integrated_server.rs`

### 4. ACK Recording Location Fix (Phase 5)
**Problem**: WAL receiver was calling `tracker.record_ack()` locally instead of on leader.
**Solution**: Only leader's tracker records ACKs (removed local call).

**Files Modified**:
- `crates/chronik-server/src/wal_replication.rs` (removed incorrect `record_ack()` call)

### 5. Comprehensive Logging (Debugging)
**Added**: INFO-level logging throughout ACK tracking flow.

**Files Modified**:
- `crates/chronik-server/src/isr_ack_tracker.rs`

---

## Current Bug: Partition Leadership Assignment 🐛 (Session 2)

### The Problem

**Single-partition topics work**, but **multi-partition topics fail** with acks=all.

**Evidence**:
- Topic: `test-metadata-fix` (1 partition, replicas=[1, 2, 3]) → ✅ SUCCESS
- Topic: `test-auto-multipart` (3 partitions) → ❌ TIMEOUT on first message

### Session 2 Progress (Current)

**Fixes Applied:**
1. ✅ **Code Path Conflict Fixed** (Lines 2305-2311):
   - Added check to skip Path 1 (PartitionAssignment module) if Path 2 already initialized partitions
   - Prevents redundant/conflicting partition assignments
   - Path 1 now only runs as fallback if initialize_raft_partitions() fails

2. ✅ **Leader Assignment Fixed** (Line 2657):
   - Changed from checking Raft leader to always using `replicas[0]`
   - Ensures correct round-robin leader distribution

**Current Status: DEEPER BUG IDENTIFIED** 🔍

Despite correct code logic, ALL partitions are being set to `leader=1`:

```
# What the code calculates (CORRECT):
✓ Initialized partition metadata for test-auto-multipart-0: replicas=[1, 2, 3], leader=1
✓ Initialized partition metadata for test-auto-multipart-1: replicas=[2, 3, 1], leader=2
✓ Initialized partition metadata for test-auto-multipart-2: replicas=[3, 1, 2], leader=3

# What actually gets applied (WRONG):
SetPartitionLeader { partition: 0, leader: 1 }  ✅ Correct
SetPartitionLeader { partition: 1, leader: 1 }  ❌ WRONG! Should be 2
SetPartitionLeader { partition: 2, leader: 1 }  ❌ WRONG! Should be 3
```

**Evidence of Bug:**
```
# Logs show discrepancy:
[INFO]  Initialized partition metadata for test-auto-multipart-1: leader=2  # Variable prints correctly
[DEBUG] Replicating metadata command: SetPartitionLeader { partition: 1, leader: 1 }  # Command has wrong value!

# Leadership check returns wrong value:
[DEBUG] Raft leadership check for test-auto-multipart-1: leader=1, is_leader=true  # Node 1 thinks it's leader for partition 1
```

**Root Cause Hypothesis:**
One of the following is happening:
1. **Variable capture bug**: The `leader` variable is being incorrectly captured/shadowed
2. **Serialization bug**: MetadataCommand is being corrupted during serialization
3. **Hidden code path**: Another piece of code is overwriting the leader after it's set
4. **Closure bug**: The leader value is being captured from wrong scope

**CRITICAL DISCOVERY** (Current Session):

Debug logging revealed the EXACT location of the bug:

```
# Commands created CORRECTLY:
[INFO] Created command: SetPartitionLeader { partition: 0, leader: 1 }  ✅
[INFO] Created command: SetPartitionLeader { partition: 1, leader: 2 }  ✅
[INFO] Created command: SetPartitionLeader { partition: 2, leader: 3 }  ✅

# Commands CORRUPTED during replication:
[DEBUG] Replicating: SetPartitionLeader { partition: 0, leader: 1 }  ✅
[DEBUG] Replicating: SetPartitionLeader { partition: 1, leader: 1 }  ❌ WRONG! Was 2
[DEBUG] Replicating: SetPartitionLeader { partition: 2, leader: 1 }  ❌ WRONG! Was 3
```

**The Bug Location:**
The corruption happens between:
1. Command creation (`produce_handler.rs` line 2663-2667) - CORRECT
2. Metadata WAL replication (`metadata_wal_replication.rs` line 80-104) - CORRUPTED

**Likely Root Cause:**
- Variable capture bug in event publishing/replication
- The loop variable or command might be getting overwritten before async replication completes
- Event bus might be holding references instead of cloned values

**BUG IDENTIFIED - ROOT CAUSE CONFIRMED:**

The bug is in `produce_handler.rs` lines 2672-2674 where I call:
```rust
raft.apply_metadata_command_direct(leader_cmd)
```

**Problem:** This method ONLY applies to the state machine. It does NOT:
1. Write to the metadata WAL
2. Trigger replication to followers
3. Publish events

**Evidence:** The replication logs show "offset=0" repeatedly, indicating WAL writes aren't happening.

**The Correct Pattern** (from `raft_metadata_store.rs` lines 700-750):
```rust
// 1. Write to metadata WAL
let offset = self.metadata_wal.append_command(&cmd)?;

// 2. Apply to state machine
self.raft.apply_metadata_command_direct(cmd.clone())?;

// 3. Trigger async replication
let replicator = self.metadata_wal_replicator.clone();
tokio::spawn(async move {
    if let Err(e) = replicator.replicate(&cmd, offset).await {
        error!("Failed to replicate: {}", e);
    }
});
```

**THE FIX ATTEMPTS (Session 2):**

**Attempt 1**: Removed broken `initialize_raft_partitions()` code path - PARTIAL SUCCESS

**Attempt 2**: Fixed cluster mode detection and used proper Raft proposal methods:

**Changes Made** (`produce_handler.rs`):
1. ✅ **Removed `#[cfg(feature = "raft")]`** guard (line 2236) - was blocking cluster code
2. ✅ **Fixed to use `self.raft_cluster`** instead of `self.raft_manager`
3. ✅ **Used `raft_cluster.get_all_nodes().await`** to get cluster nodes
4. ✅ **Used `propose_partition_assignment_and_wait()`** for proper Raft proposal with WAL writes
5. ✅ **Used `propose_set_partition_leader()`** for leader assignment
6. ✅ **Used `propose()` for ISR initialization**

**CURRENT STATUS**: Partition assignments ARE created correctly:
```
✓ Assigned partition 0 with replica list: [1, 2, 3] (leader: 1)  ← CORRECT
✓ Assigned partition 1 with replica list: [2, 3, 1] (leader: 2)  ← CORRECT
✓ Assigned partition 2 with replica list: [3, 1, 2] (leader: 3)  ← CORRECT
```

**BUT THEN GET OVERWRITTEN 2ms later**:
```
AssignPartition handler: partition=0, replicas=[1]  ← WRONG
AssignPartition handler: partition=1, replicas=[1]  ← WRONG
AssignPartition handler: partition=2, replicas=[1]  ← WRONG
```

**ROOT CAUSE**: Another code path is overwriting the correct assignments!

**IDENTIFIED CULPRIT**: `handler.rs` line 6628 in `create_topic_in_metadata()` creates
assignments with `replicas=[self.broker_id]` when `online_brokers.is_empty()`.

**THE REAL ISSUE**: Something is calling partition assignment AFTER my code runs, overwriting
the correct assignments with single-broker assignments.

**NEXT STEPS**:
1. Find what triggers the second set of AssignPartition commands
2. Either:
   - Option A: Prevent that code from running
   - Option B: Make partition assignment idempotent (don't overwrite if already correct)
   - Option C: Fix the code that's creating wrong assignments to use cluster-aware logic

**OLD FIX (Session 2 - Incomplete):**

Instead of implementing the 3-step pattern in `initialize_raft_partitions()`, I **removed the entire broken code path**:

**Changes Made** (`produce_handler.rs`):
1. **Removed lines 2229-2297**: Entire `initialize_raft_partitions()` call and follower wait logic
2. **Removed lines 2237-2243**: Skip check that was preventing normal Path 1 execution
3. **Restored normal flow**: Let PartitionAssignment module in `raft_metadata_store.rs` handle everything

**Why This Fix Works:**
- Path 1 (PartitionAssignment module) ALREADY has proper WAL writes (lines 700-750 in `raft_metadata_store.rs`)
- Path 1 ALREADY has proper replication and event publishing
- Path 1 ALREADY uses `replicas[0]` as leader (correct round-robin)
- Having two paths was unnecessary complexity
- One well-tested path is better than two conflicting paths

**Code After Fix:**
```rust
if let Some(ref raft_manager) = self.raft_manager {
    if raft_manager.is_enabled() {
        // Clustered mode: Use round-robin assignment
        use chronik_common::partition_assignment::PartitionAssignment as AssignmentManager;

        let peers = raft_manager.get_peers().await;
        info!("Auto-creating topic '{}' in cluster mode with {} peers", topic_name, peers.len());

        // v2.2.7 FIX: Partition initialization happens through PartitionAssignment module below
        // which has proper metadata WAL writes and replication (in raft_metadata_store.rs)
        // The broken initialize_raft_partitions() method has been removed.

        // ... rest of normal PartitionAssignment flow
    }
}
```

**Next Steps:**
1. ✅ Fix implemented - broken code path removed
2. 🔄 Build and restart cluster
3. 🔄 Test multi-partition topics with acks=all
4. 🔄 Verify single-partition topics still work

### Previous Attempt (Session 1)

**File**: `crates/chronik-server/src/produce_handler.rs`
**Line**: 2648 (now 2657)
**Initial Fix**:
```rust
// Changed leader calculation to always use first replica
let leader = replicas[0];  // Instead of checking Raft leader
```

This fix was correct but insufficient - there's a deeper bug preventing the correct leader value from being applied.

---

## Test Results

### Single Partition ✅
```bash
Topic: test-metadata-fix
Partitions: 1
Replicas: [1, 2, 3]
Leader: 1
Result: ✅ acks=all works perfectly
```

### Multi-Partition ❌
```bash
Topic: test-acks-multiple
Partitions: 3
Partition 0: replicas=[1, 2, 3], leader=1
Partition 1: replicas=[2, 3, 1], leader=2
Partition 2: replicas=[3, 1, 2], leader=3
Result: ❌ Timeout on first message (10 seconds)
```

**Log Evidence**:
- Produce request sent to Node 1
- Client picks a partition (likely partition 0)
- Request times out before any data appears in logs

---

## Architecture Overview

### Metadata Replication Flow

```
Raft Leader (Node 1)
  ↓
1. Partition assigned via Raft metadata command
  ↓
2. MetadataStateMachine.apply() processes command
  ↓
3. MetadataEventBus publishes PartitionAssigned event
  ↓
4. MetadataWalReplicator listens to event
  ↓
5. Replicates via WalReplicationManager to followers
  ↓
6. Followers receive WAL frame on port 9291/9292/9293
  ↓
7. Followers apply to their MetadataStateMachine
```

### acks=all Flow (When Working)

```
Producer → Node 1 (leader for partition 0)
  ↓
Node 1: ProduceHandler receives acks=-1 request
  ↓
Node 1: Write to WAL
  ↓
Node 1: Register acks=-1 wait in IsrAckTracker (quorum=2)
  ↓
Node 1: Send WAL frames to followers (Node 2, Node 3)
  ↓
Node 2: Receive WAL frame, replicate, send ACK back
Node 3: Receive WAL frame, replicate, send ACK back
  ↓
Node 1: WalReplicationManager receives ACKs
  ↓
Node 1: IsrAckTracker.record_ack() called (2 times)
  ↓
Node 1: Quorum reached (2/2) → notify producer
  ↓
Producer: Receive success response
```

---

## File Reference

### Core Implementation Files

1. **`crates/chronik-server/src/produce_handler.rs`**
   - Line 1060-1094: Partition leadership check (during produce)
   - Line 2300-2370: Cluster mode partition creation (PartitionAssignment module)
   - Line 2600-2679: WAL-ONLY partition initialization (FIXED line 2648)

2. **`crates/chronik-server/src/raft_metadata.rs`**
   - Line 235-245: AssignPartition command handler
   - Line 247-250: SetPartitionLeader command handler
   - Line 346-350: get_partition_leader() implementation

3. **`crates/chronik-server/src/isr_ack_tracker.rs`**
   - Line 68-94: register_wait() - Producer registers acks=-1 request
   - Line 106-183: record_ack() - Records follower ACKs

4. **`crates/chronik-server/src/metadata_wal_replication.rs`**
   - Line 152-178: Event listener for metadata replication
   - Line 181-254: Event handlers (PartitionAssigned, LeaderChanged, etc.)

5. **`crates/chronik-server/src/wal_replication.rs`**
   - Line 1523-1570: Event listener for partition follower registration
   - Line 1724-1820: ACK reader (receives ACKs from followers)

6. **`crates/chronik-common/src/partition_assignment.rs`**
   - Line 75-108: assign_partition() - Correctly sets leader to replicas[0]

### Documentation Files

1. **`docs/METADATA_LEADERSHIP_BUG.md`** - Complete root cause analysis
2. **`docs/ACKS_ALL_DEADLOCK_ROOT_CAUSE.md`** - Raft deadlock explanation
3. **`docs/ACKS_ALL_FIX_STATUS.md`** - Previous phase status
4. **`docs/WORK_SUMMARY.md`** - Implementation history

---

## Next Steps

### Immediate (Next Session)

1. **Investigate AdminClient vs Auto-Creation Paths**
   - Determine which code path is used when topic created via AdminClient
   - Check if both paths run and overwrite each other
   - Add logging to identify which path executes

2. **Fix Path Conflict**
   - Option A: Ensure only ONE path runs for a given topic creation
   - Option B: Make both paths use the same leader assignment logic
   - Option C: Disable one path entirely (prefer PartitionAssignment module)

3. **Test Multi-Partition Fix**
   - Create topic with 3 partitions
   - Send messages with acks=all
   - Verify all partitions accept produce requests correctly

### Verification Tests

```python
# Test 1: Single partition (should still work)
python3 -c "
from kafka.admin import KafkaAdminClient, NewTopic
from kafka import KafkaProducer
admin = KafkaAdminClient(bootstrap_servers='localhost:9092')
topic = NewTopic('test-1part', num_partitions=1, replication_factor=3)
admin.create_topics([topic])
producer = KafkaProducer(bootstrap_servers='localhost:9092', acks='all')
future = producer.send('test-1part', b'Test')
print('✅ SUCCESS' if future.get(timeout=10) else '✗ FAILED')
"

# Test 2: Multi-partition (should work after fix)
python3 -c "
from kafka.admin import KafkaAdminClient, NewTopic
from kafka import KafkaProducer
admin = KafkaAdminClient(bootstrap_servers='localhost:9092')
topic = NewTopic('test-3part', num_partitions=3, replication_factor=3)
admin.create_topics([topic])
producer = KafkaProducer(bootstrap_servers='localhost:9092', acks='all')
for i in range(10):
    future = producer.send('test-3part', f'Msg {i}'.encode())
    future.get(timeout=10)
print('✅ ALL MESSAGES SENT')
"
```

---

## Key Concepts

### Raft Leader vs Partition Leader

**CRITICAL DISTINCTION**:

1. **Raft Leader** (Cluster-Level)
   - One leader for the entire cluster
   - Handles metadata operations (create topic, assign partitions)
   - In our test: Node 1 is the Raft leader

2. **Partition Leader** (Partition-Level)
   - DIFFERENT leader for EACH partition
   - Determined by partition assignment (first replica in list)
   - Handles data operations (produce, fetch for that partition)
   - In multi-partition topic:
     - Partition 0 leader: Node 1
     - Partition 1 leader: Node 2
     - Partition 2 leader: Node 3

**The Bug**: Code was checking if current node is Raft leader instead of partition leader.

### acks=all Quorum Calculation

```rust
let isr_size = isr.len();
let quorum_size = (isr_size + 1) / 2;  // Ceiling division

// Examples:
// ISR=[1, 2, 3] → isr_size=3 → quorum=(3+1)/2=2
// ISR=[1, 2]    → isr_size=2 → quorum=(2+1)/2=1
// ISR=[1]       → isr_size=1 → quorum=(1+1)/2=1
```

Producer waits for `quorum_size` ACKs from followers before returning success.

---

## Build & Test Commands

### Build
```bash
/home/ubuntu/.cargo/bin/cargo build --release --bin chronik-server
```

### Restart Cluster
```bash
./tests/cluster/stop.sh && ./tests/cluster/start.sh
```

### View Logs
```bash
# Leadership checks
grep "Raft leadership check" tests/cluster/logs/node1.log

# Partition assignments
grep "PartitionAssigned" tests/cluster/logs/node1.log

# ACK tracking
grep "IsrAckTracker" tests/cluster/logs/node1.log

# Specific topic
grep "test-acks-multiple" tests/cluster/logs/node1.log
```

---

## Known Working Configuration

**Test Cluster** (3 nodes, all on localhost):
- Node 1: Kafka=9092, WAL=9291, Raft=5001
- Node 2: Kafka=9093, WAL=9292, Raft=5002
- Node 3: Kafka=9094, WAL=9293, Raft=5003

**Configuration Files**:
- `tests/cluster/node1.toml`
- `tests/cluster/node2.toml`
- `tests/cluster/node3.toml`

**Start/Stop Scripts**:
- `tests/cluster/start.sh`
- `tests/cluster/stop.sh`

---

## Session Handoff Checklist

When continuing in a new session, verify:

- [ ] Cluster is running (`./tests/cluster/start.sh`)
- [ ] Latest code is built (`cargo build --release --bin chronik-server`)
- [ ] Read this document completely
- [ ] Review `docs/METADATA_LEADERSHIP_BUG.md` for root cause details
- [ ] Check current git status to see pending changes
- [ ] Don't start from scratch - build on existing fixes!

---

## Contact/Debug Commands

```bash
# Check cluster status
ps aux | grep chronik-server

# Check logs in real-time
tail -f tests/cluster/logs/node*.log

# Test basic functionality
python3 test_node_ready.py

# Kill all background Python processes
pkill -f "python3.*test_acks"
```
