# Chronik Stream

[![Build Status](https://github.com/lspecian/chronik-stream/workflows/CI/badge.svg)](https://github.com/lspecian/chronik-stream/actions)
[![Release](https://img.shields.io/github/v/release/lspecian/chronik-stream)](https://github.com/lspecian/chronik-stream/releases)
[![Docker Image](https://img.shields.io/badge/docker-ghcr.io-blue)](https://github.com/lspecian/chronik-stream/pkgs/container/chronik-stream)
[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.75%2B-orange.svg)](https://www.rust-lang.org)

**An event-native data platform that unifies streaming, analytics, search, and semantic retrieval in a single binary.**

Write events once. Query them immediately — with SQL, full-text search, vector similarity, or Kafka consumers. No ETL. No glue. No pipeline tax.

---

## The Problem

Modern real-time architectures require stitching together many systems:

```
Producer -> Kafka -> Flink/Spark -> Data Warehouse -> BI Tools
                 |-> Kafka Connect -> Elasticsearch -> Search API
                 |-> Custom ETL -> Vector DB -> AI/Agent API
                 |-> S3 Sink -> Athena/Trino -> Ad-hoc Queries
```

Each arrow is a failure point, a latency penalty, and an operational burden. Data is copied, reformatted, and re-indexed across systems that each serve one purpose. By the time an event is searchable, queryable, and semantically retrievable, it has passed through 4-8 systems with minutes to hours of delay.

## The Chronik Approach

Chronik treats the event log as the source of truth and derives all query capabilities directly from it — at ingestion time, in the same process:

```
Producer -> Chronik -> Done.
               |
               |  (internally, at write time)
               |-> WAL (durability, Kafka consumers)
               |-> Arrow/Parquet (SQL via DataFusion)
               |-> Tantivy (full-text search, BM25)
               |-> HNSW (vector embeddings, semantic search)
```

Events become queryable across all modalities within seconds of ingestion. There is no ETL pipeline between the log and the query engine — they are the same system.

```
┌─────────────────────────────────────────────────────────────┐
│                        Chronik Stream                        │
│                                                              │
│  Kafka Protocol (9092)          Unified API (6092)           │
│  ├─ Produce / Fetch             ├─ /_sql    (DataFusion)     │
│  ├─ Consumer Groups             ├─ /_search (BM25/Tantivy)   │
│  ├─ Transactions                ├─ /_vector (HNSW)           │
│  └─ 24 Kafka APIs               ├─ /_query  (Orchestrator)   │
│                                  └─ /admin   (Cluster ops)   │
│                                                              │
│  One binary. One log. Multiple query modalities.             │
└─────────────────────────────────────────────────────────────┘
```

This replaces what traditionally requires Kafka + ClickHouse + Elasticsearch + a vector database — with one Rust binary that speaks the Kafka wire protocol.

## Per-Topic Capability Model

Features are orthogonal and enabled per topic. A topic with no features enabled is a pure Kafka topic. Each capability adds a background indexing path — nothing changes in the write path.

```bash
# Pure Kafka — no extras, just streaming
kafka-topics.sh --create --topic logs

# SQL analytics — enables /_sql queries
kafka-topics.sh --create --topic orders \
  --config columnar.enabled=true

# Full-text search — enables /_search queries
kafka-topics.sh --create --topic documents \
  --config searchable.enabled=true

# Vector search — enables /_vector semantic queries
kafka-topics.sh --create --topic support-tickets \
  --config vector.enabled=true \
  --config vector.embedding.provider=openai

# All capabilities on a single topic
kafka-topics.sh --create --topic events \
  --config columnar.enabled=true \
  --config searchable.enabled=true \
  --config vector.enabled=true
```

Enable what you need, pay only for what you use. A topic with nothing enabled has zero indexing overhead.

### What Gets Enabled

| Capability | Config | Query Endpoint | Engine | Latency |
|------------|--------|----------------|--------|---------|
| **Streaming** | always on | Kafka Fetch (9092) | WAL | < 1ms |
| **SQL** | `columnar.enabled=true` | `POST /_sql` | DataFusion + Arrow/Parquet | 0-200ms |
| **Search** | `searchable.enabled=true` | `POST /_search` | Tantivy (BM25) | 1-500ms |
| **Vector** | `vector.enabled=true` | `POST /_vector/:topic/search` | HNSW + embeddings | 5-100ms |
| **Orchestrator** | automatic | `POST /_query` | RRF multi-backend fusion | 10-500ms |

### How It Works Internally

All indexing happens asynchronously after the produce ack. The write path is unaffected:

```
Producer -> WAL (fsync) -> Produce Ack      (~2-10ms, same as Kafka)
               |
               v  (background, async)
          WalIndexer
               |
    ┌──────────┼──────────┐
    v          v          v
 Tantivy    Parquet    Embeddings
 (search)   (SQL)      (vector)
```

Query availability after ingestion:
- **Kafka Fetch**: immediate
- **SQL (hot buffer)**: ~1 second
- **Full-text search**: 30-60 seconds
- **Vector search**: 30-90 seconds (includes embedding API call)

## Architecture Comparison

### What Chronik Replaces

| Capability | Traditional Stack | Chronik |
|------------|-------------------|---------|
| Event streaming | Apache Kafka | Built-in (Kafka protocol) |
| Stream processing | Flink / Spark Streaming | DataFusion SQL at query time |
| Data warehouse | ClickHouse / BigQuery | Arrow hot buffer + Parquet cold storage |
| Full-text search | Elasticsearch / OpenSearch | Tantivy (BM25), validated above ES quality |
| Vector search | Pinecone / Weaviate / Qdrant | HNSW with pluggable embeddings |
| ETL pipelines | Kafka Connect / Debezium / dbt | Eliminated — indexing is built-in |
| Object storage | S3 sink connector | Native S3/GCS/Azure tiered storage |

### What Chronik Does Not Replace

Chronik is not a general-purpose OLAP database, a graph database, or a batch processing framework. It is purpose-built for event data: append-only logs where records arrive in time order and are queried by content, time range, or semantic similarity.

## Benchmarks and Validation

Validated on a 3-node bare metal cluster (Dell Xeon E5-2667v4, 32 cores, 256GB RAM, 1GbE) running MicroK8s.

### Streaming Throughput

| Configuration | Messages/sec | Throughput | Errors |
|---------------|-------------|------------|--------|
| Cluster, 12 ingestors, 256B | **837,284 msg/s** | 245 MB/s | 0.00% |
| Cluster, 36 ingestors, 1KB | 420,609 msg/s | **411 MB/s** | 0.00% |
| Standalone, acks=1 | 309,000 msg/s | — | 0.00% |

Zero data loss across 1B+ messages. See [BARE_METAL_PERFORMANCE.md](BARE_METAL_PERFORMANCE.md).

### Search Quality (WANDS Benchmark — v2.4.0)

| System | NDCG@10 | Notes |
|--------|---------|-------|
| Elasticsearch BM25 | ~0.50-0.56 | Published baselines |
| **Chronik BM25** (Tantivy) | **0.5927** | +6-18% vs Elasticsearch |
| Elasticsearch Hybrid | ~0.58-0.65 | Published baselines |
| **Chronik Hybrid** (BM25 + Vector, RRF) | **0.6095** | Within ES hybrid range |

Tested against the [WANDS dataset](https://github.com/wayfair/WANDS) (42,994 products, 480 queries). See [docs/DATASET_VALIDATION_REPORT.md](docs/DATASET_VALIDATION_REPORT.md).

### Scale Validation (Amazon Reviews — v2.4.0)

| Dataset | Records | Text Search p50 | SQL p50 | Errors |
|---------|---------|-----------------|---------|--------|
| Amazon Appliances | 2.1M | 2.8ms @ 1000 VUs | 43ms | 0.00% |
| Amazon Grocery | 14.3M | 8.7ms @ 1000 VUs | 360ms | 0.00% |
| Amazon Electronics | 43.9M | 112ms @ 1000 VUs | 185ms | 0.00% |


## Use Cases

### Operational Analytics
Replace Kafka + Flink + ClickHouse. Produce events via Kafka protocol. Query with SQL. No pipeline.

```bash
# Produce events from your application (any Kafka client)
producer.send('orders', {'user_id': 42, 'amount': 99.50, 'city': 'Berlin'})

# Query immediately via SQL
curl -X POST http://localhost:6092/_sql \
  -d '{"query": "SELECT city, COUNT(*), AVG(amount) FROM orders GROUP BY city"}'
```

### Real-Time Search
Replace Kafka + Kafka Connect + Elasticsearch. Events become searchable within seconds.

```bash
# Full-text search across all messages
curl -X POST http://localhost:6092/_search \
  -d '{"index": "support-tickets", "query": {"match": {"_all": "login timeout error"}}}'
```

### AI and Agent Memory
Replace Kafka + custom ETL + Pinecone/Weaviate. Events are embedded and indexed for semantic retrieval automatically.

```bash
# Semantic search — "what happened recently that's similar to this?"
curl -X POST http://localhost:6092/_vector/incidents/search \
  -d '{"query": "database connection pool exhausted", "k": 10}'

# Hybrid search — combine keyword precision with semantic recall
curl -X POST http://localhost:6092/_vector/incidents/hybrid \
  -d '{"query": "OOM kill postgres", "k": 10, "vector_weight": 0.7}'
```

### Multi-Modal Retrieval
Use the query orchestrator to search across modalities and topics in a single request:

```bash
curl -X POST http://localhost:6092/_query \
  -d '{
    "query": "payment failures in EU",
    "topics": ["orders", "support-tickets", "incidents"],
    "backends": ["search", "vector"],
    "k": 20
  }'
```

Results are fused via Reciprocal Rank Fusion (RRF) and returned as a single ranked list.

## Quick Start

### Docker (Recommended)

```bash
# Single node — just works
docker run -d -p 9092:9092 -p 6092:6092 \
  -e CHRONIK_ADVERTISED_ADDR=localhost \
  ghcr.io/lspecian/chronik-stream:latest start

# With persistent storage
docker run -d --name chronik \
  -p 9092:9092 -p 6092:6092 \
  -v chronik-data:/data \
  -e CHRONIK_ADVERTISED_ADDR=localhost \
  ghcr.io/lspecian/chronik-stream:latest start
```

**IMPORTANT**: `CHRONIK_ADVERTISED_ADDR` is required when binding to `0.0.0.0` or running in Docker. Without it, clients receive `0.0.0.0:9092` and fail to connect.

### Binary

```bash
# Linux x86_64
curl -L https://github.com/lspecian/chronik-stream/releases/latest/download/chronik-server-linux-amd64.tar.gz | tar xz
./chronik-server start

# macOS (Apple Silicon)
curl -L https://github.com/lspecian/chronik-stream/releases/latest/download/chronik-server-darwin-arm64.tar.gz | tar xz
./chronik-server start
```

### From Source

```bash
git clone https://github.com/lspecian/chronik-stream.git
cd chronik-stream
cargo build --release --bin chronik-server
./target/release/chronik-server start
```

### Test with a Kafka Client

```python
from kafka import KafkaProducer, KafkaConsumer

producer = KafkaProducer(
    bootstrap_servers='localhost:9092',
    api_version=(0, 10, 0)
)
producer.send('test-topic', b'Hello Chronik!')
producer.flush()

consumer = KafkaConsumer(
    'test-topic',
    bootstrap_servers='localhost:9092',
    api_version=(0, 10, 0),
    auto_offset_reset='earliest'
)
for message in consumer:
    print(f"Received: {message.value}")
```

## SQL API

Query Kafka topics directly with SQL. No external database required.

```bash
# Create a columnar-enabled topic
curl -X POST http://localhost:6092/topics \
  -H "Content-Type: application/json" \
  -d '{"name": "orders", "partitions": 3, "config": {"columnar.enabled": "true"}}'

# Produce data via any Kafka client, then query immediately
curl -X POST http://localhost:6092/_sql \
  -d '{"query": "SELECT city, COUNT(*) as cnt, AVG(amount) FROM orders GROUP BY city ORDER BY cnt DESC LIMIT 10"}'

# Recent data (0-1ms from in-memory hot buffer)
curl -X POST http://localhost:6092/_sql \
  -d '{"query": "SELECT * FROM orders_hot WHERE amount > 100 LIMIT 10"}'

# Historical data (Parquet files)
curl -X POST http://localhost:6092/_sql \
  -d '{"query": "SELECT * FROM orders_cold WHERE _partition = 0 LIMIT 10"}'
```

JSON message values are automatically inferred into typed columns (Int64, Float64, Boolean, Utf8). Query with standard SQL — no UDFs or schema definitions needed.

**Tables per topic:**

| Table | Source | Latency |
|-------|--------|---------|
| `{topic}_hot` | In-memory Arrow buffer (recent data) | 0-1ms |
| `{topic}_cold` | Parquet files (historical data) | 1-200ms |
| `{topic}` | Unified view (hot + cold) | 1-200ms |

See [docs/COLUMNAR_STORAGE_GUIDE.md](docs/COLUMNAR_STORAGE_GUIDE.md) for full SQL reference.

## Cluster Mode

Chronik supports multi-node clusters with Raft consensus, automatic replication, and zero-downtime operations. Minimum 3 nodes for quorum.

```bash
# Start a 3-node cluster (one terminal per node)
./chronik-server start --config cluster-node1.toml
./chronik-server start --config cluster-node2.toml
./chronik-server start --config cluster-node3.toml
```

```toml
# cluster-node1.toml
node_id = 1
replication_factor = 3
min_insync_replicas = 2

[[peers]]
id = 1
kafka = "node1:9092"
wal = "node1:9291"
raft = "node1:5001"

[[peers]]
id = 2
kafka = "node2:9092"
wal = "node2:9291"
raft = "node2:5001"

[[peers]]
id = 3
kafka = "node3:9092"
wal = "node3:9291"
raft = "node3:5001"
```

**Cluster management:**

```bash
# Add a node (zero downtime)
./chronik-server cluster add-node 4 --kafka node4:9092 --wal node4:9291 --raft node4:5001 --config cluster.toml

# Check status
./chronik-server cluster status --config cluster.toml

# Remove a node gracefully
./chronik-server cluster remove-node 4 --config cluster.toml
```

See [docs/RUNNING_A_CLUSTER.md](docs/RUNNING_A_CLUSTER.md) for the complete cluster guide.

## Kafka Compatibility

Chronik implements 24 Kafka wire protocol APIs and is tested with real clients:

| API | Versions | API | Versions |
|-----|----------|-----|----------|
| Produce | v0-v9 | ApiVersions | v0-v3 |
| Fetch | v0-v13 | CreateTopics | v0-v7 |
| ListOffsets | v0-v7 | DeleteTopics | v0-v6 |
| Metadata | v0-v12 | CreatePartitions | v0-v3 |
| OffsetCommit | v0-v8 | InitProducerId | v0-v4 |
| OffsetFetch | v0-v8 | AddPartitionsToTxn | v0-v3 |
| FindCoordinator | v0-v4 | AddOffsetsToTxn | v0-v3 |
| JoinGroup | v0-v9 | EndTxn | v0-v3 |
| SyncGroup | v0-v5 | TxnOffsetCommit | v0-v3 |
| Heartbeat | v0-v4 | SaslHandshake | v0-v1 |
| LeaveGroup | v0-v5 | SaslAuthenticate | v0-v2 |
| DescribeGroups | v0-v5 | ListGroups | v0-v4 |

**Tested clients:** kafka-python, confluent-kafka, KSQLDB, Apache Flink.

Chronik also provides a Confluent-compatible **Schema Registry** (Avro, JSON Schema, Protobuf) on the admin API port. See [docs/SCHEMA_REGISTRY.md](docs/SCHEMA_REGISTRY.md).

## Configuration Reference

### Commands

```bash
chronik-server start [OPTIONS]          # Start server (auto-detects single-node or cluster)
chronik-server cluster <SUBCOMMAND>     # Manage cluster (add-node, remove-node, status)
chronik-server version                  # Display version info
chronik-server compact <SUBCOMMAND>     # Manage WAL compaction
```

### Key Environment Variables

```bash
# Server
CHRONIK_DATA_DIR              # Data directory (default: ./data)
CHRONIK_ADVERTISED_ADDR       # Address advertised to Kafka clients (REQUIRED for Docker)
CHRONIK_UNIFIED_API_PORT      # Unified API port (default: 6092)
RUST_LOG                      # Log level: error, warn, info, debug, trace

# Performance
CHRONIK_WAL_PROFILE           # WAL batch interval: low (2ms), medium (10ms), high (50ms), ultra (100ms)
CHRONIK_PRODUCE_PROFILE       # Flush profile: low-latency, balanced, high-throughput, extreme

# SQL / Columnar
CHRONIK_HOT_BUFFER_ENABLED    # Enable in-memory SQL queries (default: true)
CHRONIK_HOT_BUFFER_MAX_RECORDS  # Max records per partition in hot buffer (default: 100000)

# Vector / Embeddings
OPENAI_API_KEY                # OpenAI API key for vector embeddings
CHRONIK_EMBEDDING_PROVIDER    # Embedding provider: openai, external

# Object Storage (tiered storage)
OBJECT_STORE_BACKEND          # Backend: s3, gcs, azure, local (default: local)
S3_BUCKET                     # S3 bucket name
S3_REGION                     # AWS region
S3_ENDPOINT                   # Custom endpoint (e.g., MinIO)

# Cluster
CHRONIK_ADMIN_API_KEY         # Admin API authentication (REQUIRED for production clusters)
```

## Security

```bash
# SASL authentication (PLAIN, SCRAM-SHA-256, SCRAM-SHA-512)
CHRONIK_SASL_USERS='myuser:securepass123,app:app-secret' ./chronik-server start

# Disable default test users in production
CHRONIK_SASL_NO_DEFAULTS=1 ./chronik-server start
```

Admin API and Schema Registry support API key and HTTP Basic Auth respectively. See [docs/ADMIN_API_SECURITY.md](docs/ADMIN_API_SECURITY.md).

## Performance Tuning

### WAL Profiles

The WAL auto-detects system resources and selects an appropriate batch interval. Override when needed:

```bash
CHRONIK_WAL_PROFILE=low        # Containers, small VMs (2ms batch)
CHRONIK_WAL_PROFILE=medium     # Typical servers (10ms batch)
CHRONIK_WAL_PROFILE=high       # Dedicated servers (50ms batch)
CHRONIK_WAL_PROFILE=ultra      # Maximum throughput (100ms batch)
```

### Producer Flush Profiles

| Profile | Flush Interval | Buffer | Use Case |
|---------|----------------|--------|----------|
| `low-latency` | 10ms | 16MB | Real-time analytics, instant messaging |
| `balanced` | 100ms | 32MB | General-purpose workloads |
| `high-throughput` | 500ms | 128MB | Data pipelines, ETL, batch processing |
| `extreme` | 2000ms | 512MB | Bulk ingestion, data migrations |

## Project Structure

```
chronik-stream/
├── crates/
│   ├── chronik-server/       # Unified server binary
│   ├── chronik-protocol/     # Kafka wire protocol (24 APIs)
│   ├── chronik-wal/          # Write-Ahead Log, group commit, recovery
│   ├── chronik-storage/      # Storage abstraction, segment I/O, WalIndexer
│   ├── chronik-columnar/     # Arrow/Parquet, DataFusion SQL, HNSW vector search
│   ├── chronik-embeddings/   # Embedding providers (OpenAI, external)
│   ├── chronik-search/       # Tantivy full-text search
│   ├── chronik-query/        # Query processing
│   ├── chronik-common/       # Shared types, metadata traits
│   ├── chronik-raft/         # Raft consensus
│   ├── chronik-raft-bridge/  # Raft integration bridge
│   ├── chronik-operator/     # Kubernetes operator (ChronikCluster CRD)
│   ├── chronik-auth/         # SASL, TLS, ACLs
│   ├── chronik-monitoring/   # Prometheus metrics
│   ├── chronik-config/       # Configuration management
│   ├── chronik-backup/       # Backup and disaster recovery
│   └── chronik-bench/        # Benchmarking tool
├── tests/                    # Integration tests
├── docs/                     # Guides and references
└── .github/workflows/        # CI/CD
```

## Project Status

Chronik Stream is in active development. The core streaming, SQL, search, and vector capabilities are validated at scale (43.9M records, 3-node cluster). The Kafka protocol layer has been tested with real clients across production workloads.

**Current version: v2.4.1** — See [CHANGELOG.md](CHANGELOG.md) for release history.

**Roadmap priorities:**
- Cross-encoder reranking for search quality (Phase 10)
- Query governance and policy controls (Phase 11)
- Production hardening and ecosystem integrations

See [docs/ROADMAP_RELEVANCE_ENGINE.md](docs/ROADMAP_RELEVANCE_ENGINE.md) for the full roadmap.

## Documentation

| Guide | Description |
|-------|-------------|
| [CHANGELOG.md](CHANGELOG.md) | Release history |
| [docs/RUNNING_A_CLUSTER.md](docs/RUNNING_A_CLUSTER.md) | Cluster setup guide |
| [docs/COLUMNAR_STORAGE_GUIDE.md](docs/COLUMNAR_STORAGE_GUIDE.md) | SQL and Parquet storage |
| [docs/SEARCHABLE_TOPICS.md](docs/SEARCHABLE_TOPICS.md) | Full-text search |
| [docs/VECTOR_SEARCH_GUIDE.md](docs/VECTOR_SEARCH_GUIDE.md) | Vector search and embeddings |
| [docs/SCHEMA_REGISTRY.md](docs/SCHEMA_REGISTRY.md) | Confluent-compatible Schema Registry |
| [docs/KSQL_INTEGRATION_GUIDE.md](docs/KSQL_INTEGRATION_GUIDE.md) | KSQLDB integration |
| [docs/API_REFERENCE.md](docs/API_REFERENCE.md) | Unified REST API reference |
| [docs/DATASET_VALIDATION_REPORT.md](docs/DATASET_VALIDATION_REPORT.md) | Scale and quality validation |
| [docs/DISASTER_RECOVERY.md](docs/DISASTER_RECOVERY.md) | Backup and recovery |
| [docs/ADMIN_API_SECURITY.md](docs/ADMIN_API_SECURITY.md) | Admin API security |
| [BASELINE_PERFORMANCE.md](BASELINE_PERFORMANCE.md) | Performance benchmarks |
| [BARE_METAL_PERFORMANCE.md](BARE_METAL_PERFORMANCE.md) | Cluster throughput testing |

## Contributing

Contributions are welcome. Please feel free to submit a Pull Request.

## License

Apache License 2.0. See [LICENSE](LICENSE) for details.
