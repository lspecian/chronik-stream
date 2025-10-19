# Raft Configuration Reference

Complete reference for tuning Chronik's Raft clustering configuration.

## Table of Contents

- [Raft Timing Configuration](#raft-timing-configuration)
- [Snapshot Configuration](#snapshot-configuration)
- [ISR Configuration](#isr-configuration)
- [Network Tuning](#network-tuning)
- [Performance Tuning](#performance-tuning)
- [Default Values Summary](#default-values-summary)
- [Tuning Recommendations](#tuning-recommendations)

---

## Raft Timing Configuration

Raft timing parameters control leader election and heartbeat behavior. These are critical for cluster stability.

### election_timeout_ms

**Description:** Time a follower waits before starting a new election if it hasn't received a heartbeat from the leader.

**Default:** `300` ms

**Range:** `150-500` ms (recommended), `50-5000` ms (supported)

**Configuration:**
```toml
[raft]
election_timeout_ms = 300
```

**Tuning guidelines:**
- **Lower values (150-200ms)**: Faster failover, but more prone to spurious elections on network hiccups
- **Default (300ms)**: Good balance for most deployments
- **Higher values (500-1000ms)**: More stable in high-latency or congested networks

**When to tune:**
- **Decrease** if you need sub-second failover and have reliable low-latency network
- **Increase** if you see frequent leader elections in logs (split-brain warnings)

**Example scenarios:**
```toml
# Low-latency datacenter (1-5ms RTT)
[raft]
election_timeout_ms = 200

# Cross-datacenter or WAN (50-100ms RTT)
[raft]
election_timeout_ms = 1000
```

---

### heartbeat_interval_ms

**Description:** How often the leader sends heartbeat messages to followers to maintain leadership.

**Default:** `30` ms

**Range:** `10-100` ms (recommended)

**Relationship to election timeout:** Should be **1/10th** of `election_timeout_ms`

**Configuration:**
```toml
[raft]
heartbeat_interval_ms = 30
```

**Tuning guidelines:**
- Must be significantly smaller than `election_timeout_ms` (recommended ratio: 10:1)
- Lower values = more network traffic but faster leader liveness detection
- Higher values = less overhead but slower failure detection

**Auto-calculation:**
If not specified, Chronik automatically sets `heartbeat_interval_ms = election_timeout_ms / 10`.

**Example:**
```toml
# For election_timeout_ms = 500
[raft]
election_timeout_ms = 500
heartbeat_interval_ms = 50  # 500 / 10
```

---

### max_entries_per_batch

**Description:** Maximum number of Raft log entries sent in a single AppendEntries RPC.

**Default:** `100`

**Range:** `10-1000`

**Configuration:**
```toml
[raft]
max_entries_per_batch = 100
```

**Tuning guidelines:**
- **Smaller batches (10-50)**: Lower latency per append, better for real-time workloads
- **Larger batches (200-500)**: Higher throughput, better for bulk ingestion
- Network MTU considerations: Each entry is ~1-10KB, batch should fit in ~10-20 MTU frames

**When to tune:**
- **Increase** for high-throughput workloads with large message volumes
- **Decrease** for low-latency workloads where every millisecond counts

**Example:**
```toml
# High-throughput bulk ingestion
[raft]
max_entries_per_batch = 500

# Low-latency real-time streaming
[raft]
max_entries_per_batch = 50
```

---

## Snapshot Configuration

Snapshots compact the Raft log to prevent unbounded growth and enable fast recovery.

### snapshot_threshold

**Description:** Create a snapshot after the Raft log reaches this many entries.

**Default:** `10000`

**Range:** `1000-100000`

**Configuration:**
```toml
[raft]
snapshot_threshold = 10000
```

**Tuning guidelines:**
- **Lower values (1000-5000)**: More frequent snapshots, smaller log size, faster recovery, higher overhead
- **Higher values (20000-50000)**: Less overhead, larger logs, slower recovery

**Disk usage impact:**
- Raft log size ≈ `snapshot_threshold * avg_entry_size`
- Snapshot size ≈ partition state size (usually < 100MB for metadata)

**When to tune:**
- **Decrease** if disk space is limited or you want faster recovery from snapshots
- **Increase** if snapshot creation is causing CPU/IO spikes

**Example:**
```toml
# Low-disk environment
[raft]
snapshot_threshold = 5000

# High-throughput environment
[raft]
snapshot_threshold = 50000
```

---

### snapshot_compression

**Description:** Compression algorithm for snapshots.

**Options:**
- `none` - No compression (fastest, largest size)
- `gzip` - Gzip compression (slower, better ratio)
- `zstd` - Zstandard compression (fast, good ratio, **recommended**)

**Default:** `gzip`

**Configuration:**
```toml
[raft.snapshot]
compression = "zstd"
```

**Compression ratios (approximate):**
- `none`: 1.0x (baseline)
- `gzip`: 3-5x smaller
- `zstd`: 3-4x smaller (faster than gzip)

**When to tune:**
- **Use `none`** if CPU is constrained and disk/network is cheap
- **Use `zstd`** for best balance (recommended for most deployments)
- **Use `gzip`** for maximum compression (slower)

---

### snapshot_retention_count

**Description:** Number of old snapshots to keep in object storage.

**Default:** `3`

**Range:** `1-10`

**Configuration:**
```toml
[raft.snapshot]
retention_count = 3
```

**Tuning guidelines:**
- Keep at least 2 snapshots for safety
- More snapshots = more storage cost, but better recovery options

**Storage cost:**
- S3 cost ≈ `retention_count * snapshot_size * $0.023/GB/month`
- Example: 3 snapshots × 100MB × $0.023/GB = $0.007/month (negligible)

---

## ISR Configuration

ISR (In-Sync Replica) tracking determines which replicas are caught up with the leader.

### max_lag_ms

**Description:** Maximum time lag (milliseconds) for a follower to be considered in-sync.

**Default:** `10000` (10 seconds)

**Range:** `1000-60000`

**Configuration:**
```toml
[isr]
max_lag_ms = 10000
```

**Tuning guidelines:**
- **Lower values (5000-10000)**: Stricter ISR requirements, faster ISR shrink
- **Higher values (20000-30000)**: More lenient, tolerates network hiccups

**When to tune:**
- **Decrease** for low-latency networks where replicas should always be caught up
- **Increase** for high-latency or unreliable networks (WAN, cross-DC)

**Example:**
```toml
# Same datacenter (low latency)
[isr]
max_lag_ms = 5000

# Cross-datacenter (high latency)
[isr]
max_lag_ms = 30000
```

---

### max_lag_entries

**Description:** Maximum number of log entries a follower can be behind the leader to be in-sync.

**Default:** `10000`

**Range:** `1000-100000`

**Configuration:**
```toml
[isr]
max_lag_entries = 10000
```

**Tuning guidelines:**
- Entry lag = `leader_commit_index - follower_applied_index`
- Must accommodate burst traffic: if you produce 1000 msg/s, follower needs to catch up within seconds

**Calculation:**
```
max_lag_entries = max_produce_rate (msg/s) × max_lag_ms / 1000
```

**Example:**
- Produce rate: 5000 msg/s
- Lag tolerance: 10s
- `max_lag_entries = 5000 × 10 = 50000`

**Configuration:**
```toml
[isr]
max_lag_entries = 50000
```

---

### min_insync_replicas

**Description:** Minimum number of in-sync replicas required for produce to succeed (including leader).

**Default:** `2`

**Range:** `1` to `replication_factor`

**Configuration:**
```toml
min_insync_replicas = 2
```

**Durability vs. Availability tradeoff:**

| Value | Durability | Availability | Use Case |
|-------|------------|--------------|----------|
| `1` | Lowest (leader-only) | Highest | Dev/test, can tolerate data loss |
| `2` | Medium (quorum) | Medium | Most production workloads |
| `3` | Highest (all replicas) | Lowest | Critical financial data |

**Example:**
```toml
# High availability (allow produce even if 1 replica down)
min_insync_replicas = 2

# Maximum durability (require all 3 replicas)
min_insync_replicas = 3
```

---

### check_interval_ms

**Description:** How often the ISR manager checks replica lag and updates ISR.

**Default:** `1000` (1 second)

**Range:** `100-10000`

**Configuration:**
```toml
[isr]
check_interval_ms = 1000
```

**Tuning guidelines:**
- **Lower values (500ms)**: Faster ISR updates, more CPU overhead
- **Higher values (5000ms)**: Less overhead, slower ISR shrink/expand

**When to tune:**
- **Decrease** if you need fast ISR shrink for critical workloads
- **Increase** for large clusters with many partitions (reduce overhead)

---

## Network Tuning

### rpc_timeout_ms

**Description:** Timeout for Raft RPC calls (AppendEntries, RequestVote, etc.).

**Default:** `5000` (5 seconds)

**Range:** `1000-30000`

**Configuration:**
```toml
[raft.network]
rpc_timeout_ms = 5000
```

**Tuning guidelines:**
- Must be > network RTT × 2
- Lower values = faster failure detection, but more spurious timeouts
- Higher values = more tolerance for slow networks, but slower failure detection

**Network-based recommendations:**
- **Same datacenter (1-5ms RTT)**: 2000-3000ms
- **Cross-datacenter (50-100ms RTT)**: 10000-15000ms
- **WAN (100-200ms RTT)**: 20000-30000ms

---

### max_inflight_messages

**Description:** Maximum number of concurrent Raft RPC calls per peer.

**Default:** `10`

**Range:** `1-100`

**Configuration:**
```toml
[raft.network]
max_inflight_messages = 10
```

**Tuning guidelines:**
- Higher values = more parallelism, higher throughput, more memory
- Lower values = less memory, less network congestion

**When to tune:**
- **Increase (20-50)** for high-throughput workloads with fast network
- **Decrease (5)** for memory-constrained environments

---

## Performance Tuning

### WAL Rotation Size

**Description:** Size threshold for sealing WAL segments (per partition).

**Default:** `256MB` (from `CHRONIK_WAL_ROTATION_SIZE`)

**Range:** `100KB` to `1GB`

**Configuration:**
```bash
# Environment variable
CHRONIK_WAL_ROTATION_SIZE=512MB ./chronik-server standalone
```

**Tuning guidelines:**
- Larger segments = less overhead, fewer segment files
- Smaller segments = more frequent uploads to S3, better granularity

**Recommendations:**
- **High-throughput**: 512MB-1GB
- **Low-latency**: 100MB-250MB
- **Default**: 256MB (good balance)

---

### CPU and Memory Recommendations

**Per partition resource usage:**

| Workload | CPU per partition | Memory per partition |
|----------|-------------------|---------------------|
| Low-throughput | 0.1 cores | 50MB |
| Medium-throughput | 0.5 cores | 200MB |
| High-throughput | 1-2 cores | 500MB-1GB |

**Cluster sizing:**
- **3-node cluster with 100 partitions**: 4-8 cores × 3 nodes, 16GB RAM × 3 nodes
- **3-node cluster with 1000 partitions**: 16-32 cores × 3 nodes, 64GB RAM × 3 nodes

**Thread pool tuning:**
```bash
# Tokio runtime threads (default: number of CPUs)
TOKIO_WORKER_THREADS=8 ./chronik-server standalone
```

---

## Default Values Summary

Complete table of all Raft configuration defaults:

| Parameter | Default | Range | Unit |
|-----------|---------|-------|------|
| **Raft Timing** | | | |
| `election_timeout_ms` | 300 | 150-5000 | milliseconds |
| `heartbeat_interval_ms` | 30 | 10-100 | milliseconds |
| `max_entries_per_batch` | 100 | 10-1000 | entries |
| **Snapshot** | | | |
| `snapshot_threshold` | 10000 | 1000-100000 | entries |
| `snapshot_compression` | gzip | none/gzip/zstd | - |
| `snapshot_retention_count` | 3 | 1-10 | count |
| **ISR** | | | |
| `max_lag_ms` | 10000 | 1000-60000 | milliseconds |
| `max_lag_entries` | 10000 | 1000-100000 | entries |
| `min_insync_replicas` | 2 | 1-RF | count |
| `check_interval_ms` | 1000 | 100-10000 | milliseconds |
| **Network** | | | |
| `rpc_timeout_ms` | 5000 | 1000-30000 | milliseconds |
| `max_inflight_messages` | 10 | 1-100 | count |
| **Cluster** | | | |
| `replication_factor` | 3 | 1-7 | count |
| `quorum_wait_timeout` | 30000 | 5000-120000 | milliseconds |

---

## Tuning Recommendations

### Low-Latency Configuration

**Use case:** Real-time streaming, instant messaging, live dashboards

**Target:** < 50ms p99 end-to-end latency

**Configuration:**
```toml
enabled = true
node_id = 1
replication_factor = 3
min_insync_replicas = 2

[raft]
election_timeout_ms = 200
heartbeat_interval_ms = 20
max_entries_per_batch = 50
snapshot_threshold = 5000

[isr]
max_lag_ms = 5000
max_lag_entries = 5000
check_interval_ms = 500

[raft.network]
rpc_timeout_ms = 2000

[raft.snapshot]
compression = "zstd"
```

**Environment:**
```bash
CHRONIK_PRODUCE_PROFILE=low-latency \
CHRONIK_WAL_ROTATION_SIZE=100MB \
./chronik-server standalone
```

**Network requirements:**
- Same datacenter deployment (1-5ms RTT)
- 10 Gbps network minimum

---

### High-Throughput Configuration

**Use case:** Log aggregation, data pipelines, bulk ETL

**Target:** > 100K msg/s sustained throughput

**Configuration:**
```toml
enabled = true
node_id = 1
replication_factor = 3
min_insync_replicas = 2

[raft]
election_timeout_ms = 300
heartbeat_interval_ms = 30
max_entries_per_batch = 500
snapshot_threshold = 50000

[isr]
max_lag_ms = 15000
max_lag_entries = 50000
check_interval_ms = 2000

[raft.network]
rpc_timeout_ms = 5000
max_inflight_messages = 50

[raft.snapshot]
compression = "zstd"
retention_count = 2
```

**Environment:**
```bash
CHRONIK_PRODUCE_PROFILE=high-throughput \
CHRONIK_WAL_ROTATION_SIZE=1GB \
TOKIO_WORKER_THREADS=16 \
./chronik-server standalone
```

**Hardware requirements:**
- 16+ cores per node
- 32GB+ RAM per node
- NVMe SSD storage

---

### WAN / Multi-DC Configuration

**Use case:** Cross-region replication, disaster recovery, geo-distribution

**Target:** Stability over 50-200ms RTT

**Configuration:**
```toml
enabled = true
node_id = 1
replication_factor = 3
min_insync_replicas = 2

[raft]
election_timeout_ms = 1000
heartbeat_interval_ms = 100
max_entries_per_batch = 100
snapshot_threshold = 10000

[isr]
max_lag_ms = 30000
max_lag_entries = 50000
check_interval_ms = 5000

[raft.network]
rpc_timeout_ms = 15000

[raft.snapshot]
compression = "zstd"
```

**Network requirements:**
- Dedicated low-latency links between DCs
- 1 Gbps minimum bandwidth
- < 100ms RTT between nodes

---

### Balanced Configuration (Default)

**Use case:** General-purpose streaming, typical microservices

**Target:** Good balance of latency and throughput

**Configuration:**
```toml
enabled = true
node_id = 1
replication_factor = 3
min_insync_replicas = 2

[raft]
election_timeout_ms = 300
heartbeat_interval_ms = 30
max_entries_per_batch = 100
snapshot_threshold = 10000

[isr]
max_lag_ms = 10000
max_lag_entries = 10000
check_interval_ms = 1000

[raft.network]
rpc_timeout_ms = 5000

[raft.snapshot]
compression = "zstd"
```

**Environment:**
```bash
CHRONIK_PRODUCE_PROFILE=balanced \
./chronik-server standalone
```

This is the default configuration and works well for most deployments.

---

## See Also

- [RAFT_DEPLOYMENT_GUIDE.md](RAFT_DEPLOYMENT_GUIDE.md) - Cluster setup instructions
- [RAFT_TROUBLESHOOTING.md](RAFT_TROUBLESHOOTING.md) - Common issues and solutions
- [RAFT_ARCHITECTURE.md](RAFT_ARCHITECTURE.md) - How Raft works in Chronik
