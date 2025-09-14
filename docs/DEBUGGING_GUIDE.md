# Chronik Stream - Debugging & Immediate Action Guide

## Current Critical Issues (Observed in Testing)

### 1. Protocol Compatibility - Clients Cannot Connect 🔴
**Symptom**: Kafka clients report "NoBrokersAvailable" or timeout
**Root Cause**: Incomplete/incorrect Kafka wire protocol implementation

#### Immediate Debugging Steps:
```bash
# 1. Enable debug logging to see protocol negotiation
RUST_LOG=debug cargo run --bin chronik 2>&1 | tee debug.log

# 2. Check metadata responses specifically
RUST_LOG=debug cargo run --bin chronik 2>&1 | grep -i "metadata\|version\|api"

# 3. Analyze what the client is sending vs what we're responding
RUST_LOG=trace cargo run --bin chronik 2>&1 | grep -i "request\|response"
```

#### Known Protocol Issues to Fix:
- **ApiVersionsRequest**: Not returning supported API versions correctly
- **MetadataRequest**: Returning incomplete broker information
- **Correlation IDs**: Not matching request/response pairs
- **Response Headers**: Missing or malformed

#### Test with Different Clients:
```python
# Test with kafka-python (simplest)
from kafka import KafkaProducer
producer = KafkaProducer(bootstrap_servers='localhost:9092', api_version=(0,10))

# Test with debug enabled
import logging
logging.basicConfig(level=logging.DEBUG)
```

### 2. High CPU Usage Issue (156%) 🔴
**Symptom**: Server uses excessive CPU even when idle
**Root Cause**: Likely busy loop in connection handler

#### Diagnostic Commands:
```bash
# 1. Profile CPU usage
cargo build --release
perf record -F 99 -p $(pgrep chronik) sleep 10
perf report

# 2. Check for busy loops
RUST_LOG=trace cargo run --bin chronik 2>&1 | head -1000 | grep -c "poll\|loop"

# 3. Use flamegraph
cargo install flamegraph
cargo flamegraph --bin chronik
```

#### Likely Locations of Busy Loops:
- `crates/chronik-protocol/src/kafka_handler.rs` - Connection accept loop
- `crates/chronik-protocol/src/handler.rs` - Request processing loop
- `crates/chronik-wal/src/manager.rs` - WAL management and recovery

#### Fix Pattern:
```rust
// BAD - Busy loop
loop {
    if let Some(msg) = receiver.try_recv() {
        // process
    }
}

// GOOD - Async wait
loop {
    let msg = receiver.recv().await?;
    // process
}
```

### 3. Connection Handler Issues 🟠
**Symptom**: Connections accepted but immediately fail
**Root Cause**: Protocol handler panicking or returning errors

#### Debug Connection Flow:
```bash
# 1. Trace connection lifecycle
RUST_LOG=chronik_protocol=trace cargo run --bin chronik

# 2. Use tcpdump to see actual traffic
sudo tcpdump -i lo -w kafka.pcap port 9092
# In another terminal, run your test
# Then analyze: tcpdump -r kafka.pcap -X

# 3. Use netcat to test raw connection
echo -n -e '\x00\x00\x00\x00' | nc localhost 9092 | xxd
```

## Immediate Fixes Priority List

### Today - Critical Fixes
1. **Fix API Versions Response**:
   ```rust
   // In crates/chronik-protocol/src/kafka_handler.rs
   // Ensure ApiVersionsResponse returns all supported APIs
   ```

2. **Fix Metadata Response**:
   ```rust
   // Return proper broker information
   // Include cluster_id, controller_id, topics
   ```

3. **Fix Busy Loop**:
   ```rust
   // Add tokio::time::sleep or use async channels properly
   ```

### Tomorrow - Stability
1. **Replace all unwrap() in connection paths**
2. **Add comprehensive logging**
3. **Implement correlation ID tracking**

### This Week - Functionality
1. **Implement missing consumer APIs**
2. **Fix message persistence**
3. **Add integration tests**

