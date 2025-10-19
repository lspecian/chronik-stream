# Chronik Raft Clustering Documentation

Welcome to the Chronik Raft clustering documentation. This directory contains comprehensive guides for deploying, configuring, and operating Chronik in clustered mode.

## Table of Contents

- [Quick Start](#quick-start)
- [Documentation Structure](#documentation-structure)
- [Learning Path](#learning-path)
- [Key Concepts](#key-concepts)
- [When to Use Clustering](#when-to-use-clustering)

## Quick Start

**Prerequisites**:
- Chronik v2.0 or later
- 3 or 5 nodes (odd number for quorum)
- Network connectivity between nodes (port 9093)
- Basic understanding of Raft consensus

**Bootstrap 3-node cluster**:

```bash
# Node 1 (bootstrap)
cat > /etc/chronik/chronik.toml <<EOF
[cluster]
enabled = true
node_id = 1
node_addr = "0.0.0.0:9093"
advertised_node_addr = "node1:9093"
seed_nodes = []
EOF

chronik-server cluster

# Node 2
cat > /etc/chronik/chronik.toml <<EOF
[cluster]
enabled = true
node_id = 2
node_addr = "0.0.0.0:9093"
advertised_node_addr = "node2:9093"
seed_nodes = ["1@node1:9093"]
EOF

chronik-server cluster

# Node 3
cat > /etc/chronik/chronik.toml <<EOF
[cluster]
enabled = true
node_id = 3
node_addr = "0.0.0.0:9093"
advertised_node_addr = "node3:9093"
seed_nodes = ["1@node1:9093", "2@node2:9093"]
EOF

chronik-server cluster

# Verify cluster
curl http://node1:8080/admin/cluster/members
```

**Next Steps**:
- Read [ARCHITECTURE.md](./ARCHITECTURE.md) to understand the design
- Review [CONFIGURATION.md](./CONFIGURATION.md) for production tuning
- Bookmark [TROUBLESHOOTING.md](./TROUBLESHOOTING.md) for operational issues

## Documentation Structure

### 1. [ARCHITECTURE.md](./ARCHITECTURE.md)

**Audience**: Developers, architects, advanced operators

**Topics**:
- Multi-Raft design decision (why partition-level Raft groups)
- WAL as RaftLogStorage rationale
- Architecture diagrams (request flow, leader election, etc.)
- Component interactions (RaftNode, RaftCluster, RaftNetwork)
- State machine design (PartitionStateMachine, MetadataStateMachine)

**When to read**:
- Before implementing changes to clustering code
- When designing capacity planning
- To understand failure modes and recovery

### 2. [CONFIGURATION.md](./CONFIGURATION.md)

**Audience**: DevOps, SREs, system administrators

**Topics**:
- Complete `chronik.toml` reference
- Environment variable mapping
- Configuration profiles (development, production, high-throughput, multi-DC)
- Common scenarios (bootstrap, add node, upgrade, etc.)
- Security configuration (TLS, mTLS)

**When to read**:
- Before deploying a cluster
- When tuning for performance
- When troubleshooting configuration issues

### 3. [MIGRATION_v1_v2.md](./MIGRATION_v1_v2.md)

**Audience**: Operators upgrading from Chronik v1.x

**Topics**:
- Breaking changes (WAL format, configuration, ports)
- Pre-migration checklist
- Migration paths (in-place, blue-green, greenfield)
- Step-by-step upgrade procedures
- WAL format migration (V2 → V3)
- Rollback strategy
- Post-migration validation

**When to read**:
- Planning v1.x → v2.0 upgrade
- Experiencing migration issues
- Preparing rollback plan

### 4. [TROUBLESHOOTING.md](./TROUBLESHOOTING.md)

**Audience**: Operators, on-call engineers

**Topics**:
- Quick diagnostic checklist
- Common issues with solutions (split-brain, replication lag, etc.)
- Diagnostic commands
- Metric interpretation (Prometheus queries)
- Log analysis patterns
- Emergency procedures (leader transfer, cluster rebuild)

**When to read**:
- When cluster is experiencing issues
- During incident response
- For runbook creation

## Learning Path

### For New Users

**Goal**: Understand basics and deploy first cluster

1. Read **Key Concepts** (below)
2. Skim [ARCHITECTURE.md](./ARCHITECTURE.md) (focus on diagrams)
3. Follow **Quick Start** (above)
4. Review [CONFIGURATION.md](./CONFIGURATION.md) → Development Profile
5. Bookmark [TROUBLESHOOTING.md](./TROUBLESHOOTING.md) → Quick Diagnostic Checklist

**Time**: 2-3 hours

### For Operators

**Goal**: Deploy and maintain production cluster

1. Read [ARCHITECTURE.md](./ARCHITECTURE.md) → Architecture Diagrams
2. Study [CONFIGURATION.md](./CONFIGURATION.md) → Production Profile
3. Review [TROUBLESHOOTING.md](./TROUBLESHOOTING.md) → Common Issues
4. Practice emergency procedures (leader transfer, node failure)

**Time**: 1 day

### For Developers

**Goal**: Understand internals and contribute

1. Deep dive [ARCHITECTURE.md](./ARCHITECTURE.md) (all sections)
2. Review OpenRaft documentation: https://docs.rs/openraft/
3. Study Raft paper: https://raft.github.io/raft.pdf
4. Review codebase:
   - `crates/chronik-cluster/src/raft_node.rs`
   - `crates/chronik-cluster/src/raft_storage.rs`
   - `crates/chronik-cluster/src/state_machine.rs`
5. Run integration tests: `cargo test --test raft_integration`

**Time**: 2-3 days

### For v1.x Upgraders

**Goal**: Safely migrate from v1.x to v2.0

1. Read [MIGRATION_v1_v2.md](./MIGRATION_v1_v2.md) → Breaking Changes
2. Complete [MIGRATION_v1_v2.md](./MIGRATION_v1_v2.md) → Pre-Migration Checklist
3. Choose migration path (in-place vs blue-green)
4. Test migration in staging environment
5. Execute production migration
6. Validate with [MIGRATION_v1_v2.md](./MIGRATION_v1_v2.md) → Post-Migration Validation

**Time**: 1-2 days (including staging testing)

## Key Concepts

### Raft Consensus

Chronik uses the **Raft consensus algorithm** to ensure data consistency across multiple nodes.

**Core principles**:
- **Leader election**: One node is elected leader, handles all writes
- **Log replication**: Leader replicates log entries to followers
- **Quorum**: Majority (N/2 + 1) must agree for commit
- **Safety**: Committed entries never lost, even with failures

**Example**: 3-node cluster
- Quorum = 2 nodes
- Can tolerate 1 node failure
- If leader fails, new leader elected in < 1 second

### Multi-Raft Design

Chronik uses **one Raft group per partition** (Multi-Raft).

**Why**:
- **Scalability**: 100 partitions = 100 independent Raft groups
- **Isolation**: Partition 0 failure doesn't affect partition 1
- **Throughput**: Parallel replication across partitions

**Example**:
```
Topic: "orders" (3 partitions, RF=3)
  ├─ Partition 0 → RaftGroup[orders-0] (Leader: Node1)
  ├─ Partition 1 → RaftGroup[orders-1] (Leader: Node2)
  └─ Partition 2 → RaftGroup[orders-2] (Leader: Node3)

Result: 3 Raft groups, load balanced across nodes
```

### WAL as Raft Log

Chronik reuses the existing **GroupCommitWal** as Raft log storage.

**Benefits**:
- Single fsync path (no duplication)
- Unified recovery mechanism
- Consistent monitoring

**Trade-off**: WAL records now include Raft metadata (term, index, leader_id)

### Partition Leadership

Each partition has an **independent leader** (elected via Raft).

**Produce flow**:
1. Client sends ProduceRequest to any node
2. Node routes to partition leader (via RaftCluster)
3. Leader appends to Raft log
4. Followers replicate log entry
5. Leader commits after quorum ACK
6. Leader responds to client

**Fetch flow**:
1. Client sends FetchRequest to any node
2. Node routes to partition leader or follower
3. Leader serves from state machine (linearizable read)
4. Follower may serve stale read (lower latency)

### Replication Factor

**Replication factor (RF)** determines how many nodes have a copy of each partition.

**Common values**:
- RF=1: No replication (standalone mode, no fault tolerance)
- RF=3: 3 copies, tolerates 1 node failure (recommended for production)
- RF=5: 5 copies, tolerates 2 node failures (high availability)

**Quorum** = RF/2 + 1
- RF=3 → Quorum=2 (can lose 1 node)
- RF=5 → Quorum=3 (can lose 2 nodes)

### Cluster Membership

**Static discovery** (current): Nodes configured with seed_nodes list

**Dynamic discovery** (future): Nodes discover peers via DNS, Consul, etcd

**Adding a node**:
1. Configure new node with seed_nodes
2. New node contacts seed node
3. Seed node adds learner (non-voting member)
4. Learner catches up via log replication or snapshot
5. Learner promoted to voter (voting member)

**Removing a node**:
1. Leader calls `change_membership` (removes node from voter set)
2. Raft replicates membership change
3. Removed node shuts down gracefully

## When to Use Clustering

### Use Clustering If

- **High availability required**: Cannot tolerate downtime
- **Data durability critical**: Must survive node failures
- **Horizontal scaling needed**: Single node cannot handle load
- **Multi-datacenter deployment**: Need geo-replication

### Use Standalone If

- **Development/testing**: Simple setup, no HA needed
- **Small workloads**: Single node sufficient
- **Cost-sensitive**: Clustering requires 3+ nodes
- **Batch processing**: Downtime acceptable

### Comparison Table

| Aspect | Standalone (v1.x) | Clustered (v2.0) |
|--------|-------------------|------------------|
| **Availability** | Single point of failure | Tolerates N/2 failures |
| **Durability** | Local WAL only | Replicated to majority |
| **Latency** | Low (no consensus) | +1-2ms (Raft quorum) |
| **Throughput** | Moderate | High (parallel partitions) |
| **Complexity** | Simple | Moderate |
| **Cost** | 1 node | 3+ nodes |

## Related Documentation

- **Main README**: [../../README.md](../../README.md)
- **CLAUDE.md**: [../../CLAUDE.md](../../CLAUDE.md) (development guide)
- **Implementation Plan**: [../../.conductor/lahore/implementation-plan.md](../../.conductor/lahore/implementation-plan.md)
- **Disaster Recovery**: [../DISASTER_RECOVERY.md](../DISASTER_RECOVERY.md)

## External Resources

- **OpenRaft**: https://docs.rs/openraft/
- **Raft Paper**: https://raft.github.io/raft.pdf
- **Raft Visualization**: http://thesecretlivesofdata.com/raft/
- **Kafka Protocol**: https://kafka.apache.org/protocol.html

## Contributing

Found an issue or have a suggestion?

- **Documentation**: Open PR with changes
- **Code**: See [../../CONTRIBUTING.md](../../CONTRIBUTING.md)
- **Questions**: Ask in #chronik-support Slack channel

## Version History

- **v2.0.0** (2024-Q4): Initial Raft clustering release
  - Multi-Raft design
  - WAL V3 format
  - Static cluster discovery
  - gRPC network layer

- **v2.1.0** (planned): Dynamic discovery
  - DNS-based discovery
  - Consul integration
  - Auto-scaling support

- **v2.2.0** (planned): Advanced features
  - Cross-datacenter replication
  - Partition rebalancing
  - Read replicas
