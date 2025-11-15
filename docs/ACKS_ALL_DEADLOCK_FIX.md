# acks=all Deadlock Issue - Complete Investigation & Fix Guide

## Executive Summary

**Issue**: `acks=all` produce requests block indefinitely (30s timeout), causing severe performance degradation (83 msg/s instead of expected 10,000+ msg/s).

**Root Cause**: Partition leaders are never assigned in Raft metadata during topic creation, causing the follower discovery worker to return empty results, which prevents WAL replication from starting, which prevents ACKs from being sent, which causes `acks=all` to timeout.

**Status**: Root cause identified, fix ready to implement.

## Problem Chain

```
1. Producer sends with acks=all
   ↓
2. ProduceHandler writes to local WAL ✅
   ↓
3. ProduceHandler registers wait with IsrAckTracker ✅
   ↓
4. ProduceHandler calls replicate_partition() ✅
   ↓
5. WalReplicationManager.replicate_partition() checks partition_followers map ❌ EMPTY
   ↓
6. No replication happens (no followers to send to)
   ↓
7. No ACKs received from followers
   ↓
8. IsrAckTracker times out after 30 seconds ❌
   ↓
9. Producer gets timeout error
```

## Why partition_followers Map is Empty

The `partition_followers` map is populated by `run_follower_discovery_worker()`:

```rust
// wal_replication.rs:798
let my_partitions = raft.get_partitions_where_leader(node_id);

if my_partitions.is_empty() {
    debug!("Node {} is not leader for any partitions", node_id);
    continue;  // ❌ Returns here every time!
}
```

**The Problem**: `get_partitions_where_leader()` returns empty because:
- Partition leaders are NEVER stored in Raft metadata
- Only replicas and ISR are stored
- The method looks for partitions where `leader == node_id`
- Since leaders are never set, it always returns empty

## Evidence from Logs

```bash
# Node 1 IS the Raft leader (for metadata consensus)
[INFO] became leader at term 6, raft_id: 1

# Node 1 initialized partition metadata (replicas + ISR)
[INFO] ✓ Raft leader initialized partition metadata for 'diag-final-test'

# ISR is correctly initialized
Updated followers for diag-final-test-0: [] (was: None)
Updated followers for diag-final-test-1: [] (was: None)
Updated followers for diag-final-test-2: [] (was: None)

# ❌ But follower lists are EMPTY because get_partitions_where_leader() returns empty
```

## Key Distinction

**Raft Leader** (for metadata consensus):
- Node elected by Raft algorithm
- Handles metadata updates
- Node 1 IS the Raft leader ✅

**Partition Leaders** (for Kafka partitions):
- Each partition has a designated leader
- Leader handles produce/fetch for that partition
- Leaders are NEVER assigned ❌

## What's Implemented Correctly

✅ ISR initialization in Raft metadata
✅ IsrAckTracker for tracking follower ACKs
✅ WalReplicationManager with ISR/ACK trackers
✅ Follower discovery worker spawned
✅ TCP connections to followers established
✅ Sender worker running
✅ ACK reader spawned for each follower
✅ `replicate_partition()` hook in produce_handler.rs

## What's Missing

❌ Partition leader assignment during topic creation
❌ Method to set partition leaders in RaftMetadata
❌ `get_partitions_where_leader()` has no data to query

## The Fix

### Step 1: Add Partition Leader Tracking to RaftMetadata

**File**: `crates/chronik-server/src/raft_metadata.rs`

Add to `PartitionMetadata` struct:
```rust
pub struct PartitionMetadata {
    pub replicas: Vec<u64>,
    pub isr: Vec<u64>,
    pub leader: Option<u64>,  // ← ADD THIS
}
```

