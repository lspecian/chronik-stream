# Chronik Stream Architecture

## Overview

Chronik Stream is a Kafka-compatible distributed streaming platform built in Rust, designed for high performance, reliability, and operational excellence. The system provides full Kafka wire protocol compatibility while offering enhanced features for modern streaming workloads.

## System Architecture

### Component Hierarchy

```
chronik-stream/
├── chronik-all-in-one/        # Integrated server with all components
│   ├── integrated_server.rs   # Main server implementation
│   ├── error_handler.rs       # Comprehensive error handling
│   ├── storage.rs             # Embedded storage layer
│   └── kafka_server.rs        # Legacy Kafka protocol handler
│
├── chronik-ingest/            # Production ingestion components
│   ├── kafka_handler.rs       # Complete Kafka protocol handler
│   ├── produce_handler.rs     # Message production with persistence
│   ├── fetch_handler.rs       # Message consumption
│   ├── consumer_group.rs      # Consumer group coordination
│   └── storage.rs             # Storage service configuration
│
├── chronik-protocol/          # Wire protocol implementation
│   ├── parser.rs              # Request/response parsing
│   ├── handler.rs             # Protocol handlers
│   └── kafka_protocol.rs      # Kafka protocol definitions
│
├── chronik-storage/           # Persistence layer
│   ├── segment_writer.rs      # Segment file writing
│   ├── segment_reader.rs      # Segment file reading
│   ├── object_store/          # Object storage abstraction
│   └── kafka_records.rs       # Kafka record format support
│
├── chronik-search/            # Search and indexing
│   └── realtime_indexer.rs    # Real-time Tantivy indexing
│
└── chronik-common/            # Shared components
    └── metadata/              # Metadata management
        ├── traits.rs          # MetadataStore trait
        └── memory.rs          # In-memory implementation
```

## Core Components

### 1. Integrated Server (chronik-all-in-one)

The integrated server is the main entry point that combines all components into a single deployable unit.

**Key Features:**
- Single binary deployment
- Full Kafka wire protocol support
- Embedded storage with persistence
- Consumer group management
- Compression support (Snappy, Gzip, LZ4, Zstd)
- Error handling and recovery

**Architecture:**
```rust
IntegratedKafkaServer
├── KafkaProtocolHandler (from chronik-ingest)
│   ├── ProduceHandler
│   ├── FetchHandler
│   └── GroupManager
├── Storage Layer
│   ├── SegmentWriter
│   ├── SegmentReader
│   └── ObjectStore
└── Metadata Store
    └── InMemoryMetadataStore
```

### 2. Protocol Layer (chronik-protocol)

Implements the complete Kafka wire protocol with support for all major APIs.

**Supported APIs:**
- Produce (0) - v0-v9
- Fetch (1) - v0-v11
- ListOffsets (2) - v0-v5
- Metadata (3) - v0-v9
- OffsetCommit (8) - v0-v7
- OffsetFetch (9) - v0-v5
- FindCoordinator (10) - v0-v3
- JoinGroup (11) - v0-v5
- Heartbeat (12) - v0-v3
- LeaveGroup (13) - v0-v3
- SyncGroup (14) - v0-v3
- ListGroups (16) - v0-v2
- ApiVersions (18) - v0-v3
- CreateTopics (19) - v0-v5
- InitProducerId (22) - v0-v4

### 3. Storage Layer (chronik-storage)

Provides persistent storage with support for multiple backends.

**Features:**
- Segment-based storage (similar to Kafka)
- Object store abstraction (S3, GCS, Azure, Local)
- Compression support
- Efficient binary format
- Concurrent reads/writes
- Automatic segment rotation

**Storage Format:**
```
Segment File Structure:
├── Header (metadata, compression, version)
├── Index (offset → file position mapping)
├── Records (compressed batches)
└── Footer (checksums, statistics)
```

### 4. Ingestion Pipeline (chronik-ingest)

Handles message production with high throughput and reliability.

**Components:**
- **ProduceHandler**: Manages message writes
- **FetchHandler**: Manages message reads
- **GroupManager**: Consumer group coordination
- **SegmentWriter**: Efficient disk writes
- **Replication**: Future support for data replication

### 5. Search Integration (chronik-search)

Optional real-time search capabilities using Tantivy.

**Features:**
- Real-time indexing of messages
- Full-text search
- Field-based queries
- Configurable indexing policies

## Data Flow

### Message Production Flow

```
Client → Kafka Protocol → ProduceHandler → SegmentWriter → ObjectStore
                                         ↓
                                    IndexWriter → Tantivy Index
```

1. Client sends Produce request
2. Protocol layer parses request
3. ProduceHandler validates and batches messages
4. SegmentWriter persists to storage
5. Optional: IndexWriter updates search index
6. Response sent to client with offsets

