# Integration Test Suite Documentation

## Overview

Task #24 implements a comprehensive integration test suite for Chronik Stream that verifies Kafka protocol compatibility, end-to-end data flow, and search functionality across multi-node clusters.

## Architecture

### Test Framework Structure

```
tests/integration/
├── mod.rs              # Test module entry point
├── common.rs           # Shared test utilities
├── kafka_protocol.rs   # Kafka compatibility tests
├── data_flow.rs        # End-to-end pipeline tests
├── search.rs           # Search functionality tests
├── consumer_groups.rs  # Consumer coordination tests
└── cluster.rs          # Multi-node cluster tests
```

### Key Components

1. **TestCluster** - Orchestrates multi-node test environments
2. **Test Utilities** - Common helpers for Kafka operations
3. **Port Allocation** - Dynamic port assignment to avoid conflicts
4. **Health Checks** - Service readiness verification

## Test Categories

### 1. Kafka Protocol Compatibility (`kafka_protocol.rs`)

Tests verify full Kafka wire protocol support:
- Metadata API operations
- Topic creation/deletion
- Producer/consumer operations
- Consumer group management
- Offset commit/fetch

### 2. End-to-End Data Flow (`data_flow.rs`)

Tests the complete pipeline from produce to search:
- JSON document ingestion
- Real-time indexing
- Streaming updates
- Multi-partition ordering
- Large document handling (up to 5MB)

### 3. Search Functionality (`search.rs`)

Comprehensive search feature testing:
- Query types: match, term, range, bool, wildcard, prefix
- Aggregations: terms, stats, date histogram, nested
- Pagination with from/size and search_after
- Result highlighting
- Cross-index searches

### 4. Consumer Groups (`consumer_groups.rs`)

KIP-848 compliant consumer group tests:
- Rebalancing with partition redistribution
- Offset management and recovery
- Failure handling and recovery
- Incremental cooperative rebalancing

### 5. Multi-Node Clusters (`cluster.rs`)

Distributed system behavior tests:
- Multi-controller consensus
- Data distribution verification
- Node failure recovery
- Cross-node search queries
- Load balancing validation

## Test Utilities

### TestCluster

Manages complete test cluster lifecycle:

```rust
pub struct TestClusterConfig {
    pub num_controllers: usize,
    pub num_ingest_nodes: usize,
    pub num_search_nodes: usize,
    pub data_dir: Option<PathBuf>,
    pub object_storage: ObjectStorageType,
    pub enable_tls: bool,
    pub enable_auth: bool,
}
```

### Helper Functions

- `allocate_ports()` - Finds available ports for services
- `wait_for_tcp_endpoint()` - TCP service health check
- `wait_for_http_endpoint()` - HTTP service health check
- `create_test_producer()` - Kafka producer setup
- `create_test_consumer()` - Kafka consumer setup

## Running Tests

### Prerequisites

1. Build all services:
```bash
cargo build --release
```

2. Add binaries to PATH:
```bash
export PATH=$PATH:$(pwd)/target/release
```

### Execution

Run all integration tests:
```bash
cargo test --test integration -- --test-threads=1
```

Run specific test module:
```bash
cargo test --test integration kafka_protocol
```

Run with debug logging:
```bash
RUST_LOG=chronik=debug cargo test --test integration -- --nocapture
```

## Test Coverage

### Protocol Compliance
- ✅ Kafka metadata protocol
- ✅ Producer/consumer protocols
- ✅ Group coordination protocol
- ✅ Offset management

### Data Pipeline
- ✅ Produce → Store → Index → Search
- ✅ Real-time updates
- ✅ Partition ordering
- ✅ Large message support

### Search Features
- ✅ All Elasticsearch query types
- ✅ Aggregation framework
- ✅ Pagination methods
- ✅ Multi-index queries

### Distributed Features
- ✅ Consumer rebalancing
- ✅ Offset persistence
- ✅ Node failure handling
- ✅ Load distribution

## Future Enhancements

1. **Performance Tests** (Task #8)
   - Throughput benchmarks
   - Latency profiling
   - Resource utilization
   - Regression detection

2. **Security Tests**
   - TLS communication
   - SASL authentication
   - ACL enforcement

3. **Chaos Testing**
   - Network partitions
   - Process crashes
   - Disk failures

4. **Scale Testing**
   - 100+ node clusters
   - Million+ topics
   - Billion+ messages

## Best Practices

1. **Test Isolation**: Each test creates its own cluster
2. **Cleanup**: Automatic via Drop trait
3. **Timeouts**: Configurable per operation
4. **Logging**: Structured with tracing
5. **Assertions**: Clear failure messages

## Troubleshooting

### Common Issues

1. **Binary not found**: Ensure PATH includes target/release
2. **Port conflicts**: Tests use dynamic allocation
3. **Timeouts**: Increase for slower systems
4. **Resource limits**: Check ulimits for file descriptors

### Debug Tools

```bash
# View detailed logs
RUST_LOG=trace cargo test --test integration -- --nocapture

# Run single test
cargo test --test integration test_kafka_metadata_api -- --exact

# Check for leaked processes
ps aux | grep chronik
```