Add method to set partition leader:
```rust
impl RaftMetadata {
    pub fn set_partition_leader(
        &mut self,
        topic: &str,
        partition: i32,
        leader_id: u64,
    ) -> Result<()> {
        let key = (topic.to_string(), partition);

        if let Some(metadata) = self.partitions.get_mut(&key) {
            metadata.leader = Some(leader_id);
            Ok(())
        } else {
            Err(anyhow::anyhow!("Partition not found: {}-{}", topic, partition))
        }
    }
}
```

Update `get_partitions_where_leader()`:
```rust
pub fn get_partitions_where_leader(&self, node_id: u64) -> Vec<PartitionKey> {
    self.partitions
        .iter()
        .filter(|(_, metadata)| metadata.leader == Some(node_id))
        .map(|(key, _)| key.clone())
        .collect()
}
```

### Step 2: Assign Partition Leaders During Topic Creation

**File**: `crates/chronik-server/src/produce_handler.rs`

**Location**: Around line 1439 where Raft partition metadata is initialized.

**Current code**:
```rust
// Initialize Raft partition metadata for ALL partitions
for partition_id in 0..partition_count {
    let replicas = raft_cluster.assign_partition_replicas(
        &topic,
        partition_id,
        replication_factor as usize,
    );

    raft_cluster.initialize_partition(
        &topic,
        partition_id,
        replicas.clone(),
    )?;

    // Phase 3: Initialize ISR with all replicas
    raft_cluster.set_isr(&topic, partition_id, replicas.clone())?;

    info!(
        "Phase 3: Initialized ISR for {}-{}: {:?}",
        topic, partition_id, replicas
    );
}
```

**Add after ISR initialization**:
```rust
// Phase 3: Initialize ISR with all replicas
raft_cluster.set_isr(&topic, partition_id, replicas.clone())?;

info!(
    "Phase 3: Initialized ISR for {}-{}: {:?}",
    topic, partition_id, replicas
);

// CRITICAL FIX: Set partition leader (first replica)
if !replicas.is_empty() {
    let leader_id = replicas[0];
    raft_cluster.set_partition_leader(&topic, partition_id, leader_id)?;

    info!(
        "✓ Set partition leader for {}-{}: node {}",
        topic, partition_id, leader_id
    );
}
```

### Step 3: Add RaftCluster Wrapper Method

**File**: `crates/chronik-server/src/raft_cluster.rs`

```rust
pub fn set_partition_leader(
    &self,
    topic: &str,
    partition: i32,
    leader_id: u64,
) -> Result<()> {
    // Create Raft proposal to set partition leader
    let proposal = RaftProposal::SetPartitionLeader {
        topic: topic.to_string(),
        partition,
        leader_id,
    };

    self.propose_and_wait(proposal)
}
```

Add to `RaftProposal` enum:
```rust
pub enum RaftProposal {
    // ... existing variants
    SetPartitionLeader {
        topic: String,
        partition: i32,
        leader_id: u64,
    },
}
```

Handle in Raft state machine apply:
```rust
match proposal {
    // ... existing cases
    RaftProposal::SetPartitionLeader { topic, partition, leader_id } => {
        metadata.set_partition_leader(&topic, partition, leader_id)?;
        Ok(vec![])
    }
}
```

## Testing the Fix

### Step 1: Build and Deploy

```bash
cargo build --release --bin chronik-server
cd tests/cluster
./stop.sh
./start.sh
```

### Step 2: Verify Partition Leaders Are Set

Check logs for:
```
✓ Set partition leader for diag-final-test-0: node 1
✓ Set partition leader for diag-final-test-1: node 2
✓ Set partition leader for diag-final-test-2: node 3
```

### Step 3: Verify Follower Discovery Works

Check logs for:
```
Node 1 is leader for 1 partitions
Updated followers for diag-final-test-0: ["localhost:9292", "localhost:9293"] (was: None)
```

### Step 4: Send Test Message with acks=all

