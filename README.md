# Chronik Stream

> ⚠️ **EXPERIMENTAL SOFTWARE - EARLY ALPHA VERSION** ⚠️
> 
> **This is the very first version (v0.1.0-alpha) of Chronik Stream, an experimental distributed streaming platform.**
> 
> **IMPORTANT DISCLAIMERS:**
> - This software is in early development and should **NOT** be used in production environments
> - APIs, protocols, and storage formats may change without notice
> - Data loss, corruption, or service interruptions may occur
> - Security features are incomplete and not suitable for sensitive data
> - Performance optimizations are ongoing and current benchmarks are not representative
> - Documentation may be incomplete or outdated
> 
> **This project is intended for:**
> - Testing and experimentation
> - Development and proof-of-concept work
> - Contributing to the open-source community
> - Learning about distributed systems
> 
> **By using this software, you acknowledge that:**
> - You understand the experimental nature and associated risks
> - You will not use it for production workloads or critical data
> - The authors and contributors are not liable for any issues arising from its use
> 
> For production streaming needs, please use established solutions like Apache Kafka, Redpanda, or AWS Kinesis.

---

A high-performance, Kafka-compatible distributed streaming platform with built-in search capabilities and real-time analytics.

## Features

- **Kafka Wire Protocol Compatibility**: Drop-in replacement for Apache Kafka with full protocol support
- **Built-in Search**: Full-text search on message content powered by an integrated search engine
- **Real-time Analytics**: Stream processing with windowed aggregations
- **Cloud-Native Storage**: Pluggable object storage backends (S3, GCS, Azure Blob, Local)
- **Kubernetes Native**: Custom operator for easy deployment and management
- **Multi-tenancy**: Built-in authentication, authorization, and resource isolation
- **Observability**: Prometheus metrics and OpenTelemetry tracing

## Architecture

```
┌─────────────────┐     ┌─────────────────┐     ┌─────────────────┐
│   Kafka Client  │────▶│  Ingest Node    │────▶│ Object Storage  │
└─────────────────┘     └─────────────────┘     └─────────────────┘
                               │                          │
                               ▼                          ▼
                        ┌─────────────────┐     ┌─────────────────┐
                        │ Controller Node │     │ Search Service  │
                        └─────────────────┘     └─────────────────┘
                               │
                               ▼
                        ┌─────────────────┐
                        │  TiKV Cluster   │
                        └─────────────────┘
```

## Quick Start

### Using Docker Images

#### All-in-One Image (Simplest)

The all-in-one image contains all Chronik Stream services in a single container, perfect for development, testing, and small deployments:

```bash
# Pull the all-in-one image from GitHub Container Registry
docker pull ghcr.io/lspecian/chronik-stream:latest

# Run with embedded storage (development/testing)
docker run -d --name chronik \
  -p 9092:9092    # Kafka API
  -p 3000:3000    # Admin API
  -p 9090:9090    # Metrics
  -v chronik-data:/data \
  ghcr.io/lspecian/chronik-stream:latest

# Run with external storage (production)
docker run -d --name chronik \
  -p 9092:9092 -p 3000:3000 -p 9090:9090 \
  -v chronik-data:/data \
  -e STORAGE_BACKEND=s3 \
  -e S3_BUCKET=my-chronik-bucket \
  -e AWS_ACCESS_KEY_ID=your-key \
  -e AWS_SECRET_ACCESS_KEY=your-secret \
  ghcr.io/lspecian/chronik-stream:latest

# Or use docker-compose for easier management
curl -O https://raw.githubusercontent.com/lspecian/chronik-stream/main/docker-compose.all-in-one.yml
docker-compose -f docker-compose.all-in-one.yml up -d
```

#### Individual Service Images

For production deployments, you can use individual service images for better resource management and scaling:

```bash
# Available images:
# - ghcr.io/lspecian/chronik-stream-ingest:latest     # Kafka protocol handler
# - ghcr.io/lspecian/chronik-stream-controller:latest # Metadata & coordination
# - ghcr.io/lspecian/chronik-stream-search:latest     # Search service
# - ghcr.io/lspecian/chronik-stream-admin:latest      # Admin API

# Example: Run controller
docker run -d --name chronik-controller \
  -p 9090:9090 \
  -v controller-data:/data \
  ghcr.io/lspecian/chronik-stream-controller:latest

# Example: Run ingest node
docker run -d --name chronik-ingest \
  -p 9092:9092 \
  -e CONTROLLER_ADDR=chronik-controller:9090 \
  ghcr.io/lspecian/chronik-stream-ingest:latest
```

#### Docker Compose Deployment

For a complete multi-service deployment:

