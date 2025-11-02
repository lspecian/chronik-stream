# Cluster Configuration Improvement Plan (v2.5.0 - Pre-Release)

**Current Version**: v2.5.0 (unreleased - in development on feat/v2.5.0-kafka-cluster branch)

**Problem**: Current cluster configuration is manual, error-prone, and doesn't integrate with Raft leader election. User reported confusion and repeated configuration mistakes during testing.

**Goals**:
1. Single, simple cluster config that automatically handles leader election and WAL replication discovery
2. Clean, intuitive CLI that's hard to use incorrectly
3. Zero-downtime node addition and removal
4. Disaster recovery without manual intervention

---

## Current Pain Points

### A. Configuration Issues

### 1. Manual Leader/Follower Configuration
```bash
# Current (MANUAL):
# Leader node:
CHRONIK_REPLICATION_FOLLOWERS=localhost:9291,localhost:9292 \
  ./chronik-server --node-id 1 --kafka-port 9092 standalone

# Follower nodes:
CHRONIK_WAL_RECEIVER_ADDR="0.0.0.0:9291" \
  ./chronik-server --node-id 2 --kafka-port 9093 standalone

CHRONIK_WAL_RECEIVER_ADDR="0.0.0.0:9292" \
  ./chronik-server --node-id 3 --kafka-port 9094 standalone
```

**Issues**:
- Must manually specify follower addresses on leader
- Must manually specify receiver address on each follower
- Leader failover requires manual reconfiguration
- No integration with Raft leader election

### 2. Confusing Environment Variables
- `CHRONIK_WAL_RECEIVER_PORT` vs `CHRONIK_WAL_RECEIVER_ADDR` (both exist!)
- `CHRONIK_REPLICATION_FOLLOWERS` vs `CHRONIK_CLUSTER_PEERS` (two systems)
- Easy to mix up, no validation

### 3. Port Auto-Derivation is Complex
```rust
// main.rs:601-606 - Hard to understand
if cli.metrics_port == 9093 && is_cluster_mode {
    cli.metrics_port = cli.kafka_port + 4000;  // Why 4000?
}
if cli.search_port == 6080 && is_cluster_mode {
    cli.search_port = if cli.kafka_port >= 3000 {
        cli.kafka_port - 3000  // Why -3000?
    } else {
        cli.kafka_port + 3000  // Fallback logic confusing
    };
}
```

### 4. No Dynamic Leader Change Support
- Raft elects new leader → WAL replication still sends to old leader
- Requires manual restart with new `CHRONIK_REPLICATION_FOLLOWERS`

### B. CLI Issues

#### 1. Too Many Global Flags (16 flags!)
```bash
chronik-server [16 FLAGS] [SUBCOMMAND]
  --kafka-port, --admin-port, --metrics-port, --data-dir, --log-level,
  --bind-addr, --advertised-addr, --advertised-port, --enable-backup,
  --cluster-config, --node-id, --search-port, --disable-search,
  --enable-dynamic-config, ...
```
**Problem**: Overwhelming, unclear which flags are required for which mode

