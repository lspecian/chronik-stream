# Raft E2E Test Status Report

**Date**: October 18, 2025
**Task**: Phase 1 Raft Single-Partition E2E Test Implementation
**Status**: ❌ CRITICAL ISSUES IDENTIFIED

## Executive Summary

The Raft single-partition E2E test infrastructure exists but is currently **NON-FUNCTIONAL** due to critical integration issues in the Raft metadata replication layer. The test timeout reveals fundamental problems with topic creation and broker registration in clustered mode.

## Test Infrastructure Status

### Existing Tests ✅

1. **Unit Test**: `tests/integration/raft_single_partition.rs`
   - Tests Raft layer in isolation using `PartitionReplica` directly
   - Status: **PASSING** (tests work at Raft layer level)
   - Coverage: Leader election, replication, failover at Raft protocol level

2. **Python E2E Test**: `test_raft_single_partition_simple.py`
   - Tests full stack: chronik-server processes + Kafka clients
   - Status: **FAILING** (times out waiting for cluster)
   - Coverage: Intended to test end-to-end message flow

3. **Leader Failover Test**: `test_raft_leader_failover.py`
   - Tests leader failover with continuous production
   - Status: **NOT TESTED** (depends on basic E2E working)

4. **Shell Bootstrap Test**: `test_healthcheck_bootstrap.sh`
   - Tests health-check based cluster bootstrap
   - Status: **FAILING** (same issues as Python test)

### Test Configuration Approaches

**Working Approach** (from shell scripts):
```bash
# Create cluster config file
cat > chronik-cluster-node1.toml <<EOF
enabled = true
node_id = 1
replication_factor = 3
min_insync_replicas = 2

[[peers]]
id = 1
addr = "localhost:9092"
raft_port = 9192
# ... more peers
[/gossip]
bind_addr = "0.0.0.0:7946"
bootstrap_expect = 3
EOF

# Start server with config file
./target/release/chronik-server \
    --advertised-addr localhost \
    --cluster-config chronik-cluster-node1.toml \
    standalone
```

**Non-Working Approach** (environment variables):
```bash
# Python test uses env vars - may not be wired up correctly
CHRONIK_CLUSTER_ENABLED=true
CHRONIK_NODE_ID=1
CHRONIK_CLUSTER_PEERS=...
```

## Critical Issues Identified

### Issue 1: Raft Metadata Entries Not Committing ❌

**Symptom**:
```
[INFO] ready() EXTRACTING for __meta-0: messages=0, entries=1, committed=0
[ERROR] Failed to auto-create topic 'test-topic' after 508ms:
    StorageError("Topic not found after creation")
```

**Root Cause**:
- Raft metadata replica (`__meta-0`) proposes entries but they never commit
- Commit index stays at 0 despite entries being proposed
- Appears to be a quorum/replication issue in the metadata Raft group

**Impact**:
- **BLOCKS ALL TOPIC CREATION** in cluster mode
- Metadata operations (create topic, register broker) fail
- Cluster cannot function

**Location**:
- `crates/chronik-server/src/raft_integration.rs` - ChronikStateMachine
- `crates/chronik-raft/src/replica.rs` - PartitionReplica

### Issue 2: No Brokers Registered in Metadata ❌

**Symptom**:
```
[WARN] No online brokers available for auto-topic creation
[INFO] Got 0 brokers from metadata store (filtered)
[ERROR] CRITICAL: Metadata response has NO brokers - AdminClient will fail!
```

**Root Cause**:
- Broker registration via Raft metadata fails due to Issue #1
- `register_broker()` calls Raft metadata propose which doesn't commit
- Metadata store remains empty

**Impact**:
- Kafka Metadata API returns empty broker list
- Clients cannot discover cluster topology
- AdminClient operations fail with "No resolvable bootstrap urls"

**Location**:
- `crates/chronik-server/src/integrated_server.rs` - broker registration
- `crates/chronik-common/src/metadata/raft_state_machine.rs` - broker storage

### Issue 3: Port Conflict on Node 3 ⚠️

**Symptom**:
```
[ERROR] Server task failed: Address already in use (os error 48)
```

**Root Cause**:
- Search API port calculation conflict
- Node 3 trying to bind to already-used port

**Impact**:
- Node 3 fails to start properly
- Reduces cluster to 2 nodes (no quorum)

**Location**:
- `crates/chronik-server/src/main.rs` - port configuration

### Issue 4: Topic Creation Timeout Loop 🔄

**Symptom**:
- ProduceHandler attempts to auto-create topic
- Waits 500ms for topic to appear in metadata
- Topic never appears (because Raft entry never commits)
- Returns error, client retries
- Infinite loop

**Root Cause**:
- Cascading failure from Issue #1
- No backoff or circuit breaker

**Impact**:
- Resource exhaustion
- Test timeouts