## Testing Strategy

### 1. Unit Tests First
```bash
# Run all unit tests to ensure basics work
cargo test --all 2>&1 | tee test_results.txt

# Run specific protocol tests
cargo test -p chronik-protocol 2>&1 | tee protocol_tests.txt
```

### 2. Manual Protocol Testing
```python
# minimal_test.py - Test basic connectivity
from kafka.client import SimpleClient
from kafka.protocol.admin import ApiVersionRequest_v0
import socket

sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
sock.connect(('localhost', 9092))

# Send API versions request
request = ApiVersionRequest_v0()
sock.send(request.encode())
response = sock.recv(1024)
print(f"Response: {response.hex()}")
```

### 3. Integration Testing
```bash
# Start server with debug
RUST_LOG=debug cargo run --bin chronik &

# Run Python test suite
python3 tests/python/integration/test_all_fixes.py

# Check logs for errors
grep ERROR debug.log
```

## Performance Profiling

### CPU Profiling
```bash
# Using perf
perf record -g cargo run --bin chronik
perf report

# Using valgrind
valgrind --tool=callgrind cargo run --bin chronik
kcachegrind callgrind.out.*
```

### Memory Profiling
```bash
# Check for leaks
valgrind --leak-check=full cargo run --bin chronik

# Monitor memory usage
while true; do ps aux | grep chronik | grep -v grep; sleep 1; done
```

## Error Handling Audit

### Find all unwrap/expect calls:
```bash
# Count unwraps per file
rg "\.unwrap\(\)" --type rust -c | sort -t: -k2 -rn | head -20

# Find unwraps in critical paths
rg "\.unwrap\(\)" crates/chronik-protocol/src/
rg "\.unwrap\(\)" crates/chronik-ingest/src/
```

### Replace Pattern:
```rust
// Replace this:
let value = something.unwrap();

// With this:
let value = something.map_err(|e| {
    error!("Failed to get value: {}", e);
    ChronikError::Internal(format!("Failed: {}", e))
})?;
```

## Monitoring & Observability

### Add Metrics:
```rust
// In main.rs
use prometheus::{Encoder, TextEncoder, Counter, Gauge, Histogram};

lazy_static! {
    static ref REQUEST_COUNTER: Counter = Counter::new("requests_total", "Total requests").unwrap();
    static ref ACTIVE_CONNECTIONS: Gauge = Gauge::new("connections_active", "Active connections").unwrap();
    static ref REQUEST_DURATION: Histogram = Histogram::new("request_duration_seconds", "Request duration").unwrap();
}
```

### Add Health Check:
```rust
// Add endpoint on different port
async fn health_check() -> &'static str {
    // Check components
    if storage_healthy() && metadata_healthy() {
        "OK"
    } else {
        "UNHEALTHY"
    }
}
```

## Quick Wins Checklist

- [ ] Add `tokio::time::sleep(Duration::from_millis(10))` to any tight loops
- [ ] Log all API requests with correlation IDs
- [ ] Return proper ApiVersionsResponse with all supported APIs
- [ ] Fix MetadataResponse to include broker details
- [ ] Replace unwrap() in connection handler with proper error handling
- [ ] Add connection timeout to prevent hanging
- [ ] Implement graceful shutdown
- [ ] Add metrics endpoint
- [ ] Add health check endpoint
- [ ] Enable TCP keepalive on connections

## Emergency Fixes

If server is crashing:
1. Check for panics: `RUST_LOG=debug cargo run 2>&1 | grep -i panic`
2. Add catch_unwind around connection handler
3. Log all errors before returning them
4. Use `RUST_BACKTRACE=full` for stack traces

If clients can't connect:
1. Check server is listening: `netstat -an | grep 9092`
2. Test with telnet: `telnet localhost 9092`
3. Check firewall: `sudo iptables -L`
4. Try different client versions

If high CPU:
1. Add sleep in loops
2. Use blocking recv instead of try_recv
3. Profile with flamegraph
4. Check for infinite recursion