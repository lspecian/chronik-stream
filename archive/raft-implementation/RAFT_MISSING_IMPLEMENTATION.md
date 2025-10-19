# Missing Raft Implementation - Complete Analysis

**Date**: 2025-10-16
**Status**: Raft infrastructure operational, consensus logic NOT active

## Test Results Summary

### ✅ What Works (Infrastructure Layer)
1. **Multi-node cluster startup** - 3 nodes start successfully
2. **Kafka protocol** - Clients connect and produce/consume messages
3. **Network layer** - All Kafka and Raft gRPC ports listening
4. **Local storage** - Messages persist to WAL on receiving node
5. **Code structure** - Raft components exist and compile

### ❌ What's Missing (Consensus Layer)

The Raft consensus logic is **NOT running**. Messages are stored only on the node that receives them, with **zero replication**.

## Critical Missing Components

### 1. **Automatic Replica Creation** 🚨 CRITICAL
**Status**: NOT IMPLEMENTED
**Impact**: No Raft groups are created, so no replication happens

**Current Behavior**:
- Topic `test-raft` created with 3 partitions
- ProduceHandler has `raft_manager` set
- But `create_replica()` is NEVER called

**Evidence**:
```bash
# From logs:
✅ "Raft integration enabled"
✅ "Setting RaftReplicaManager for ProduceHandler"
❌ NO "Creating Raft replica for test-raft-0" logs
```

**Why**:
The code in `produce_handler.rs:2041-2091` creates replicas, but it's ONLY called in the **topic creation path**, which runs when:
- Explicitly calling `create_topic_metadata()` from produce_handler
- This path is NOT triggered by auto-topic-creation in ProtocolHandler

**Location**: `crates/chronik-server/src/produce_handler.rs:2041-2091`

**What's Needed**:
```rust
// In produce_handler.rs, after topic auto-creation:
if let Some(ref raft_manager) = self.raft_manager {
    for partition in 0..num_partitions {
        raft_manager.create_replica(
            topic.clone(),
            partition as i32,
            segment_writer,
            wal_log_storage,
            peers, // Need peer node IDs from cluster config
        ).await?;
    }
}
```

**Priority**: 🔴 **P0 - Must implement first**

---

### 2. **Replica Tick Loop** 🚨 CRITICAL
**Status**: IMPLEMENTED but NOT RUNNING
**Impact**: Raft state machine doesn't advance, no leader election

**Current State**:
- Code exists: `raft_integration.rs:372-425`
- `start_processing_loop()` spawns a task that calls:
  - `replica.tick()` every 10ms
  - `replica.ready()` to get messages and committed entries
- But the loop is NEVER STARTED because replicas are never created

**Location**: `crates/chronik-server/src/raft_integration.rs:372-425`

**What's Needed**:
- Automatically call `start_processing_loop()` when replica is created
- Add to `create_replica()`:
```rust
// After creating replica
let replica_clone = replica.clone();
let state_machine_clone = state_machine.clone();
let raft_client = self.raft_client.clone();

tokio::spawn(async move {
    Self::start_processing_loop(
        replica_clone,
        state_machine_clone,
        raft_client,
        topic.clone(),
        partition,
    ).await;
});
```

**Priority**: 🔴 **P0 - Blocks leader election**

---

### 3. **Peer Configuration** 🟡 MODERATE
**Status**: PARTIALLY IMPLEMENTED
**Impact**: Replicas created with empty peer list, can't form quorum

**Current State**:
- `raft_cluster.rs` parses `--peers` flag correctly
- Calls `raft_manager.add_peer()` for each peer
- But replica creation uses `peers = vec![]` (empty)

**Evidence**:
```rust
// produce_handler.rs:2082
let peers = vec![]; // WRONG!
```

**What's Needed**:
- Pass peer node IDs from cluster config to `create_replica()`
- Modify `RaftReplicaManager` to store global peer list:
```rust
pub struct RaftReplicaManager {
    peer_nodes: Arc<RwLock<Vec<u64>>>, // Add this
    // ... existing fields
}

// Then use it:
let peers = self.peer_nodes.read().await.clone();
raft_manager.create_replica(..., peers).await?;
```