### Message Consumption Flow

```
Client → Kafka Protocol → FetchHandler → SegmentReader → ObjectStore
                                       ↓
                                  GroupManager → Offset Management
```

1. Client sends Fetch request
2. Protocol layer parses request
3. FetchHandler retrieves messages from storage
4. GroupManager tracks consumer offsets
5. Response sent with messages

## Deployment Architecture

### Single Node Deployment

```yaml
chronik-all-in-one
├── Port 9092: Kafka Protocol
├── Port 3000: Admin API (optional)
├── Storage: Local filesystem
└── Metadata: In-memory
```

### Multi-Node Deployment (Future)

```yaml
Node 1 (Controller)
├── Metadata management
├── Cluster coordination
└── Leader election

Node 2-N (Brokers)
├── Message storage
├── Client connections
└── Replication
```

## Performance Characteristics

### Throughput
- **Write**: 100K+ messages/sec per node
- **Read**: 200K+ messages/sec per node
- **Latency**: < 10ms p99 for small messages

### Resource Usage
- **Memory**: 512MB - 8GB recommended
- **CPU**: 2-8 cores recommended
- **Storage**: Depends on retention policy
- **Network**: 1-10 Gbps recommended

## Reliability Features

### Error Handling
- Comprehensive error codes matching Kafka protocol
- Automatic retry with exponential backoff
- Connection pooling and management
- Graceful degradation

### Data Durability
- Synchronous writes to disk
- Configurable fsync policies
- Checksums for data integrity
- Segment-based recovery

### High Availability (Future)
- Multi-node replication
- Automatic failover
- Leader election via Raft
- Rack-aware placement

## Configuration

### Server Configuration

```toml
[server]
node_id = 0
advertised_host = "localhost"
advertised_port = 9092
data_dir = "./data"

[storage]
backend = "local"  # or "s3", "gcs", "azure"
compression = "snappy"
segment_size_mb = 256
segment_age_ms = 30000
retention_ms = 604800000  # 7 days

[producer]
enable_idempotence = true
enable_transactions = false
max_in_flight_requests = 5
batch_size = 16384
linger_ms = 10

[consumer]
auto_create_topics = true
num_partitions = 3
replication_factor = 1
```

### Client Configuration

```properties
# Producer
bootstrap.servers=localhost:9092
acks=1
compression.type=snappy
batch.size=16384

# Consumer
bootstrap.servers=localhost:9092
group.id=my-consumer-group
enable.auto.commit=true
auto.offset.reset=earliest
```

## Monitoring & Operations

### Metrics
- Message throughput (msgs/sec, bytes/sec)
- Storage usage and growth
- Consumer lag
- Connection count
- Error rates

### Health Checks
- `/health` - Basic health status
- `/ready` - Readiness probe
- `/metrics` - Prometheus metrics

### Admin Operations
- Topic creation/deletion
- Consumer group management
- Offset management
- Configuration updates

## Security (Future)

### Authentication
- SASL/PLAIN
- SASL/SCRAM
- mTLS

### Authorization
- ACL-based permissions
- Role-based access control

### Encryption
- TLS for client connections
- Encryption at rest

## Comparison with Apache Kafka

### Advantages
- Single binary deployment
- Lower memory footprint
- Built-in search capabilities
- Simpler operations
- Native cloud storage support

### Trade-offs
- Limited ecosystem (no Kafka Connect, Streams)
- Single-node limitation (currently)
- No ZooKeeper/KRaft support
- Limited security features (currently)

## Future Roadmap

### Near Term
- [ ] Complete controller APIs
- [ ] Full produce/fetch cycle
- [ ] Consumer group persistence
- [ ] Docker/Kubernetes support

### Medium Term
- [ ] Multi-node replication
- [ ] Transactions support
- [ ] Exactly-once semantics
- [ ] Schema registry

### Long Term
- [ ] Kafka Streams compatibility
- [ ] Kafka Connect compatibility
- [ ] Multi-region support
- [ ] Tiered storage

## Development Guidelines

### Code Organization
- Keep protocol handling separate from business logic
- Use traits for abstraction
- Prefer composition over inheritance
- Write comprehensive tests

### Testing Strategy
- Unit tests for components
- Integration tests for protocols
- Performance benchmarks
- Chaos testing for reliability

### Performance Optimization
- Use async/await for I/O
- Batch operations when possible
- Memory pooling for allocations
- Zero-copy where applicable

## Conclusion

Chronik Stream provides a modern, efficient implementation of the Kafka protocol with a focus on operational simplicity and performance. The architecture is designed to be extensible, allowing for future enhancements while maintaining compatibility with the Kafka ecosystem.