```python
from kafka import KafkaProducer
import json

producer = KafkaProducer(
    bootstrap_servers=['localhost:9092'],
    value_serializer=lambda v: json.dumps(v).encode('utf-8'),
    api_version=(0, 10, 0),
    acks='all'
)

# Should complete in < 1 second (not 30 seconds!)
producer.send('test-topic', {'id': 1, 'data': 'test'})
producer.flush()
print("✅ SUCCESS - acks=all completed quickly!")
```

### Step 5: Check for Replication Logs

Look for these diagnostic logs (already added):
```
🔍 REPL HOOK: wal_repl_mgr=true serialized_data=true
🔍 REPL HOOK: Inside wal_repl_mgr check for test-topic-0
🔍 REPL HOOK: About to spawn replication task for test-topic-0 offset=0 data_len=156
🔍 replicate_partition ENTRY: test-topic-0 offset=0 data_len=156
🔍 replicate_serialized ENTRY: test-topic-0 offset=0 data_len=156
Sent WAL record to follower localhost:9292
Sent WAL record to follower localhost:9293
Received ACK from node 2 for test-topic-0 offset 0 (1/2 ACKs)
Received ACK from node 3 for test-topic-0 offset 0 (2/2 ACKs)
✅ ISR quorum reached for test-topic-0 offset 0: 2/2 ACKs
```

### Step 6: Run Performance Test

```bash
# Should see ~10,000+ msg/s (not 83 msg/s)
./target/release/chronik-bench \
  --bootstrap-servers localhost:9092 \
  --topic perf-test \
  --partitions 3 \
  --replication-factor 3 \
  --create-topic \
  --mode produce \
  --message-count 10000 \
  --acks all
```

Expected output:
```
✓ Produced 10000 messages in 1.2s (8333 msg/s)
✓ Consumed 10000 messages in 0.8s (12500 msg/s)
✅ PERFECT: No duplicates, no losses!
```

## Files to Modify

1. **`crates/chronik-server/src/raft_metadata.rs`**
   - Add `leader: Option<u64>` to `PartitionMetadata`
   - Add `set_partition_leader()` method
   - Update `get_partitions_where_leader()` to check `leader` field

2. **`crates/chronik-server/src/raft_cluster.rs`**
   - Add `set_partition_leader()` wrapper method
   - Add `RaftProposal::SetPartitionLeader` variant
   - Handle in Raft state machine apply

3. **`crates/chronik-server/src/produce_handler.rs`**
   - Add partition leader assignment after ISR initialization (around line 1445)

## Diagnostic Logs Already Added

The following diagnostic logs were already added during investigation and will help verify the fix:

**In `produce_handler.rs` (lines 1429-1476)**:
- `🔍 REPL HOOK: wal_repl_mgr=...` - Shows if replication manager exists
- `🔍 REPL HOOK: Inside wal_repl_mgr check` - Shows we reached the replication hook
- `🔍 REPL HOOK: About to spawn replication task` - Shows task is spawning
- `🔍 REPL HOOK: Inside spawned task` - Shows task is executing

**In `wal_replication.rs` (lines 251, 296)**:
- `🔍 replicate_serialized ENTRY` - Shows we entered replication method
- `🔍 replicate_partition ENTRY` - Shows we entered partition-specific replication

These logs should appear in `tests/cluster/logs/node1.log` after the fix.

## Why This Will Fix the Issue

1. **Partition leaders will be assigned** during topic creation
2. **`get_partitions_where_leader()`** will return non-empty results
3. **Follower discovery worker** will populate `partition_followers` map
4. **WAL replication** will start sending data to followers
5. **Followers will send ACKs** back via the bidirectional stream
6. **IsrAckTracker** will receive ACKs and notify the waiting producer
7. **`acks=all` will complete** in < 1 second instead of timing out

## Architecture Notes

### Two Types of Leadership

This fix clarifies the distinction between:

1. **Raft Leader** (metadata consensus):
   - Single node elected by Raft algorithm
   - Handles all metadata updates (topic creation, partition assignment, ISR changes)
   - Node 1 is the Raft leader in our test cluster