**Priority**: 🟡 **P1 - Required for multi-node replication**

---

### 4. **Raft RPC Implementation** 🟡 MODERATE
**Status**: STUB IMPLEMENTATION
**Impact**: Nodes can't communicate Raft messages

**Current State**:
```rust
// rpc.rs:104
async fn append_entries(&self, request: Request<AppendEntriesRequest>)
    -> Result<Response<AppendEntriesResponse>, Status> {
    todo!("Implement append_entries RPC")
}

// rpc.rs:116
async fn request_vote(&self, request: Request<RequestVoteRequest>)
    -> Result<Response<RequestVoteResponse>, Status> {
    todo!("Implement request_vote RPC")
}
```

**What's Needed**:
```rust
async fn append_entries(&self, request: Request<AppendEntriesRequest>)
    -> Result<Response<AppendEntriesResponse>, Status> {
    let req = request.into_inner();
    let partition_key = (req.topic.clone(), req.partition);

    let replica = self.replicas.get(&partition_key)
        .ok_or_else(|| Status::not_found("Replica not found"))?;

    // Convert to tikv/raft Message
    let raft_msg = Message {
        msg_type: MessageType::MsgAppend,
        from: req.leader_id,
        to: replica.node_id(),
        term: req.term,
        entries: req.entries.into_iter().map(|e| Entry {
            index: e.index,
            term: e.term,
            data: e.data,
            ..Default::default()
        }).collect(),
        ..Default::default()
    };

    // Step the message (raft.rs handles it)
    replica.step(raft_msg).await?;

    Ok(Response::new(AppendEntriesResponse { success: true }))
}
```

**Priority**: 🟡 **P1 - Required for replication**

---

### 5. **Bootstrap Leader Election** 🟢 LOW
**Status**: NOT IMPLEMENTED
**Impact**: No initial leader, cluster stuck

**Current State**:
- `--bootstrap` flag exists but does nothing
- No code to trigger initial leader election

**What's Needed**:
```rust
// In raft_cluster.rs after creating replicas
if config.bootstrap {
    // Campaign to become leader
    for partition in all_partitions {
        if let Some(replica) = raft_manager.get_replica(&topic, partition) {
            replica.campaign().await?;
        }
    }
}
```

**Priority**: 🟢 **P2 - Nice to have for initial setup**

---

### 6. **WAL-based Raft Log Storage** 🟡 MODERATE
**Status**: INTERFACE EXISTS, IMPLEMENTATION INCOMPLETE
**Impact**: Raft logs not durable, lose state on restart

**Current State**:
- `wal_raft_storage.rs` exists with correct interface
- Uses `WalManager` for storage
- But not fully tested with actual Raft operations

**What's Needed**:
- Test WAL-based storage with real Raft operations
- Verify recovery works correctly
- Add compaction for old Raft log entries

**Priority**: 🟡 **P1 - Required for production durability**

---

## Implementation Priority Order

### Phase 1: Minimal Working Raft (Single Partition)
**Goal**: Get ONE partition replicating across 3 nodes

1. **P0**: Fix automatic replica creation ⏱️ 2 hours
   - Hook into topic creation path
   - Create replicas for all partitions
   - Start processing loops automatically

2. **P0**: Implement Raft RPC handlers ⏱️ 4 hours
   - `append_entries()` - replicate log entries
   - `request_vote()` - leader election
   - Message conversion (gRPC ↔ tikv/raft)

3. **P1**: Configure peers correctly ⏱️ 1 hour
   - Pass peer list from cluster config
   - Store in RaftReplicaManager
   - Use when creating replicas

4. **P1**: Test and debug ⏱️ 4 hours
   - Produce messages
   - Verify leader election works
   - Verify replication to followers
   - Test failover

**Estimated Time**: 11 hours (1-2 days)

---

### Phase 2: Multi-Partition Raft
**Goal**: All partitions replicated, basic cluster operations

5. **P2**: Bootstrap leader election ⏱️ 2 hours
   - Trigger campaign on bootstrap node
   - Verify all partitions have leaders

