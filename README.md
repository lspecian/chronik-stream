# Chronik Stream

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
                        │  Sled Database  │
                        └─────────────────┘
```

## Quick Start

### Using Docker Compose

```bash
docker-compose up -d
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

## Components

### Controller Node
Manages cluster metadata, coordinates brokers, and handles administrative operations using a simplified Raft consensus protocol. Uses embedded Sled database for metadata persistence.

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
- `METADATA_PATH`: Path to Sled metadata database (controller nodes)
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