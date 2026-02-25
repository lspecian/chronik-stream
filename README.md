# Chronik Stream

[![Build Status](https://github.com/lspecian/chronik-stream/workflows/CI/badge.svg)](https://github.com/lspecian/chronik-stream/actions)
[![Release](https://img.shields.io/github/v/release/lspecian/chronik-stream)](https://github.com/lspecian/chronik-stream/releases)
[![Docker Image](https://img.shields.io/badge/docker-ghcr.io-blue)](https://github.com/lspecian/chronik-stream/pkgs/container/chronik-stream)
[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.75%2B-orange.svg)](https://www.rust-lang.org)

A high-performance streaming platform built in Rust that implements core Kafka wire protocol functionality with comprehensive Write-Ahead Log (WAL) durability and automatic recovery.

**Latest Release: v2.3.0** - Query Orchestrator with multi-backend search, RRF ranking, and validated at 30K messages / 132K vectors on bare metal k8s. See [CHANGELOG.md](CHANGELOG.md) for full release history.

## What's New in v2.3.0

**Query Orchestrator & Multi-Backend Search (Phase 9):**
- **Query Orchestrator** (`/_query` endpoint) — Fan-out queries across multiple topics and backends (text, vector, SQL, hybrid) in parallel, merge results with Reciprocal Rank Fusion (RRF)
- **Ranking Profiles** — Configurable scoring strategies (BM25, vector similarity, hybrid RRF) with named profiles
- **Multi-topic search** — Single query searches across multiple topics simultaneously
- **OpenAI embeddings at scale** — Validated with `text-embedding-3-small` on 30K messages producing 132K vectors
- **Performance on bare metal k8s** (3x Dell Xeon, 256GB RAM each):

| Query Type | p50 Latency | p99 Latency | Notes |
|------------|-------------|-------------|-------|
| Full-text (Tantivy BM25) | **3.4ms** | 8.2ms | Faster than Elasticsearch |
| Vector (HNSW + OpenAI) | 372ms | 524ms | Dominated by embedding API |
| Hybrid (text + vector RRF) | 376ms | 531ms | Best relevance quality |
| SQL (DataFusion) | **5.9ms** | 12.1ms | 10-100x faster than ksqlDB |
| Orchestrator (multi-topic) | 244ms | 680ms | 3-topic text fan-out |

**Also in v2.3.0:**
- Prometheus metrics for all query backends (vectors indexed, tokens processed, API calls)
- k8s performance test suite (k6 load generators, Grafana dashboards)
- Examples: log analysis pipeline, RAG chatbot

## Previous Highlights

- **v2.2.25**: 837K msg/s bare metal cluster, fixed closed-socket spin loop, Kubernetes Operator
- **v2.2.22**: Hot Buffer SQL (0-1ms), Columnar Storage (DataFusion), Vector Search (HNSW), Unified API

## 🚀 Features

### Core Streaming
- **Kafka Wire Protocol**: Full Kafka wire protocol with consumer group and transactional support
- **Zero Message Loss**: WAL ensures durability for all acks modes (0, 1, -1) even during unexpected shutdowns
- **Automatic Recovery**: WAL records are automatically replayed on startup to restore state with 100% accuracy
- **Transactional APIs**: Full support for Kafka transactions (InitProducerId, AddPartitionsToTxn, EndTxn)
- **Full Compression Support**: All Kafka compression codecs (Gzip, Snappy, LZ4, Zstd) - see [COMPRESSION_SUPPORT.md](COMPRESSION_SUPPORT.md)

### SQL & Columnar Storage (v2.2.22+)
- **Hot Buffer SQL**: Query recent data in **0-1ms** via in-memory Arrow tables - sub-second latency for live data
- **Columnar Storage**: DataFusion 44 SQL engine for analytics over Parquet files
- **Unified Views**: Automatic UNION of hot (MemTable) and cold (Parquet) data - seamless time-range queries
- **S3/GCS/Azure Upload**: Optional cloud storage for Parquet files with local-first defaults
- **10K+ msg/s under load**: Load tested with concurrent producers and SQL queries, P99 <20ms

### Search & Analytics
- **Searchable Topics**: Opt-in real-time full-text search with Tantivy (3% overhead) - see [docs/SEARCHABLE_TOPICS.md](docs/SEARCHABLE_TOPICS.md)
- **Vector Search**: HNSW-based semantic search with embedding providers (OpenAI, custom)
- **Query Orchestrator** (v2.3.0+): Multi-topic, multi-backend fan-out with RRF ranking via `/_query` endpoint
- **Unified REST API**: Single endpoint for SQL (`/_sql`), search (`/_search`), vector (`/_vector`), orchestrator (`/_query`), and admin ops

### Operations
- **WAL-based Metadata**: ChronikMetaLog provides event-sourced metadata persistence
- **GroupCommitWal**: PostgreSQL-style group commit with per-partition background workers and batched fsync
- **Real Client Testing**: Tested with kafka-python, confluent-kafka, KSQL, and Apache Flink
- **Stress Tested**: Verified at scale with 1B+ messages, zero data loss, 837K msgs/sec on bare metal cluster
- **High Performance**: Async architecture with zero-copy networking optimizations
- **Multi-Architecture**: Native support for x86_64 and ARM64 (Apple Silicon, AWS Graviton)
- **Container Ready**: Docker deployment with proper network configuration
- **Simplified Operations**: Single-process architecture reduces operational complexity

## 🏗️ Architecture - Modular Feature System

Chronik is **pure Kafka by default** with optional features that can be independently enabled per topic:

```
┌─────────────────────────────────────────────────────────────────┐
│                    Chronik Streaming Platform                   │
├─────────────────────────────────────────────────────────────────┤
│  BASE LAYER (always on) - Pure Kafka Compatible                │
│  ├─ Kafka Protocol (produce, consume, consumer groups)         │
│  ├─ WAL Durability (zero message loss, automatic recovery)     │
│  └─ Standard Kafka fetch/produce at wire-protocol level        │
├─────────────────────────────────────────────────────────────────┤
│  OPTIONAL FEATURES (per-topic, independent toggles)             │
│                                                                  │
│  ┌─────────────────────────┐    ┌─────────────────────────┐     │
│  │  columnar.enabled=true  │    │  searchable.enabled=true│     │
│  ├─────────────────────────┤    ├─────────────────────────┤     │
│  │ Format: Parquet files   │    │ Format: Tantivy indexes │     │
│  │ Hot: Arrow MemTable     │    │ Hot: In-memory index    │     │
│  │ Cold: Parquet on S3     │    │ Cold: Tantivy on S3     │     │
│  │ API: /_sql endpoint     │    │ API: /_search endpoint  │     │
│  │ Latency: 0-10ms         │    │ Latency: 1-500ms        │     │
│  └─────────────────────────┘    └─────────────────────────┘     │
│              ↓                           ↓                      │
│         Can enable BOTH → SQL + Search on same topic            │
├─────────────────────────────────────────────────────────────────┤
│  STORAGE TIER (applies to enabled features)                     │
│  ├─ Local (default) - fast queries, bounded disk               │
│  └─ S3/GCS/Azure (opt-in) - unlimited retention                │
└─────────────────────────────────────────────────────────────────┘
```

### Topic Configuration Examples

```bash
# Pure Kafka - no extras, just streaming
kafka-topics.sh --create --topic logs

# SQL analytics - enables /_sql queries
kafka-topics.sh --create --topic orders \
  --config columnar.enabled=true

# Full-text search - enables /_search queries
kafka-topics.sh --create --topic documents \
  --config searchable.enabled=true

# Both SQL + Search on same topic
kafka-topics.sh --create --topic events \
  --config columnar.enabled=true \
  --config searchable.enabled=true

# With S3 archival for unlimited retention
kafka-topics.sh --create --topic audit \
  --config columnar.enabled=true \
  --config searchable.enabled=true \
  --config storage.s3.enabled=true \
  --config retention.ms=-1
```

### Hot/Cold is About Latency, Not Format

Both Parquet (SQL) and Tantivy (Search) have hot and cold tiers:

| Feature | Hot (In-Memory) | Cold (S3/Disk) |
|---------|-----------------|----------------|
| **Columnar (SQL)** | Arrow MemTable: 0-1ms | Parquet files: 1-200ms |
| **Searchable** | Memory index: 1-10ms | Tantivy S3: 100-500ms |

The system automatically queries hot first, falls back to cold seamlessly.

### Key Differentiators vs Kafka

| Feature | Apache Kafka | Chronik |
|---------|--------------|---------|
| **Pure Kafka mode** | ✅ Yes | ✅ Yes (default) |
| **SQL Queries** | ❌ NO | ✅ `columnar.enabled=true` |
| **Full-text Search** | ❌ NO | ✅ `searchable.enabled=true` |
| **Hot Buffer SQL** | ❌ NO | ✅ 0-1ms via Arrow |
| **Tiered Storage** | ✅ S3 (raw) | ✅ S3 (Parquet + Tantivy) |
| **Unlimited Retention** | ✅ Yes | ✅ Yes |
| **Local Disk** | Grows forever | Bounded (S3 offload) |

**The key insight**: Features are orthogonal. Enable what you need, pay only for what you use.

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

## 🔍 SQL API (v2.2.22+)

Query your Kafka topics directly with SQL - no external database required:

```bash
# Create a columnar-enabled topic
curl -X POST http://localhost:6092/topics \
  -H "Content-Type: application/json" \
  -d '{"name": "orders", "partitions": 3, "config": {"columnar.enabled": "true"}}'

# Produce some data (via Kafka client)
# ... produce messages to 'orders' topic ...

# Query recent data (0-1ms latency from hot buffer)
curl -X POST http://localhost:6092/_sql \
  -H "Content-Type: application/json" \
  -d '{"query": "SELECT COUNT(*) FROM orders_hot"}'

# Query all data (hot + cold unified view)
curl -X POST http://localhost:6092/_sql \
  -H "Content-Type: application/json" \
  -d '{"query": "SELECT * FROM orders WHERE _offset > 1000 LIMIT 10"}'

# Aggregations
curl -X POST http://localhost:6092/_sql \
  -H "Content-Type: application/json" \
  -d '{"query": "SELECT _partition, COUNT(*) as cnt FROM orders GROUP BY _partition"}'

# Describe table schema
curl http://localhost:6092/_sql/describe/orders
```

**Available Tables per Topic:**
| Table | Description | Latency |
|-------|-------------|---------|
| `{topic}_hot` | In-memory Arrow buffer (recent data) | 0-1ms |
| `{topic}_cold` | Parquet files (historical data) | 1-10ms |
| `{topic}` | Unified view (hot + cold UNION) | 1-10ms |

For detailed SQL syntax and examples, see [docs/COLUMNAR_STORAGE_GUIDE.md](docs/COLUMNAR_STORAGE_GUIDE.md).

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

# Hot Buffer SQL (v2.2.22+)
CHRONIK_HOT_BUFFER_ENABLED   Enable sub-second SQL queries (default: true)
CHRONIK_HOT_BUFFER_MAX_RECORDS  Max records per partition in hot buffer (default: 100000)
CHRONIK_HOT_BUFFER_REFRESH_MS   Hot buffer refresh interval in ms (default: 1000)

# Columnar Storage (v2.2.22+)
CHRONIK_COLUMNAR_USE_OBJECT_STORE  Upload Parquet to S3/GCS/Azure (default: false)
CHRONIK_COLUMNAR_S3_PREFIX   Object storage prefix (default: columnar)
CHRONIK_COLUMNAR_KEEP_LOCAL  Keep local Parquet copies for fast queries (default: true)

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

### WAL Performance Profiles

The Write-Ahead Log is the primary performance lever. It automatically detects system resources (CPU, memory, Docker/K8s limits) and selects an appropriate profile. Override with:

```bash
CHRONIK_WAL_PROFILE=low        # Containers, small VMs (≤1 CPU, <512MB) - 2ms batch
CHRONIK_WAL_PROFILE=medium     # Typical servers (2-4 CPUs, 512MB-4GB) - 10ms batch
CHRONIK_WAL_PROFILE=high       # Dedicated servers (4-16 CPUs, 4-16GB) - 50ms batch
CHRONIK_WAL_PROFILE=ultra      # Maximum throughput (16+ CPUs, 16GB+) - 100ms batch
```

**Benchmark results use `high` profile** - see [BASELINE_PERFORMANCE.md](BASELINE_PERFORMANCE.md) for detailed methodology.

### Producer Flush Profiles

Control when buffered messages become visible to consumers:

| Profile | Batches | Flush Interval | Buffer | Use Case |
|---------|---------|----------------|--------|----------|
| `low-latency` (default) | 1 | 10ms | 16MB | Real-time analytics, instant messaging |
| `balanced` | 10 | 100ms | 32MB | General-purpose workloads |
| `high-throughput` | 100 | 500ms | 128MB | Data pipelines, ETL, batch processing |
| `extreme` | 500 | 2000ms | 512MB | Bulk ingestion, data migrations |

```bash
# Set producer profile (low-latency is default, use high-throughput for batch workloads)
CHRONIK_PRODUCE_PROFILE=high-throughput ./chronik-server start
```

### Benchmarking

Run the built-in benchmark tool:
```bash
cargo build --release
./target/release/chronik-bench -c 128 -s 256 -d 30s -m produce
```

## 📦 Docker Images

All images support both **linux/amd64** and **linux/arm64** architectures:

| Image | Tags | Description |
|-------|------|-------------|
| `ghcr.io/lspecian/chronik-stream` | `latest`, `v2.3.0`, `2.3` | Chronik server with SQL, search, query orchestrator, and KSQL support |

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
│   ├── chronik-columnar/    # Columnar storage & SQL (DataFusion) [v2.2.22+]
│   ├── chronik-embeddings/  # Vector embeddings & HNSW search [v2.2.22+]
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
│   ├── chronik-raft-bridge/ # Raft integration bridge
│   ├── raft/                # Vendored TiKV raft (prost 0.12)
│   └── raft-proto/          # Vendored raft-proto (prost codegen)
├── tests/                   # Integration tests
├── Dockerfile              # Multi-arch Docker build
├── docker-compose.yml      # Local development setup
└── .github/workflows/      # CI/CD pipelines
```

## Performance

### Bare Metal Cluster (v2.2.25)

Tested on 3x Dell Xeon E5-2667v4 (32 cores, 256GB RAM, 1GbE) running a 3-node Raft cluster with full replication (acks=all). Traffic flows through a realistic HTTP ingestor pipeline with k6 load generators simulating thousands of concurrent users.

| Configuration | Messages/sec | Throughput | Errors |
|---------------|-------------|------------|--------|
| 12 ingestors, 256B messages | **837,284 msg/s** | 245 MB/s | 0.00% |
| 12 ingestors, 1KB messages | 270,000 msg/s | 270 MB/s | 0.00% |
| 36 ingestors, 1KB messages | 420,609 msg/s | **411 MB/s** | 0.00% |

- **Zero data loss** across 1B+ messages
- CPU returns to idle within seconds of load completion
- Cluster never exceeded 80% CPU utilization (I/O bound, not CPU bound)

See [BARE_METAL_PERFORMANCE.md](BARE_METAL_PERFORMANCE.md) for full test methodology, architecture diagrams, and bottleneck analysis.

### Single-Node Benchmarks

| Mode | Configuration | Throughput | p99 Latency |
|------|---------------|------------|-------------|
| **Standalone** | acks=1 | **309K msg/s** | 0.59ms |
| **Standalone** | acks=all | **348K msg/s** | 0.56ms |
| **Cluster (3 nodes)** | acks=1 | **188K msg/s** | 2.81ms |
| **Cluster (3 nodes)** | acks=all | **166K msg/s** | 1.80ms |

### Key Performance Features

- **High Throughput**: 837K msg/s cluster (bare metal), 348K msg/s standalone
- **Low Latency**: Sub-millisecond p99 latency standalone, sub-3ms cluster
- **SQL Query Latency**: 0-1ms for hot buffer, <20ms P99 under 10K msg/s load
- **Zero Data Loss**: All acks modes (0, 1, -1) guaranteed durable with WAL
- **Group Commit**: PostgreSQL-style batched fsync reduces I/O overhead
- **Recovery**: Full WAL recovery in seconds even for large datasets

See [BASELINE_PERFORMANCE.md](BASELINE_PERFORMANCE.md) for single-node benchmark methodology.

## 🔒 Security

### SASL Authentication

Chronik Stream supports SASL authentication with the following mechanisms:
- **PLAIN** - Username/password authentication
- **SCRAM-SHA-256** - Challenge-response authentication
- **SCRAM-SHA-512** - Challenge-response authentication (stronger)

**Configuration:**
```bash
# Production: Configure custom users
CHRONIK_SASL_USERS='myuser:securepass123,kafka-app:app-secret' ./chronik-server start

# Production: Disable all default users
CHRONIK_SASL_NO_DEFAULTS=1 ./chronik-server start

# Development only: Default test users (NOT for production!)
# - admin/admin123, user/user123, kafka/kafka-secret
# These are used if no CHRONIK_SASL_* env vars are set
```

```python
# Python example with SASL/PLAIN
from kafka import KafkaProducer

producer = KafkaProducer(
    bootstrap_servers='localhost:9092',
    security_protocol='SASL_PLAINTEXT',
    sasl_mechanism='PLAIN',
    sasl_plain_username='myuser',
    sasl_plain_password='securepass123'
)
```

### Additional Security Features

- **TLS/SSL**: End-to-end encryption (infrastructure in `chronik-auth` crate)
- **ACLs**: Topic and consumer group access control framework
- **Admin API**: Secured with API key authentication (cluster management)

> ⚠️ **Security Note**: Default test users are for development only. Always set `CHRONIK_SASL_USERS` or `CHRONIK_SASL_NO_DEFAULTS=1` in production.

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

### SQL & Columnar Storage (v2.2.22+)
- [docs/COLUMNAR_STORAGE_GUIDE.md](docs/COLUMNAR_STORAGE_GUIDE.md) - **Hot buffer SQL & Parquet storage guide**
- [docs/API_REFERENCE.md](docs/API_REFERENCE.md) - Unified REST API reference (`/_sql`, `/_search`, admin)

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
