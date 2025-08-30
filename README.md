# Chronik Stream v0.5.0

[![Build Status](https://github.com/lspecian/chronik-stream/workflows/CI/badge.svg)](https://github.com/lspecian/chronik-stream/actions)
[![Release](https://img.shields.io/github/v/release/lspecian/chronik-stream)](https://github.com/lspecian/chronik-stream/releases)
[![Docker Image](https://img.shields.io/badge/docker-ghcr.io-blue)](https://github.com/lspecian/chronik-stream/pkgs/container/chronik-stream)
[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.75%2B-orange.svg)](https://www.rust-lang.org)

A high-performance, Kafka-compatible distributed streaming platform built in Rust. Drop-in replacement for Apache Kafka with enhanced search capabilities and cloud-native storage.

## 🚀 Features

- **Full Kafka Protocol Compatibility**: Complete support for Kafka wire protocol v0-v9 with all 19 core APIs
- **Drop-in Replacement**: Works with all standard Kafka clients (Java, Python, Go, Node.js, etc.)
- **Built-in Search**: Full-text search on message content powered by Tantivy search engine
- **Cloud-Native Storage**: Pluggable object storage backends (S3, GCS, Azure Blob, Local)
- **High Performance**: Zero-copy networking, optimized for low latency (0% idle CPU usage)
- **Multi-Architecture**: Native support for x86_64 and ARM64 (Apple Silicon, AWS Graviton)
- **Observability**: Prometheus metrics and OpenTelemetry tracing

## 🏗️ Architecture

```
┌─────────────────┐     ┌─────────────────┐     ┌─────────────────┐
│   Kafka Client  │────▶│   Chronik       │────▶│ Object Storage  │
│  (Any Language) │     │   (All-in-One)  │     │  (S3/GCS/Local) │
└─────────────────┘     └─────────────────┘     └─────────────────┘
                               │
                               ├── Protocol Handler (Port 9092)
                               ├── Metadata Store
                               ├── Search Engine (Tantivy)
                               └── Storage Manager
```

## ⚡ Quick Start

### Using Docker (Recommended)

```bash
# Quick start - single command
docker run -d -p 9092:9092 ghcr.io/lspecian/chronik-stream:v0.5.0

# With persistent storage
docker run -d --name chronik \
  -p 9092:9092 \
  -v chronik-data:/data \
  ghcr.io/lspecian/chronik-stream:v0.5.0

# Using docker-compose
curl -O https://raw.githubusercontent.com/lspecian/chronik-stream/main/docker-compose.yml
docker-compose up -d
```

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

# Consumer
consumer = KafkaConsumer(
    'test-topic',
    bootstrap_servers='localhost:9092',
    api_version=(0, 10, 0),
    auto_offset_reset='earliest'
)
for message in consumer:
    print(message.value)
```

### Using Binary

```bash
# Download latest release (Linux x86_64)
curl -L https://github.com/lspecian/chronik-stream/releases/download/v0.5.0/chronik-linux-amd64.tar.gz -o chronik.tar.gz
tar xzf chronik.tar.gz
./chronik --bind-addr 0.0.0.0:9092

# macOS (Apple Silicon)
curl -L https://github.com/lspecian/chronik-stream/releases/download/v0.5.0/chronik-darwin-arm64.tar.gz -o chronik.tar.gz
tar xzf chronik.tar.gz
./chronik --bind-addr 0.0.0.0:9092
```

### Building from Source

```bash
# Clone repository
git clone https://github.com/lspecian/chronik-stream.git
cd chronik-stream

# Build release binary
cargo build --release --bin chronik

# Run
./target/release/chronik --bind-addr 0.0.0.0:9092
```


## 📦 Docker Images

All images support both **linux/amd64** and **linux/arm64** architectures:

| Image | Tags | Size | Description |
|-------|------|------|-------------|
| `ghcr.io/lspecian/chronik-stream` | `v0.5.0`, `0.5`, `latest` | ~50MB | All-in-one Chronik server |

### Supported Platforms

- ✅ **Linux x86_64** (amd64)
- ✅ **Linux ARM64** (aarch64) - AWS Graviton, Raspberry Pi 4+
- ✅ **macOS x86_64** (Intel)
- ✅ **macOS ARM64** (Apple Silicon M1/M2/M3)



## ✅ Kafka Compatibility

### Supported Kafka APIs (19 total)

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

### Compatible Clients

- ✅ **kafka-python** - Python client (specify `api_version=(0,10,0)`)
- ✅ **confluent-kafka-python** - Python client with C bindings
- ✅ **sarama** - Go client library
- ✅ **librdkafka** - High-performance C/C++ library
- ✅ **kafka-clients** - Official Java client
- ✅ **node-rdkafka** - Node.js bindings

## 🔧 Configuration

### Command Line Options

```bash
chronik [OPTIONS]

Options:
  --bind-addr <ADDR>      Bind address (default: 0.0.0.0:9092)
  --data-dir <PATH>       Data directory (default: ./data)
  --log-level <LEVEL>     Log level: debug, info, warn, error (default: info)
  --storage <TYPE>        Storage backend: local, s3, gcs, azure (default: local)
  --help                  Print help
  --version               Print version
```

### Environment Variables

```bash
# Core Settings
RUST_LOG=info                    # Log level
CHRONIK_DATA_DIR=/data          # Data directory
CHRONIK_BIND_ADDR=0.0.0.0:9092 # Bind address

# Storage Configuration (S3)
STORAGE_BACKEND=s3
S3_BUCKET=my-chronik-bucket
S3_REGION=us-east-1
AWS_ACCESS_KEY_ID=xxx
AWS_SECRET_ACCESS_KEY=xxx

# Storage Configuration (Local)
STORAGE_BACKEND=local
LOCAL_STORAGE_PATH=/data/segments
```


## 🛠️ Development

### Prerequisites

- Rust 1.75+
- Docker & Docker Compose (for testing)
- Python 3.8+ with kafka-python (for client testing)

### Building

```bash
# Build all components
cargo build --release

# Run tests
cargo test

# Run benchmarks
cargo bench
```

### Project Structure

```
chronik-stream/
├── crates/
│   ├── chronik-all-in-one/  # Main server binary
│   ├── chronik-protocol/    # Kafka wire protocol implementation
│   ├── chronik-storage/     # Storage abstraction layer
│   ├── chronik-ingest/      # Message ingestion service
│   ├── chronik-search/      # Search engine integration
│   ├── chronik-query/       # Query processing
│   ├── chronik-common/      # Shared utilities
│   ├── chronik-auth/        # Authentication & authorization
│   ├── chronik-monitoring/  # Metrics & observability
│   ├── chronik-config/      # Configuration management
│   └── chronik-janitor/     # Maintenance tasks
├── tests/                   # Integration tests
├── Dockerfile              # Multi-arch Docker build
├── docker-compose.yml      # Local development setup
└── .github/workflows/      # CI/CD pipelines
```

## ⚡ Performance

Chronik Stream is optimized for production workloads:

- **CPU Usage**: 0% idle (down from 163% in v0.4.0)
- **Memory**: Efficient memory usage with zero-copy networking
- **Latency**: < 10ms p99 produce latency
- **Throughput**: 100K+ messages/second per node
- **Search**: Sub-second full-text search with Tantivy
- **Storage**: Efficient compression (Snappy, LZ4, Zstd)

## 🔒 Security

- **SASL Authentication**: PLAIN, SCRAM-SHA-256/512
- **TLS/SSL**: End-to-end encryption
- **ACLs**: Topic and consumer group access control
- **Audit Logging**: Track all administrative actions

## 📊 Monitoring

### Prometheus Metrics

```bash
# Expose metrics endpoint
chronik --metrics-port 9090

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

## 🚀 Latest Release: v0.5.0

### What's New
- ✅ Complete Kafka protocol compatibility with all 19 core APIs
- ✅ Fixed ApiVersions negotiation for proper client compatibility
- ✅ Optimized performance: 0% idle CPU usage (was 163%)
- ✅ Multi-architecture Docker images (amd64, arm64)
- ✅ Improved broker advertisement for reliable connections
- ✅ Production-ready with comprehensive error handling

### Breaking Changes
None - fully backward compatible with Kafka clients.

### Known Issues
- Python kafka-python client requires `api_version=(0,10,0)` parameter
- Some older Kafka clients (< 0.10) may need explicit version configuration