## Test Execution Results

### Test Run 1: test_raft_single_partition_simple.py

```
Command: python3 test_raft_single_partition_simple.py
Result: TIMEOUT (180 seconds)
```

**Timeline**:
- 0s: Start 3 nodes
- 3s: Nodes come online, Raft metadata leaders elected
- 5s: Broker registration attempted, entries proposed but not committed
- 10s: Test tries to produce, auto-create topic fails
- 10s-180s: Continuous retry loop, no progress
- 180s: Test timeout

### Test Run 2: test_healthcheck_bootstrap.sh

```
Command: ./test_healthcheck_bootstrap.sh
Result: FAILED (topic creation fails)
```

**Observations**:
- All 3 nodes start successfully
- Each node elects itself as metadata leader (expected for initial bootstrap)
- Broker registration logs show "Successfully registered broker N"
- BUT: Metadata store still shows 0 brokers (entries not committed)
- Topic creation via `kafka-topics` fails

## Root Cause Analysis

### Why Raft Metadata Entries Aren't Committing

**Hypothesis 1: No Message Routing Between Metadata Replicas**
- Each node has its own `__meta-0` PartitionReplica
- Raft messages from one replica to another may not be routed
- Without message exchange, no quorum, no commits

**Evidence**:
```
# Each node logs this independently:
[INFO] ready() HAS_READY for __meta-0: raft_state=Leader, term=1, leader=1
[INFO] ready() HAS_READY for __meta-0: raft_state=Leader, term=1, leader=2
[INFO] ready() HAS_READY for __meta-0: raft_state=Leader, term=1, leader=3
```

This suggests **3 separate Raft groups**, not 1 replicated group!

**Missing Component**: Raft message routing/RPC layer for metadata replicas

**Hypothesis 2: Incorrect Raft Group Initialization**
- Metadata replicas may be initialized without proper peer configuration
- Each node thinks it's the only member of the group
- Single-node "quorum" commits immediately, but state doesn't replicate

**Evidence**:
- `commit=0` never advances despite entries being proposed
- No "replication" messages in logs between nodes

### Where the Integration is Broken

```
┌─────────────────────────────────────────────────────────────┐
│                   WORKING LAYERS                            │
├─────────────────────────────────────────────────────────────┤
│ ✅ Kafka Protocol   - Clients can connect                   │
│ ✅ WAL Layer        - GroupCommitWal works                  │
│ ✅ Raft Protocol    - Unit tests pass                       │
│ ✅ Cluster Config   - Config files parsed correctly         │
├─────────────────────────────────────────────────────────────┤
│                   BROKEN LAYERS                             │
├─────────────────────────────────────────────────────────────┤
│ ❌ Raft Metadata Replication                                │
│    - Entries proposed but never committed                   │
│    - Appears to be 3 separate groups, not 1 replicated      │
│                                                              │
│ ❌ Raft Message Routing (for metadata)                      │
│    - No RPC layer between metadata replicas                 │
│    - Messages not exchanged between nodes                   │
│                                                              │
│ ❌ Metadata Store Integration                               │
│    - Dependent on Raft commits working                      │
│    - Broker registration fails                              │
│    - Topic creation fails                                   │
└─────────────────────────────────────────────────────────────┘
```

## Code Locations Requiring Investigation

### Primary Suspects

1. **Metadata Replica Initialization**
   - File: `crates/chronik-server/src/integrated_server.rs`
   - Function: `create_raft_metadata_replica()`
   - Issue: May not be configuring peer list correctly

2. **Raft Message Routing**
   - File: `crates/chronik-raft/src/rpc.rs`
   - Issue: RPC service may not be wired up for metadata replicas
   - Missing: Background task to route messages between nodes

3. **State Machine Apply**
   - File: `crates/chronik-raft/src/metadata_state_machine.rs`
   - Function: `apply()`
   - Issue: May not be called even when entries commit

### Related Code Paths

```rust
// integrated_server.rs - Broker registration
async fn register_broker_in_raft_metadata() {
    let op = MetadataOp::RegisterBroker { ... };
    let data = bincode::serialize(&op)?;

    // This propose never commits!
    let index = self.raft_metadata.propose(data).await?;

    // This timeout always fires because commit_index never reaches 'index'
    self.wait_for_metadata_commit(index, timeout).await?;
}

// replica.rs - Propose
pub async fn propose(&self, data: Vec<u8>) -> Result<u64> {
    let mut node = self.raw_node.write();
    node.propose(vec![], data)?;  // Propose entry

    // But how do AppendEntries messages get sent to peers?
    // Where is the RPC layer?
}

// Missing:
// - Background task calling raw_node.ready() periodically
// - Extraction of messages from ready()
// - Sending messages to peer nodes via RPC
// - Receiving messages from peers and calling raw_node.step()
```

## Recommendations

