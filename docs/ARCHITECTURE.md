# Chronik Stream Architecture

## Overview

Chronik Stream is a Kafka-compatible distributed streaming platform built in Rust. It provides full Kafka wire protocol compatibility with multi-node clustering via Raft consensus, WAL-based durability, and optional real-time full-text search.

**Current Version:** v2.2.9+

## Crate Structure

```
chronik-stream/
├── chronik-server/          # Main server binary (standalone + cluster modes)
├── chronik-protocol/        # Kafka wire protocol implementation
├── chronik-wal/             # Write-ahead log with group commit
├── chronik-storage/         # Segment storage, object store abstraction
├── chronik-raft/            # Raft consensus implementation (OpenRaft)
├── chronik-raft-bridge/     # Bridge between Raft and server components
├── chronik-search/          # Real-time Tantivy indexing
├── chronik-common/          # Shared types, metadata traits
├── chronik-config/          # Configuration management
├── chronik-auth/            # SASL, TLS authentication
├── chronik-monitoring/      # Prometheus metrics, tracing
├── chronik-query/           # Query processing
├── chronik-backup/          # Backup functionality
└── chronik-bench/           # Performance benchmarks
```

## Core Components

### 1. Server (`chronik-server`)

Single binary supporting two operational modes:

**Standalone Mode** (default):
```bash
./chronik-server start --advertise localhost:9092
```

**Cluster Mode** (3+ nodes with Raft):
```bash
./chronik-server start --config cluster.toml
```

The server uses a 14-stage builder pattern (`IntegratedKafkaServerBuilder`) for initialization, handling WAL recovery, Raft cluster setup, metadata replication, and handler initialization.

### 2. Kafka Protocol (`chronik-protocol`)

Full Kafka wire protocol implementation with 19+ supported APIs:

| API | Versions | Status |
|-----|----------|--------|
| Produce (0) | v0-v9 | Full support |
| Fetch (1) | v0-v13 | Full support |
| ListOffsets (2) | v0-v5 | Full support |
| Metadata (3) | v0-v12 | Full support |
| OffsetCommit (8) | v0-v7 | Full support |
| OffsetFetch (9) | v0-v5 | Full support |
| FindCoordinator (10) | v0-v3 | Full support |
| JoinGroup (11) | v0-v5 | Full support |
| Heartbeat (12) | v0-v3 | Full support |
| LeaveGroup (13) | v0-v3 | Full support |
| SyncGroup (14) | v0-v3 | Full support |
| ApiVersions (18) | v0-v3 | Full support |
| CreateTopics (19) | v0-v5 | Full support |
| DeleteTopics (20) | v0-v4 | Full support |
| InitProducerId (22) | v0-v4 | Full support |

### 3. Write-Ahead Log (`chronik-wal`)

Mandatory durability layer providing zero message loss guarantee:

- **GroupCommitWal**: Batched fsync for high throughput
- **WalRecord V2**: Includes Kafka CRC-32C for data integrity
- **Automatic Recovery**: Replays WAL on startup
- **Configurable Profiles**: `low`, `medium`, `high`, `ultra`

```
Write Path:
Producer → WalProduceHandler → GroupCommitWal (fsync) → ProduceHandler → Storage
```

### 4. Storage Layer (`chronik-storage`)

3-tier storage architecture:

| Tier | Latency | Backend | Purpose |
|------|---------|---------|---------|
| Hot (WAL) | <1ms | Local disk | Recent messages, durability |
| Warm (Segments) | 1-10ms | S3/GCS/Azure/Local | Raw message data |
| Cold (Tantivy) | 100-500ms | S3/GCS/Azure/Local | Searchable indexes |

### 5. Raft Clustering (`chronik-raft`)

Multi-node replication using OpenRaft:

- **Multi-Raft Design**: One Raft group per partition
- **Quorum-Based Replication**: Survives minority failures
- **Automatic Leader Election**: <1 second failover
- **Zero-Downtime Operations**: Node addition/removal without service interruption

```
Cluster Configuration:
┌─────────────────────────────────────────────────────┐
│  Node 1: kafka=9092, wal=9291, raft=5001           │
│  Node 2: kafka=9093, wal=9292, raft=5002           │
│  Node 3: kafka=9094, wal=9293, raft=5003           │
│                                                     │
│  Replication Factor: 3                              │
│  Min In-Sync Replicas: 2                           │
│  Quorum: 2/3 (tolerates 1 failure)                 │
└─────────────────────────────────────────────────────┘
```

### 6. Real-Time Search (`chronik-search`)

Optional Tantivy integration for full-text search:

- **Real-time Indexing**: Messages indexed during produce path
- **Searchable Topics**: Per-topic configuration
- **Performance**: ~3% overhead standalone, ~33% cluster

## Data Flow

### Message Production

```
Client
   │
   ▼
Protocol Handler (parse request)
   │
   ▼
WalProduceHandler (WAL write + fsync)
   │
   ▼
ProduceHandler (in-memory state, ISR tracking)
   │
   ├─► SegmentWriter (warm storage)
   │
   └─► WalIndexer (if searchable topic)
         │
         ▼
       Tantivy Index
```

### Message Consumption