```yaml
# docker-compose.yml
version: '3.8'

services:
  controller:
    image: ghcr.io/lspecian/chronik-stream-controller:latest
    ports:
      - "9090:9090"
    volumes:
      - controller-data:/data
    environment:
      - RUST_LOG=info

  ingest:
    image: ghcr.io/lspecian/chronik-stream-ingest:latest
    ports:
      - "9092:9092"
    environment:
      - CONTROLLER_ADDR=controller:9090
      - RUST_LOG=info
    depends_on:
      - controller

  search:
    image: ghcr.io/lspecian/chronik-stream-search:latest
    ports:
      - "9200:9200"
    volumes:
      - search-data:/data
    environment:
      - CONTROLLER_ADDR=controller:9090
      - RUST_LOG=info
    depends_on:
      - controller

  admin:
    image: ghcr.io/lspecian/chronik-stream-admin:latest
    ports:
      - "3000:3000"
    environment:
      - CONTROLLER_ADDR=controller:9090
      - RUST_LOG=info
    depends_on:
      - controller

volumes:
  controller-data:
  search-data:
```

### Using Snap (Linux)

```bash
sudo snap install chronik-stream
```

### Using Binary

```bash
# Download latest release
curl -L https://github.com/lspecian/chronik-stream/releases/latest/download/chronik-linux-x86_64 -o chronik
chmod +x chronik
./chronik start
```

### Using Kubernetes

```bash
# Install the operator
kubectl apply -f deploy/k8s/operator.yaml

# Create a Chronik cluster
kubectl apply -f deploy/k8s/chronik-cluster-example.yaml
```

### Using the CLI

```bash
# Install the CLI
cargo install --path crates/chronik-cli

# Create a topic
chronik-ctl topic create events -p 6 -r 3

# List topics
chronik-ctl topic list

# Produce messages (using Kafka client)
kafka-console-producer --broker-list localhost:9092 --topic events

# Consume messages (using Kafka client)
kafka-console-consumer --bootstrap-server localhost:9092 --topic events --from-beginning
```

## Cloud Deployment

### Deploy to Hetzner Cloud

```bash
# Download deployment script
curl -O https://raw.githubusercontent.com/lspecian/chronik-stream/main/scripts/deploy-hetzner.sh
chmod +x deploy-hetzner.sh

# Deploy single all-in-one instance
./deploy-hetzner.sh

# Deploy cluster (3 nodes)
./deploy-hetzner.sh cluster

# Destroy deployment
./deploy-hetzner.sh destroy
```

### Deploy to AWS

```bash
# Download deployment script
curl -O https://raw.githubusercontent.com/lspecian/chronik-stream/main/scripts/deploy-aws.sh
chmod +x deploy-aws.sh

# Deploy single instance with persistent storage
./deploy-aws.sh with-storage 100  # 100GB EBS volume

# Deploy cluster
./deploy-aws.sh cluster

# Destroy deployment
./deploy-aws.sh destroy
```

## Docker Images

All images are available from GitHub Container Registry (ghcr.io):

| Image | Description | Size | Use Case |
|-------|-------------|------|----------|
| `ghcr.io/lspecian/chronik-stream:latest` | All-in-one image with all services | ~91MB | Development, testing, small deployments |
| `ghcr.io/lspecian/chronik-stream-ingest:latest` | Kafka protocol handler | ~119MB | Production ingestion nodes |
| `ghcr.io/lspecian/chronik-stream-controller:latest` | Metadata & coordination | ~95MB | Production control plane |
| `ghcr.io/lspecian/chronik-stream-search:latest` | Search service | ~90MB | Production search nodes |
| `ghcr.io/lspecian/chronik-stream-admin:latest` | Admin API | ~111MB | Production management |

### Environment Variables

#### All-in-One Image
```bash
CHRONIK_DATA_DIR=/data          # Data directory (default: /data)
STORAGE_BACKEND=embedded        # Storage backend: embedded, s3, gcs, azure
RUST_LOG=info                   # Log level: debug, info, warn, error
KAFKA_PORT=9092                 # Kafka API port
ADMIN_PORT=3000                 # Admin API port
METRICS_PORT=9090               # Prometheus metrics port
```

#### Storage Configuration
```bash
# S3 Backend
STORAGE_BACKEND=s3
S3_BUCKET=my-chronik-bucket
S3_REGION=us-east-1
AWS_ACCESS_KEY_ID=xxx
AWS_SECRET_ACCESS_KEY=xxx

# GCS Backend
STORAGE_BACKEND=gcs
GCS_BUCKET=my-chronik-bucket
GOOGLE_APPLICATION_CREDENTIALS=/path/to/credentials.json

# Azure Backend
STORAGE_BACKEND=azure
AZURE_CONTAINER=my-chronik-container
AZURE_STORAGE_ACCOUNT=myaccount
AZURE_STORAGE_KEY=xxx
```

