# Chronik Stream

[![Build Status](https://github.com/lspecian/chronik-stream/workflows/CI/badge.svg)](https://github.com/lspecian/chronik-stream/actions)
[![Release](https://img.shields.io/github/v/release/lspecian/chronik-stream)](https://github.com/lspecian/chronik-stream/releases)
[![Docker Image](https://img.shields.io/badge/docker-ghcr.io-blue)](https://github.com/lspecian/chronik-stream/pkgs/container/chronik-stream)
[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.75%2B-orange.svg)](https://www.rust-lang.org)

A high-performance streaming platform built in Rust that implements core Kafka wire protocol functionality with comprehensive Write-Ahead Log (WAL) durability and automatic recovery.

See [CHANGELOG.md](CHANGELOG.md) for release history and latest updates.

## 🚀 Features

- **Kafka Wire Protocol**: Full Kafka wire protocol with consumer group and transactional support
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

## 🏗️ Architecture

```
┌─────────────────┐     ┌─────────────────────────────────────────┐
│   Kafka Client  │────▶│            Chronik Server               │
│  (Any Language) │     │  ┌─────────────┐  ┌─────────────────┐  │
└─────────────────┘     │  │ Kafka Proto │  │  ChronikMetaLog │  │
                        │  │ Handler     │  │  (WAL Metadata) │  │
                        │  │ (Port 9092) │  │                 │  │
                        │  └─────────────┘  └─────────────────┘  │
                        │  ┌─────────────┐  ┌─────────────────┐  │
                        │  │   Search    │  │  Storage Mgr    │  │
                        │  │  (Tantivy)  │  │                 │  │
                        │  └─────────────┘  └─────────────────┘  │
                        └─────────────────────────────────────────┘
                                            │
                                            ▼
                                ┌─────────────────────┐
                                │   Object Storage    │
                                │  (S3/GCS/Local)     │
                                └─────────────────────┘
```

## ⚡ Quick Start

### Using Docker (Recommended)

```bash
# Quick start - single command
docker run -d -p 9092:9092 \
  -e CHRONIK_ADVERTISED_ADDR=localhost \
  ghcr.io/lspecian/chronik-stream:latest

# With persistent storage and custom configuration
docker run -d --name chronik \
  -p 9092:9092 \
  -v chronik-data:/data \
  -e CHRONIK_ADVERTISED_ADDR=localhost \
  -e RUST_LOG=info \
  ghcr.io/lspecian/chronik-stream:latest

# Using docker-compose
curl -O https://raw.githubusercontent.com/lspecian/chronik-stream/main/docker-compose.yml
docker-compose up -d
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

# Producer
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

### Using Binary

```bash
# Download latest release (Linux x86_64)
curl -L https://github.com/lspecian/chronik-stream/releases/latest/download/chronik-server-linux-amd64.tar.gz -o chronik-server.tar.gz
tar xzf chronik-server.tar.gz
./chronik-server --advertised-addr localhost standalone

# macOS (Apple Silicon)
curl -L https://github.com/lspecian/chronik-stream/releases/latest/download/chronik-server-darwin-arm64.tar.gz -o chronik-server.tar.gz
tar xzf chronik-server.tar.gz
./chronik-server --advertised-addr localhost standalone
```

### Building from Source

```bash
# Clone repository
git clone https://github.com/lspecian/chronik-stream.git
cd chronik-stream

# Build release binary
cargo build --release --bin chronik-server

# Run
./target/release/chronik-server standalone
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

The unified `chronik-server` binary supports multiple operational modes:

### Standalone Mode (Default)
Single-node Kafka-compatible server, perfect for development and small deployments:
```bash
chronik-server standalone
# or just
chronik-server
```

### All Mode
Run all components (Kafka protocol, search, backup) in a single process:
```bash
chronik-server all
```

### Configuration Options
```bash
chronik-server [OPTIONS] [COMMAND]

Options:
  -p, --kafka-port <PORT>      Kafka protocol port (default: 9092)
  -a, --admin-port <PORT>      Admin API port (default: 3000)
  -d, --data-dir <PATH>        Data directory (default: ./data)
  -b, --bind-addr <ADDR>       Bind address (default: 0.0.0.0)
  --advertised-addr <ADDR>     Address advertised to clients (REQUIRED for Docker/remote access)
  --advertised-port <PORT>     Port advertised to clients (default: kafka port)
  --file-metadata              Use file-based metadata store instead of WAL-based (legacy mode)
  --enable-search              Enable search functionality
  --enable-backup              Enable backup functionality

Environment Variables:
  CHRONIK_KAFKA_PORT           Kafka protocol port
  CHRONIK_BIND_ADDR            Server bind address (just host, no port)
  CHRONIK_ADVERTISED_ADDR      Address advertised to clients (CRITICAL for Docker)
  CHRONIK_ADVERTISED_PORT      Port advertised to clients
  CHRONIK_DATA_DIR             Data directory path
  CHRONIK_FILE_METADATA        Set to "true" to use legacy file-based metadata store
  RUST_LOG                     Log level (error, warn, info, debug, trace)
```

## 📦 Docker Images

All images support both **linux/amd64** and **linux/arm64** architectures:

| Image | Tags | Description |
|-------|------|-------------|
| `ghcr.io/lspecian/chronik-stream` | `latest`, `1.3`, `v1.3.46` | Chronik server with full KSQL support |

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
│   ├── chronik-search/      # Search engine integration
│   ├── chronik-query/       # Query processing
│   ├── chronik-common/      # Shared utilities
│   ├── chronik-auth/        # Authentication & authorization
│   ├── chronik-monitoring/  # Metrics & observability
│   ├── chronik-config/      # Configuration management
│   ├── chronik-backup/      # Backup functionality
│   ├── chronik-benchmarks/  # Performance benchmarks
│   ├── chronik-cli/         # Command line interface
│   └── chronik-wal/         # Write-Ahead Log & metadata store
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

- [CHANGELOG.md](CHANGELOG.md) - Detailed release history
- [CLAUDE.md](CLAUDE.md) - Development guide for AI assistants
- [docs/KSQL_INTEGRATION_GUIDE.md](docs/KSQL_INTEGRATION_GUIDE.md) - KSQL setup and usage
