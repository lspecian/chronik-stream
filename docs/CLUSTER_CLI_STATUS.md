# Cluster CLI Status

**Date**: 2025-10-24
**Status**: Mock Implementation Only - NOT FUNCTIONAL

---

## Summary

The `chronik-server cluster` command is a **stub/mock implementation** that returns hardcoded data. It does NOT connect to a real cluster and is NOT functional for production use.

---

## What It Is

**Command**: `chronik-server cluster <subcommand>`

**Purpose**: Administrative CLI for managing running Raft clusters (similar to `kubectl` for Kubernetes)

**Available Subcommands**:
```bash
chronik-server cluster status              # Cluster status
chronik-server cluster list-nodes          # List all nodes
chronik-server cluster add-node            # Add node to cluster
chronik-server cluster remove-node         # Remove node from cluster
chronik-server cluster list-partitions     # List partitions
chronik-server cluster partition-info      # Partition details
chronik-server cluster rebalance           # Rebalance partitions
chronik-server cluster isr-status          # In-Sync Replicas status
chronik-server cluster metadata-status     # Metadata leader/status
chronik-server cluster health              # Health check
chronik-server cluster ping-node           # Ping specific node
```

---

## What It Actually Does

**ALL commands return MOCK/HARDCODED data**. Example:

```bash
$ chronik-server cluster status
╔═══════════════════════════════════════════════════════════════════════╗
║  ⚠️  WARNING: Cluster CLI Returns MOCK DATA Only                      ║
╠═══════════════════════════════════════════════════════════════════════╣
║  This cluster management CLI is a stub implementation that returns    ║
║  hardcoded mock data. It does NOT connect to a real cluster.         ║
║                                                                       ║
║  For actual cluster management, use Kafka protocol tools:            ║
║    • kafka-topics --bootstrap-server localhost:9092 --list           ║
║    • kafka-topics --describe --topic <name>                          ║
║    • kafka-consumer-groups --list                                    ║
║                                                                       ║
║  This feature is planned for implementation in a future release.    ║
╚═══════════════════════════════════════════════════════════════════════╝

Cluster Status
==============
Nodes: 3
Healthy: 3
Unhealthy: 0
Metadata Leader: Node 1

┌──────┬───────────────────┬───────────┬─────────┬────────────┐
│ Node │      Address      │ Raft Port │ Status  │ Partitions │
├──────┼───────────────────┼───────────┼─────────┼────────────┤
│ 1    │ 192.168.1.10:9092 │ 5001      │ Healthy │ 12         │  ← FAKE DATA
│ 2    │ 192.168.1.11:9092 │ 5001      │ Healthy │ 12         │  ← FAKE DATA
│ 3    │ 192.168.1.12:9092 │ 5001      │ Healthy │ 12         │  ← FAKE DATA
└──────┴───────────────────┴───────────┴─────────┴────────────┘
```

**All data is hardcoded** in [crates/chronik-server/src/cli/client.rs](../crates/chronik-server/src/cli/client.rs).

---

## Why It Exists

**Purpose**: Placeholder for future administrative API

**Implementation Notes** (from source code):
```rust
// From client.rs:24-41
pub async fn connect(addr: &str) -> Result<Self> {
    // For now, just store the address
    // TODO: Phase 5 - Implement actual gRPC connection and admin API
    Ok(Self {
        addr: addr.to_string(),
        timeout: Duration::from_secs(10),
    })
}

// From client.rs:33-64
pub async fn get_cluster_status(&self, detailed: bool) -> Result<ClusterStatus> {
    // Mock implementation - Phase 5 will implement actual gRPC calls
    Ok(ClusterStatus {
        total_nodes: 3,              // ← Hardcoded
        healthy_nodes: 3,            // ← Hardcoded
        nodes: vec![/* fake data */] // ← Hardcoded
    })
}
```

**Every method** returns mock data. No actual gRPC calls are made.

---

## Confusion with `raft-cluster` Mode

**IMPORTANT**: Do not confuse these two commands:

| Command | Purpose | Status |
|---------|---------|--------|
| `chronik-server raft-cluster` | **START** a Raft cluster node | ✅ **WORKING** |
| `chronik-server cluster` | **MANAGE** a running cluster (admin CLI) | ❌ **MOCK ONLY** |

**Analogy**:
- `raft-cluster` = `docker run` (starts a server)
- `cluster` = `docker ps` (queries running servers)

**Correct Usage**:
```bash
# ✅ Start a Raft cluster node (WORKS)
chronik-server raft-cluster --node-id 1 --raft-addr 0.0.0.0:9192 ...

# ❌ Query cluster status (RETURNS MOCK DATA)
chronik-server cluster status
```

---

## Alternatives for Cluster Management

Since the `cluster` CLI is not functional, use these alternatives:

### 1. Kafka Protocol Tools

**List Topics**:
```bash
kafka-topics --bootstrap-server localhost:9092 --list
```

