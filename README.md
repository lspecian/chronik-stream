# Chronik Stream

[![Build Status](https://github.com/lspecian/chronik-stream/workflows/CI/badge.svg)](https://github.com/lspecian/chronik-stream/actions)
[![Release](https://img.shields.io/github/v/release/lspecian/chronik-stream)](https://github.com/lspecian/chronik-stream/releases)
[![Docker Image](https://img.shields.io/badge/docker-ghcr.io-blue)](https://github.com/lspecian/chronik-stream/pkgs/container/chronik-stream)
[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.75%2B-orange.svg)](https://www.rust-lang.org)

A high-performance streaming platform built in Rust that implements core Kafka wire protocol functionality with comprehensive Write-Ahead Log (WAL) durability and automatic recovery.

**Latest Release: v2.2.16** - Searchable topics with real-time Tantivy indexing. See [CHANGELOG.md](CHANGELOG.md) for full release history.

## ✨ What's New in v2.2.16

🔍 **Searchable Topics**: Opt-in real-time full-text indexing with Tantivy for instant message search
⚡ **Minimal Overhead**: Only 3% throughput overhead in standalone mode (192K vs 198K msg/s)
🎛️ **Per-Topic Control**: Enable searchable per-topic or server-wide via `CHRONIK_DEFAULT_SEARCHABLE`
📊 **Comprehensive Benchmarks**: Standalone 198K msg/s, Cluster 183K msg/s (non-searchable baseline)

**Upgrade Recommendation**: All users should upgrade to v2.2.16 for searchable topics support.

## 🚀 Features

- **Kafka Wire Protocol**: Full Kafka wire protocol with consumer group and transactional support
- **Searchable Topics**: Opt-in real-time full-text search with Tantivy (3% overhead) - see [docs/SEARCHABLE_TOPICS.md](docs/SEARCHABLE_TOPICS.md)
- **Full Compression Support**: All Kafka compression codecs (Gzip, Snappy, LZ4, Zstd) - see [COMPRESSION_SUPPORT.md](COMPRESSION_SUPPORT.md)
- **WAL-based Metadata**: ChronikMetaLog provides event-sourced metadata persistence
- **GroupCommitWal**: PostgreSQL-style group commit with per-partition background workers and batched fsync
- **Zero Message Loss**: WAL ensures durability for all acks modes (0, 1, -1) even during unexpected shutdowns
- **Automatic Recovery**: WAL records are automatically replayed on startup to restore state with 100% accuracy
- **Real Client Testing**: Tested with kafka-python, confluent-kafka, KSQL, and Apache Flink
- **Stress Tested**: Verified at scale with 50K+ messages, zero duplicates, 39K+ msgs/sec throughput
- **Transactional APIs**: Full support for Kafka transactions (InitProducerId, AddPartitionsToTxn, EndTxn)
- **High Performance**: Async architecture with zero-copy networking optimizations
- **Multi-Architecture**: Native support for x86_64 and ARM64 (Apple Silicon, AWS Graviton)
- **Container Ready**: Docker deployment with proper network configuration
- **Simplified Operations**: Single-process architecture reduces operational complexity

## 🏗️ Architecture - 3-Tier Seamless Storage

Chronik implements a unique 3-tier storage system with automatic failover that provides **infinite retention** without requiring infinite local disk:

```
┌─────────────────────────────────────────────────────────────────┐
│              Chronik 3-Tier Seamless Storage                     │
│                   (Infinite Retention Design)                    │
├─────────────────────────────────────────────────────────────────┤
│  Tier 1: WAL (Hot - Local Disk)                                 │
│  ├─ Location: ./data/wal/{topic}/{partition}/                   │
│  ├─ Latency: <1ms (in-memory buffer)                            │
│  └─ Retention: Until sealed (250MB or 30min by default)         │
│        ↓ Background WalIndexer (every 30s)                       │
│                                                                   │
│  Tier 2: Raw Segments in S3 (Warm - Object Storage)             │
│  ├─ Location: s3://bucket/segments/{topic}/{partition}/{range}  │
│  ├─ Latency: 50-200ms (download + deserialize)                  │
│  ├─ Retention: Unlimited (cheap object storage)                 │
│  └─ Purpose: Message consumption after local WAL deletion        │
│        ↓ PLUS ↓                                                  │
│                                                                   │
│  Tier 3: Tantivy Indexes in S3 (Cold - Searchable)              │
│  ├─ Location: s3://bucket/indexes/{topic}/partition-{p}/...     │
│  ├─ Latency: 100-500ms (download + decompress + search)         │
│  ├─ Retention: Unlimited                                         │
│  └─ Purpose: Full-text search WITHOUT downloading raw data       │
│                                                                   │
│  Consumer Fetch Flow (Automatic Fallback):                      │
│    Phase 1: Try WAL buffer (hot, in-memory) → μs latency        │
│    Phase 2: Try local WAL (warm, local disk) → ms latency       │
│    Phase 3: Download raw segment from S3 → 50-200ms latency     │
│    Phase 4: Search Tantivy index → 100-500ms latency            │
│                                                                   │
│  Local Disk Cleanup:                                             │
│    - WAL files DELETED after successful upload to S3             │
│    - Old messages still accessible from S3 indefinitely          │
│    - No infinite local disk space required!                      │
└─────────────────────────────────────────────────────────────────┘

    ┌─────────────────┐
    │   Kafka Client  │  (kafka-python, Java clients, KSQL, etc.)
    │  (Any Language) │
    └────────┬────────┘
             │
             ▼
    ┌────────────────────────────────────────┐
    │         Chronik Server                  │
    │  ┌──────────────┐  ┌─────────────────┐ │
    │  │ Kafka Proto  │  │ ChronikMetaLog  │ │
    │  │ Handler      │  │ (WAL Metadata)  │ │
    │  │ (Port 9092)  │  │                 │ │
    │  └──────────────┘  └─────────────────┘ │
    │  ┌──────────────┐  ┌─────────────────┐ │
    │  │   Search     │  │  Storage Mgr    │ │
    │  │  (Tantivy)   │  │  (3-Tier)       │ │
    │  └──────────────┘  └─────────────────┘ │
    └───────────┬────────────────────────────┘
                │
                ▼
    ┌───────────────────────────┐
    │    Object Storage         │
    │  (S3/GCS/Azure/Local)     │
    │  • Raw segments (Tier 2)  │
    │  • Tantivy indexes (Tier 3)│
    └───────────────────────────┘
```

### Key Differentiators vs Kafka Tiered Storage

| Feature | Kafka Tiered Storage | Chronik Layered Storage |
|---------|---------------------|-------------------------|
| **Hot Storage** | Local disk | WAL + Segments (local) |
| **Cold Storage** | S3 (raw data) | S3 raw segments + Tantivy indexes |
| **Auto-archival** | ✅ Yes | ✅ Yes (WalIndexer background task) |
| **Query by Offset** | ✅ Yes | ✅ Yes (download from S3 as needed) |
| **Full-text Search** | ❌ NO | ✅ **YES** (Tantivy indexes, no download!) |
| **Local Disk** | Grows forever | Bounded (old WAL deleted after S3 upload) |

**Unique Advantage**: Chronik's Tier 3 isn't just "cold storage" - it's a **searchable indexed archive**. You can query old data by content or timestamp range without downloading or scanning raw data!

## ⚡ Quick Start

### Using Docker (Recommended)

```bash
# Quick start - single node
docker run -d -p 9092:9092 \
  -e CHRONIK_ADVERTISED_ADDR=localhost \
  ghcr.io/lspecian/chronik-stream:latest start

# With persistent storage
docker run -d --name chronik \
  -p 9092:9092 \
  -v chronik-data:/data \
  -e CHRONIK_ADVERTISED_ADDR=localhost \
  -e RUST_LOG=info \
  ghcr.io/lspecian/chronik-stream:latest start

# Using docker-compose
curl -O https://raw.githubusercontent.com/lspecian/chronik-stream/main/docker-compose.yml
docker-compose up -d
```

### With S3/MinIO Object Storage

```bash
# MinIO for development
docker run -d --name chronik \
  -p 9092:9092 \
  -e CHRONIK_ADVERTISED_ADDR=localhost \
  -e OBJECT_STORE_BACKEND=s3 \
  -e S3_ENDPOINT=http://minio:9000 \
  -e S3_BUCKET=chronik-storage \
  -e S3_ACCESS_KEY=minioadmin \
  -e S3_SECRET_KEY=minioadmin \
  -e S3_PATH_STYLE=true \
  ghcr.io/lspecian/chronik-stream:latest start

# AWS S3 for production (uses IAM role)
docker run -d --name chronik \
  -p 9092:9092 \
  -e CHRONIK_ADVERTISED_ADDR=localhost \
  -e OBJECT_STORE_BACKEND=s3 \
  -e S3_REGION=us-west-2 \
  -e S3_BUCKET=chronik-prod-archives \
  ghcr.io/lspecian/chronik-stream:latest start
```

### ⚠️ Critical Docker Configuration

**IMPORTANT**: When running Chronik Stream in Docker or binding to `0.0.0.0`, you **MUST** set `CHRONIK_ADVERTISED_ADDR`:

```yaml
# docker-compose.yml example
services:
  chronik-stream:
    image: ghcr.io/lspecian/chronik-stream:latest
    ports:
      - "9092:9092"
    environment:
      CHRONIK_BIND_ADDR: "0.0.0.0"  # Just host, no port
      CHRONIK_ADVERTISED_ADDR: "chronik-stream"  # REQUIRED - use container name for Docker networks
      # or "localhost" for host access, or your public hostname/IP for remote access
```

Without `CHRONIK_ADVERTISED_ADDR`, clients will receive `0.0.0.0:9092` in metadata responses and fail to connect.

### Test with Kafka Client

```python
# Python example
from kafka import KafkaProducer, KafkaConsumer

# Single-node setup
producer = KafkaProducer(
    bootstrap_servers='localhost:9092',
    api_version=(0, 10, 0)  # Important: specify version
)
producer.send('test-topic', b'Hello Chronik!')
producer.flush()

# Consumer
consumer = KafkaConsumer(
    'test-topic',
    bootstrap_servers='localhost:9092',
    api_version=(0, 10, 0),
    auto_offset_reset='earliest'
)
for message in consumer:
    print(f"Received: {message.value}")
```

**⚠️ CRITICAL for Cluster Deployments**: When using a multi-node cluster, **ALWAYS configure clients with ALL cluster brokers** for 100% message consumption success:

```python
# ✅ CORRECT - Cluster configuration (ALL brokers)
producer = KafkaProducer(
    bootstrap_servers='localhost:9092,localhost:9093,localhost:9094',  # All 3 brokers!
    api_version=(0, 10, 0)
)

# ❌ WRONG - Single broker causes leadership rejections and message loss
producer = KafkaProducer(
    bootstrap_servers='localhost:9092',  # Only one broker - NOT RECOMMENDED for clusters!
    api_version=(0, 10, 0)
)
```

See [docs/100_PERCENT_CONSUMPTION_INVESTIGATION.md](docs/100_PERCENT_CONSUMPTION_INVESTIGATION.md) for detailed analysis.

### Using Binary

```bash
# Download latest release (Linux x86_64)
curl -L https://github.com/lspecian/chronik-stream/releases/latest/download/chronik-server-linux-amd64.tar.gz -o chronik-server.tar.gz
tar xzf chronik-server.tar.gz
./chronik-server start

# macOS (Apple Silicon)
curl -L https://github.com/lspecian/chronik-stream/releases/latest/download/chronik-server-darwin-arm64.tar.gz -o chronik-server.tar.gz
tar xzf chronik-server.tar.gz
./chronik-server start
```

### Building from Source

```bash
# Clone repository
git clone https://github.com/lspecian/chronik-stream.git
cd chronik-stream

# Build release binary
cargo build --release --bin chronik-server

# Run single-node
./target/release/chronik-server start

# Or run 3-node cluster locally
./target/release/chronik-server start --config config/examples/cluster/chronik-cluster-node1.toml
./target/release/chronik-server start --config config/examples/cluster/chronik-cluster-node2.toml
./target/release/chronik-server start --config config/examples/cluster/chronik-cluster-node3.toml
```

## 🌟 KSQL Integration

Chronik Stream provides **full compatibility** with KSQLDB (Confluent's SQL engine for Kafka) including transactional support. Simply point KSQLDB at Chronik's Kafka endpoint:

```properties
# ksql-server.properties
bootstrap.servers=localhost:9092
ksql.service.id=ksql_service_1
```

For detailed KSQL setup and usage examples, see [docs/KSQL_INTEGRATION_GUIDE.md](docs/KSQL_INTEGRATION_GUIDE.md).

## 🎯 Operational Modes

The unified `chronik-server` binary supports two deployment modes via the `start` command:

### Single-Node Mode (Default)
Perfect for development, testing, and single-node production deployments:

```bash
# Simplest - just start
./chronik-server start

# With custom data directory
./chronik-server start --data-dir /var/lib/chronik

# With advertised address (required for Docker/remote clients)
./chronik-server start --advertise my-hostname.com:9092
```

**Features:**
- ✅ Full Kafka protocol compatibility
- ✅ WAL-based durability (zero message loss)
- ✅ Automatic crash recovery
- ✅ 3-tier storage (local + S3/GCS/Azure)
- ✅ Full-text search with Tantivy

### Cluster Mode (Multi-Node Replication)
**Available in v2.2.0+**: Production-ready multi-node cluster with Raft consensus, automatic replication, and zero-downtime operations.

**Minimum 3 nodes required** for quorum-based replication.

**Quick Start (Local Testing):**
```bash
# Terminal 1 - Node 1
./chronik-server start --config config/examples/cluster/chronik-cluster-node1.toml

# Terminal 2 - Node 2
./chronik-server start --config config/examples/cluster/chronik-cluster-node2.toml

# Terminal 3 - Node 3
./chronik-server start --config config/examples/cluster/chronik-cluster-node3.toml
```

**Production Setup (3 Machines):**
```bash
# On each node, create config file with unique node_id
# Example node1.toml:
enabled = true
node_id = 1
replication_factor = 3
min_insync_replicas = 2

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

# Start each node
./chronik-server start --config /etc/chronik/node1.toml
```

**Key Features:**
- ✅ Quorum-based replication (survives minority node failures)
- ✅ Automatic leader election via Raft consensus
- ✅ Strong consistency (linearizable reads/writes)
- ✅ Zero-downtime node addition and removal (v2.2.0+)
- ✅ Automatic partition rebalancing
- ✅ Comprehensive monitoring via Prometheus metrics

**Cluster Management:**
```bash
# Add node to running cluster
export CHRONIK_ADMIN_API_KEY=<key>
./chronik-server cluster add-node 4 \
  --kafka node4:9092 \
  --wal node4:9291 \
  --raft node4:5001 \
  --config cluster.toml

# Query cluster status
./chronik-server cluster status --config cluster.toml

# Remove node gracefully
./chronik-server cluster remove-node 4 --config cluster.toml
```

**Complete Guide:** See [docs/RUNNING_A_CLUSTER.md](docs/RUNNING_A_CLUSTER.md) for step-by-step instructions.

### Configuration Options

**Commands:**
```bash
chronik-server start [OPTIONS]          # Start server (auto-detects single-node or cluster)
chronik-server cluster <SUBCOMMAND>     # Manage cluster (add-node, remove-node, status)
chronik-server version                  # Display version info
chronik-server compact <SUBCOMMAND>     # Manage WAL compaction
```

**Start Command Options:**
```bash
chronik-server start [OPTIONS]

Options:
  -d, --data-dir <DIR>         Data directory (default: ./data)
  --config <FILE>              Cluster config file (enables cluster mode)
  --node-id <ID>               Override node ID from config
  --advertise <ADDR:PORT>      Advertised Kafka address (for remote clients)
  -l, --log-level <LEVEL>      Log level (error/warn/info/debug/trace)
```

**Key Environment Variables:**
```bash
# Server Configuration
CHRONIK_DATA_DIR             Data directory path (default: ./data)
CHRONIK_ADVERTISED_ADDR      Address advertised to clients (CRITICAL for Docker)
RUST_LOG                     Log level (error, warn, info, debug, trace)

# Performance Tuning
CHRONIK_WAL_PROFILE          WAL performance: low/medium/high/ultra (auto-detects)
CHRONIK_PRODUCE_PROFILE      Producer flush: low-latency/balanced/high-throughput
CHRONIK_WAL_ROTATION_SIZE    WAL segment size: 100KB/250MB (default)/1GB

# Cluster Management (v2.2.0+)
CHRONIK_ADMIN_API_KEY        Admin API authentication key (REQUIRED for production clusters)

# Object Store (3-Tier Storage)
OBJECT_STORE_BACKEND         Backend: s3/gcs/azure/local (default: local)

# S3/MinIO Configuration
S3_ENDPOINT                  S3 endpoint (e.g., http://minio:9000)
S3_REGION                    AWS region (default: us-east-1)
S3_BUCKET                    Bucket name (default: chronik-storage)
S3_ACCESS_KEY                Access key ID
S3_SECRET_KEY                Secret access key
S3_PATH_STYLE                Path-style URLs (default: true, required for MinIO)
S3_DISABLE_SSL               Disable SSL (default: false)

# GCS Configuration
GCS_BUCKET                   GCS bucket name
GCS_PROJECT_ID               GCP project ID

# Azure Configuration
AZURE_ACCOUNT_NAME           Storage account name
AZURE_CONTAINER              Container name
```

## ⚡ Performance Tuning

Chronik Stream provides two layers of performance tuning for different workloads:

### Producer Flush Profiles

Control when buffered messages become visible to consumers. **Default changed to HighThroughput in v2.1.0** based on benchmark results.

**High-Throughput** - Production deployments (DEFAULT as of v2.1.0)
```bash
./chronik-server ...  # No env var needed - optimal for most workloads
```
- Settings: 100 batches / 500ms flush / 128MB buffer
- Performance: **27,700 msg/s** throughput, **3.94ms p99 latency**
- Use for: Production deployments, data pipelines, ETL, batch processing
- **Why default**: 93% faster than previous Balanced profile with better latency!

**Low-Latency** - Real-time applications
```bash
CHRONIK_PRODUCE_PROFILE=low-latency ./chronik-server ...
```
- Settings: 1 batch / 10ms flush / 16MB buffer
- Target: < 20ms p99 latency
- Use for: Real-time analytics, instant messaging, live dashboards (when sub-20ms critical)

**Balanced** - Legacy compatibility
```bash
CHRONIK_PRODUCE_PROFILE=balanced ./chronik-server ...
```
- Settings: 10 batches / 100ms flush / 32MB buffer
- Performance: 14,300 msg/s, 7.72ms p99 latency
- Use for: Compatibility with older configurations (not recommended for new deployments)

**Extreme** - Experimental maximum batching
```bash
CHRONIK_PRODUCE_PROFILE=extreme ./chronik-server ...
```
- Settings: 500 batches / 2000ms flush / 512MB buffer
- Performance: ~27,700 msg/s (same as HighThroughput - hits fsync hardware limit)
- Use for: Bulk ingestion experiments, data migrations

### WAL Performance Profiles

The Write-Ahead Log automatically detects system resources (CPU, memory, Docker/K8s limits) and selects an appropriate profile. Override with:

```bash
CHRONIK_WAL_PROFILE=low        # Containers, small VMs (≤1 CPU, <512MB)
CHRONIK_WAL_PROFILE=medium     # Typical servers (2-4 CPUs, 512MB-4GB)
CHRONIK_WAL_PROFILE=high       # Dedicated servers (4-16 CPUs, 4-16GB)
CHRONIK_WAL_PROFILE=ultra      # Maximum throughput (16+ CPUs, 16GB+)
```

### Benchmarking

Test performance profiles with your workload:
```bash
cargo build --release
python3 tests/test_profiles_quick.py  # Quick 10K message test
python3 tests/test_produce_profiles.py  # Comprehensive benchmark
```

## 📦 Docker Images

All images support both **linux/amd64** and **linux/arm64** architectures:

| Image | Tags | Description |
|-------|------|-------------|
| `ghcr.io/lspecian/chronik-stream` | `latest`, `v2.2.16`, `2.2` | Chronik server with full KSQL support |

### Supported Platforms

- ✅ **Linux x86_64** (amd64)
- ✅ **Linux ARM64** (aarch64) - AWS Graviton, Raspberry Pi 4+
- ✅ **macOS x86_64** (Intel)
- ✅ **macOS ARM64** (Apple Silicon M1/M2/M3)

## ✅ Kafka Compatibility

### Supported Kafka APIs (24 total)

| API | Version | Status | Description |
|-----|---------|--------|-------------|
| Produce | v0-v9 | ✅ Full | Send messages to topics |
| Fetch | v0-v13 | ✅ Full | Retrieve messages from topics |
| ListOffsets | v0-v7 | ✅ Full | Query partition offsets |
| Metadata | v0-v12 | ✅ Full | Get cluster metadata |
| OffsetCommit | v0-v8 | ✅ Full | Commit consumer offsets |
| OffsetFetch | v0-v8 | ✅ Full | Retrieve consumer offsets |
| FindCoordinator | v0-v4 | ✅ Full | Find group coordinator |
| JoinGroup | v0-v9 | ✅ Full | Join consumer group |
| Heartbeat | v0-v4 | ✅ Full | Consumer heartbeat |
| LeaveGroup | v0-v5 | ✅ Full | Leave consumer group |
| SyncGroup | v0-v5 | ✅ Full | Sync group assignments |
| ApiVersions | v0-v3 | ✅ Full | Negotiate API versions |
| CreateTopics | v0-v7 | ✅ Full | Create new topics |
| DeleteTopics | v0-v6 | ✅ Full | Delete topics |
| DescribeGroups | v0-v5 | ✅ Full | Describe consumer groups |
| ListGroups | v0-v4 | ✅ Full | List all groups |
| SaslHandshake | v0-v1 | ✅ Full | SASL authentication |
| SaslAuthenticate | v0-v2 | ✅ Full | SASL auth exchange |
| CreatePartitions | v0-v3 | ✅ Full | Add partitions to topics |
| InitProducerId | v0-v4 | ✅ Full | Initialize transactional producer |
| AddPartitionsToTxn | v0-v3 | ✅ Full | Add partitions to transaction |
| AddOffsetsToTxn | v0-v3 | ✅ Full | Add consumer offsets to transaction |
| EndTxn | v0-v3 | ✅ Full | Commit or abort transaction |
| TxnOffsetCommit | v0-v3 | ✅ Full | Commit offsets within transaction |

### Tested Clients

- ✅ **kafka-python** - Python client (full compatibility)
- ✅ **confluent-kafka** - High-performance C-based client
- ✅ **KSQLDB** - Full support including transactional operations
- ✅ **Apache Flink** - Stream processing integration

## 🛠️ Development

### Prerequisites

- Rust 1.75+
- Docker & Docker Compose (for testing)
- Python 3.8+ with kafka-python (for client testing)

### Building

```bash
# Build all components
cargo build --release

# Run tests (unit and bin tests only)
cargo test --workspace --lib --bins

# Run integration tests (requires setup)
cargo test --test integration

# Run benchmarks
cargo bench
```

### Project Structure

```
chronik-stream/
├── crates/
│   ├── chronik-server/      # Main server binary (unified)
│   ├── chronik-protocol/    # Kafka wire protocol implementation
│   ├── chronik-storage/     # Storage abstraction layer
│   ├── chronik-search/      # Search engine integration (Tantivy)
│   ├── chronik-query/       # Query processing
│   ├── chronik-common/      # Shared utilities
│   ├── chronik-auth/        # Authentication & authorization
│   ├── chronik-monitoring/  # Metrics & observability
│   ├── chronik-config/      # Configuration management
│   ├── chronik-backup/      # Backup functionality
│   ├── chronik-bench/       # Performance benchmarking tool
│   ├── chronik-wal/         # Write-Ahead Log & metadata store
│   ├── chronik-raft/        # Raft consensus implementation
│   └── chronik-raft-bridge/ # Raft integration bridge
├── tests/                   # Integration tests
├── Dockerfile              # Multi-arch Docker build
├── docker-compose.yml      # Local development setup
└── .github/workflows/      # CI/CD pipelines
```

## ⚡ Performance

Chronik Stream is optimized for production workloads:

- **CPU Usage**: 0% idle (efficient resource utilization)
- **Memory**: Efficient memory usage with zero-copy networking
- **Latency**: < 10ms p99 produce latency with acks=1
- **Throughput**: 39K+ messages/second (tested at 50K message scale)
- **Recovery**: 100% message recovery with zero duplicates
- **Search**: Sub-second full-text search with Tantivy
- **Storage**: Efficient compression (Snappy, LZ4, Zstd)

### WAL Performance
- **Write Throughput**: 39,200 msgs/sec for acks=1 (50K message test)
- **Recovery Speed**: Full recovery in seconds even for large datasets
- **Zero Data Loss**: All acks modes (0, 1, -1) guaranteed durable
- **Group Commit**: PostgreSQL-style batched fsync reduces I/O overhead

## 🔒 Security

- **SASL Authentication**: PLAIN, SCRAM-SHA-256/512
- **TLS/SSL**: End-to-end encryption
- **ACLs**: Topic and consumer group access control
- **Audit Logging**: Track all administrative actions

## 📊 Monitoring

### Prometheus Metrics

```bash
# Expose metrics endpoint
chronik --metrics-port 9093

# Key metrics:
- chronik_messages_received_total
- chronik_messages_stored_total
- chronik_produce_latency_seconds
- chronik_fetch_latency_seconds
- chronik_storage_usage_bytes
- chronik_active_connections
```

## 🤝 Contributing

Contributions are welcome! Please feel free to submit a Pull Request.

## 📄 License

Apache License 2.0. See [LICENSE](LICENSE) for details.

## 📚 Documentation

### Getting Started
- [CHANGELOG.md](CHANGELOG.md) - Detailed release history
- [docs/RUNNING_A_CLUSTER.md](docs/RUNNING_A_CLUSTER.md) - **Complete cluster setup guide (v2.2.0+)**
- [docs/SEARCHABLE_TOPICS.md](docs/SEARCHABLE_TOPICS.md) - **Searchable topics with real-time indexing (v2.2.16+)**
- [docs/KSQL_INTEGRATION_GUIDE.md](docs/KSQL_INTEGRATION_GUIDE.md) - KSQL setup and usage

### v2.2.8 Release (Critical Fixes)
- [docs/WATERMARK_IDEMPOTENCE_FIX_v2.2.7.md](docs/WATERMARK_IDEMPOTENCE_FIX_v2.2.7.md) - Watermark idempotence fix details
- [docs/WATERMARK_OVERWRITE_BUG_ROOT_CAUSE.md](docs/WATERMARK_OVERWRITE_BUG_ROOT_CAUSE.md) - Root cause analysis
- [docs/100_PERCENT_CONSUMPTION_INVESTIGATION.md](docs/100_PERCENT_CONSUMPTION_INVESTIGATION.md) - **100% consumption guide**
- [docs/WATERMARK_REPLICATION_TEST_RESULTS_v2.2.7.2.md](docs/WATERMARK_REPLICATION_TEST_RESULTS_v2.2.7.2.md) - Test results and findings

### Operations & Performance
- [BASELINE_PERFORMANCE.md](BASELINE_PERFORMANCE.md) - **Performance benchmarks (standalone vs cluster, searchable vs non-searchable)**
- [docs/WAL_AUTO_TUNING.md](docs/WAL_AUTO_TUNING.md) - WAL performance auto-tuning guide
- [docs/DISASTER_RECOVERY.md](docs/DISASTER_RECOVERY.md) - Disaster recovery and backup strategies
- [docs/ADMIN_API_SECURITY.md](docs/ADMIN_API_SECURITY.md) - Admin API security configuration

### Cluster Management (v2.2.0+)
- [docs/TESTING_NODE_REMOVAL.md](docs/TESTING_NODE_REMOVAL.md) - Testing node addition/removal
- [docs/PRIORITY4_COMPLETE.md](docs/PRIORITY4_COMPLETE.md) - Node removal implementation details

### Development
- [CLAUDE.md](CLAUDE.md) - Development guide for AI assistants
- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) - System architecture and design
- [docs/BUILD_INSTRUCTIONS.md](docs/BUILD_INSTRUCTIONS.md) - Build and development setup
