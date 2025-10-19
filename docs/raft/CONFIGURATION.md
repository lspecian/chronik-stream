# Chronik Raft Clustering Configuration Guide

This document provides comprehensive guidance on configuring Chronik's Raft clustering for different deployment scenarios.

## Table of Contents

- [Configuration Overview](#configuration-overview)
- [Configuration File Reference](#configuration-file-reference)
- [Environment Variables](#environment-variables)
- [Configuration Profiles](#configuration-profiles)
- [Common Scenarios](#common-scenarios)
- [Advanced Configuration](#advanced-configuration)
- [Security Configuration](#security-configuration)

## Configuration Overview

Chronik clustering configuration is organized into sections:

1. **[cluster]**: Cluster-wide settings (node ID, discovery, etc.)
2. **[cluster.raft]**: Raft consensus algorithm parameters
3. **[cluster.network]**: gRPC network communication settings
4. **[cluster.storage]**: WAL and snapshot storage configuration

### Configuration Methods

Chronik supports three configuration methods (in order of precedence):

1. **Environment variables** (highest priority)
2. **Configuration file** (`chronik.toml` or path via `CHRONIK_CONFIG`)
3. **Default values** (lowest priority)

Example precedence:
```bash
# Environment variable overrides config file
export CHRONIK_NODE_ID=2
# Uses NODE_ID=2 even if chronik.toml says node_id=1
cargo run --bin chronik-server -- cluster
```

## Configuration File Reference

### Complete `chronik.toml` Example

```toml
[cluster]
# Unique node ID (REQUIRED for clustering)
# Range: 1-65535
# Must be unique within cluster
node_id = 1

# Node address for cluster communication (gRPC)
# Format: "host:port"
# Default: "0.0.0.0:9093"
node_addr = "0.0.0.0:9093"

# Advertised address for peers to connect
# CRITICAL: Set this to externally-accessible hostname/IP
# If not set, uses node_addr (may not work across networks)
advertised_node_addr = "node1.example.com:9093"

# Cluster discovery mode
# Options: "static", "dns", "consul", "etcd"
# Default: "static"
discovery_mode = "static"

# Seed nodes for cluster bootstrap (static discovery)
# Format: ["node_id@host:port", ...]
# Leave empty for first node in cluster
seed_nodes = [
    "2@node2.example.com:9093",
    "3@node3.example.com:9093"
]

# Enable clustering (default: false for standalone mode)
enabled = true

[cluster.raft]
# Election timeout (milliseconds)
# If no heartbeat from leader within this time, trigger election
# Recommended: 150-300ms for low-latency networks
# Recommended: 300-500ms for high-latency/WAN scenarios
# Default: 200
election_timeout_ms = 200

# Heartbeat interval (milliseconds)
# Leader sends heartbeats to followers at this interval
# MUST be less than election_timeout_ms (typically 1/10)
# Default: 20
heartbeat_interval_ms = 20

# Maximum entries per AppendEntries RPC
# Higher = more throughput, more memory
# Lower = lower latency, less memory
# Default: 1000
max_payload_entries = 1000

# Snapshot log size threshold (entries)
# Create snapshot when log exceeds this size
# Default: 100000
snapshot_log_size_threshold = 100000

# Snapshot policy
# Options: "periodic", "log_size", "disabled"
# Default: "log_size"
snapshot_policy = "log_size"

# Periodic snapshot interval (seconds, only if snapshot_policy = "periodic")
# Default: 3600 (1 hour)
snapshot_interval_secs = 3600

# Log compaction threshold (entries)
# Compact WAL when log exceeds this size
# Default: 50000
log_compaction_threshold = 50000

# Enable log compaction
# Default: true
log_compaction_enabled = true

# Install snapshot timeout (seconds)
# Max time to install snapshot from leader
# Default: 300 (5 minutes)
install_snapshot_timeout_secs = 300

# Maximum snapshot chunk size (bytes)
# Snapshots are streamed in chunks of this size
# Default: 1048576 (1MB)
max_snapshot_chunk_size = 1048576

# Enable pipeline replication
# Leader sends AppendEntries without waiting for ACK
# Improves throughput at cost of memory
# Default: true
enable_pipeline = true

# Pipeline max in-flight requests
# Only used if enable_pipeline = true
# Default: 1000
max_in_flight = 1000

[cluster.network]
# gRPC server settings

# Max message size (bytes)
# Must be large enough for largest snapshot chunk
# Default: 4194304 (4MB)
max_message_size = 4194304

# Connection timeout (milliseconds)
# Default: 5000 (5 seconds)
connect_timeout_ms = 5000

# Request timeout (milliseconds)
# Default: 10000 (10 seconds)
request_timeout_ms = 10000

# Keep-alive interval (seconds)
# Send TCP keep-alive probes at this interval
# Default: 10
keepalive_interval_secs = 10

# Keep-alive timeout (seconds)
# Close connection if no response within this time
# Default: 30
keepalive_timeout_secs = 30

# Enable TLS for cluster communication
# Default: false
tls_enabled = false

# TLS certificate file (PEM format)
# Required if tls_enabled = true
tls_cert_path = "/etc/chronik/certs/server.crt"

# TLS private key file (PEM format)
# Required if tls_enabled = true
tls_key_path = "/etc/chronik/certs/server.key"

# TLS CA certificate for peer verification (PEM format)
# Optional, enables mutual TLS
tls_ca_path = "/etc/chronik/certs/ca.crt"

[cluster.storage]
# WAL directory for Raft logs
# Default: "./data/wal"
wal_dir = "/var/lib/chronik/wal"

# Snapshot directory
# Default: "./data/snapshots"
snapshot_dir = "/var/lib/chronik/snapshots"

# WAL sync mode
# Options: "always" (fsync on every write), "periodic" (group commit)
# Default: "periodic"
wal_sync_mode = "periodic"

# WAL sync interval (milliseconds, only if wal_sync_mode = "periodic")
# Default: 10
wal_sync_interval_ms = 10

# WAL segment size (bytes)
# Rotate WAL segment when it reaches this size
# Default: 268435456 (256MB)
wal_segment_size = 268435456

# Snapshot compression
# Options: "none", "gzip", "zstd"
# Default: "zstd"
snapshot_compression = "zstd"

# Snapshot compression level (1-9 for gzip, 1-22 for zstd)
# Higher = better compression, slower
# Default: 3
snapshot_compression_level = 3
```

## Environment Variables

All configuration options can be set via environment variables using the prefix `CHRONIK_` and uppercase snake_case:

| Config Key | Environment Variable | Example |
|------------|---------------------|---------|
| `cluster.node_id` | `CHRONIK_NODE_ID` | `CHRONIK_NODE_ID=1` |
| `cluster.node_addr` | `CHRONIK_NODE_ADDR` | `CHRONIK_NODE_ADDR=0.0.0.0:9093` |
| `cluster.advertised_node_addr` | `CHRONIK_ADVERTISED_NODE_ADDR` | `CHRONIK_ADVERTISED_NODE_ADDR=node1:9093` |
| `cluster.discovery_mode` | `CHRONIK_DISCOVERY_MODE` | `CHRONIK_DISCOVERY_MODE=static` |
| `cluster.seed_nodes` | `CHRONIK_SEED_NODES` | `CHRONIK_SEED_NODES=2@node2:9093,3@node3:9093` |
| `cluster.raft.election_timeout_ms` | `CHRONIK_ELECTION_TIMEOUT_MS` | `CHRONIK_ELECTION_TIMEOUT_MS=200` |
| `cluster.raft.heartbeat_interval_ms` | `CHRONIK_HEARTBEAT_INTERVAL_MS` | `CHRONIK_HEARTBEAT_INTERVAL_MS=20` |
| `cluster.network.tls_enabled` | `CHRONIK_TLS_ENABLED` | `CHRONIK_TLS_ENABLED=true` |

**Complete Example**:
```bash
export CHRONIK_NODE_ID=1
export CHRONIK_ADVERTISED_NODE_ADDR=node1.example.com:9093
export CHRONIK_SEED_NODES="2@node2.example.com:9093,3@node3.example.com:9093"
export CHRONIK_ELECTION_TIMEOUT_MS=200
export CHRONIK_HEARTBEAT_INTERVAL_MS=20

cargo run --bin chronik-server -- cluster
```

## Configuration Profiles

### Development Profile

**Use case**: Local testing, quick iteration

```toml
[cluster]
node_id = 1
node_addr = "127.0.0.1:9093"
advertised_node_addr = "127.0.0.1:9093"
discovery_mode = "static"
seed_nodes = []  # Single node
enabled = true

[cluster.raft]
election_timeout_ms = 150  # Fast elections
heartbeat_interval_ms = 15
max_payload_entries = 500  # Small batches for debugging
snapshot_log_size_threshold = 1000  # Frequent snapshots
log_compaction_threshold = 500
enable_pipeline = false  # Easier debugging

[cluster.network]
connect_timeout_ms = 1000
request_timeout_ms = 5000
tls_enabled = false

[cluster.storage]
wal_dir = "./data/dev/wal"
snapshot_dir = "./data/dev/snapshots"
wal_sync_mode = "periodic"
wal_sync_interval_ms = 10
snapshot_compression = "none"  # Faster for debugging
```

**Run**:
```bash
cargo run --bin chronik-server -- cluster
```

### Production Profile

**Use case**: Production deployment, high availability

```toml
[cluster]
node_id = 1  # Set per node
node_addr = "0.0.0.0:9093"
advertised_node_addr = "node1.prod.example.com:9093"
discovery_mode = "static"
seed_nodes = [
    "2@node2.prod.example.com:9093",
    "3@node3.prod.example.com:9093"
]
enabled = true

[cluster.raft]
election_timeout_ms = 300  # Conservative for network variance
heartbeat_interval_ms = 30
max_payload_entries = 1000
snapshot_log_size_threshold = 100000
log_compaction_threshold = 50000
enable_pipeline = true
max_in_flight = 1000

[cluster.network]
max_message_size = 4194304  # 4MB
connect_timeout_ms = 5000
request_timeout_ms = 10000
keepalive_interval_secs = 10
keepalive_timeout_secs = 30
tls_enabled = true
tls_cert_path = "/etc/chronik/certs/server.crt"
tls_key_path = "/etc/chronik/certs/server.key"
tls_ca_path = "/etc/chronik/certs/ca.crt"

[cluster.storage]
wal_dir = "/var/lib/chronik/wal"
snapshot_dir = "/var/lib/chronik/snapshots"
wal_sync_mode = "periodic"
wal_sync_interval_ms = 10
wal_segment_size = 268435456  # 256MB
snapshot_compression = "zstd"
snapshot_compression_level = 3
```

**Run**:
```bash
# Node 1
CHRONIK_NODE_ID=1 \
CHRONIK_ADVERTISED_NODE_ADDR=node1.prod.example.com:9093 \
chronik-server cluster

# Node 2
CHRONIK_NODE_ID=2 \
CHRONIK_ADVERTISED_NODE_ADDR=node2.prod.example.com:9093 \
chronik-server cluster

# Node 3
CHRONIK_NODE_ID=3 \
CHRONIK_ADVERTISED_NODE_ADDR=node3.prod.example.com:9093 \
chronik-server cluster
```

### High-Throughput Profile

**Use case**: Bulk ingestion, data pipelines, high write volume

```toml
[cluster]
node_id = 1
node_addr = "0.0.0.0:9093"
advertised_node_addr = "node1.example.com:9093"
discovery_mode = "static"
seed_nodes = ["2@node2:9093", "3@node3:9093"]
enabled = true

[cluster.raft]
election_timeout_ms = 300
heartbeat_interval_ms = 30
max_payload_entries = 5000  # Large batches
snapshot_log_size_threshold = 500000  # Less frequent snapshots
log_compaction_threshold = 250000
enable_pipeline = true
max_in_flight = 5000  # High concurrency

[cluster.network]
max_message_size = 8388608  # 8MB
connect_timeout_ms = 10000
request_timeout_ms = 30000

[cluster.storage]
wal_sync_mode = "periodic"
wal_sync_interval_ms = 50  # Less frequent sync for throughput
wal_segment_size = 536870912  # 512MB segments
snapshot_compression = "zstd"
snapshot_compression_level = 1  # Fast compression
```

**Tuning**:
```bash
# Also adjust WAL profile for maximum throughput
export CHRONIK_WAL_PROFILE=ultra
export CHRONIK_PRODUCE_PROFILE=high-throughput

chronik-server cluster
```

### Multi-Datacenter Profile

**Use case**: Cross-datacenter replication, WAN scenarios

```toml
[cluster]
node_id = 1
node_addr = "0.0.0.0:9093"
advertised_node_addr = "node1.dc1.example.com:9093"
discovery_mode = "static"
seed_nodes = [
    "2@node2.dc2.example.com:9093",
    "3@node3.dc3.example.com:9093"
]
enabled = true

[cluster.raft]
election_timeout_ms = 1000  # Higher for WAN latency
heartbeat_interval_ms = 100
max_payload_entries = 2000
snapshot_log_size_threshold = 100000
enable_pipeline = true
max_in_flight = 500  # Lower for WAN

[cluster.network]
connect_timeout_ms = 10000
request_timeout_ms = 30000
keepalive_interval_secs = 30  # More tolerant of network issues
keepalive_timeout_secs = 90
tls_enabled = true
tls_cert_path = "/etc/chronik/certs/server.crt"
tls_key_path = "/etc/chronik/certs/server.key"
tls_ca_path = "/etc/chronik/certs/ca.crt"

[cluster.storage]
wal_sync_mode = "periodic"
wal_sync_interval_ms = 20
snapshot_compression = "zstd"
snapshot_compression_level = 6  # Higher compression for network transfer
```

## Common Scenarios

### Scenario 1: Bootstrap New Cluster

**Step 1**: Start first node (bootstrap)
```bash
# Node 1 (bootstrap)
cat > node1.toml <<EOF
[cluster]
node_id = 1
node_addr = "0.0.0.0:9093"
advertised_node_addr = "node1:9093"
enabled = true
discovery_mode = "static"
seed_nodes = []  # Empty for bootstrap
EOF

CHRONIK_CONFIG=node1.toml chronik-server cluster
```

**Step 2**: Start remaining nodes
```bash
# Node 2
cat > node2.toml <<EOF
[cluster]
node_id = 2
node_addr = "0.0.0.0:9093"
advertised_node_addr = "node2:9093"
enabled = true
discovery_mode = "static"
seed_nodes = ["1@node1:9093"]
EOF

CHRONIK_CONFIG=node2.toml chronik-server cluster

# Node 3
cat > node3.toml <<EOF
[cluster]
node_id = 3
node_addr = "0.0.0.0:9093"
advertised_node_addr = "node3:9093"
enabled = true
discovery_mode = "static"
seed_nodes = ["1@node1:9093", "2@node2:9093"]
EOF

CHRONIK_CONFIG=node3.toml chronik-server cluster
```

**Step 3**: Verify cluster
```bash
# Check cluster status
curl http://node1:8080/metrics | grep chronik_cluster_nodes
```

### Scenario 2: Add Node to Existing Cluster

**Step 1**: Configure new node
```bash
# Node 4 (new)
cat > node4.toml <<EOF
[cluster]
node_id = 4
node_addr = "0.0.0.0:9093"
advertised_node_addr = "node4:9093"
enabled = true
discovery_mode = "static"
seed_nodes = ["1@node1:9093", "2@node2:9093", "3@node3:9093"]
EOF

CHRONIK_CONFIG=node4.toml chronik-server cluster
```

**Step 2**: Node automatically joins via seed nodes

**Step 3**: Verify membership
```bash
# Check node4 joined
curl http://node1:8080/metrics | grep chronik_cluster_nodes
# Should show: chronik_cluster_nodes 4
```

### Scenario 3: Replace Failed Node

**Step 1**: Provision replacement node with **same node_id**
```bash
# Original node 2 failed, provision node2-replacement
cat > node2-replacement.toml <<EOF
[cluster]
node_id = 2  # SAME ID as failed node
node_addr = "0.0.0.0:9093"
advertised_node_addr = "node2-replacement:9093"
enabled = true
discovery_mode = "static"
seed_nodes = ["1@node1:9093", "3@node3:9093"]
EOF

CHRONIK_CONFIG=node2-replacement.toml chronik-server cluster
```

**Step 2**: Node recovers partitions from Raft logs/snapshots

**Step 3**: Update DNS/load balancer to point to new node

### Scenario 4: Upgrade Configuration Without Downtime

**Goal**: Change `election_timeout_ms` from 200 to 300

**Step 1**: Rolling update (one node at a time)
```bash
# Node 1
# 1. Update chronik.toml: election_timeout_ms = 300
# 2. Restart node 1
systemctl restart chronik

# Wait for node 1 to rejoin cluster (30 seconds)

# Node 2
# 1. Update chronik.toml: election_timeout_ms = 300
# 2. Restart node 2
systemctl restart chronik

# Wait...

# Node 3
# 1. Update chronik.toml: election_timeout_ms = 300
# 2. Restart node 3
systemctl restart chronik
```

**Step 2**: Verify cluster health after each restart
```bash
curl http://node1:8080/health
```

## Advanced Configuration

### Dynamic Membership (Future)

**Note**: Static discovery is supported in initial release. Dynamic discovery (DNS/Consul/etcd) will be added in future versions.

**DNS Discovery** (planned):
```toml
[cluster]
discovery_mode = "dns"
dns_domain = "chronik.service.consul"
dns_port = 9093
```

**Consul Discovery** (planned):
```toml
[cluster]
discovery_mode = "consul"
consul_addr = "http://consul.example.com:8500"
consul_service = "chronik-cluster"
```

### Custom Snapshot Policy

**Periodic snapshots every hour**:
```toml
[cluster.raft]
snapshot_policy = "periodic"
snapshot_interval_secs = 3600
```

**Log-size-based snapshots** (default):
```toml
[cluster.raft]
snapshot_policy = "log_size"
snapshot_log_size_threshold = 100000
```

**Disable automatic snapshots** (manual only):
```toml
[cluster.raft]
snapshot_policy = "disabled"
```

Trigger manual snapshot:
```bash
curl -X POST http://node1:8080/admin/snapshot
```

### Custom Log Compaction Strategy

**Disable automatic compaction**:
```toml
[cluster.raft]
log_compaction_enabled = false
```

**Manual compaction via API**:
```bash
curl -X POST http://node1:8080/admin/compact
```

### Multi-Region Configuration

**3 regions, 3 nodes each (9 total)**:

```toml
# Nodes in each region have local seed nodes first (latency optimization)

# us-east-1a (nodes 1-3)
seed_nodes = [
    "2@node2.us-east-1a:9093",
    "3@node3.us-east-1a:9093",
    "4@node4.us-west-2a:9093",  # Cross-region
]

# us-west-2a (nodes 4-6)
seed_nodes = [
    "5@node5.us-west-2a:9093",
    "6@node6.us-west-2a:9093",
    "1@node1.us-east-1a:9093",  # Cross-region
]

# eu-central-1a (nodes 7-9)
seed_nodes = [
    "8@node8.eu-central-1a:9093",
    "9@node9.eu-central-1a:9093",
    "1@node1.us-east-1a:9093",  # Cross-region
]
```

**Partition placement strategy**: Prefer local replicas
```bash
# Configure replication factor = 3 with rack awareness
# Chronik will distribute replicas across regions
```

## Security Configuration

### TLS/SSL Configuration

**Generate self-signed certificates** (development only):
```bash
# CA certificate
openssl req -x509 -newkey rsa:4096 -keyout ca.key -out ca.crt -days 365 -nodes

# Server certificate
openssl req -newkey rsa:4096 -keyout server.key -out server.csr -nodes
openssl x509 -req -in server.csr -CA ca.crt -CAkey ca.key -CAcreateserial -out server.crt -days 365
```

**Configure Chronik**:
```toml
[cluster.network]
tls_enabled = true
tls_cert_path = "/etc/chronik/certs/server.crt"
tls_key_path = "/etc/chronik/certs/server.key"
tls_ca_path = "/etc/chronik/certs/ca.crt"
```

**Production**: Use proper CA-signed certificates (Let's Encrypt, internal PKI)

### Mutual TLS (mTLS)

**Enable peer authentication**:
```toml
[cluster.network]
tls_enabled = true
tls_cert_path = "/etc/chronik/certs/server.crt"
tls_key_path = "/etc/chronik/certs/server.key"
tls_ca_path = "/etc/chronik/certs/ca.crt"
verify_peer = true  # Require client certificates
```

Each node must present valid certificate signed by CA.

### Network Isolation

**Bind cluster port to private network only**:
```toml
[cluster]
node_addr = "10.0.1.10:9093"  # Private IP
advertised_node_addr = "10.0.1.10:9093"
```

**Firewall rules**:
```bash
# Allow cluster traffic only from known nodes
iptables -A INPUT -p tcp --dport 9093 -s 10.0.1.0/24 -j ACCEPT
iptables -A INPUT -p tcp --dport 9093 -j DROP
```

## Configuration Validation

**Validate configuration before starting**:
```bash
chronik-server validate-config --config chronik.toml
```

**Common validation errors**:
- `heartbeat_interval_ms >= election_timeout_ms` (must be less)
- `node_id = 0` (must be 1-65535)
- Missing required fields (node_id, node_addr)
- Invalid seed_nodes format

## References

- **Architecture Guide**: [docs/raft/ARCHITECTURE.md](./ARCHITECTURE.md)
- **Troubleshooting Guide**: [docs/raft/TROUBLESHOOTING.md](./TROUBLESHOOTING.md)
- **Migration Guide**: [docs/raft/MIGRATION_v1_v2.md](./MIGRATION_v1_v2.md)
- **OpenRaft Documentation**: https://docs.rs/openraft/