#### 2. Confusing Subcommands
- `standalone` - What does this mean? Single node? Manual replication?
- `raft-cluster` - Sounds like Raft does data replication (it doesn't!)
- `ingest` / `search` / `all` - Future features, confusing for current users

#### 3. Inconsistent Flag Names
```bash
--kafka-port 9092           # Port only
--advertised-addr localhost # Address only
--raft-addr 0.0.0.0:5001   # Full address with port
--peers 2@node2:5001       # Special format with @ separator
```
**Problem**: Different formats for same concept (address)

#### 4. Port Confusion
```bash
--kafka-port 9092        # Kafka API
--admin-port 3000        # Admin API (unused?)
--metrics-port 9093      # Metrics
--search-port 6080       # Search API
--raft-addr 0.0.0.0:5001 # Raft port (embedded in address?)
# Plus: CHRONIK_WAL_RECEIVER_ADDR=0.0.0.0:9291 (env var only!)
```
**Problem**: 6 different ports to configure, easy conflicts, no clear defaults

#### 5. No Validation or Clear Errors
```bash
# This silently fails (WAL receiver never starts):
CHRONIK_WAL_RECEIVER_PORT=9291 ./chronik-server standalone
# Should be: CHRONIK_WAL_RECEIVER_ADDR="0.0.0.0:9291"

# This doesn't error (but does nothing):
./chronik-server --node-id 2 standalone
# node-id is ignored unless cluster-config is provided
```

### C. Node Management Issues

#### 1. No Zero-Downtime Node Addition
```bash
# Current: Must restart entire cluster to add a node
1. Stop all nodes
2. Update cluster config on all nodes
3. Restart all nodes with new config
4. Pray nothing breaks
```
**Problem**: Violates basic cloud-native principles

#### 2. No Zero-Downtime Node Removal
```bash
# Current: Must manually decommission
1. Stop node to remove
2. Manually reassign partitions (how?)
3. Update cluster config on all remaining nodes
4. Restart all remaining nodes
```
**Problem**: Can't handle node failures gracefully

#### 3. No Cluster Membership Commands
```bash
# Want to do this:
chronik-server cluster add-node 4 node4:9092:9291:5001
chronik-server cluster remove-node 3
chronik-server cluster list-nodes

# But these don't exist!
```

---

## Proposed Solution: Complete Cluster Redesign

### Design Principles

1. **Single Source of Truth**: One config defines entire cluster
2. **Automatic Discovery**: Nodes discover each other via Raft cluster membership
3. **Dynamic Leader Tracking**: WAL replication follows Raft leader election
4. **Simple CLI**: Minimal required args, smart defaults, clear subcommands
5. **Disaster Recovery**: Automatic reconnection and leader failover
6. **Zero-Downtime Ops**: Add/remove nodes without cluster restart

---

## Part 1: CLI Redesign (Clean, Intuitive, Hard to Misuse)

### New CLI Structure

**Simplified top-level commands:**
```bash
chronik-server <COMMAND>

Commands:
  start           Start a Chronik server (single-node or cluster mode)
  cluster         Manage cluster membership (add/remove nodes, list status)
  compact         Manage WAL compaction
  version         Show version information
  help            Show help

# Remove confusing subcommands:
#   standalone  → replaced by "start" (auto-detects mode)
#   raft-cluster → replaced by "start --cluster"
#   ingest/search/all → remove (not implemented, confusing)
```

### `start` Command (Replaces: standalone, raft-cluster)

**Design philosophy**: Auto-detect mode from config, minimal flags

```bash
# Single-node mode (default):
chronik-server start

# Cluster mode (from config file):
chronik-server start --config cluster.toml

# Cluster mode (from env vars):
CHRONIK_NODE_ID=1 CHRONIK_CLUSTER_PEERS="..." chronik-server start

chronik-server start [OPTIONS]

Options:
  -c, --config <FILE>       Cluster config file (TOML) [env: CHRONIK_CONFIG=]
  -d, --data-dir <DIR>      Data directory [default: ./data] [env: CHRONIK_DATA_DIR=]
      --log-level <LEVEL>   Log level: error|warn|info|debug|trace [default: info] [env: RUST_LOG=]
      --bind <ADDR>         Bind address [default: 0.0.0.0] [env: CHRONIK_BIND=]
      --advertise <ADDR>    Advertised address for clients [env: CHRONIK_ADVERTISE=]
  -h, --help                Print help

# REMOVED CONFUSING FLAGS:
#   --kafka-port, --admin-port, --metrics-port, --search-port (now in config file)
#   --raft-addr, --peers, --bootstrap (now in config file)
#   --cluster-config, --node-id (renamed to --config, derived from config)
#   --bind-addr (renamed to --bind for consistency)
#   --advertised-addr, --advertised-port (merged into --advertise)
```

**How it works:**
1. If `--config` provided or `CHRONIK_CONFIG` set → **cluster mode**
2. If `CHRONIK_CLUSTER_PEERS` env var set → **cluster mode** (env-based config)
3. Otherwise → **single-node mode**

**Port configuration:** ALL ports defined in config file, no CLI flags!

### Cluster Config File (cluster.toml)

```toml
# Node identity
node_id = 1

# Replication settings
replication_factor = 3
min_insync_replicas = 2

# This node's addresses (CLEAR, CONSISTENT FORMAT)
[node.addresses]
kafka = "0.0.0.0:9092"          # Kafka API (bind address)
wal = "0.0.0.0:9291"            # WAL receiver (bind address)
raft = "0.0.0.0:5001"           # Raft gRPC (bind address)
metrics = "0.0.0.0:13092"       # Metrics endpoint (optional)
search = "0.0.0.0:6092"         # Search API (optional, if compiled with --features search)

[node.advertise]
kafka = "node1.example.com:9092"   # What clients connect to
wal = "node1.example.com:9291"     # What followers connect to
raft = "node1.example.com:5001"    # What Raft peers connect to

# Cluster peers (ALL nodes including this one)
[[peers]]
id = 1
kafka = "node1.example.com:9092"
wal = "node1.example.com:9291"
raft = "node1.example.com:5001"

[[peers]]
id = 2
kafka = "node2.example.com:9092"
wal = "node2.example.com:9291"
raft = "node2.example.com:5001"

[[peers]]
id = 3
kafka = "node3.example.com:9092"
wal = "node3.example.com:9291"
raft = "node3.example.com:5001"
```

**Benefits:**
- ✅ Clear, consistent address format
- ✅ Separate bind vs advertise (for NAT/Docker)
- ✅ All ports in one place, easy to see conflicts
- ✅ No magic port offsets (+4000, -3000, etc.)

### `cluster` Command (NEW - Zero-Downtime Operations)

```bash
chronik-server cluster <SUBCOMMAND>

Subcommands:
  status          Show cluster status (nodes, leaders, ISR)
  add-node        Add a new node to the cluster (zero-downtime)
  remove-node     Remove a node from the cluster (zero-downtime)
  rebalance       Manually trigger partition rebalancing
  help            Show help

Examples:
  # Show cluster status
  chronik-server cluster status --config cluster.toml

  # Add a new node (updates Raft membership + triggers rebalancing)
  chronik-server cluster add-node 4 \
    --kafka node4:9092 \
    --wal node4:9291 \
    --raft node4:5001 \
    --config cluster.toml

  # Remove a node gracefully (reassigns partitions first)
  chronik-server cluster remove-node 3 --config cluster.toml

  # Force remove (node is dead)
  chronik-server cluster remove-node 3 --force --config cluster.toml
```

**How `add-node` works (zero-downtime):**
1. Connect to existing cluster via config
2. Submit Raft proposal: `AddNode{id: 4, kafka: ..., wal: ..., raft: ...}`
3. Raft commits the change → all nodes learn about new peer
4. New node starts, joins Raft cluster
5. Partition rebalancer detects new capacity → reassigns some partitions
6. Followers start replicating to new node
7. Done - no restarts required!

**How `remove-node` works (zero-downtime):**
1. Check if node is alive (if `--force`, skip to step 3)
2. Reassign all partitions owned by node to other nodes
3. Wait for data to replicate to new owners
4. Submit Raft proposal: `RemoveNode{id: 3}`
5. Raft commits → node removed from cluster
6. Node can be safely shut down
7. Done!

### Environment Variable Support (For Docker/K8s)

**Simple, consistent naming:**
```bash
# Required for cluster mode:
CHRONIK_NODE_ID=1
CHRONIK_CLUSTER_PEERS="node1:9092:9291:5001,node2:9092:9291:5001,node3:9092:9291:5001"

# Optional overrides:
CHRONIK_DATA_DIR=/data
CHRONIK_BIND=0.0.0.0
CHRONIK_ADVERTISE=node1.prod.internal
CHRONIK_LOG_LEVEL=debug

# REMOVED (now in cluster config):
#   CHRONIK_KAFKA_PORT, CHRONIK_METRICS_PORT, CHRONIK_SEARCH_PORT
#   CHRONIK_REPLICATION_FOLLOWERS, CHRONIK_WAL_RECEIVER_ADDR
#   CHRONIK_RAFT_ADDR, CHRONIK_RAFT_PEERS
```

### Validation & Clear Errors

```rust
// Validate config on startup
fn validate_config(config: &ClusterConfig) -> Result<()> {
    // Check node_id is in peers list
    if !config.peers.iter().any(|p| p.id == config.node_id) {
        return Err(anyhow!(
            "ERROR: node_id {} not found in peers list\n\
             \n\
             Your config has node_id={}, but the peers list only contains: {:?}\n\
             \n\
             Fix: Add a [[peers]] entry with id={} to your config file",
            config.node_id,
            config.node_id,
            config.peers.iter().map(|p| p.id).collect::<Vec<_>>(),
            config.node_id
        ));
    }

    // Check for port conflicts
    let mut ports = HashMap::new();
    for (service, port) in [
        ("Kafka", config.node.addresses.kafka),
        ("WAL", config.node.addresses.wal),
        ("Raft", config.node.addresses.raft),
        ("Metrics", config.node.addresses.metrics),
    ] {
        if let Some(conflicting) = ports.insert(port, service) {
            return Err(anyhow!(
                "ERROR: Port conflict on {}\n\
                 \n\
                 Both {} and {} are trying to bind to port {}\n\
                 \n\
                 Fix: Change one of these ports in your config file",
                port, conflicting, service, port
            ));
        }
    }

    // Check for duplicate node IDs
    let mut seen = HashSet::new();
    for peer in &config.peers {
        if !seen.insert(peer.id) {
            return Err(anyhow!(
                "ERROR: Duplicate node ID: {}\n\
                 \n\
                 Each node must have a unique ID\n\
                 \n\
                 Fix: Change duplicate IDs in [[peers]] section",
                peer.id
            ));
        }
    }

    // More validations...
    Ok(())
}
```

### Migration from Old CLI (Backward Compatibility)

**Keep old subcommands with deprecation warnings:**
```bash
# Old command still works (with warning):
$ chronik-server standalone
⚠️  WARNING: 'standalone' subcommand is deprecated
⚠️  Use 'chronik-server start' instead
⚠️  See: https://docs.chronik.dev/migration-guide

# Old env vars still work (with warning):
$ CHRONIK_REPLICATION_FOLLOWERS=localhost:9291 chronik-server start
⚠️  WARNING: CHRONIK_REPLICATION_FOLLOWERS is deprecated
⚠️  Use cluster config file instead: chronik-server start --config cluster.toml
⚠️  See: https://docs.chronik.dev/cluster-setup
```

---

## Part 2: Zero-Downtime Node Operations

### Architecture: Dynamic Cluster Membership via Raft

**Key insight**: Raft already supports membership changes (`ConfChange` messages). We just need to:
1. Expose it via CLI
2. Trigger partition rebalancing automatically
3. Handle data migration gracefully

### Add Node Flow

```
┌──────────────────────────────────────────────────────────────┐
│         Zero-Downtime Node Addition (v2.5.0 Final)           │
├──────────────────────────────────────────────────────────────┤
│                                                              │
│  Step 1: Admin runs command                                 │
│    $ chronik-server cluster add-node 4 \                    │
│        --kafka node4:9092 \                                 │
│        --wal node4:9291 \                                   │
│        --raft node4:5001 \                                  │
│        --config cluster.toml                                │
│                                                              │
│  Step 2: CLI connects to any existing node                  │
│    → Sends Raft proposal: AddNode{id: 4, ...}              │
│                                                              │
│  Step 3: Raft leader commits proposal                       │
│    → ConfChangeType::AddNode committed                      │
│    → All nodes learn about new peer (via Raft log)         │
│                                                              │
│  Step 4: New node starts                                    │
│    $ chronik-server start --config cluster.toml  (node_id=4)│
│    → Joins Raft cluster (already in voter set)             │
│    → Starts WAL receiver on node4:9291                      │
│    → Waits for partition assignments                        │
│                                                              │
│  Step 5: Partition rebalancer detects new capacity          │
│    → Queries Raft: "Who are all the nodes?"                │
│    → Sees: [node1, node2, node3, node4] (4 nodes now!)     │
│    → Recalculates partition assignments (round-robin)       │
│    → Proposes new assignments via Raft                      │
│                                                              │
│  Step 6: Leaders start replicating to node4                 │
│    → topic-orders-partition-0: leader=node1, replicas=[2,4] │
│    → Node1's WalReplicationManager discovers node4          │
│    → Connects to node4:9291 and starts streaming            │
│                                                              │
│  Step 7: Node4 catches up                                   │
│    → WAL receiver writes to local WAL                       │
│    → Catches up to high watermark                           │
│    → Joins ISR (in-sync replica set)                        │
│                                                              │
│  Step 8: Done! (zero downtime)                              │
│    → Cluster now has 4 nodes                                │
│    → Partitions rebalanced across all 4 nodes              │
│    → No client interruptions                                │
│                                                              │
└──────────────────────────────────────────────────────────────┘
```

### Remove Node Flow (Graceful)

```
┌──────────────────────────────────────────────────────────────┐
│      Zero-Downtime Node Removal (v2.5.0 Final)               │
├──────────────────────────────────────────────────────────────┤
│                                                              │
│  Step 1: Admin runs command                                 │
│    $ chronik-server cluster remove-node 3 --config cluster.toml│
│                                                              │
│  Step 2: CLI connects to cluster                            │
│    → Queries Raft: "What partitions does node3 own?"       │
│    → Finds: topic-orders-2, topic-payments-1, ...          │
│                                                              │
│  Step 3: Reassign partitions                                │
│    → For each partition owned by node3:                     │
│      - Calculate new replica set (exclude node3)            │
│      - Propose assignment change via Raft                   │
│      - Wait for new replicas to catch up                    │
│                                                              │
│  Step 4: Wait for data migration                            │
│    → Monitor ISR for each partition                         │
│    → Wait until new replicas are in-sync                    │
│    → Typical time: 30s-5min (depends on data size)         │
│                                                              │
│  Step 5: Remove from Raft cluster                           │
│    → Propose: RemoveNode{id: 3}                             │
│    → Raft commits → node3 removed from voter set           │
│                                                              │
│  Step 6: Node3 can be safely shut down                      │
│    → No longer owns any partitions                          │
│    → No longer in Raft voter set                            │
│    → Graceful shutdown                                      │
│                                                              │
│  Step 7: Done! (zero downtime)                              │
│    → Cluster now has 2 nodes                                │
│    → All partitions still available                         │
│    → No client interruptions                                │
│                                                              │
└──────────────────────────────────────────────────────────────┘
```

### Remove Node Flow (Force - Node Dead)

```bash
# Node3 crashed and won't come back
$ chronik-server cluster remove-node 3 --force --config cluster.toml

# What happens:
1. Immediately remove node3 from Raft cluster
2. Partitions owned by node3 are under-replicated
3. Trigger automatic re-replication to other nodes
4. Wait for catch-up (may take longer - no existing replica to copy from)
5. Done - cluster recovered from node loss
```

### Implementation: Raft Membership Changes

**Add to `raft_cluster.rs`:**
```rust
impl RaftCluster {
    /// Propose adding a new node to the cluster
    pub async fn propose_add_node(&self, node: NodeConfig) -> Result<()> {
        let mut conf_change = ConfChange::default();
        conf_change.set_change_type(ConfChangeType::AddNode);
        conf_change.node_id = node.id;

        // Encode node config as context (so all nodes know addresses)
        let node_json = serde_json::to_vec(&node)?;
        conf_change.set_context(node_json.into());

        self.propose_conf_change(conf_change).await?;
        info!("Proposed adding node {} to cluster", node.id);
        Ok(())
    }

    /// Propose removing a node from the cluster
    pub async fn propose_remove_node(&self, node_id: u64) -> Result<()> {
        let mut conf_change = ConfChange::default();
        conf_change.set_change_type(ConfChangeType::RemoveNode);
        conf_change.node_id = node_id;

        self.propose_conf_change(conf_change).await?;
        info!("Proposed removing node {} from cluster", node_id);
        Ok(())
    }

    /// Low-level: Propose a configuration change
    async fn propose_conf_change(&self, cc: ConfChange) -> Result<()> {
        let mut raft_node = self.raft_node.write().unwrap();
        raft_node.propose_conf_change(vec![], cc)?;

        // Must call ready() to actually send the proposal
        if raft_node.has_ready() {
            let ready = raft_node.ready();
            // Handle ready (persist, send messages, apply entries)
            // ... (existing ready handling logic)
        }

        Ok(())
    }
}
```

### Implementation: Partition Rebalancer (NEW)

**Add `crates/chronik-server/src/partition_rebalancer.rs`:**
```rust
//! Automatic partition rebalancing on cluster membership changes
//!
//! Watches Raft for AddNode/RemoveNode events and triggers rebalancing.

pub struct PartitionRebalancer {
    raft_cluster: Arc<RaftCluster>,
    metadata_store: Arc<dyn MetadataStore>,
    rebalance_interval: Duration,
}

impl PartitionRebalancer {
    pub fn start(
        raft: Arc<RaftCluster>,
        metadata: Arc<dyn MetadataStore>,
    ) -> Arc<Self> {
        let rebalancer = Arc::new(Self {
            raft_cluster: raft,
            metadata_store: metadata,
            rebalance_interval: Duration::from_secs(30),
        });

        // Start background worker
        Self::spawn_rebalance_worker(rebalancer.clone());

        rebalancer
    }

    fn spawn_rebalance_worker(rebalancer: Arc<Self>) {
        tokio::spawn(async move {
            info!("Starting partition rebalancer");

            while !rebalancer.shutdown.load(Ordering::Relaxed) {
                // Check if cluster membership changed
                if let Ok(members) = rebalancer.raft_cluster.get_all_nodes().await {
                    let current_count = members.len();

                    // If membership changed, trigger rebalancing
                    if current_count != rebalancer.last_member_count {
                        info!(
                            "Cluster membership changed: {} → {} nodes",
                            rebalancer.last_member_count, current_count
                        );

                        // Rebalance all topics
                        if let Err(e) = rebalancer.rebalance_all_topics(&members).await {
                            error!("Failed to rebalance partitions: {}", e);
                        }

                        rebalancer.last_member_count = current_count;
                    }
                }

                tokio::time::sleep(rebalancer.rebalance_interval).await;
            }
        });
    }

    async fn rebalance_all_topics(&self, nodes: &[u64]) -> Result<()> {
        let topics = self.metadata_store.list_topics().await?;

        for topic in topics {
            self.rebalance_topic(&topic.name, nodes).await?;
        }

        Ok(())
    }

    async fn rebalance_topic(&self, topic: &str, nodes: &[u64]) -> Result<()> {
        // Get current assignments
        let current = self.metadata_store
            .get_partition_assignments(topic)
            .await?;

        // Calculate new assignments (round-robin with RF)
        let partition_count = current.len();
        let new_assignments = self.calculate_assignments(
            partition_count,
            nodes,
            replication_factor,
        );

        // Propose changes via Raft
        for (partition, new_replicas) in new_assignments {
            self.raft_cluster
                .propose_partition_assignment(topic, partition, new_replicas)
                .await?;
        }

        info!("Rebalanced topic '{}' across {} nodes", topic, nodes.len());
        Ok(())
    }
}
```

---

## Phase 1: CLI Redesign & Unified Config

### New Configuration Approach

**Single cluster config** (TOML file or env vars):

```toml
# cluster.toml (NEW)
[cluster]
enabled = true
node_id = 1
replication_factor = 3
min_insync_replicas = 2

# List of ALL nodes in cluster (including this one)
[[cluster.nodes]]
id = 1
kafka_addr = "node1.example.com:9092"
wal_addr = "node1.example.com:9291"   # NEW: WAL receiver address
raft_addr = "node1.example.com:5001"

[[cluster.nodes]]
id = 2
kafka_addr = "node2.example.com:9092"
wal_addr = "node2.example.com:9291"
raft_addr = "node2.example.com:5001"

[[cluster.nodes]]
id = 3
kafka_addr = "node3.example.com:9092"
wal_addr = "node3.example.com:9291"
raft_addr = "node3.example.com:5001"
```

**Environment variable equivalent**:
```bash
CHRONIK_NODE_ID=1
CHRONIK_CLUSTER_PEERS="node1:9092:9291:5001,node2:9092:9291:5001,node3:9092:9291:5001"
#                       ^host ^kafka ^wal  ^raft
```

### Simplified Startup Commands

**BEFORE** (v2.5.0 - Manual, Error-Prone):
```bash
# Node 1 (leader):
CHRONIK_REPLICATION_FOLLOWERS=localhost:9291,localhost:9292 \
  ./chronik-server --node-id 1 --kafka-port 9092 --data-dir ./data-node-1 standalone

# Node 2 (follower):
CHRONIK_WAL_RECEIVER_ADDR="0.0.0.0:9291" \
  ./chronik-server --node-id 2 --kafka-port 9093 --data-dir ./data-node-2 standalone

# Node 3 (follower):
CHRONIK_WAL_RECEIVER_ADDR="0.0.0.0:9292" \
  ./chronik-server --node-id 3 --kafka-port 9094 --data-dir ./data-node-3 standalone
```

**AFTER** (v2.5.0 Final - Simple, Automatic):
```bash
# All nodes (identical command except node_id and data_dir):
./chronik-server cluster --config cluster.toml --data-dir ./data-node-1
./chronik-server cluster --config cluster.toml --data-dir ./data-node-2
./chronik-server cluster --config cluster.toml --data-dir ./data-node-3
```

**Or with env vars** (for Docker/K8s):
```bash
# Node 1:
CHRONIK_NODE_ID=1 \
CHRONIK_CLUSTER_PEERS="node1:9092:9291:5001,node2:9092:9291:5001,node3:9092:9291:5001" \
  ./chronik-server cluster

# Node 2:
CHRONIK_NODE_ID=2 \
CHRONIK_CLUSTER_PEERS="node1:9092:9291:5001,node2:9092:9291:5001,node3:9092:9291:5001" \
  ./chronik-server cluster

# Node 3:
CHRONIK_NODE_ID=3 \
CHRONIK_CLUSTER_PEERS="node1:9092:9291:5001,node2:9092:9291:5001,node3:9092:9291:5001" \
  ./chronik-server cluster
```

---

## Phase 2: Automatic WAL Replication Discovery

### How It Works

1. **Raft Cluster Membership**: All nodes know about each other via Raft
2. **Leader Election**: Raft elects partition leaders (existing)
3. **WAL Receiver Auto-Start**: Every node starts WAL receiver on `wal_addr` port
4. **Leader Auto-Discovery**: Leaders query Raft for partition replicas → connect to their `wal_addr`

### Implementation Steps

#### Step 2.1: Add `wal_addr` to NodeConfig

```rust
// crates/chronik-config/src/cluster.rs
#[derive(Debug, Clone, Serialize, Deserialize, Validate)]
pub struct NodeConfig {
    pub id: u64,
    pub addr: String,           // Kafka API address (e.g., "node1:9092")
    pub wal_addr: String,       // NEW: WAL receiver address (e.g., "node1:9291")
    pub raft_port: u16,         // Raft gRPC port (e.g., 5001)
}
```

#### Step 2.2: Auto-Start WAL Receiver on All Nodes

```rust
// crates/chronik-server/src/integrated_server.rs (line 887+)
// BEFORE: Start WAL receiver only if CHRONIK_WAL_RECEIVER_ADDR is set
if let Ok(receiver_addr) = std::env::var("CHRONIK_WAL_RECEIVER_ADDR") {
    if !receiver_addr.is_empty() {
        let wal_receiver = WalReceiver::new_with_isr_tracker(...);
        tokio::spawn(async move { wal_receiver.run().await });
    }
}

// AFTER: Auto-start WAL receiver if cluster config is present
if let Some(ref cluster) = cluster_config {
    let this_node = cluster.this_node().unwrap();
    let wal_receiver_addr = &this_node.wal_addr;

    info!("Auto-starting WAL receiver on {} (cluster mode)", wal_receiver_addr);

    let wal_receiver = WalReceiver::new_with_isr_tracker(
        wal_receiver_addr.clone(),
        wal_manager.clone(),
        isr_ack_tracker.clone(),
        node_id as u64,
    );

    tokio::spawn(async move {
        if let Err(e) = wal_receiver.run().await {
            error!("WAL receiver failed: {}", e);
        }
    });
}
```

#### Step 2.3: Auto-Discover Followers from Raft

```rust
// crates/chronik-server/src/integrated_server.rs (new function)
/// Query Raft cluster for partition replicas and build follower list
fn build_follower_list_from_raft(
    cluster_config: &ClusterConfig,
    raft_cluster: &RaftCluster,
    topic: &str,
    partition: i32,
) -> Vec<String> {
    // Query Raft for partition replicas (e.g., [node1, node2, node3])
    let replicas = match raft_cluster.get_partition_replicas(topic, partition) {
        Ok(r) => r,
        Err(e) => {
            warn!("Failed to query Raft for partition replicas: {}", e);
            return vec![];
        }
    };

    // Filter out this node (leader doesn't replicate to itself)
    let other_replicas: Vec<u64> = replicas
        .into_iter()
        .filter(|&id| id != cluster_config.node_id)
        .collect();

    // Map node IDs to WAL receiver addresses
    let mut follower_addrs = Vec::new();
    for node_id in other_replicas {
        if let Some(node) = cluster_config.peers.iter().find(|p| p.id == node_id) {
            follower_addrs.push(node.wal_addr.clone());
        }
    }

    follower_addrs
}
```

#### Step 2.4: Create WalReplicationManager with Raft Integration

```rust
// crates/chronik-server/src/integrated_server.rs
// BEFORE: Manual follower list from env var
let follower_addrs = std::env::var("CHRONIK_REPLICATION_FOLLOWERS")
    .ok()
    .map(|s| s.split(',').map(|x| x.to_string()).collect())
    .unwrap_or_default();

let wal_replication_manager = if !follower_addrs.is_empty() {
    Some(WalReplicationManager::new(follower_addrs))
} else {
    None
};

// AFTER: Automatic discovery from Raft cluster
let wal_replication_manager = if let Some(ref cluster) = cluster_config {
    // In cluster mode, use Raft to discover replicas dynamically
    info!("Cluster mode: enabling automatic WAL replication discovery");

    Some(WalReplicationManager::new_with_cluster(
        cluster.clone(),
        raft_cluster.clone(),
        isr_tracker.clone(),
        isr_ack_tracker.clone(),
    ))
} else if let Ok(manual_followers) = std::env::var("CHRONIK_REPLICATION_FOLLOWERS") {
    // Fallback: manual follower list (for backward compatibility)
    warn!("Using manual CHRONIK_REPLICATION_FOLLOWERS - consider using cluster config");
    let follower_addrs: Vec<String> = manual_followers
        .split(',')
        .map(|s| s.to_string())
        .collect();

    Some(WalReplicationManager::new(follower_addrs))
} else {
    None
};
```

#### Step 2.5: Dynamic Follower Discovery in WalReplicationManager

```rust
// crates/chronik-server/src/wal_replication.rs
impl WalReplicationManager {
    /// NEW: Create replication manager with automatic Raft discovery
    pub fn new_with_cluster(
        cluster_config: ClusterConfig,
        raft_cluster: Arc<RaftCluster>,
        isr_tracker: Option<Arc<IsrTracker>>,
        isr_ack_tracker: Option<Arc<IsrAckTracker>>,
    ) -> Arc<Self> {
        info!("Creating WalReplicationManager with automatic Raft discovery");

        // Start with empty follower list - will be populated dynamically
        let manager = Self {
            queue: Arc::new(SegQueue::new()),
            connections: Arc::new(DashMap::new()),
            followers: vec![],  // Empty initially
            shutdown: Arc::new(AtomicBool::new(false)),
            total_queued: Arc::new(AtomicU64::new(0)),
            total_sent: Arc::new(AtomicU64::new(0)),
            total_dropped: Arc::new(AtomicU64::new(0)),
            raft_cluster: Some(raft_cluster.clone()),
            isr_tracker,
            isr_ack_tracker,
            cluster_config: Some(cluster_config),  // NEW field
        };

        let manager = Arc::new(manager);

        // Start background worker for connection management
        Self::spawn_connection_manager(manager.clone());

        // Start background worker for sending queued records
        Self::spawn_sender_worker(manager.clone());

        // NEW: Start background worker for dynamic follower discovery
        Self::spawn_follower_discovery_worker(manager.clone());

        manager
    }

    /// NEW: Background worker that watches Raft for partition assignment changes
    fn spawn_follower_discovery_worker(manager: Arc<Self>) {
        tokio::spawn(async move {
            info!("Starting follower discovery worker");

            while !manager.shutdown.load(Ordering::Relaxed) {
                // For each partition we're the leader for, update follower list
                if let Some(ref raft) = manager.raft_cluster {
                    if let Some(ref config) = manager.cluster_config {
                        // Query Raft for all topics/partitions we're leader for
                        // (This requires adding get_my_partitions() to RaftCluster)
                        if let Ok(my_partitions) = raft.get_partitions_where_leader(config.node_id) {
                            for (topic, partition) in my_partitions {
                                // Get replicas for this partition
                                let replicas = match raft.get_partition_replicas(&topic, partition) {
                                    Ok(r) => r,
                                    Err(e) => {
                                        warn!("Failed to get replicas for {}-{}: {}", topic, partition, e);
                                        continue;
                                    }
                                };

                                // Filter out this node
                                let followers: Vec<u64> = replicas
                                    .into_iter()
                                    .filter(|&id| id != config.node_id)
                                    .collect();

                                // Map to WAL addresses
                                let follower_addrs: Vec<String> = followers
                                    .iter()
                                    .filter_map(|&id| {
                                        config.peers.iter()
                                            .find(|p| p.id == id)
                                            .map(|p| p.wal_addr.clone())
                                    })
                                    .collect();

                                // Update connections (add new followers, remove old ones)
                                manager.update_followers_for_partition(&topic, partition, follower_addrs);
                            }
                        }
                    }
                }

                // Check every 10 seconds
                tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;
            }

            info!("Follower discovery worker shutting down");
        });
    }

    /// NEW: Update follower list for a specific partition
    fn update_followers_for_partition(&self, topic: &str, partition: i32, new_followers: Vec<String>) {
        // Close connections to followers no longer in the list
        let current_followers: Vec<String> = self.connections
            .iter()
            .map(|entry| entry.key().clone())
            .collect();

        for old_follower in current_followers {
            if !new_followers.contains(&old_follower) {
                info!("Removing follower {} for {}-{} (no longer a replica)", old_follower, topic, partition);
                self.connections.remove(&old_follower);
            }
        }

        // Add new followers
        for new_follower in &new_followers {
            if !self.connections.contains_key(new_follower) {
                info!("Adding follower {} for {}-{}", new_follower, topic, partition);
                // Connection will be established by connection_manager
            }
        }

        debug!("Updated followers for {}-{}: {:?}", topic, partition, new_followers);
    }
}
```

---

## Phase 3: Disaster Recovery & Dynamic Leader Change

### Leader Failover Scenario

**Current (v2.5.0)**: Manual intervention required
```
1. Node 1 (leader) crashes
2. Raft elects Node 2 as new leader
3. ❌ WAL replication still tries to send to Node 1
4. ❌ Node 2 doesn't know it should replicate to Node 3
5. ❌ Manual restart required with new CHRONIK_REPLICATION_FOLLOWERS
```

**Proposed (v2.5.0 Final)**: Automatic recovery
```
1. Node 1 (leader) crashes
2. Raft elects Node 2 as new leader
3. ✅ Node 2's follower discovery worker detects it's now leader
4. ✅ Queries Raft for partition replicas → finds Node 3
5. ✅ Automatically starts replicating to Node 3
6. ✅ Node 3's WAL receiver already listening → accepts connection
7. ✅ Zero downtime, no manual intervention
```

### Implementation

Already covered in Phase 2 - `spawn_follower_discovery_worker()` handles this automatically:
- Watches Raft for leadership changes every 10 seconds
- When node becomes leader, automatically discovers followers
- When node loses leadership, closes replication connections

---

## Phase 4: Simplified CLI & Smart Defaults

### New `cluster` Subcommand

```bash
chronik-server cluster [OPTIONS]

Options:
  --config <FILE>          Path to cluster config TOML file
  --data-dir <DIR>         Data directory (default: ./data)
  --log-level <LEVEL>      Log level (default: info)

# All other settings derived from cluster config:
# - node_id (from config file or CHRONIK_NODE_ID env var)
# - kafka_port (from cluster config node address)
# - wal_addr (from cluster config node address)
# - raft_addr (from cluster config node address)
# - peers (from cluster config)
```

### Backward Compatibility

Keep existing `standalone` and `raft-cluster` subcommands for backward compatibility:
- `standalone` - Single node or manual WAL replication (current behavior)
- `raft-cluster` - Manual Raft setup (current behavior)
- `cluster` - **NEW**: Unified automatic cluster mode

---

## Phase 5: Port Auto-Derivation Simplification

### Current Complexity (main.rs:601-617)

```rust
// Metrics port = kafka_port + 4000 (why?)
if cli.metrics_port == 9093 && is_cluster_mode {
    cli.metrics_port = cli.kafka_port + 4000;
}

// Search port = kafka_port - 3000 (why?)
if cli.search_port == 6080 && is_cluster_mode {
    cli.search_port = if cli.kafka_port >= 3000 {
        cli.kafka_port - 3000
    } else {
        cli.kafka_port + 3000
    };
}
```

### Proposed: Explicit Config or Standard Offsets

**Option A**: Require explicit ports in cluster config (recommended)
```toml
[[cluster.nodes]]
id = 1
kafka_addr = "node1:9092"
wal_addr = "node1:9291"
raft_addr = "node1:5001"
metrics_port = 13092    # Explicit
search_port = 6092      # Explicit
```

**Option B**: Standard port offsets with clear documentation
```rust
// Standard Chronik port layout (well-documented):
// Kafka API:    base_port (e.g., 9092)
// WAL receiver: base_port + 199 (e.g., 9291)
// Raft gRPC:    base_port + 909 (e.g., 5001) - separate range
// Metrics:      base_port + 4000 (e.g., 13092)
// Search API:   base_port - 3000 (e.g., 6092)

const KAFKA_BASE: u16 = 9092;
const WAL_OFFSET: u16 = 199;   // 9092 + 199 = 9291
const RAFT_BASE: u16 = 5001;   // Separate range
const METRICS_OFFSET: u16 = 4000;  // 9092 + 4000 = 13092
const SEARCH_OFFSET_DOWN: u16 = 3000;  // 9092 - 3000 = 6092
```

**Recommendation**: Use Option A (explicit ports in config) for production deployments.

---

## Migration from Current Implementation

### Step 1: Add Unified Config Support (Backward Compatible)

```rust
// Support BOTH old env vars AND new cluster config
let wal_replication_manager = if let Some(ref cluster) = cluster_config {
    // NEW: Automatic cluster mode
    Some(WalReplicationManager::new_with_cluster(cluster, raft, isr, ack))
} else if let Ok(manual) = std::env::var("CHRONIK_REPLICATION_FOLLOWERS") {
    // OLD: Manual env var mode (still works)
    Some(WalReplicationManager::new(parse_followers(manual)))
} else {
    None
};
```

### Step 2: Deprecation Warnings

```rust
if std::env::var("CHRONIK_REPLICATION_FOLLOWERS").is_ok() {
    warn!("⚠️  CHRONIK_REPLICATION_FOLLOWERS is deprecated");
    warn!("⚠️  Use cluster config instead: chronik-server cluster --config cluster.toml");
}

if std::env::var("CHRONIK_WAL_RECEIVER_ADDR").is_ok() {
    warn!("⚠️  CHRONIK_WAL_RECEIVER_ADDR is deprecated");
    warn!("⚠️  WAL receiver now auto-starts in cluster mode");
}
```

### Step 3: Documentation Update

- Update CLAUDE.md with new cluster setup instructions
- Update HYBRID_CLUSTERING_ARCHITECTURE.md with automatic discovery
- Add examples to docs/CLUSTER_SETUP_GUIDE.md

---

## Testing Strategy

### Unit Tests

1. `ClusterConfig::from_env()` parsing
2. `NodeConfig::wal_addr()` address formatting
3. Port validation logic

### Integration Tests

1. **3-node cluster startup**:
   - All nodes start with same config
   - WAL receivers auto-start on all nodes
   - Raft elects leader
   - Leader auto-discovers followers

2. **Leader failover**:
   - Kill leader node
   - Verify Raft elects new leader
   - Verify new leader starts replicating
   - Verify followers accept connections

3. **Partition reassignment**:
   - Create topic with RF=2
   - Verify leader replicates to correct follower
   - Reassign partition to different replica set
   - Verify replication switches to new follower

### Performance Tests

- Ensure automatic discovery doesn't add latency
- Benchmark: unified config vs manual config (should be identical)

---

## Implementation Checklist

### Phase 1: Unified Config (Week 1)
- [ ] Add `wal_addr: String` to `NodeConfig`
- [ ] Update `ClusterConfig::from_env()` to parse 4-part format: `host:kafka:wal:raft`
- [ ] Add `cluster` subcommand to CLI
- [ ] Write unit tests for config parsing
- [ ] Update CLAUDE.md with new examples

### Phase 2: Automatic Discovery (Week 2)
- [ ] Add `cluster_config` field to `WalReplicationManager`
- [ ] Implement `WalReplicationManager::new_with_cluster()`
- [ ] Implement `spawn_follower_discovery_worker()`
- [ ] Auto-start WAL receiver on all nodes (integrated_server.rs)
- [ ] Add `RaftCluster::get_partitions_where_leader()` method
- [ ] Add `RaftCluster::get_partition_replicas()` method (if not exists)
- [ ] Write integration test: 3-node cluster with auto-discovery

### Phase 3: Disaster Recovery (Week 3)
- [ ] Test leader failover scenario
- [ ] Verify follower discovery on leadership change
- [ ] Add metrics: `leader_changes_total`, `follower_discovery_time`
- [ ] Write integration test: kill leader, verify new leader takes over

### Phase 4: CLI Simplification (Week 3)
- [ ] Implement `Commands::Cluster` variant
- [ ] Add backward compatibility for old subcommands
- [ ] Add deprecation warnings for old env vars
- [ ] Update all documentation

### Phase 5: Port Simplification (Week 4)
- [ ] Document standard port layout
- [ ] Add validation for port conflicts
- [ ] Add helper: `derive_ports_from_kafka_port(base: u16)`
- [ ] Update examples in docs

### Testing & Documentation (Week 4)
- [ ] Write comprehensive integration tests
- [ ] Benchmark: auto-discovery vs manual config
- [ ] Update CLAUDE.md with full examples
- [ ] Update HYBRID_CLUSTERING_ARCHITECTURE.md
- [ ] Create docs/CLUSTER_SETUP_GUIDE.md
- [ ] Create docs/MIGRATION_GUIDE_V2.6.md

---

## Example: Complete Cluster Setup (v2.5.0 Final)

### cluster.toml
```toml
[cluster]
enabled = true
replication_factor = 3
min_insync_replicas = 2

[[cluster.nodes]]
id = 1
kafka_addr = "10.0.1.10:9092"
wal_addr = "10.0.1.10:9291"
raft_port = 5001

[[cluster.nodes]]
id = 2
kafka_addr = "10.0.1.11:9092"
wal_addr = "10.0.1.11:9291"
raft_port = 5001

[[cluster.nodes]]
id = 3
kafka_addr = "10.0.1.12:9092"
wal_addr = "10.0.1.12:9291"
raft_port = 5001
```

### Startup Commands (ALL IDENTICAL)
```bash
# Node 1:
./chronik-server cluster --config cluster.toml --data-dir /data/node-1

# Node 2:
./chronik-server cluster --config cluster.toml --data-dir /data/node-2

# Node 3:
./chronik-server cluster --config cluster.toml --data-dir /data/node-3
```

### What Happens Automatically
1. ✅ All nodes start Raft cluster
2. ✅ All nodes start WAL receivers on their `wal_addr`
3. ✅ Raft elects partition leaders
4. ✅ Leaders query Raft for partition replicas
5. ✅ Leaders connect to followers' WAL receivers
6. ✅ Replication starts automatically
7. ✅ On leader failure, new leader auto-discovers followers
8. ✅ Zero manual configuration required

---

## Benefits

### For Users
- **Simple**: Same command on all nodes
- **Safe**: No manual follower lists, no typos
- **Dynamic**: Handles leader changes automatically
- **Clear**: One config file, no env var confusion

### For Operations
- **Reliable**: Automatic disaster recovery
- **Observable**: Clear logs show discovery events
- **Flexible**: Add/remove nodes without reconfiguring all nodes
- **Maintainable**: No manual intervention for common failures

### For Development
- **Clean**: Remove redundant env var logic
- **Testable**: Integration tests can simulate real scenarios
- **Extensible**: Easy to add gossip discovery later
- **Documented**: Clear architecture, less confusion

---

## Future Enhancements (v2.7.0+)

### Gossip-Based Bootstrap
- Use Serf/memberlist for automatic peer discovery
- No config file needed, just seed nodes
- Dynamic cluster membership

### Service Discovery Integration
- Consul integration for dynamic node registry
- Kubernetes StatefulSet support with headless service
- DNS SRV record discovery

### Advanced Disaster Recovery
- Snapshot-based catch-up for lagging replicas
- Automatic partition reassignment on node failure
- Cross-datacenter replication with rack awareness

---

## Open Questions

1. **Should we remove `standalone` + manual `CHRONIK_REPLICATION_FOLLOWERS` entirely?**
   - **Recommendation**: Keep for backward compatibility, add deprecation warnings

2. **Should `cluster` subcommand require Raft, or support WAL-only replication?**
   - **Recommendation**: Require Raft for metadata coordination (this is the hybrid design)

3. **How often should follower discovery worker check Raft?**
   - **Recommendation**: 10 seconds (configurable via `CHRONIK_FOLLOWER_DISCOVERY_INTERVAL_SECS`)

4. **Should we validate port ranges in config validation?**
   - **Recommendation**: Yes - add `validate_ports_no_conflicts()` to `ClusterConfig`

5. **What happens if a node joins mid-operation?**
   - **Recommendation**: Phase 1 handles static clusters, gossip bootstrap (v2.7.0) handles dynamic membership
