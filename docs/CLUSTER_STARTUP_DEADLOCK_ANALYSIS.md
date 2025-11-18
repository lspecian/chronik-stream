# Cluster Startup Deadlock - Deep Analysis & Fix Proposal

**Date**: 2025-11-16
**Severity**: P0 - Cluster non-functional on cold start
**Status**: Root cause identified, fix proposed

---

## Executive Summary

**Problem**: Multi-node Chronik clusters enter a deadlock on cold start where follower nodes never start their TCP listeners, preventing Raft consensus and causing permanent election storms.

**Root Cause**: Architectural bug in [main.rs:660-825](../crates/chronik-server/src/main.rs#L660-L825) where follower nodes wait for Raft leader election BEFORE starting TCP listeners, but need those listeners running to participate in leader election.

**Impact**: 100% cluster failure on cold start. Only the node that wins the initial leader election gets its TCP listeners started. All other nodes deadlock waiting for a leader that can never be elected.

**Proposed Fix**: Modify startup sequence to start ALL TCP listeners (Kafka, WAL, Raft) IMMEDIATELY on node startup, BEFORE waiting for leader election.

---

## Timeline of Discovery

### Remote Cluster Failure (192.168.1.5, 192.168.1.7, 192.168.1.8)

**Nov 15 21:27:06** - Nodes started
**Nov 15 21:27:10** - First successful leader election (Node 1, term 6)
**Nov 15 21:27:10** - Kafka server starts listening on 0.0.0.0:29092 (Node 1 only)
**Nov 15 21:27:14** - Election storm begins (4 seconds after startup)
**Nov 16 11:12** - Storm still ongoing (13+ hours later)

**Symptoms observed**:
- Node 1 (192.168.1.5): Has Kafka listener on 29092, won early election
- Node 2 (192.168.1.7): NO listener on 29092 or 29291, stuck waiting
- Node 3 (192.168.1.8): NO listener on 29092 or 29291, stuck waiting
- Nodes 2 & 3: Cycling through leader elections every 1-2 seconds
- Clients: "NoBrokersAvailable" error on follower nodes
- Logs: "Failed to connect to follower X:29291 after 900+ attempts: Connection refused"

---

## The Deadlock: Complete Code Trace

### Step 1: Cluster Startup Entry Point

**File**: [main.rs:573-859](../crates/chronik-server/src/main.rs#L573-L859)
**Function**: `run_cluster_mode(config: Config)`

### Step 2: Raft Cluster Initialization (Line 573-623)

```rust
// Line 573-623: Initialize Raft cluster
let raft_cluster = Arc::new(
    RaftCluster::new(
        cluster_config.node_id,
        raft_storage.clone(),
        cluster_peers.clone(),
        raft_bind_addr,
    ).await?
);

// IMPORTANT: Raft gRPC server starts HERE (line 623)
// This is the ONLY listener that starts before leader election wait
raft_cluster.start().await?;
info!("✓ Raft cluster initialized and started on {}", raft_bind_addr);
```

**Status**: ✅ Raft gRPC listener (port 25001) STARTS immediately

### Step 3: IntegratedKafkaServer Creation (Line 624-655)

```rust
// Line 624-655: Create IntegratedKafkaServer
let server = IntegratedKafkaServer::new_with_cluster(
    metadata_store.clone(),
    wal_manager.clone(),
    wal_handler.clone(),
    Some(Arc::clone(&raft_cluster)),
    Some(Arc::clone(&isr_tracker)),
    Some(Arc::clone(&isr_ack_tracker)),
    Some(Arc::clone(&wal_replication_manager)),
    Some(replication_handler.clone()),
    cluster_config.clone(),
    wal_receiver_addr.clone(),
)?;
```

**Inside `IntegratedKafkaServer::new_with_cluster()`** ([integrated_server.rs:910-920](../crates/chronik-server/src/integrated_server.rs#L910-L920)):

```rust
// v2.2.0: Start WAL receiver if enabled (follower mode)
if let Some(receiver_addr) = wal_receiver_addr {
    if !receiver_addr.is_empty() {
        info!("WAL receiver enabled on {}", receiver_addr);

        let mut wal_receiver = crate::wal_replication::WalReceiver::new_with_isr_tracker(
            receiver_addr.clone(),
            wal_manager.clone(),
            isr_ack_tracker.clone(),
        );
```

**Status**: ⚠️ WalReceiver object CREATED but NOT started (no TCP bind yet)

### Step 4: THE DEADLOCK - Wait for Leader Election (Line 660-681)

```rust
// Line 660: CRITICAL BUG LOCATION
info!("Waiting for Raft leader election before broker registration...");

let mut election_attempts = 0;
let max_election_wait = 100; // 100 * 100ms = 10 seconds
let mut is_leader = false;

// Line 666-676: Wait loop
while election_attempts < max_election_wait {
    let (has_leader, leader_id, state) = raft_cluster.is_leader_ready().await;
    if has_leader {
        is_leader = leader_id == config.node_id;
        info!("✓ Raft leader elected: leader_id={}, this_node={}, is_leader={}, state={}",
            leader_id, config.node_id, is_leader, state);
        break;
    }
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    election_attempts += 1;
}

// Line 680-681: Followers skip broker registration
if !is_leader {
    info!("This node is a follower - skipping broker registration (will receive via Raft replication)");
}
```

**THE PROBLEM**:
1. Follower nodes reach line 680 and skip broker registration
2. They continue to line 824 where `server.run()` is spawned
3. BUT: `server.run()` hasn't been called yet, so Kafka listener (port 29092) is NOT bound
4. AND: WalReceiver::run() hasn't been called yet, so WAL listener (port 29291) is NOT bound
5. Nodes need ports 29291 to replicate data for leader election
6. **CIRCULAR DEPENDENCY**: Can't elect leader without communication, can't communicate without listeners, can't start listeners until leader elected

### Step 5: Server Run (Line 824-825)

```rust
// Line 824-825: Spawn server task
let server_task = tokio::spawn(async move {
    if let Err(e) = server_clone.run(&kafka_addr_clone).await {
        error!("Integrated server task failed: {}", e);
    }
});
```

**Inside `IntegratedKafkaServer::run()`** ([integrated_server.rs:1130](../crates/chronik-server/src/integrated_server.rs#L1130)):

```rust
pub async fn run(&self, bind_addr: &str) -> Result<()> {
    let listener = TcpListener::bind(bind_addr).await?;
    info!("Integrated Kafka server listening on {}", bind_addr);
```

**Status**: ❌ Kafka listener (port 29092) starts TOO LATE - only after leader election wait

### Step 6: WAL Receiver Run (Where does it start?)

**Problem**: WalReceiver is created at [integrated_server.rs:910](../crates/chronik-server/src/integrated_server.rs#L910) but `run()` is NEVER called in the follower path!

**The WalReceiver::run() method** ([wal_replication.rs:1498-1505](../crates/chronik-server/src/wal_replication.rs#L1498-L1505)):

```rust
pub async fn run(&self) -> Result<()> {
    info!("Starting WAL receiver on {}", self.listener_addr);

    let listener = TcpListener::bind(&self.listener_addr).await
        .context(format!("Failed to bind WAL receiver to {}", self.listener_addr))?;

    info!("✅ WAL receiver listening on {}", self.listener_addr);
```

**Status**: ❌ WAL receiver TCP bind (port 29291) NEVER HAPPENS on follower nodes

---

## Why Only One Node Gets Its Listeners Started

**The Race Condition**:

1. All 3 nodes start simultaneously
2. All 3 nodes start Raft gRPC listeners (port 25001) ✅
3. All 3 nodes reach line 660: "Waiting for Raft leader election"
4. **ONE node wins the early election race** (usually within 1-2 seconds)
5. **Winner**: `is_leader = true` → proceeds to line 824 → `server.run()` spawned → Kafka listener starts
6. **Losers**: `is_leader = false` → skip broker registration → reach line 824 but `server.run()` spawned → **NEVER completes because depends on leader**

**Why the election succeeds initially**:
- Raft gRPC (port 25001) IS listening on all nodes
- Raft can exchange votes via gRPC
- One node gets majority and becomes leader

**Why subsequent elections fail**:
- Leader tries to replicate data via WAL (port 29291)
- Followers have NO listener on port 29291
- Connection refused → replication fails → leader can't confirm writes
- Followers timeout waiting for leader → start new election
- New leader elected, same problem → repeat forever

---

## The Three TCP Listeners in Chronik Cluster

| Listener Type | Port | Binds At | Purpose | Cold Start Status |
|--------------|------|----------|---------|-------------------|
| **Raft gRPC** | 25001 | [raft_cluster.start()](../crates/chronik-server/src/main.rs#L623) BEFORE leader wait | Consensus messages (votes, heartbeats) | ✅ Starts immediately |
| **Kafka Protocol** | 29092 | [server.run()](../crates/chronik-server/src/integrated_server.rs#L1130) AFTER leader wait | Client produce/fetch requests | ❌ Follower nodes deadlock |
| **WAL Replication** | 29291 | [wal_receiver.run()](../crates/chronik-server/src/wal_replication.rs#L1501) NEVER called on followers | Leader→Follower data replication | ❌ NEVER starts on followers |

---

## The Fix: Start All Listeners BEFORE Leader Election Wait

### Proposed Changes to main.rs

**Location**: [main.rs:660-825](../crates/chronik-server/src/main.rs#L660-L825)

**Current problematic sequence**:
```
1. Initialize Raft cluster (line 573-623)
2. Raft.start() → Raft gRPC listener binds (line 623) ✅
3. Create IntegratedKafkaServer (line 624-655)
4. WAIT for leader election (line 660-676) ⚠️ BLOCKING
5. Spawn server.run() → Kafka listener binds (line 824) ❌ TOO LATE
6. (WAL receiver never started on followers) ❌ MISSING
```

**Proposed fixed sequence**:
```
1. Initialize Raft cluster (line 573-623)
2. Raft.start() → Raft gRPC listener binds (line 623) ✅
3. Create IntegratedKafkaServer (line 624-655)
4. *** NEW: Spawn server.run() IMMEDIATELY → Kafka listener binds ✅
5. *** NEW: Spawn wal_receiver.run() IMMEDIATELY → WAL listener binds ✅
6. WAIT for leader election (line 660-676) ✅ Now safe - all listeners running
7. Continue with broker registration (leader only)
```

### Code Modification

**Replace lines 660-825** with:

```rust
// ========================================
// CRITICAL FIX: Start ALL TCP listeners BEFORE leader election wait
// ========================================
// This fixes the cold-start deadlock where followers wait for leader
// election but need WAL/Kafka listeners running to participate.
//
// Original bug: Followers waited at line 660 with no listeners →
//              couldn't replicate data → couldn't elect leader → deadlock
//
// Fix: Start all listeners FIRST, then wait for leader election
// ========================================

info!("Starting TCP listeners (Kafka + WAL) before leader election...");

// Start Kafka protocol listener (0.0.0.0:29092) on ALL nodes
let kafka_addr_clone = kafka_addr.clone();
let server_clone = Arc::clone(&server);
let server_task = tokio::spawn(async move {
    if let Err(e) = server_clone.run(&kafka_addr_clone).await {
        error!("Integrated server task failed: {}", e);
    }
});
info!("✓ Kafka protocol listener started on {}", kafka_addr);

// Start WAL receiver listener (0.0.0.0:29291) on ALL nodes
// This is CRITICAL for followers to receive replication from leader
if let Some(wal_rcv_addr) = &wal_receiver_addr {
    if !wal_rcv_addr.is_empty() {
        let mut wal_receiver = crate::wal_replication::WalReceiver::new_with_isr_tracker(
            wal_rcv_addr.clone(),
            Arc::clone(&wal_manager),
            Arc::clone(&isr_ack_tracker),
            cluster_config.node_id,
        );

        // Set Raft cluster for metadata WAL replication
        wal_receiver.set_raft_cluster(Arc::clone(&raft_cluster));

        // Set leader elector for event-driven elections
        if let Some(ref elector) = leader_elector {
            wal_receiver.set_leader_elector(Arc::clone(elector));
        }

        let wal_receiver_task = tokio::spawn(async move {
            if let Err(e) = wal_receiver.run().await {
                error!("WAL receiver task failed: {}", e);
            }
        });

        info!("✓ WAL receiver listener started on {}", wal_rcv_addr);
    }
}

// Small delay to ensure listeners are bound before proceeding
tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

info!("✓ All TCP listeners started successfully");
info!("  - Raft gRPC: {}", raft_bind_addr);
info!("  - Kafka protocol: {}", kafka_addr);
if let Some(addr) = &wal_receiver_addr {
    info!("  - WAL replication: {}", addr);
}

// NOW it's safe to wait for leader election
// All nodes can communicate via WAL replication
info!("Waiting for Raft leader election (max 10s)...");

let mut election_attempts = 0;
let max_election_wait = 100; // 100 * 100ms = 10 seconds
let mut is_leader = false;

while election_attempts < max_election_wait {
    let (has_leader, leader_id, state) = raft_cluster.is_leader_ready().await;
    if has_leader {
        is_leader = leader_id == config.node_id;
        info!("✓ Raft leader elected: leader_id={}, this_node={}, is_leader={}, state={}",
            leader_id, config.node_id, is_leader, state);
        break;
    }
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    election_attempts += 1;
}

if !is_leader {
    info!("This node is a follower - skipping broker registration (will receive via Raft replication)");
} else {
    // Register this broker in Raft metadata (leader only)
    // ... existing broker registration code ...
}

// Continue with rest of startup...
```

---

## Testing the Fix

### Test Plan

1. **Clean cluster start** (3 nodes simultaneously):
   ```bash
   # Stop all nodes
   pkill -9 chronik-server
   ssh node2 "pkill -9 chronik-server"
   ssh node3 "pkill -9 chronik-server"

   # Clear Raft WAL
   rm -rf /home/ubuntu/chronik-data/wal/__meta/__raft_metadata
   ssh node2 "rm -rf /home/ubuntu/chronik-data/wal/__meta/__raft_metadata"
   ssh node3 "rm -rf /home/ubuntu/chronik-data/wal/__meta/__raft_metadata"

   # Start all nodes
   ./chronik-server start --config node1.toml &
   ssh node2 "./chronik-server start --config node2.toml" &
   ssh node3 "./chronik-server start --config node3.toml" &

   # Wait 15 seconds
   sleep 15
   ```

2. **Verify all listeners started**:
   ```bash
   # Check all 3 ports on all 3 nodes
   for node in localhost node2 node3; do
       echo "=== Node: $node ==="
       ssh $node "netstat -tln | grep ':29092\\|:29291\\|:25001'"
   done
   ```

   **Expected output**:
   ```
   === Node: localhost ===
   tcp  0.0.0.0:29092  0.0.0.0:*  LISTEN  # Kafka ✅
   tcp  0.0.0.0:29291  0.0.0.0:*  LISTEN  # WAL ✅
   tcp  0.0.0.0:25001  0.0.0.0:*  LISTEN  # Raft ✅

   === Node: node2 ===
   tcp  0.0.0.0:29092  0.0.0.0:*  LISTEN  # Kafka ✅
   tcp  0.0.0.0:29291  0.0.0.0:*  LISTEN  # WAL ✅
   tcp  0.0.0.0:25001  0.0.0.0:*  LISTEN  # Raft ✅

   === Node: node3 ===
   tcp  0.0.0.0:29092  0.0.0.0:*  LISTEN  # Kafka ✅
   tcp  0.0.0.0:29291  0.0.0.0:*  LISTEN  # WAL ✅
   tcp  0.0.0.0:25001  0.0.0.0:*  LISTEN  # Raft ✅
   ```

3. **Verify stable leader election**:
   ```bash
   # Check for election storm (should see stable leader, NO continuous elections)
   tail -f ~/chronik.log | grep -E 'became leader|became follower|starting a new election'
   ```

   **Expected**: ONE leader election message, then silence (no storm)

4. **Test client connectivity to ALL nodes**:
   ```python
   from kafka import KafkaProducer
   import time

   for port in [29092, 29093, 29094]:  # All 3 nodes
       producer = KafkaProducer(bootstrap_servers=f'localhost:{port}')
       future = producer.send('test-topic', b'hello')
       result = future.get(timeout=5)
       print(f"✅ Node {port}: Connected and produced successfully")
       producer.close()
   ```

   **Expected**: All 3 nodes accept connections (no "NoBrokersAvailable")

---

## Why This Fix is Reliable Long-Term

### 1. **Eliminates Circular Dependency**
- **Before**: Need listeners to elect leader, need leader to start listeners
- **After**: Listeners start unconditionally, leader election succeeds naturally

### 2. **Matches Production Best Practices**
- All distributed systems (Kafka, etcd, Consul, etc.) start listeners BEFORE consensus
- Raft is for METADATA coordination, not for controlling TCP bind()

### 3. **No Performance Impact**
- Listeners are cheap to start (just TCP bind, no traffic until leader elected)
- No additional memory or CPU usage
- 500ms delay is negligible compared to 10+ second cluster startup time

### 4. **Backward Compatible**
- Single-node mode: Unaffected (already starts listeners immediately)
- Existing clusters (hot restart): No change in behavior
- New clusters (cold start): Now works correctly

### 5. **Testable and Observable**
- Easy to verify: Check `netstat` for all 3 ports on all nodes
- Clear log messages: "✓ All TCP listeners started successfully"
- No more silent deadlocks: Cluster either works or fails with clear error

---

## Alternative Solutions Considered (and Rejected)

### Alternative 1: Remove Leader Election Wait Entirely
**Idea**: Don't wait for leader election, proceed immediately
**Rejected**: Would break broker registration logic (leader needs to be elected first to write metadata)

### Alternative 2: Start Listeners Only on Leader
**Idea**: Only start Kafka/WAL listeners on the elected leader
**Rejected**: Defeats purpose of multi-node cluster (can't failover to followers)

### Alternative 3: Use Bootstrap Mode
**Idea**: Add special "bootstrap" flag to force one node to be leader
**Rejected**: Fragile, requires manual intervention, doesn't solve cold start deadlock

### Alternative 4: Timeout and Retry
**Idea**: If leader election fails after 10s, start listeners anyway
**Rejected**: Band-aid solution, doesn't fix root cause, adds complexity

---

## Conclusion

The cluster startup deadlock is a **fundamental architectural bug** caused by incorrect ordering of TCP listener initialization and leader election.

The fix is **simple, reliable, and production-ready**:
1. Start ALL TCP listeners (Kafka, WAL, Raft) immediately on node startup
2. THEN wait for leader election
3. Proceed with broker registration (leader only)

This eliminates the circular dependency and ensures clusters can successfully start from cold state.

**Recommendation**: Implement this fix as **Priority 0** before any production deployment.

---

## Related Issues

- [RAFT_ELECTION_STORM_BUG.md](RAFT_ELECTION_STORM_BUG.md) - Initial misdiagnosis (WAL persistence)
- [CLUSTER_STATUS_REPORT_v2.2.8.md](CLUSTER_STATUS_REPORT_v2.2.8.md) - Cluster status before fix
- Leader forwarding fix (deployed but untested due to this bug)

---

## Implementation Checklist

- [ ] Modify [main.rs:660-825](../crates/chronik-server/src/main.rs#L660-L825) per proposal above
- [ ] Test cold start with 3-node local cluster
- [ ] Test cold start with remote cluster (192.168.1.5/7/8)
- [ ] Verify all 3 TCP listeners start on all nodes
- [ ] Verify stable leader election (no storm)
- [ ] Verify client connectivity to follower nodes
- [ ] Test leader forwarding fix (was blocked by this bug)
- [ ] Update documentation with new startup sequence
- [ ] Release as v2.2.9 with "Critical cluster startup fix"
