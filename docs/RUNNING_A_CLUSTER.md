# Running a Chronik Cluster

Quick guide to running a Chronik cluster (v2.2.0+).

## Table of Contents

- [Quick Start (Local 3-Node Cluster)](#quick-start-local-3-node-cluster)
- [Production Deployment](#production-deployment)
- [Configuration Reference](#configuration-reference)
- [Cluster Management](#cluster-management)
- [Verification and Testing](#verification-and-testing)

---

## Quick Start (Local 3-Node Cluster)

The fastest way to get a cluster running on your local machine for testing.

### Step 1: Build the Server

```bash
# Build the release binary
cargo build --release --bin chronik-server

# Verify build
./target/release/chronik-server version
```

### Step 2: Use Test Configs

Chronik provides pre-configured test cluster setup:

```bash
# View the test cluster configs
ls -la tests/cluster/
# node1.toml
# node2.toml
# node3.toml
# start.sh  (starts all 3 nodes)
# stop.sh   (stops all 3 nodes)
```

Each config file defines a complete cluster with 3 nodes on different ports:
- **Node 1**: Kafka 9092, WAL 9291, Raft 5001
- **Node 2**: Kafka 9093, WAL 9292, Raft 5002
- **Node 3**: Kafka 9094, WAL 9293, Raft 5003

### Step 3: Start the Cluster

**Recommended (uses start script):**
```bash
./tests/cluster/start.sh
# Starts all 3 nodes, cleans data directories
# Logs available at tests/cluster/logs/node*.log
```

**Or manually** (open 3 terminal windows):

**Terminal 1 - Node 1:**
```bash
./target/release/chronik-server start --config tests/cluster/node1.toml
```

**Terminal 2 - Node 2:**
```bash
./target/release/chronik-server start --config tests/cluster/node2.toml
```

**Terminal 3 - Node 3:**
```bash
./target/release/chronik-server start --config tests/cluster/node3.toml
```

**To stop:**
```bash
./tests/cluster/stop.sh
```

### Step 4: Verify Cluster

```bash
# Set admin API key (check node 1's startup logs for the key)
export CHRONIK_ADMIN_API_KEY=<key-from-logs>

# Check cluster status
./target/release/chronik-server cluster status --config tests/cluster/node1.toml
```

**Expected output:**
```
Chronik Cluster Status
======================

Nodes: 3
Leader: Node 1 (elected by Raft)

Node Details:
  Node 1: kafka=localhost:9092, wal=localhost:9291, raft=localhost:5001
  Node 2: kafka=localhost:9093, wal=localhost:9292, raft=localhost:5002
  Node 3: kafka=localhost:9094, wal=localhost:9293, raft=localhost:5003
```

### Step 5: Test with Kafka Clients

```bash
# Create a topic (connect to any node)
kafka-topics --create \
  --topic test-topic \
  --bootstrap-server localhost:9092 \
  --partitions 3 \
  --replication-factor 3

# Produce messages to node 1
echo "Hello from cluster!" | kafka-console-producer \
  --topic test-topic \
  --bootstrap-server localhost:9092

# Consume from node 2 (should work - data is replicated)
kafka-console-consumer \
  --topic test-topic \
  --from-beginning \
  --bootstrap-server localhost:9093
```

---

## Production Deployment

For production, run each node on a separate machine.

### Architecture

```
┌─────────────────────────────────────────────────────────────┐
│               Chronik Production Cluster                     │
├─────────────────────────────────────────────────────────────┤
│                                                              │
│  Node 1 (host1.example.com)                                 │
│    ├─ Kafka: 0.0.0.0:9092 → advertise host1.example.com    │
│    ├─ WAL:   0.0.0.0:9291 → advertise host1.example.com    │
│    └─ Raft:  0.0.0.0:5001 → advertise host1.example.com    │
│                                                              │
│  Node 2 (host2.example.com)                                 │
│    ├─ Kafka: 0.0.0.0:9092 → advertise host2.example.com    │
│    ├─ WAL:   0.0.0.0:9291 → advertise host2.example.com    │
│    └─ Raft:  0.0.0.0:5001 → advertise host2.example.com    │
│                                                              │
│  Node 3 (host3.example.com)                                 │
│    ├─ Kafka: 0.0.0.0:9092 → advertise host3.example.com    │
│    ├─ WAL:   0.0.0.0:9291 → advertise host3.example.com    │
│    └─ Raft:  0.0.0.0:5001 → advertise host3.example.com    │
│                                                              │
│  Clients connect to: host1,host2,host3:9092                 │
│  Replication: 3 copies, quorum 2/3 required                 │
└─────────────────────────────────────────────────────────────┘
```

### Step 1: Create Config Files

Create a config file for each node. Example for **Node 1** (`node1.toml`):

```toml
enabled = true
node_id = 1
replication_factor = 3
min_insync_replicas = 2

# Define all peers (including self)
[[peers]]
id = 1
kafka = "host1.example.com:9092"
wal = "host1.example.com:9291"
raft = "host1.example.com:5001"

[[peers]]
id = 2
kafka = "host2.example.com:9092"
wal = "host2.example.com:9291"
raft = "host2.example.com:5001"

[[peers]]
id = 3
kafka = "host3.example.com:9092"
wal = "host3.example.com:9291"
raft = "host3.example.com:5001"
```

**Node 2** (`node2.toml`): Same as above, but change `node_id = 2`

**Node 3** (`node3.toml`): Same as above, but change `node_id = 3`

**Key Points:**
- `node_id` must be unique for each node
- All nodes have **identical** `[[peers]]` sections
- `kafka` = where Kafka clients connect
- `wal` = where WAL replication happens (internal)
- `raft` = where Raft consensus happens (internal)

### Step 2: Network Configuration

**Firewall Rules:**
- Allow TCP `9092` (Kafka) from client networks
- Allow TCP `9291` (WAL) from other cluster nodes only
- Allow TCP `5001` (Raft) from other cluster nodes only

**DNS/Hosts:**
- Ensure all nodes can resolve each other by hostname
- Use static IPs or reliable DNS entries

### Step 3: Start Nodes

**On host1.example.com:**
```bash
./target/release/chronik-server start --config /etc/chronik/node1.toml
```

**On host2.example.com:**
```bash
./target/release/chronik-server start --config /etc/chronik/node2.toml
```

**On host3.example.com:**
```bash
./target/release/chronik-server start --config /etc/chronik/node3.toml
```

### Step 4: Systemd Service (Recommended)

Create `/etc/systemd/system/chronik.service` on each node:

```ini
[Unit]
Description=Chronik Stream Server
After=network.target

[Service]
Type=simple
User=chronik
Group=chronik
WorkingDirectory=/opt/chronik
ExecStart=/opt/chronik/bin/chronik-server start --config /etc/chronik/node1.toml
Restart=on-failure
RestartSec=10
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
```

**Enable and start:**
```bash
sudo systemctl enable chronik
sudo systemctl start chronik
sudo systemctl status chronik

# View logs
journalctl -u chronik -f
```

---

## Configuration Reference

### Complete Config File Format

```toml
# Enable cluster mode
enabled = true

# Unique node ID (1-255)
node_id = 1

# Default replication factor for new topics
replication_factor = 3

# Minimum in-sync replicas for acks=all
min_insync_replicas = 2

# Peer definitions (all nodes in cluster)
[[peers]]
id = 1                                  # Unique peer ID
kafka = "host1.example.com:9092"       # Kafka endpoint (clients connect here)
wal = "host1.example.com:9291"         # WAL replication endpoint (internal)
raft = "host1.example.com:5001"        # Raft consensus endpoint (internal)

[[peers]]
id = 2
kafka = "host2.example.com:9092"
wal = "host2.example.com:9291"
raft = "host2.example.com:5001"

[[peers]]
id = 3
kafka = "host3.example.com:9092"
wal = "host3.example.com:9291"
raft = "host3.example.com:5001"
```

### Environment Variables

Optional environment variables for additional configuration:

```bash
# Data directory (default: ./data)
CHRONIK_DATA_DIR=/var/lib/chronik

# Admin API key (REQUIRED for production)
CHRONIK_ADMIN_API_KEY=your-secure-key-here

# Log level
RUST_LOG=info

# Object storage (for disaster recovery)
OBJECT_STORE_BACKEND=s3
S3_BUCKET=chronik-prod-backups
S3_REGION=us-west-2
```

### Port Reference

| Port | Purpose | Clients |
|------|---------|---------|
| `9092` | Kafka protocol | External (producers, consumers) |
| `9291` | WAL replication | Internal only (cluster nodes) |
| `5001` | Raft consensus | Internal only (cluster nodes) |
| `10000+node_id` | Admin HTTP API | Internal (cluster management) |

**Example for Node 1:**
- Kafka: 9092 (external)
- WAL: 9291 (internal)
- Raft: 5001 (internal)
- Admin API: 10001 (internal)

---

## Cluster Management

### Add a Node (Zero-Downtime)

**Note:** Zero-downtime node addition is available in v2.2.0+.

Add a 4th node to a running 3-node cluster:

```bash
# Step 1: Add node to cluster membership (run on any existing node)
export CHRONIK_ADMIN_API_KEY=<your-key>

./target/release/chronik-server cluster add-node 4 \
  --kafka host4.example.com:9092 \
  --wal host4.example.com:9291 \
  --raft host4.example.com:5001 \
  --config /etc/chronik/node1.toml

# Step 2: Update node4's config file to include all 4 peers
# Step 3: Start node 4
./target/release/chronik-server start --config /etc/chronik/node4.toml

# Step 4: Verify
./target/release/chronik-server cluster status --config /etc/chronik/node1.toml
```

### Remove a Node (Zero-Downtime)

Remove a node gracefully:

```bash
# Graceful removal (reassigns partitions first)
export CHRONIK_ADMIN_API_KEY=<your-key>

./target/release/chronik-server cluster remove-node 4 \
  --config /etc/chronik/node1.toml

# What happens:
# 1. Partitions on node 4 are reassigned to other nodes
# 2. Wait for new replicas to catch up
# 3. Node 4 is removed from Raft cluster
# 4. Safe to shut down node 4

# Force removal (for dead/crashed nodes)
./target/release/chronik-server cluster remove-node 4 \
  --force \
  --config /etc/chronik/node1.toml
```

### Query Cluster Status

```bash
export CHRONIK_ADMIN_API_KEY=<your-key>

./target/release/chronik-server cluster status \
  --config /etc/chronik/node1.toml
```

**Example output:**
```
Chronik Cluster Status
======================

Nodes: 3
Leader: Node 1 (elected by Raft)

Node Details:
  Node 1: kafka=host1.example.com:9092, wal=host1.example.com:9291, raft=host1.example.com:5001
  Node 2: kafka=host2.example.com:9092, wal=host2.example.com:9291, raft=host2.example.com:5001
  Node 3: kafka=host3.example.com:9092, wal=host3.example.com:9291, raft=host3.example.com:5001

Partition Assignments:
  my-topic-0: leader=1, replicas=[1, 2, 3], ISR=[1, 2, 3]
  my-topic-1: leader=2, replicas=[2, 3, 1], ISR=[2, 3, 1]
  my-topic-2: leader=3, replicas=[3, 1, 2], ISR=[3, 1, 2]
```

### HTTP Admin API

All cluster commands also work via HTTP:

```bash
# Get API key from server logs
grep "Admin API key" /var/log/chronik/server.log

# Query status
curl http://host1.example.com:10001/admin/status \
  -H "X-API-Key: <api-key>"

# Add node
curl -X POST http://host1.example.com:10001/admin/add-node \
  -H "X-API-Key: <api-key>" \
  -H "Content-Type: application/json" \
  -d '{
    "node_id": 4,
    "kafka_addr": "host4.example.com:9092",
    "wal_addr": "host4.example.com:9291",
    "raft_addr": "host4.example.com:5001"
  }'

# Remove node
curl -X POST http://host1.example.com:10001/admin/remove-node \
  -H "X-API-Key: <api-key>" \
  -H "Content-Type: application/json" \
  -d '{"node_id": 4, "force": false}'
```

**Admin API runs on:** `10000 + node_id`
- Node 1 → port 10001
- Node 2 → port 10002
- Node 3 → port 10003

---

## Verification and Testing

### Create Replicated Topic

```bash
kafka-topics --create \
  --topic orders \
  --bootstrap-server host1.example.com:9092,host2.example.com:9092,host3.example.com:9092 \
  --partitions 3 \
  --replication-factor 3

# Verify replication
kafka-topics --describe \
  --topic orders \
  --bootstrap-server host1.example.com:9092
```

**Expected output:**
```
Topic: orders  PartitionCount: 3  ReplicationFactor: 3
  Partition: 0  Leader: 1  Replicas: 1,2,3  Isr: 1,2,3
  Partition: 1  Leader: 2  Replicas: 2,3,1  Isr: 2,3,1
  Partition: 2  Leader: 3  Replicas: 3,1,2  Isr: 3,1,2
```

### Test Fault Tolerance

**Scenario: Kill a follower node**

```bash
# On host2 (assuming it's a follower)
sudo systemctl stop chronik

# Produce should still work (quorum 2/3 still alive)
echo "Still working!" | kafka-console-producer \
  --topic orders \
  --bootstrap-server host1.example.com:9092

# Consume should still work
kafka-console-consumer \
  --topic orders \
  --from-beginning \
  --bootstrap-server host3.example.com:9092

# Restart node2
sudo systemctl start chronik

# Node2 will rejoin and catch up automatically
```

### Health Check Script

```bash
#!/bin/bash
# health_check.sh - Verify cluster health

NODES=("host1.example.com:9092" "host2.example.com:9092" "host3.example.com:9092")

echo "Checking Chronik cluster health..."

for node in "${NODES[@]}"; do
  # Try to connect with Kafka protocol
  if kafka-broker-api-versions --bootstrap-server $node > /dev/null 2>&1; then
    echo "✅ $node - REACHABLE"
  else
    echo "❌ $node - UNREACHABLE"
  fi
done

# Check topic replication
echo ""
echo "Checking topic replication..."
kafka-topics --describe --bootstrap-server ${NODES[0]} | grep -E "(Topic:|Isr:)"
```

### Performance Test

```bash
# Produce 100K messages
kafka-producer-perf-test \
  --topic perf-test \
  --num-records 100000 \
  --record-size 1024 \
  --throughput -1 \
  --producer-props \
    bootstrap.servers=host1.example.com:9092,host2.example.com:9092,host3.example.com:9092 \
    acks=all

# Consume and measure
kafka-consumer-perf-test \
  --topic perf-test \
  --bootstrap-server host1.example.com:9092 \
  --messages 100000 \
  --threads 1
```

---

## Troubleshooting

### Cluster Won't Form Quorum

**Symptoms:**
- Nodes start but remain in "waiting for quorum" state
- No leader elected

**Solutions:**
1. Check all nodes can reach each other:
   ```bash
   # From node1, test connectivity to node2
   telnet host2.example.com 5001  # Raft port
   telnet host2.example.com 9291  # WAL port
   ```

2. Verify config files are identical (except `node_id`):
   ```bash
   # Compare peer sections
   grep -A 20 "\[\[peers\]\]" node1.toml
   grep -A 20 "\[\[peers\]\]" node2.toml
   ```

3. Check logs for errors:
   ```bash
   journalctl -u chronik -f
   # Look for: "Failed to connect to peer", "Raft election timeout"
   ```

### Partitions Under-Replicated

**Symptoms:**
- ISR < replication factor
- `kafka-topics --describe` shows fewer ISR members

**Solutions:**
1. Check lagging node:
   ```bash
   # View replication lag
   ./target/release/chronik-server cluster status --config node1.toml
   ```

2. Restart lagging node:
   ```bash
   sudo systemctl restart chronik
   ```

3. If persistent, check network latency between nodes

### Admin API Returns 401 Unauthorized

**Solution:**
```bash
# Set API key from server logs
export CHRONIK_ADMIN_API_KEY=$(grep "Admin API key" /var/log/chronik/server.log | awk '{print $NF}')

# Or set persistent key in environment
echo 'export CHRONIK_ADMIN_API_KEY=your-secure-key' >> ~/.bashrc
```

---

## Next Steps

- [ADMIN_API_SECURITY.md](ADMIN_API_SECURITY.md) - Secure the admin API with TLS
- [DISASTER_RECOVERY.md](DISASTER_RECOVERY.md) - Configure S3 backups for metadata and data
- [TESTING_NODE_REMOVAL.md](TESTING_NODE_REMOVAL.md) - Test node removal scenarios
