# Chronik Stream Testing Guide

## Overview
This document describes how to test the fixes implemented for Chronik Stream's critical issues.

## Prerequisites

### 1. Install Python Kafka Client
```bash
pip3 install -r test_requirements.txt
# or directly:
pip3 install kafka-python
```

### 2. Build Chronik Stream
```bash
cargo build --release
```

## Running Tests

### Option 1: Quick Start (Recommended)

1. **Start the server** (in terminal 1):
```bash
./start_server.sh
```

2. **Run comprehensive tests** (in terminal 2):
```bash
python3 test_all_fixes.py
```

### Option 2: Manual Testing

1. **Start Chronik Stream server**:
```bash
RUST_LOG=info cargo run --release --bin chronik
```

2. **Test individual features**:

#### Auto-Topic Creation
```bash
python3 test_auto_create.py
```

#### Full Test Suite
```bash
python3 test_all_fixes.py
```

### Option 3: Rust Unit Tests

Run the built-in Rust tests:
```bash
# All tests
cargo test

# Protocol tests only
cargo test --package chronik-protocol

# Storage tests
cargo test --package chronik-storage
```

## Test Coverage

### ✅ Implemented and Tested

1. **Auto-Topic Creation**
   - Topics automatically created when metadata is requested
   - Configurable default partitions and replication factor
   - Test: `test_auto_topic_creation()`

2. **Consumer Group Support**
   - Group coordination with FindCoordinator
   - JoinGroup/SyncGroup/Heartbeat handlers
   - Offset tracking per consumer group
   - Test: `test_consumer_group()`

3. **Offset Management**
   - Offset commit/fetch fully working
   - Persistence to TiKV metadata store
   - Consumers can resume from last position
   - Test: `test_offset_commit_fetch()`

4. **Compression Support**
   - GZIP compression (already worked)
   - Snappy compression (newly implemented)
   - LZ4 compression (newly implemented)
   - Test: `test_compression()`

5. **Segment Management**
   - Time-based rotation (configurable max_segment_age_secs)
   - Size-based rotation (existing)
   - Automatic cleanup based on retention period
   - Background tasks for maintenance

6. **Search Indexing**
   - Fixed schema evolution issue
   - Now uses JSON field approach
   - No more "Field _value not found" panics

### ⚠️ Known Limitations

1. **Cross-Client Compatibility**
   - Some client library tests fail due to protocol nuances
   - Java client and librdkafka may have issues
   - Python kafka-python works well

2. **Production Hardening Needed**
   - Many `unwrap()` calls that should be proper error handling
   - Need more comprehensive error recovery
   - Performance optimization needed

## Expected Test Output

When running `test_all_fixes.py`, you should see:

```
============================================================
CHRONIK STREAM COMPREHENSIVE TEST SUITE
============================================================

=== Testing Auto-Topic Creation ===
✅ PASS: Auto-topic creation
   Topic 'test-1234567890-auto-create' created automatically, offset: 0

=== Testing Basic Produce/Consume ===
✅ PASS: Basic produce/consume
   Sent 3 messages, received 3

=== Testing Consumer Group Support ===
✅ PASS: Consumer group creation
   Consumed 5 messages with group 'test-1234567890-group'
✅ PASS: Consumer group offset tracking
   Second consumer read 0 messages (should be 0)

=== Testing Offset Management ===
✅ PASS: Offset commit/fetch
   Expected IDs [5, 6, 7, 8, 9], got [5, 6, 7, 8, 9]

=== Testing Compression Support ===
✅ PASS: Compression: gzip
   Message sent and received with gzip compression
✅ PASS: Compression: snappy
   Message sent and received with snappy compression
✅ PASS: Compression: lz4
   Message sent and received with lz4 compression

=== Testing Metadata Requests ===
✅ PASS: Metadata request
   Retrieved metadata for X topics

============================================================
TEST SUMMARY
============================================================
Total: 9 tests
Passed: 9
Failed: 0

🎉 ALL TESTS PASSED!
```

## Debugging Failed Tests

If tests fail, check:

1. **Is the server running?**
   - Check if port 9092 is listening: `lsof -i :9092`
   
2. **Are there build errors?**
   - Run `cargo build --release`
   - Check for compilation errors

3. **Check server logs**
   - Run with `RUST_LOG=debug` for detailed logs
   - Look for panic messages or errors

4. **Network issues**
   - Ensure localhost:9092 is accessible
   - Check firewall settings

## Performance Testing

For production readiness, also test:

1. **Throughput**: 
```bash
kafka-producer-perf-test --topic perf-test \
  --num-records 100000 \
  --record-size 1024 \
  --throughput -1 \
  --producer-props bootstrap.servers=localhost:9092
```

2. **Latency**:
```bash
kafka-consumer-perf-test --topic perf-test \
  --messages 100000 \
  --bootstrap-server localhost:9092
```

## Next Steps

After testing, areas for improvement:

1. **Error Handling**: Replace `unwrap()` with proper error handling
2. **Performance**: Optimize hot paths, add caching
3. **Monitoring**: Add Prometheus metrics
4. **Documentation**: API documentation, deployment guide
5. **Security**: Add authentication, encryption support