#### Service-Specific Configuration
```bash
# Controller
CONTROLLER_PORT=9090
METADATA_STORE=tikv           # tikv or embedded
TIKV_PD_ENDPOINTS=pd1:2379,pd2:2379

# Ingest
CONTROLLER_ADDR=controller:9090
MAX_MESSAGE_SIZE=1048576      # 1MB default
COMPRESSION_TYPE=snappy       # none, gzip, snappy, lz4, zstd

# Search
INDEX_PATH=/data/index
MAX_INDEX_SIZE=10737418240    # 10GB default
SEARCH_PORT=9200
```

### Multi-arch Support

Currently supporting `linux/amd64`. ARM64 support coming soon.

## Kafka Client Compatibility

Chronik Stream is fully compatible with standard Kafka clients:

- ✅ **kafkactl** - CLI tool for Kafka operations
- ✅ **Sarama** - Go client library
- ✅ **confluent-kafka-python** - Python client with C bindings
- ✅ **librdkafka** - High-performance C/C++ library
- ✅ **kafka-clients** - Official Java client
- ✅ **node-rdkafka** - Node.js bindings for librdkafka

See the [Kafka Client Compatibility Guide](docs/kafka-client-compatibility.md) for detailed testing results and examples.

## Components

### Controller Node
Manages cluster metadata, coordinates brokers, and handles administrative operations using a simplified Raft consensus protocol. Uses distributed TiKV cluster for metadata persistence.

### Ingest Node
Handles Kafka protocol connections, processes produce/fetch requests, and manages data indexing.

### Storage Layer
Provides abstraction over object storage systems with segment-based data organization.

### Search Service
Enables full-text search queries on message content with support for complex queries.

### Query Engine
Processes real-time analytics queries with support for windowed aggregations.

### Admin API
RESTful API for cluster management with OpenAPI documentation.

### CLI Tool
Command-line interface for cluster administration and troubleshooting.

## Configuration

### Environment Variables

- `CHRONIK_ROLE`: Node role (controller, ingest, query)
- `TIKV_PD_ENDPOINTS`: TiKV PD endpoints for metadata storage (default: localhost:2379)
- `OBJECT_STORE_TYPE`: Storage backend (s3, gcs, azure, local)
- `KAFKA_LISTENERS`: Kafka protocol listen addresses

### Storage Configuration

```yaml
storage:
  backend: s3
  bucket: chronik-data
  region: us-east-1
  segment_size: 1073741824 # 1GB
  retention_ms: 604800000   # 7 days
```

## Development

### Prerequisites

- Rust 1.70+
- Docker & Docker Compose (for testing)

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
│   ├── chronik-admin/       # Admin API server
│   ├── chronik-auth/        # Authentication & authorization
│   ├── chronik-cli/         # Command-line tool
│   ├── chronik-common/      # Common utilities
│   ├── chronik-controller/  # Controller node
│   ├── chronik-ingest/      # Ingest service
│   ├── chronik-janitor/     # Maintenance service
│   ├── chronik-monitoring/  # Metrics & tracing
│   ├── chronik-operator/    # Kubernetes operator
│   ├── chronik-protocol/    # Kafka protocol
│   ├── chronik-query/       # Query engine
│   ├── chronik-search/      # Search service
│   └── chronik-storage/     # Storage abstraction
├── deploy/                  # Deployment configurations
├── docs/                    # Documentation
├── migrations/              # Database migrations
└── tests/                   # Integration tests
```

## Performance

Chronik Stream is designed for high throughput and low latency:

- **Throughput**: 1M+ messages/second per node
- **Latency**: < 10ms p99 produce latency
- **Search**: Sub-second full-text search on billions of messages
- **Storage**: Efficient compression with configurable retention

## Security

- **TLS/SSL**: End-to-end encryption for all connections
- **SASL Authentication**: Support for PLAIN, SCRAM-SHA-256/512
- **ACLs**: Fine-grained access control
- **JWT Tokens**: For admin API authentication

## Monitoring

### Metrics (Prometheus)

- `chronik_messages_received_total`: Total messages received
- `chronik_messages_stored_total`: Total messages stored
- `chronik_produce_latency_seconds`: Produce request latency
- `chronik_fetch_latency_seconds`: Fetch request latency
- `chronik_storage_usage_bytes`: Storage usage by backend

### Tracing (OpenTelemetry)

Distributed tracing is available for all components with support for Jaeger, Zipkin, and other OTLP-compatible backends.

## Contributing

We welcome contributions! Please see [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines.

## License

Apache License 2.0. See [LICENSE](LICENSE) for details.