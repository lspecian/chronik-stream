# Raft Clustering Deployment Guide

This guide covers deploying Chronik Stream with Raft-based clustering for high availability and fault tolerance.

## Table of Contents

- [Prerequisites](#prerequisites)
- [Single-Node Setup (Development)](#single-node-setup-development)
- [3-Node Cluster Setup (Production)](#3-node-cluster-setup-production)
- [Configuration File Reference](#configuration-file-reference)
- [Environment Variables](#environment-variables)
- [Starting and Stopping Nodes](#starting-and-stopping-nodes)
- [Health Checks and Verification](#health-checks-and-verification)
- [Scaling the Cluster](#scaling-the-cluster)

---

## Prerequisites

### System Requirements

**Minimum per node:**
- CPU: 2 cores
- RAM: 4GB
- Disk: 50GB SSD (for WAL and local segments)
- Network: 1 Gbps between nodes

**Recommended per node:**
- CPU: 4-8 cores
- RAM: 8-16GB
- Disk: 100GB+ NVMe SSD
- Network: 10 Gbps between nodes

### Software Requirements

- **Rust toolchain**: 1.70+ (for building from source)
- **Operating System**: Linux (Ubuntu 20.04+, RHEL 8+), macOS 12+
- **Kafka CLI tools** (optional, for testing): `kafka-console-producer`, `kafka-console-consumer`
- **Object Storage** (optional): S3/MinIO/GCS/Azure for disaster recovery

### Network Requirements

**Ports:**
- Kafka port: `9092` (or custom via `CHRONIK_KAFKA_PORT`)
- Raft gRPC port: Kafka port + 1 (e.g., `9093` for node on `9092`)

**Firewall Rules:**
- Allow TCP traffic between all cluster nodes on both Kafka and Raft ports
- Allow incoming TCP on Kafka port from Kafka clients
- Raft ports only need intra-cluster connectivity

**DNS/Hostnames:**
- Each node must be able to resolve other nodes by hostname or IP
- Use static IPs or reliable DNS entries

---

## Single-Node Setup (Development)

For development or testing, you can run a single Chronik node **without Raft** (WAL-only mode).

### Step 1: Build Chronik

```bash
# Clone repository
git clone https://github.com/your-org/chronik-stream.git
cd chronik-stream

# Build release binary
cargo build --release --bin chronik-server

# Verify build
./target/release/chronik-server --version
```

### Step 2: Start Single-Node Server

```bash
# Start in standalone mode (no Raft)
./target/release/chronik-server \
  --advertised-addr localhost \
  standalone

# Or with environment variables
CHRONIK_ADVERTISED_ADDR=localhost \
CHRONIK_DATA_DIR=./data \
./target/release/chronik-server standalone
```

**What happens:**
- Server binds to `0.0.0.0:9092` (Kafka)
- Advertises `localhost:9092` to clients
- Uses WAL-based metadata (no Raft)
- Data stored in `./data/`

### Step 3: Test with Kafka Client

```bash
# Create topic
kafka-topics --create \
  --topic test-topic \
  --bootstrap-server localhost:9092 \
  --partitions 1 \
  --replication-factor 1

# Produce messages
echo "Hello Chronik!" | kafka-console-producer \
  --topic test-topic \
  --bootstrap-server localhost:9092

# Consume messages
kafka-console-consumer \
  --topic test-topic \
  --from-beginning \
  --bootstrap-server localhost:9092
```

**When to use single-node:**
- Development and testing
- Local experimentation
- Low-volume production (accepts data loss risk)

---

## 3-Node Cluster Setup (Production)

For production deployments, use a **3-node Raft cluster** for fault tolerance.

### Architecture Overview

```
┌─────────────────────────────────────────────────────────────┐
│                  Chronik Raft Cluster (3 Nodes)              │
├─────────────────────────────────────────────────────────────┤
│  Node 1 (Leader)          Node 2 (Follower)    Node 3 (Follower) │
│  ├─ Kafka: 9092          ├─ Kafka: 9092       ├─ Kafka: 9092    │
│  ├─ Raft: 9093           ├─ Raft: 9093        ├─ Raft: 9093     │
│  └─ Data: /data/node1    └─ Data: /data/node2 └─ Data: /data/node3 │
│                                                                 │
│  Producers → Any Node (leader forwards to Raft)                │
│  Consumers → Any Node (read from leader or followers)          │
│                                                                 │
│  Replication:                                                   │
│    Metadata: Raft consensus (__meta partition)                 │
│    Data: Per-partition Raft groups                             │
│    Quorum: 2/3 nodes required for writes                       │
└─────────────────────────────────────────────────────────────┘
```

### Step 1: Create Cluster Configuration Files

Each node needs a `cluster.toml` file defining the cluster topology.

**Node 1 (`node1-cluster.toml`):**
```toml
enabled = true
node_id = 1
replication_factor = 3
min_insync_replicas = 2

[[peers]]
id = 1
addr = "node1.example.com:9092"
raft_port = 9093

[[peers]]
id = 2
addr = "node2.example.com:9092"
raft_port = 9093

[[peers]]
id = 3
addr = "node3.example.com:9092"
raft_port = 9093
```

**Node 2 (`node2-cluster.toml`):**
```toml
enabled = true
node_id = 2
replication_factor = 3
min_insync_replicas = 2

[[peers]]
id = 1
addr = "node1.example.com:9092"
raft_port = 9093

[[peers]]
id = 2
addr = "node2.example.com:9092"
raft_port = 9093

[[peers]]
id = 3
addr = "node3.example.com:9092"
raft_port = 9093
```

**Node 3 (`node3-cluster.toml`):**
```toml
enabled = true
node_id = 3
replication_factor = 3
min_insync_replicas = 2

[[peers]]
id = 1
addr = "node1.example.com:9092"
raft_port = 9093

[[peers]]
id = 2
addr = "node2.example.com:9092"
raft_port = 9093

[[peers]]
id = 3
addr = "node3.example.com:9092"
raft_port = 9093
```

**Key points:**
- `node_id` is unique for each node
- All nodes have identical `[[peers]]` sections
- `addr` is the Kafka endpoint (advertised to clients)
- `raft_port` is the Raft gRPC endpoint (internal only)

### Step 2: Start All Nodes

Start nodes in sequence to allow quorum detection.

**On node1.example.com:**
```bash
CHRONIK_CLUSTER_CONFIG=./node1-cluster.toml \
CHRONIK_ADVERTISED_ADDR=node1.example.com \
CHRONIK_DATA_DIR=/data/chronik/node1 \
./target/release/chronik-server standalone
```

**On node2.example.com:**
```bash
CHRONIK_CLUSTER_CONFIG=./node2-cluster.toml \
CHRONIK_ADVERTISED_ADDR=node2.example.com \
CHRONIK_DATA_DIR=/data/chronik/node2 \
./target/release/chronik-server standalone
```

**On node3.example.com:**
```bash
CHRONIK_CLUSTER_CONFIG=./node3-cluster.toml \
CHRONIK_ADVERTISED_ADDR=node3.example.com \
CHRONIK_DATA_DIR=/data/chronik/node3 \
./target/release/chronik-server standalone
```

**What happens during bootstrap:**
1. Each node starts and loads cluster config
2. Nodes detect peers via heartbeat health checks
3. Once quorum (2/3 nodes) is detected, cluster initializes
4. Raft leader election occurs for `__meta` partition
5. Cluster is ready to accept requests

**Expected logs:**
```
INFO chronik_raft::cluster_coordinator - Creating ClusterCoordinator for node 1 with 2 peers
INFO chronik_raft::cluster_coordinator - Creating metadata partition with peers: [2, 3]
INFO chronik_raft::cluster_coordinator - Starting cluster bootstrap for node 1 (cluster size: 3)
INFO chronik_raft::cluster_coordinator - Waiting for quorum (need 2/3 nodes)...
INFO chronik_raft::cluster_coordinator - Quorum achieved! 3/3 peers alive
INFO chronik_raft::cluster_coordinator - Bootstrap complete for node 1
INFO chronik_server - Chronik server ready on 0.0.0.0:9092 (advertised: node1.example.com:9092)
```

### Step 3: Verify Cluster Formation

**Check cluster status:**
```bash
# Connect to any node
curl http://node1.example.com:9092/metrics | grep raft_leader

# Should see:
# raft_leader{topic="__meta",partition="0"} 1
```

**Create a replicated topic:**
```bash
# Connect to any node (Kafka protocol)
kafka-topics --create \
  --topic orders \
  --bootstrap-server node1.example.com:9092 \
  --partitions 3 \
  --replication-factor 3

# Verify replication
kafka-topics --describe \
  --topic orders \
  --bootstrap-server node1.example.com:9092
```

**Produce and consume:**
```bash
# Produce to node 1
seq 1 1000 | kafka-console-producer \
  --topic orders \
  --bootstrap-server node1.example.com:9092

# Consume from node 2 (should see all messages)
kafka-console-consumer \
  --topic orders \
  --from-beginning \
  --bootstrap-server node2.example.com:9092
```

### Step 4: Test Fault Tolerance

**Kill leader node:**
```bash
# On node1 (assuming it's leader)
killall chronik-server
```

**Verify failover:**
```bash
# Check new leader (should be node 2 or 3)
curl http://node2.example.com:9092/metrics | grep raft_leader

# Produce should still work
echo "After failover" | kafka-console-producer \
  --topic orders \
  --bootstrap-server node2.example.com:9092

# Consume should still work
kafka-console-consumer \
  --topic orders \
  --from-beginning \
  --max-messages 1 \
  --bootstrap-server node3.example.com:9092
```

**Restart node1:**
```bash
# On node1
CHRONIK_CLUSTER_CONFIG=./node1-cluster.toml \
CHRONIK_ADVERTISED_ADDR=node1.example.com \
CHRONIK_DATA_DIR=/data/chronik/node1 \
./target/release/chronik-server standalone
```

Node1 will rejoin as a follower and catch up via Raft log replication.

---

## Configuration File Reference

### Complete `cluster.toml` Example

```toml
# Enable Raft clustering
enabled = true

# Unique node ID (1-255)
node_id = 1

# Default replication factor for new topics
replication_factor = 3

# Minimum in-sync replicas required for produce
min_insync_replicas = 2

# Raft timing configuration (optional)
[raft]
election_timeout_ms = 300        # Election timeout (150-300ms recommended)
heartbeat_interval_ms = 30       # Heartbeat interval (1/10 of election timeout)
snapshot_threshold = 10000       # Create snapshot after N log entries

# ISR configuration (optional)
[isr]
max_lag_ms = 10000              # Max time lag to be in-sync (10s)
max_lag_entries = 10000         # Max entry lag to be in-sync
check_interval_ms = 1000        # ISR check interval (1s)

# Peer definitions (required)
[[peers]]
id = 1
addr = "node1.example.com:9092"  # Kafka endpoint
raft_port = 9093                 # Raft gRPC port

[[peers]]
id = 2
addr = "node2.example.com:9092"
raft_port = 9093

[[peers]]
id = 3
addr = "node3.example.com:9092"
raft_port = 9093
```

### Configuration Fields

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `enabled` | bool | Yes | - | Enable Raft clustering |
| `node_id` | u64 | Yes | - | Unique node ID (1-255) |
| `replication_factor` | u32 | Yes | 3 | Default replication for topics |
| `min_insync_replicas` | u32 | Yes | 2 | Min ISR for produce success |
| `raft.election_timeout_ms` | u64 | No | 300 | Election timeout |
| `raft.heartbeat_interval_ms` | u64 | No | 30 | Heartbeat interval |
| `raft.snapshot_threshold` | u64 | No | 10000 | Snapshot threshold |
| `isr.max_lag_ms` | u64 | No | 10000 | Max time lag for ISR |
| `isr.max_lag_entries` | u64 | No | 10000 | Max entry lag for ISR |
| `peers[].id` | u64 | Yes | - | Peer node ID |
| `peers[].addr` | string | Yes | - | Peer Kafka address |
| `peers[].raft_port` | u16 | Yes | - | Peer Raft port |

---

## Environment Variables

| Variable | Description | Default | Example |
|----------|-------------|---------|---------|
| `CHRONIK_CLUSTER_CONFIG` | Path to cluster config file | None | `./cluster.toml` |
| `CHRONIK_ADVERTISED_ADDR` | Advertised Kafka hostname | `0.0.0.0` | `node1.example.com` |
| `CHRONIK_ADVERTISED_PORT` | Advertised Kafka port | `9092` | `9092` |
| `CHRONIK_KAFKA_PORT` | Kafka bind port | `9092` | `9092` |
| `CHRONIK_BIND_ADDR` | Kafka bind address | `0.0.0.0` | `0.0.0.0` |
| `CHRONIK_DATA_DIR` | Data directory | `./data` | `/data/chronik` |
| `RUST_LOG` | Log level | `info` | `debug` |
| `OBJECT_STORE_BACKEND` | Object store type | `local` | `s3` |
| `S3_BUCKET` | S3 bucket for DR | None | `chronik-prod` |

**Minimal Cluster Startup:**
```bash
# All nodes need these 3 env vars
CHRONIK_CLUSTER_CONFIG=./cluster.toml \
CHRONIK_ADVERTISED_ADDR=node1.example.com \
./target/release/chronik-server standalone
```

---

## Starting and Stopping Nodes

### systemd Service (Recommended for Production)

Create `/etc/systemd/system/chronik.service` on each node:

```ini
[Unit]
Description=Chronik Stream Kafka Server
After=network.target

[Service]
Type=simple
User=chronik
Group=chronik
WorkingDirectory=/opt/chronik
Environment="CHRONIK_CLUSTER_CONFIG=/etc/chronik/cluster.toml"
Environment="CHRONIK_ADVERTISED_ADDR=node1.example.com"
Environment="CHRONIK_DATA_DIR=/data/chronik"
Environment="RUST_LOG=info"
ExecStart=/opt/chronik/bin/chronik-server standalone
Restart=on-failure
RestartSec=10
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
```

**Start/stop/restart:**
```bash
# Start service
sudo systemctl start chronik

# Stop service (graceful shutdown)
sudo systemctl stop chronik

# Restart service
sudo systemctl restart chronik

# Enable on boot
sudo systemctl enable chronik

# View logs
journalctl -u chronik -f
```

### Manual Start/Stop

**Start in foreground:**
```bash
CHRONIK_CLUSTER_CONFIG=./cluster.toml \
CHRONIK_ADVERTISED_ADDR=node1.example.com \
./target/release/chronik-server standalone
```

**Start in background:**
```bash
nohup ./target/release/chronik-server standalone \
  > /var/log/chronik.log 2>&1 &

# Save PID
echo $! > /var/run/chronik.pid
```

**Graceful shutdown:**
```bash
# Send SIGTERM (allows graceful shutdown)
kill -TERM $(cat /var/run/chronik.pid)

# Or
killall -TERM chronik-server
```

**Force kill (NOT recommended):**
```bash
# Only use if graceful shutdown hangs
killall -KILL chronik-server
```

### Rolling Restart Procedure

See [docs/raft/ROLLING_RESTART_PROCEDURE.md](raft/ROLLING_RESTART_PROCEDURE.md) for detailed steps.

**Quick version:**
```bash
# 1. Restart follower nodes first (one at a time)
# On node2
systemctl restart chronik
# Wait for node2 to rejoin
sleep 30

# On node3
systemctl restart chronik
# Wait for node3 to rejoin
sleep 30

# 2. Transfer leadership from node1
curl -X POST http://node1.example.com:9092/admin/transfer-leader

# 3. Restart node1
# On node1
systemctl restart chronik
```

---

## Health Checks and Verification

### Cluster Status Metrics

**Prometheus metrics endpoint:**
```bash
curl http://node1.example.com:9092/metrics
```

**Key metrics:**
```promql
# Raft leader status (1 = leader, 0 = follower)
raft_is_leader{topic="__meta",partition="0"} 1

# Raft term (increases with elections)
raft_term{topic="__meta",partition="0"} 5

# Commit index (log position)
raft_commit_index{topic="__meta",partition="0"} 12345

# Applied index (state machine position)
raft_applied_index{topic="__meta",partition="0"} 12345

# ISR size (in-sync replicas)
partition_isr_size{topic="orders",partition="0"} 3

# ISR health (1 = healthy, 0 = unhealthy)
partition_has_min_isr{topic="orders",partition="0"} 1
```

### Health Check Script

```bash
#!/bin/bash
# health_check.sh - Verify Chronik cluster health

NODES=("node1.example.com" "node2.example.com" "node3.example.com")
PORT=9092

echo "Checking Chronik cluster health..."
echo ""

for node in "${NODES[@]}"; do
  echo "Node: $node"

  # Check if node is reachable
  if ! curl -s --connect-timeout 2 http://$node:$PORT/metrics > /dev/null; then
    echo "  ❌ NOT REACHABLE"
    continue
  fi

  # Check if leader
  is_leader=$(curl -s http://$node:$PORT/metrics | \
    grep 'raft_is_leader{topic="__meta",partition="0"}' | \
    awk '{print $2}')

  if [ "$is_leader" == "1" ]; then
    echo "  ✅ LEADER"
  else
    echo "  ✅ FOLLOWER"
  fi

  # Check commit index
  commit_index=$(curl -s http://$node:$PORT/metrics | \
    grep 'raft_commit_index{topic="__meta",partition="0"}' | \
    awk '{print $2}')
  echo "  Commit Index: $commit_index"

  echo ""
done

# Check quorum
reachable=$(curl -s http://${NODES[0]}:$PORT/metrics | \
  grep 'cluster_quorum_size' | awk '{print $2}')

echo "Cluster Quorum: $reachable/3 nodes"
if [ "$reachable" -ge 2 ]; then
  echo "✅ QUORUM HEALTHY"
else
  echo "❌ QUORUM LOST"
fi
```

**Run:**
```bash
chmod +x health_check.sh
./health_check.sh
```

### Kafka Topic Verification

**List topics:**
```bash
kafka-topics --list --bootstrap-server node1.example.com:9092
```

**Describe topic replication:**
```bash
kafka-topics --describe \
  --topic orders \
  --bootstrap-server node1.example.com:9092
```

**Expected output:**
```
Topic: orders  PartitionCount: 3  ReplicationFactor: 3
  Partition: 0  Leader: 1  Replicas: 1,2,3  Isr: 1,2,3
  Partition: 1  Leader: 2  Replicas: 2,3,1  Isr: 2,3,1
  Partition: 2  Leader: 3  Replicas: 3,1,2  Isr: 3,1,2
```

---

## Scaling the Cluster

### Adding a 4th Node (Horizontal Scaling)

**Step 1: Update cluster config on ALL nodes**

Add new peer to `cluster.toml`:
```toml
[[peers]]
id = 4
addr = "node4.example.com:9092"
raft_port = 9093
```

**Step 2: Restart existing nodes (rolling restart)**

```bash
# On node1
systemctl restart chronik

# Wait 30s, then node2
systemctl restart chronik

# Wait 30s, then node3
systemctl restart chronik
```

**Step 3: Start new node**

```bash
# On node4
CHRONIK_CLUSTER_CONFIG=./node4-cluster.toml \
CHRONIK_ADVERTISED_ADDR=node4.example.com \
./target/release/chronik-server standalone
```

**Step 4: Verify node joined**

```bash
curl http://node4.example.com:9092/metrics | grep cluster_size
# Should show: cluster_size 4
```

**Step 5: Rebalance partitions (optional)**

Use admin API to trigger partition rebalancing:
```bash
curl -X POST http://node1.example.com:9092/admin/rebalance
```

### Removing a Node (Decommissioning)

See [RAFT_TROUBLESHOOTING.md](RAFT_TROUBLESHOOTING.md#removing-a-node) for detailed procedure.

---

## Next Steps

- [RAFT_CONFIGURATION_REFERENCE.md](RAFT_CONFIGURATION_REFERENCE.md) - Detailed config tuning
- [RAFT_TROUBLESHOOTING.md](RAFT_TROUBLESHOOTING.md) - Common issues and fixes
- [RAFT_ARCHITECTURE.md](RAFT_ARCHITECTURE.md) - How Raft works in Chronik
- [DISASTER_RECOVERY.md](DISASTER_RECOVERY.md) - S3 backup and recovery