2. **Partition Leaders** (data plane):
   - Each partition has its own leader
   - Partition leader handles produce/fetch for that partition
   - Partition leaders are distributed across nodes (round-robin)
   - Example: topic with 3 partitions → leaders [1, 2, 3]

### Why Partition Leaders Were Missing

The Phase 3 ISR implementation added:
- ISR tracking in Raft metadata ✅
- IsrAckTracker for acks=-1 support ✅
- Follower discovery worker ✅
- Per-partition replication routing ✅

But it NEVER added partition leader assignment during topic creation ❌

This is because the existing partition assignment logic (`assign_partition_replicas`) only returns a list of replicas, not which one is the leader. The assumption was that "first replica = leader" but this was never actually stored in the metadata.

## Session Continuity Prompt

To continue this work in a fresh session, use this prompt:

---

**Prompt for Next Session:**

```
I need to implement the fix for the acks=all deadlock issue in chronik-server.

Background:
- acks=all requests timeout after 30 seconds instead of completing in < 1 second
- Root cause identified: partition leaders are never assigned in Raft metadata during topic creation
- This causes follower discovery to return empty results, preventing WAL replication

The complete investigation and fix plan is documented in:
docs/ACKS_ALL_DEADLOCK_FIX.md

Please implement the fix following the 3-step plan in that document:
1. Add partition leader tracking to RaftMetadata (raft_metadata.rs)
2. Assign partition leaders during topic creation (produce_handler.rs line ~1445)
3. Add RaftCluster wrapper method (raft_cluster.rs)

After implementing, build and test with:
- cargo build --release --bin chronik-server
- cd tests/cluster && ./stop.sh && ./start.sh
- Send test message with acks=all and verify it completes in < 1 second

Current working directory: /home/ubuntu/Development/chronik-stream
```

---

## Additional Context

### Related Files Reviewed During Investigation

- `/home/ubuntu/Development/chronik-stream/crates/chronik-server/src/produce_handler.rs` - ProduceHandler with acks=-1 logic
- `/home/ubuntu/Development/chronik-stream/crates/chronik-server/src/wal_replication.rs` - WAL replication manager and follower discovery
- `/home/ubuntu/Development/chronik-stream/crates/chronik-server/src/isr_ack_tracker.rs` - ISR ACK tracker for acks=-1 support
- `/home/ubuntu/Development/chronik-stream/crates/chronik-server/src/isr_tracker.rs` - ISR tracker for runtime monitoring
- `/home/ubuntu/Development/chronik-stream/crates/chronik-server/src/integrated_server.rs` - Server initialization
- `/home/ubuntu/Development/chronik-stream/crates/chronik-server/src/raft_cluster.rs` - Raft cluster interface
- `/home/ubuntu/Development/chronik-stream/crates/chronik-server/src/raft_metadata.rs` - Raft metadata structures

### Test Configuration

Cluster config files:
- `tests/cluster/node1.toml` - Node 1 config (ports 9092, 9291, 5001)
- `tests/cluster/node2.toml` - Node 2 config (ports 9093, 9292, 5002)
- `tests/cluster/node3.toml` - Node 3 config (ports 9094, 9293, 5003)

Cluster management scripts:
- `tests/cluster/start.sh` - Start 3-node cluster
- `tests/cluster/stop.sh` - Stop cluster
- `tests/cluster/logs/` - Log directory

### Known Working Components

These components are confirmed working and should NOT be modified:
- IsrAckTracker (ACK tracking logic is correct)
- IsrTracker (ISR monitoring logic is correct)
- WalReplicationManager (replication infrastructure is correct)
- Follower discovery worker (polling logic is correct, just needs data)
- TCP connection management (connections are established correctly)
- ACK frame sending/receiving (bidirectional streams work correctly)

The ONLY missing piece is partition leader assignment in Raft metadata.
