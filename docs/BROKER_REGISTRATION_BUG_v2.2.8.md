# Broker Registration Bug (v2.2.8)

**Date:** 2025-11-09
**Severity:** CRITICAL
**Status:** 🐛 BLOCKING ALL CLUSTER OPERATIONS

---

## Summary

The cluster cannot handle **ANY** Kafka clients because **broker metadata is never registered**. Followers skip registration (waiting for Raft replication), but the leader also doesn't register brokers, causing a deadlock.

---

## Evidence

```bash
# Node 1 (Leader) - DEFERS but never registers
tests/cluster/logs/node1.log:
  Multi-node mode: Deferring broker registration until Raft leader election completes
  # NO SUBSEQUENT REGISTRATION!

# Node 2 (Follower) - Skips registration
tests/cluster/logs/node2.log:
  Waiting for Raft leader election before broker registration...
  This node is a follower - skipping broker registration (will receive via Raft replication)

# Node 3 (Follower) - Skips registration
tests/cluster/logs/node3.log:
  Waiting for Raft leader election before broker registration...
  This node is a follower - skipping broker registration (will receive via Raft replication)
```

Result: **NO brokers registered, cluster unusable**

---

## Root Cause

After the PARTITION_LEADERSHIP_FIX (v2.2.7), the broker registration logic has a gap:

1. **Followers**: Skip registration, wait for Raft replication ✅ (correct)
2. **Leader**: Defers registration until leader election... but NEVER actually registers! ❌

The leader election completes, but there's no code to trigger broker registration after election.

---

## Impact

- ❌ Kafka clients timeout: "Failed to update metadata after 60.0 secs"
- ❌ Topic auto-create fails (no brokers to assign partitions to)
- ❌ Metadata API returns empty broker list
- ❌ Cluster completely unusable

---

## Root Cause Update (v2.2.8 Deep Dive)

**TWO SEPARATE DEADLOCKS DISCOVERED:**

### Deadlock 1: IntegratedKafkaServer::new() waits for Raft leader election (FIXED v2.2.8)
- integrated_server.rs lines 390-416 (now REMOVED)
- Waited for Raft leader election BEFORE Raft message loop started
- **Chicken-and-egg**: new() never returns → message loop never starts → election never happens
- **FIX**: Removed blocking wait from new(), moved partition metadata init to main.rs

### Deadlock 2: register_broker() waits for Raft state machine (STILL BLOCKING)
- raft_metadata_store.rs:174: `raft.propose(RegisterBroker).await`
- Waits for Raft entry to be committed AND applied to state machine
- State machine application NEVER COMPLETES - entries persist to WAL but don't apply
- **Symptom**: Background task starts ("Registering 3 brokers") but NEVER logs success
- **Impact**: NO brokers registered → clients timeout → cluster unusable

**Why spawning background task didn't help:**
- Background task still calls `register_broker().await`
- Still waits for state machine application
- State machine application is fundamentally broken - NOT A CONCURRENCY ISSUE

## The Fix (Partial - v2.2.8)

**Fix 1**: REMOVED blocking Raft wait from IntegratedKafkaServer::new() ✅

**Fix 2 NEEDED** (BEYOND SCOPE OF v2.2.8 - deferred to v2.2.9):

**Location:** [crates/chronik-server/src/main.rs](../crates/chronik-server/src/main.rs) or [crates/chronik-server/src/integrated_server.rs](../crates/chronik-server/src/integrated_server.rs)

**After leader election:**
```rust
if raft.am_i_leader() {
    // Leader registers ALL brokers in the cluster
    for node_id in cluster_nodes {
        let broker_metadata = BrokerMetadata {
            broker_id: node_id as i32,
            host: node_addr.host,
            port: node_addr.kafka_port,
            rack: None,
        };
        metadata_store.register_broker(broker_metadata).await?;
    }
    info!("✓ Raft leader registered all {} brokers", cluster_nodes.len());
}
```

---

## Blocked Work

The v2.2.8 auto-create topic fix CANNOT be tested until broker registration works.

**Dependency chain:**
1. ✅ Fix broker registration (this bug)
2. ✅ Test auto-create on follower (v2.2.8 fix)
3. ✅ Run chronik-bench (throughput verification)

---

## Priority

**CRITICAL** - Must be fixed before ANY cluster testing can proceed.