6. **P1**: WAL-based log storage testing ⏱️ 4 hours
   - Test durability across restarts
   - Verify recovery works
   - Add log compaction

**Estimated Time**: 6 hours (1 day)

---

### Phase 3: Production Readiness
**Goal**: Handle failures, optimize performance

7. **P2**: Dynamic membership changes ⏱️ 8 hours
   - Add/remove nodes from cluster
   - Rebalance partitions

8. **P2**: Monitoring and metrics ⏱️ 4 hours
   - Raft state metrics
   - Leader election latency
   - Replication lag

9. **P3**: Performance optimization ⏱️ 8 hours
   - Batch Raft proposals
   - Pipeline replication
   - ReadIndex for consistent reads

**Estimated Time**: 20 hours (3-4 days)

---

## Quick Start: Minimal Changes to Get Raft Working

### Change 1: Auto-create replicas in ProduceHandler
**File**: `crates/chronik-server/src/produce_handler.rs`

Add this after topic creation (around line 2000):

```rust
// After creating topic metadata, create Raft replicas
#[cfg(feature = "raft")]
if let Some(ref raft_manager) = self.raft_manager {
    if raft_manager.is_enabled() {
        use chronik_storage::SegmentWriterConfig;
        use chronik_wal::WalConfig;

        for partition in 0..num_partitions {
            // Create SegmentWriter for this partition
            let writer_config = SegmentWriterConfig {
                data_dir: self.config.data_dir.clone(),
                retention_period_secs: 86400 * 7,
                compression_enabled: true,
                enable_tantivy: false,
            };
            let writer = Arc::new(SegmentWriter::new(writer_config));

            // Create WAL-based log storage
            let wal_path = format!("{}/wal/{}/{}",
                self.config.data_dir, topic_name, partition);
            let wal_config = WalConfig {
                wal_dir: wal_path.into(),
                ..Default::default()
            };
            let log_storage = Arc::new(chronik_wal::WalRaftStorage::new(wal_config)?);

            // Get peer list (FIXME: need to pass from cluster config)
            let peers = vec![1, 2, 3]; // For now, hardcode for 3-node cluster

            // Create replica
            raft_manager.create_replica(
                topic_name.to_string(),
                partition as i32,
                writer,
                log_storage,
                peers,
            ).await?;

            info!("Created Raft replica for {}-{}", topic_name, partition);
        }
    }
}
```

### Change 2: Implement append_entries RPC
**File**: `crates/chronik-raft/src/rpc.rs:104`

Replace `todo!()` with actual implementation (see section 4 above).

### Change 3: Implement request_vote RPC
**File**: `crates/chronik-raft/src/rpc.rs:116`

Replace `todo!()` with actual implementation (see section 4 above).

---

## Testing Plan

### Test 1: Leader Election
```bash
# Start 3-node cluster
./start_cluster.sh

# Check logs for leader election
grep "became leader" node*.log
grep "voted for" node*.log

# Expected: One node becomes leader for each partition
```

### Test 2: Replication
```bash
# Produce to leader
kafka-console-producer --topic test --bootstrap-server localhost:9092

# Consume from follower
kafka-console-consumer --topic test --bootstrap-server localhost:9093 --from-beginning

# Expected: Messages appear on follower
```

### Test 3: Failover
```bash
# Kill leader node
kill -9 <leader_pid>

# Wait for new leader election
sleep 5

# Produce to new leader
kafka-console-producer --topic test --bootstrap-server localhost:9093

# Expected: New leader accepts writes
```

---

## Summary

**Current State**: Raft infrastructure exists but is **NOT active**. Messages are stored locally only.

**Root Cause**: Replicas are never created, so no Raft groups exist, so no replication happens.

**Critical Path to Working Raft**:
1. Fix replica auto-creation (2 hours)
2. Implement RPC handlers (4 hours)
3. Configure peers (1 hour)
4. Test and debug (4 hours)

**Total Time to Minimal Working Raft**: ~11 hours (1-2 days of focused work)

**Next Step**: Start with Phase 1, Task 1 - Fix automatic replica creation.
