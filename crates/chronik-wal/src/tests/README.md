# WAL Rotation Load Tests

This directory contains comprehensive integration tests for the WAL (Write Ahead Log) segment rotation functionality under high load conditions.

## Test Files

- `rotation_load_test.rs` - High-throughput load tests for segment rotation
- `unit_tests.rs` - Core unit tests for WAL components

## Rotation Load Tests

The `rotation_load_test.rs` file contains several comprehensive tests that validate WAL segment rotation under various load conditions:

### Test Functions

1. **`test_rotation_load_single_partition`** - Tests rotation with 10,000+ messages in a single partition
2. **`test_rotation_load_multiple_partitions`** - Tests concurrent rotation across multiple partitions
3. **`test_rotation_under_memory_pressure`** - Tests rotation with large messages under memory pressure
4. **`test_rotation_data_integrity`** - Validates data integrity across segment boundaries
5. **`test_rotation_performance_benchmarks`** - Measures performance metrics and latency statistics
6. **`test_rotation_edge_cases`** - Tests edge cases like oversized messages and empty batches

### Running the Tests

To run all rotation load tests:
```bash
cargo test -p chronik-wal test_rotation_load
```

To run a specific test:
```bash
cargo test -p chronik-wal test_rotation_load_single_partition
```

To run with output:
```bash
cargo test -p chronik-wal test_rotation_load_single_partition -- --nocapture
```

### Test Configuration

The tests use these default configurations:
- **Segment size**: 1MB (configurable per test)
- **Message count**: 10,000+ messages
- **Message size**: 256-2048 bytes per message
- **Batch size**: 50-100 messages per batch
- **Performance thresholds**: 100+ msg/sec, <100ms latency

### Performance Metrics

Each test collects and validates:
- **Throughput**: Messages per second and MB/sec
- **Latency**: Average, P50, P95, P99 latencies
- **Segment statistics**: Number of segments, sizes, rotation frequency
- **Data integrity**: Message persistence and checksum validation

### Expected Behavior

The tests verify:
1. **Rotation triggers**: Segments rotate when size threshold is reached
2. **Data persistence**: All messages are persisted without loss
3. **File structure**: Correct segment files are created with expected naming
4. **Performance**: Meets minimum throughput and latency requirements
5. **Concurrency**: Multiple partitions can rotate independently
6. **Edge cases**: Large messages and edge conditions are handled correctly

### Test Output

Tests provide detailed console output including:
```
=== WAL Rotation Load Test Results ===
Total Messages: 10000
Total Bytes: 5.2 MB
Segments Created: 6
Rotations: 5
Throughput: 1245.67 msgs/sec
Throughput: 2.34 MB/sec
Avg Latency: 0.80 ms
Avg Segment Size: 896.32 KB
Min Segment Size: 512 KB
Max Segment Size: 1024 KB
=====================================
```

### Dependencies

The tests require:
- `tempfile` - For temporary test directories
- `regex` - For segment filename parsing
- `tokio` - For async test execution
- `chrono` - For timestamp generation

### Notes

- Tests use temporary directories that are cleaned up automatically
- Each test is independent and can be run in isolation
- Long-running tests may take 30+ seconds to complete
- Tests validate both functional correctness and performance characteristics