**Describe Topic** (shows partition leaders):
```bash
kafka-topics --bootstrap-server localhost:9092 --describe --topic my-topic

Topic: my-topic  PartitionCount: 3  ReplicationFactor: 3
  Topic: my-topic  Partition: 0  Leader: 1  Replicas: 1,2,3  Isr: 1,2,3
  Topic: my-topic  Partition: 1  Leader: 2  Replicas: 2,3,1  Isr: 2,3,1
  Topic: my-topic  Partition: 2  Leader: 3  Replicas: 3,1,2  Isr: 3,1,2
```

**List Consumer Groups**:
```bash
kafka-consumer-groups --bootstrap-server localhost:9092 --list
```

**Describe Consumer Group**:
```bash
kafka-consumer-groups --bootstrap-server localhost:9092 --describe --group my-group
```

### 2. Logs

Check server logs for cluster events:
```bash
# Leader elections
grep "Became Raft leader" node*.log

# Cluster membership changes
grep "Raft gRPC server" node*.log

# Metadata sync
grep "Metadata replicated" node*.log
```

### 3. Metrics Endpoint

Query Prometheus metrics:
```bash
# Cluster metrics
curl http://localhost:9093/metrics | grep chronik_cluster

# Raft metrics
curl http://localhost:9093/metrics | grep chronik_raft
```

---

## When Will It Be Implemented?

**Priority**: P3 (Nice-to-have, not blocking)

**Planned For**: Post-v2.0.0 GA (future release)

**Requirements**:
1. Implement gRPC admin API on server side
2. Implement 11+ RPC methods for cluster operations
3. Add authentication/authorization
4. Comprehensive integration testing

**Estimated Effort**: 40+ hours

**Blocking Issues**: None - not critical for v2.0.0 GA release

---

## Current Workarounds

### Use Case: Check Cluster Health

**Instead of**:
```bash
chronik-server cluster health  # ❌ Returns mock data
```

**Use**:
```bash
# Check if all nodes are running
ps aux | grep chronik-server

# Check if Kafka is responding
kafka-topics --bootstrap-server localhost:9092 --list

# Check logs for errors
tail -f node*.log | grep -i error
```

### Use Case: List Partition Leaders

**Instead of**:
```bash
chronik-server cluster list-partitions  # ❌ Returns mock data
```

**Use**:
```bash
# Show partition leaders for all topics
kafka-topics --bootstrap-server localhost:9092 --describe
```

### Use Case: Rebalance Partitions

**Instead of**:
```bash
chronik-server cluster rebalance  # ❌ Returns mock data
```

**Use**:
```bash
# Manual rebalancing via topic recreation
kafka-topics --bootstrap-server localhost:9092 --delete --topic my-topic
kafka-topics --bootstrap-server localhost:9092 --create --topic my-topic --partitions 6 --replication-factor 3
```

---

## Should It Be Removed?

**Options**:

1. **Keep with Warning** (✅ Current approach)
   - Shows intent for future implementation
   - Warning prevents user confusion
   - Low maintenance burden

2. **Disable with Feature Flag**
   - Requires `--features cluster-admin-cli` to enable
   - More explicit about non-functional state
   - Slightly more complex

3. **Delete Entirely**
   - Cleanest approach
   - Removes ~1200 lines of unused code
   - Can re-implement from scratch when needed

**Recommendation**: Keep current approach (Option 1) with prominent warning. Re-evaluate after v2.0.0 GA.

---

## Summary

| Aspect | Status |
|--------|--------|
| **Functionality** | ❌ Mock data only |
| **Use in Production** | ❌ NOT SUITABLE |
| **Use in Development** | ⚠️ Can view mock output for UI testing only |
| **Alternative Tools** | ✅ Use `kafka-topics`, logs, metrics |
| **Implementation Priority** | P3 (Post-v2.0.0) |
| **Warning Displayed** | ✅ Yes (as of 2025-10-24) |

**Bottom Line**: Do not rely on `chronik-server cluster` commands for any real cluster management. Use Kafka protocol tools (`kafka-topics`, etc.) instead.

---

## Files

**Implementation**:
- [crates/chronik-server/src/cli/cluster.rs](../crates/chronik-server/src/cli/cluster.rs) - Command definitions (874 lines)
- [crates/chronik-server/src/cli/client.rs](../crates/chronik-server/src/cli/client.rs) - Mock client (393 lines)
- [crates/chronik-server/src/cli/output.rs](../crates/chronik-server/src/cli/output.rs) - Output formatting

**Documentation**:
- This file: [docs/CLUSTER_CLI_STATUS.md](./CLUSTER_CLI_STATUS.md)

**Related**:
- [docs/COMPREHENSIVE_RAFT_FIX_PLAN.md](./COMPREHENSIVE_RAFT_FIX_PLAN.md) - Raft stability fixes (unrelated to CLI)
- [crates/chronik-server/src/raft_cluster.rs](../crates/chronik-server/src/raft_cluster.rs) - Actual Raft mode implementation

---

**Last Updated**: 2025-10-24