```
Client
   │
   ▼
Protocol Handler (parse request)
   │
   ▼
FetchHandler
   │
   ├─► Try WAL (hot)
   ├─► Try Segments (warm, cached)
   └─► Try Tantivy (cold)
```

## Deployment Architectures

### Standalone (Single Node)

```
┌─────────────────────────────────┐
│  chronik-server                 │
│  ├─ Port 9092: Kafka Protocol   │
│  ├─ Storage: ./data             │
│  └─ WAL: GroupCommitWal         │
└─────────────────────────────────┘
```

### Cluster (3+ Nodes)

```
┌─────────────────────────────────┐
│  Node 1 (Leader)                │
│  ├─ Kafka: 9092                 │
│  ├─ WAL Replication: 9291       │
│  └─ Raft Consensus: 5001        │
└─────────────────────────────────┘
         ↕
┌─────────────────────────────────┐
│  Node 2 (Follower)              │
│  ├─ Kafka: 9093                 │
│  ├─ WAL Replication: 9292       │
│  └─ Raft Consensus: 5002        │
└─────────────────────────────────┘
         ↕
┌─────────────────────────────────┐
│  Node 3 (Follower)              │
│  ├─ Kafka: 9094                 │
│  ├─ WAL Replication: 9293       │
│  └─ Raft Consensus: 5003        │
└─────────────────────────────────┘
```

## Performance

### Benchmarks (128 concurrency, 256 byte messages)

| Mode | Configuration | Throughput | p99 Latency |
|------|---------------|------------|-------------|
| **Standalone** | acks=1 | **309K msg/s** | 0.59ms |
| **Standalone** | acks=all | **348K msg/s** | 0.56ms |
| **Cluster (3 nodes)** | acks=1 | **188K msg/s** | 2.81ms |
| **Cluster (3 nodes)** | acks=all | **166K msg/s** | 1.80ms |

#### Searchable Topics Impact

| Configuration | Non-Searchable | Searchable | Overhead |
|--------------|---------------:|-----------:|---------:|
| Standalone | 198K msg/s | 192K msg/s | 3% |
| Cluster (3 nodes) | 183K msg/s | 123K msg/s | 33% |

### Resource Recommendations

| Deployment | Memory | CPU | Network |
|------------|--------|-----|---------|
| Development | 512MB | 2 cores | 1 Gbps |
| Production (standalone) | 4-8GB | 4-8 cores | 10 Gbps |
| Production (cluster) | 8-16GB per node | 8-16 cores | 10 Gbps |

## Reliability

### Durability Guarantees

- **WAL Persistence**: All messages written to WAL before acknowledgment
- **Fsync on Commit**: Configurable sync intervals via `CHRONIK_WAL_PROFILE`
- **CRC-32C Checksums**: Kafka protocol-level data integrity
- **Automatic Recovery**: WAL replay on startup

### Fault Tolerance (Cluster Mode)

- **Replication Factor 3**: Survives 1 node failure
- **Quorum Writes**: Majority must acknowledge
- **Leader Election**: <1 second automatic failover
- **ISR Tracking**: In-sync replica management

### Disaster Recovery

- **Metadata Backup**: Automatic upload to S3/GCS/Azure
- **Cold Start Recovery**: Restore from object storage
- **Snapshot-Based**: Periodic Raft snapshots

## Configuration

### Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `CHRONIK_ADVERTISED_ADDR` | (required) | Address advertised to clients |
| `CHRONIK_DATA_DIR` | `./data` | Data directory |
| `CHRONIK_WAL_PROFILE` | auto | `low`/`medium`/`high`/`ultra` |
| `CHRONIK_DEFAULT_SEARCHABLE` | `false` | Enable search for new topics |
| `CHRONIK_ADMIN_API_KEY` | (generated) | Admin API authentication |

### Cluster Configuration (TOML)

```toml
enabled = true
node_id = 1
replication_factor = 3
min_insync_replicas = 2

[bind]
kafka = "0.0.0.0:9092"
wal = "0.0.0.0:9291"
raft = "0.0.0.0:5001"

[advertise]
kafka = "localhost:9092"
wal = "localhost:9291"
raft = "localhost:5001"

[[peers]]
id = 1
kafka = "localhost:9092"
wal = "localhost:9291"
raft = "localhost:5001"
```

## Client Compatibility

Tested with:
- kafka-python
- confluent-kafka (Python)
- KSQLDB
- Apache Flink
- Java Kafka clients

## Comparison with Apache Kafka

| Feature | Chronik | Kafka |
|---------|---------|-------|
| Deployment | Single binary | JVM + ZK/KRaft |
| Memory | 512MB-8GB | 4GB+ recommended |
| Clustering | Raft (built-in) | KRaft/ZooKeeper |
| Full-text Search | Built-in (Tantivy) | External (Elasticsearch) |
| Object Storage | Native S3/GCS/Azure | Tiered storage (limited) |
| Protocol | Kafka wire protocol | Kafka wire protocol |

## See Also

- [RUNNING_A_CLUSTER.md](RUNNING_A_CLUSTER.md) - Cluster deployment guide
- [DISASTER_RECOVERY.md](DISASTER_RECOVERY.md) - Backup and recovery
- [SEARCHABLE_TOPICS.md](SEARCHABLE_TOPICS.md) - Full-text search feature
- [wal/](wal/) - WAL implementation details
