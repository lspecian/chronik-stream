# Chronik Stream Integration Tests

This directory contains comprehensive integration tests for Chronik Stream, validating Kafka compatibility, search functionality, failure recovery, and performance.

## Test Categories

### 1. Kafka Compatibility Tests (`kafka_compatibility_test.rs`)
Tests compatibility with real Kafka clients using the rdkafka library:
- Basic produce/consume operations
- Consumer groups and rebalancing
- Transactional producers
- Message headers
- Offset management

### 2. Search Integration Tests (`search_integration_test.rs`)
Tests the full search pipeline:
- Log indexing from Kafka topics
- Search queries (term, range, full-text)
- Aggregations (terms, stats, date histogram)
- Kafka offset tracking in search results

### 3. Failure Recovery Tests (`failure_recovery_test.rs`)
Tests system resilience:
- Node failure and recovery
- Network partitions
- Consumer group coordinator failover
- Data consistency during failures

### 4. Performance Tests (`performance_test.rs`)
Benchmarks system performance:
- Single producer throughput
- Multiple producer throughput
- Consumer throughput
- End-to-end latency
- Search indexing performance
- Concurrent operations

### 5. Multi-Language Client Tests (`multi_language_client_test.rs`)
Tests compatibility with clients in different languages:
- Java (using Apache Kafka client)
- Python (using kafka-python)
- Node.js (using kafkajs)
- Cross-language message format compatibility

## Running Tests

### Prerequisites

1. Docker (for testcontainers)
2. Rust toolchain
3. Optional: Java, Python, Node.js for multi-language tests

### Running All Tests

```bash
cargo test --test integration -- --test-threads=1
```

### Running Specific Test Suites

```bash
# Kafka compatibility only
cargo test --test integration kafka_compatibility

# Search tests only
cargo test --test integration search_integration

# Performance tests with output
cargo test --test integration performance -- --nocapture
```

### Running with Debug Output

```bash
RUST_LOG=chronik=debug,integration=debug cargo test --test integration -- --nocapture
```

## Test Infrastructure

### TestEnvironment
Manages Docker containers for dependencies:
- MinIO for S3-compatible object storage
- Automatic bucket creation
- Cleanup on test completion

### ChronikCluster
Manages a multi-node Chronik Stream cluster:
- Configurable number of nodes
- Automatic port allocation
- Health checking
- Graceful shutdown

## CI/CD Integration

Tests run automatically on:
- Push to main/master branch
- Pull requests
- Nightly schedule (2 AM UTC)

The CI pipeline:
1. Builds the project
2. Runs unit tests
3. Runs integration tests sequentially
4. Uploads test results
5. Cleans up Docker resources

## Performance Baselines

Expected performance (on modern hardware):
- Single producer: >10K msgs/sec, >10 MB/sec
- Multiple producers: >20K msgs/sec
- Consumer: >10K msgs/sec
- P50 latency: <50ms
- P99 latency: <200ms
- Search query: <100ms
- Aggregation: <200ms

## Troubleshooting

### Port Conflicts
Tests use ports starting from:
- Kafka: 19092+
- Admin API: 18080+
- Internal: 17000+

Kill any processes using these ports before running tests.

### Docker Issues
```bash
# Clean up all containers
docker ps -aq | xargs -r docker stop
docker ps -aq | xargs -r docker rm
docker volume prune -f
```

### Test Timeouts
Increase timeouts in CI environments by setting:
```bash
CI=true cargo test --test integration
```

## Adding New Tests

1. Create a new test file in `tests/integration/`
2. Add the module to `mod.rs`
3. Use `TestEnvironment` and `ChronikCluster` for setup
4. Follow the existing patterns for assertions
5. Update this README with test descriptions