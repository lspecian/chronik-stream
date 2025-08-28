# Chronik Stream - How to Run and Test

## Understanding the Architecture

Chronik Stream has multiple implementations that evolved over time:

1. **chronik-protocol** - Protocol parsing and basic handlers (some unimplemented)
2. **chronik-ingest** - Complete Kafka protocol implementation with all handlers
3. **chronik-all-in-one** - Production server that uses chronik-ingest components

**IMPORTANT**: The working implementation is in `chronik-all-in-one` which uses the `chronik-ingest` handlers. The protocol crate has some unimplemented methods that are meant to be overridden.

## Running the Server

### Method 1: Ingest Server (RECOMMENDED - Has Complete Implementation)
```bash
# Build the ingest binary (has all the working implementations)
cargo build --release --bin chronik-ingest

# Run the server
RUST_LOG=info ./target/release/chronik-ingest

# Or with debug logging  
RUST_LOG=debug ./target/release/chronik-ingest
```

### Method 2: Using Cargo Run
```bash
# Run directly with cargo
RUST_LOG=info cargo run --release --bin chronik-ingest

# With custom settings
RUST_LOG=debug \
BIND_ADDRESS=0.0.0.0:9092 \
STORAGE_PATH=./data \
cargo run --release --bin chronik-ingest
```

### Method 3: Using the Start Script
```bash
# If available, use the start script
./start_server.sh
```

## Testing the Implementation

### Prerequisites
```bash
# Install Python Kafka client
pip3 install kafka-python
```

### Running Tests

#### 1. Comprehensive Test Suite (RECOMMENDED)
```bash
# Start the server first (in terminal 1)
cargo run --release --bin chronik-ingest

# Run all tests (in terminal 2)
python3 test_all_fixes.py

# Or run the critical fixes test
python3 test_critical_fixes.py
```

This tests:
- Auto-topic creation
- Basic produce/consume
- Consumer group support
- Offset management
- Compression (GZIP, Snappy, LZ4)
- Metadata requests

#### 2. Individual Feature Tests

```bash
# Test auto-topic creation
python3 test_auto_create.py

# Test simple produce
python3 tests/python/integration/test_simple_produce.py

# Test consumer groups
python3 tests/python/integration/test_kafka.py
```

#### 3. Using Kafka Tools

```bash
# Test with kafka-console-producer
kafka-console-producer --broker-list localhost:9092 --topic test-topic

# Test with kafka-console-consumer
kafka-console-consumer --bootstrap-server localhost:9092 --topic test-topic --from-beginning

# Test metadata
kafka-metadata --bootstrap-server localhost:9092
```

## What's Currently Working

### ✅ Implemented and Functional
- **Metadata API** - Topics and broker information
- **ApiVersions** - Version negotiation
- **Produce API** - Message sending (with proper persistence via ingest handler)
- **Fetch API** - Message retrieval
- **FindCoordinator** - Consumer group coordinator discovery
- **JoinGroup/SyncGroup** - Consumer group management
- **Heartbeat** - Group session keep-alive
- **OffsetCommit/OffsetFetch** - Offset tracking (in chronik-ingest)
- **ListGroups** - List consumer groups
- **CreateTopics** - Topic creation with auto-create support
- **ListOffsets** - Partition offset queries
- **Compression** - GZIP, Snappy, LZ4 support

### ⚠️ Partially Working
- **SASL Authentication** - Handshake only, no actual auth
- **Message Persistence** - Messages are stored but segment management needs work
- **Replication** - Single broker only currently

### ❌ Not Implemented
- **DeleteTopics**
- **AlterConfigs**
- **LeaveGroup**
- **DescribeGroups**
- **Transaction APIs**
- **ACL APIs**

## Common Issues and Solutions

### Issue: "OffsetCommit not implemented"
**Solution**: Make sure you're running `chronik-ingest`, not the basic `chronik` binary. The ingest binary has the complete Kafka protocol implementation including OffsetCommit/OffsetFetch.

### Issue: Connection refused on port 9092
**Solution**: 
1. Check if the server is running: `lsof -i :9092`
2. Ensure you're binding to the right interface
3. Check firewall settings

### Issue: Test failures with consumer groups
**Solution**: The consumer group APIs are implemented in `chronik-ingest`. Make sure the all-in-one server is using the integrated server configuration.

### Issue: Messages not persisted
**Solution**: 
1. Check data directory exists: `ls -la ./data/segments/`
2. Ensure write permissions
3. Check logs for storage errors

## Debugging Tips

### Enable Debug Logging
```bash
RUST_LOG=debug cargo run --release --bin chronik-all-in-one 2>&1 | grep -E "Produce|Fetch|Offset|Consumer"
```

### Check Handler Usage
```bash
# See which handler is being called
RUST_LOG=debug cargo run --release --bin chronik-all-in-one 2>&1 | grep "handler"
```

### Verify Storage
```bash
# Check if segments are being created
find ./data -name "*.segment" -o -name "*.log"

# Check metadata store
cat ./data/metadata/metadata.json | jq .
```

## Performance Testing

### Basic Throughput Test
```bash
# Producer performance test
for i in {1..1000}; do
    echo "Message $i" | kafka-console-producer --broker-list localhost:9092 --topic perf-test
done

# Measure consumption rate
time kafka-console-consumer --bootstrap-server localhost:9092 --topic perf-test --from-beginning --max-messages 1000
```

## Development Workflow

### Making Protocol Changes
1. **DON'T** modify `chronik-protocol/src/handler.rs` for implementations
2. **DO** modify `chronik-ingest/src/kafka_handler.rs` for actual implementations
3. The all-in-one server will use the ingest implementations automatically

### Testing Changes
1. Make changes in `chronik-ingest`
2. Rebuild: `cargo build --release --bin chronik-all-in-one`
3. Test: `python3 test_all_fixes.py`
4. Check logs: `RUST_LOG=debug` for debugging

## Key Files to Understand

- `crates/chronik-all-in-one/src/integrated_server.rs` - Main server setup using ingest
- `crates/chronik-ingest/src/kafka_handler.rs` - Complete protocol implementation
- `crates/chronik-protocol/src/handler.rs` - Basic protocol structure (many unimplemented)
- `test_all_fixes.py` - Comprehensive test suite

## Production Deployment

### Recommended Configuration
```bash
# Start with optimal settings
RUST_LOG=info \
CHRONIK_DATA_DIR=/var/lib/chronik \
CHRONIK_PORT=9092 \
CHRONIK_ADVERTISED_HOST=your-hostname \
./target/release/chronik-all-in-one
```

### Systemd Service
```ini
[Unit]
Description=Chronik Stream Kafka Server
After=network.target

[Service]
Type=simple
User=chronik
Environment="RUST_LOG=info"
ExecStart=/usr/local/bin/chronik-all-in-one
Restart=always
RestartSec=10

[Install]
WantedBy=multi-user.target
```

## Summary

**Always use `chronik-all-in-one` for a working Kafka-compatible server.** This binary integrates the complete implementation from `chronik-ingest` which has all the consumer group APIs, offset management, and proper message handling. The `chronik-protocol` handler is incomplete by design - it's meant to be a base that other crates extend.