### Immediate Actions (Required for Test to Pass)

1. **Fix Raft Metadata Replication** (CRITICAL)
   - Ensure metadata replicas are initialized with full peer list
   - Wire up Raft RPC service for metadata group
   - Add background tick task to drive Raft state machine
   - Implement message routing between metadata replicas

2. **Add Diagnostic Logging**
   - Log peer configuration when creating metadata replica
   - Log all Raft messages sent/received
   - Log state transitions (Follower → Candidate → Leader)
   - Log commit index changes

3. **Fix Port Conflicts**
   - Review Search API port calculation
   - Ensure each node gets unique ports

4. **Add Integration Tests with Logging**
   - Test metadata replica initialization
   - Test broker registration commits properly
   - Test topic creation commits properly

### Investigation Steps

```bash
# Step 1: Check metadata replica peer configuration
RUST_LOG=chronik_raft::replica=trace,chronik_server::integrated_server=trace \
  ./target/release/chronik-server --cluster-config node1.toml standalone

# Look for:
# - "Creating metadata replica with peers: [...]"
# - "Initialized Raft with peers: [...]"
# - Should see [1, 2, 3], not []

# Step 2: Check if Raft messages are being sent
RUST_LOG=chronik_raft::rpc=trace,chronik_raft::replica=trace \
  ./target/release/chronik-server --cluster-config node1.toml standalone

# Look for:
# - "Sending AppendEntries to peer X"
# - "Received AppendEntries from peer Y"
# - If missing, RPC layer not wired up

# Step 3: Check state machine applies
RUST_LOG=chronik_raft::metadata_state_machine=trace \
  ./target/release/chronik-server --cluster-config node1.toml standalone

# Look for:
# - "Applying metadata operation: RegisterBroker"
# - "Broker X registered successfully"
# - If missing, entries not committing OR state machine not called
```

### Code Changes Needed

**Priority 1: Wire Up Metadata Replica Tick Loop**

```rust
// In integrated_server.rs or new raft_metadata_driver.rs

async fn drive_metadata_replica(
    replica: Arc<PartitionReplica>,
    peer_clients: HashMap<u64, RaftClient>,
) {
    let mut interval = tokio::time::interval(Duration::from_millis(100));

    loop {
        interval.tick().await;

        // Tick the Raft state machine
        replica.tick();

        // Process ready state
        if let Ok((messages, committed)) = replica.ready().await {
            // Route messages to peers
            for msg in messages {
                if let Some(client) = peer_clients.get(&msg.to) {
                    client.send_message(msg).await;
                }
            }

            // Log committed entries
            info!("Metadata replica committed {} entries", committed.len());
        }
    }
}
```

**Priority 2: Initialize Metadata Replica with Peers**

```rust
// In integrated_server.rs

fn create_raft_metadata_replica(&self) -> Result<PartitionReplica> {
    let config = RaftConfig { ... };
    let storage = Arc::new(MemoryLogStorage::new());

    // CRITICAL: Pass all peer IDs!
    let peer_ids: Vec<u64> = self.cluster_config.peers
        .iter()
        .map(|p| p.id)
        .filter(|&id| id != self.node_id)
        .collect();

    info!("Creating metadata replica with peers: {:?}", peer_ids);

    PartitionReplica::new(
        "__meta".to_string(),
        0,
        config,
        storage,
        peer_ids,  // ← This may be empty or wrong!
    )
}
```

## Conclusion

### Current State

The Raft implementation has:
- ✅ **Protocol Layer**: Working (unit tests pass)
- ✅ **Storage Layer**: Working (WAL integration good)
- ❌ **Integration Layer**: BROKEN (metadata replication fails)
- ❌ **E2E Tests**: FAILING (cannot create topics)

### Blocking Issue

**The metadata Raft group is not properly replicated**. Each node runs its own isolated metadata replica instead of participating in a unified 3-node Raft group. This causes:
- No quorum
- No commits
- No broker registration
- No topic creation
- Complete cluster dysfunction

### Next Steps

1. **URGENT**: Fix metadata replica initialization and message routing
2. Add comprehensive logging to diagnose Raft state
3. Create simpler test that just verifies metadata commits
4. Once metadata works, full E2E test should pass

### Time Estimate

- **Quick Fix**: If it's just missing peer configuration: 1-2 hours
- **Medium Fix**: If RPC layer needs wiring: 4-8 hours
- **Complex Fix**: If fundamental design issue: 1-2 days

### Test Readiness

**When Fixed, Existing Tests Will Work**:
- ✅ Test infrastructure already exists
- ✅ Test coverage is comprehensive
- ✅ Only integration layer needs fixing
- ⏸️ Can proceed with E2E testing immediately after fix

---

**Report Generated**: October 18, 2025
**Next Action**: Investigate metadata replica peer configuration and message routing
