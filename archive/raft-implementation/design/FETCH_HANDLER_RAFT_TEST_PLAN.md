# FetchHandler Raft Integration Test Plan

## Overview

This document describes the comprehensive testing strategy for the Raft-aware FetchHandler with follower read support.

## Test Categories

### 1. Unit Tests

Location: `crates/chronik-server/src/fetch_handler.rs` (test module)

#### Test 1.1: Follower Read with Committed Data
```rust
#[tokio::test]
async fn test_follower_read_committed_data() {
    // Setup:
    // - 3-node cluster (node 1 = leader, node 2 = follower, node 3 = follower)
    // - Produce 100 messages to leader
    // - Wait for Raft commit (majority replication)

    // Test:
    // - Fetch from follower (node 2) at offset 0
    // - Expect: Success, all 100 messages returned
    // - Verify: CRC preserved, offsets correct, high_watermark = 100
}
```

#### Test 1.2: Follower Read with Uncommitted Data
```rust
#[tokio::test]
async fn test_follower_read_uncommitted_data() {
    // Setup:
    // - 3-node cluster
    // - Produce 50 messages to leader
    // - BLOCK replication to follower 2 (network partition)
    // - Produce 50 more messages to leader (now uncommitted)

    // Test:
    // - Fetch from follower 2 at offset 50
    // - Expect: Empty response (data not committed)
    // - Verify: high_watermark = 50 (committed offset)
}
```

#### Test 1.3: Leader-Only Mode
```rust
#[tokio::test]
async fn test_leader_only_reads() {
    // Setup:
    // - 3-node cluster
    // - Set CHRONIK_FETCH_FROM_FOLLOWERS=false
    // - Produce 100 messages to leader

    // Test 1: Fetch from leader
    // - Expect: Success, all 100 messages returned

    // Test 2: Fetch from follower
    // - Expect: NOT_LEADER_FOR_PARTITION error
    // - Verify: preferred_read_replica points to leader
}
```

#### Test 1.4: Commit Wait Timeout
```rust
#[tokio::test]
async fn test_commit_wait_timeout() {
    // Setup:
    // - 3-node cluster
    // - Produce 100 messages to leader
    // - BLOCK replication (uncommitted)

    // Test:
    // - Fetch from follower at offset 0 with max_wait_ms=500
    // - Expect: Empty response after timeout
    // - Verify: high_watermark = 0 (nothing committed)
}
```

#### Test 1.5: Preferred Read Replica
```rust
#[tokio::test]
async fn test_preferred_read_replica() {
    // Setup:
    // - 3-node cluster
    // - Produce 100 messages

    // Test 1: Fetch from leader
    // - Expect: preferred_read_replica = leader_node_id

    // Test 2: Fetch from stable follower
    // - Expect: preferred_read_replica = follower_node_id (self)

    // Test 3: Fetch from lagging follower (lag > threshold)
    // - Expect: preferred_read_replica = leader_node_id (redirect)
}
```

### 2. Integration Tests

Location: `tests/integration/raft_fetch_follower_reads.rs`

#### Test 2.1: 3-Node Cluster Produce and Fetch
```rust
#[tokio::test]
async fn test_3node_produce_fetch_from_follower() {
    // Setup:
    // - Start 3-node Raft cluster (ports 9092, 9093, 9094)
    // - Create topic "test-topic" with 1 partition
    // - Wait for leader election

    // Phase 1: Produce
    // - Connect to leader node
    // - Produce 1000 messages
    // - Wait for Raft commit (majority replication)

    // Phase 2: Fetch from Follower
    // - Connect to follower node
    // - Fetch from offset 0
    // - Expect: All 1000 messages returned
    // - Verify: Messages match produced data (byte-for-byte)

    // Phase 3: Verify CRC
    // - Check that CRC in fetched batches matches original
    // - Use kafka-python or Java client to verify no CRC errors
}
```

#### Test 2.2: Follower Lag and Catch-Up
```rust
#[tokio::test]
async fn test_follower_lag_catchup() {
    // Setup:
    // - 3-node cluster
    // - Produce 500 messages (committed)

    // Phase 1: Create Lag
    // - Disconnect follower 2 from network
    // - Produce 500 more messages to leader
    // - Wait for commit (with 2 nodes = majority)

    // Phase 2: Fetch from Lagging Follower
    // - Try to fetch offset 600 from follower 2
    // - Expect: Empty (not committed on this follower)

    // Phase 3: Reconnect and Catch Up
    // - Reconnect follower 2
    // - Wait for Raft replication
    // - Retry fetch from follower 2
    // - Expect: Success, all messages returned
}
```

#### Test 2.3: Leader Failover During Fetch
```rust
#[tokio::test]
async fn test_leader_failover_during_fetch() {
    // Setup:
    // - 3-node cluster
    // - Produce 1000 messages

    // Phase 1: Kill Leader
    // - Start long fetch from follower (with wait)
    // - Kill leader node during fetch
    // - Wait for new leader election

    // Phase 2: Verify Behavior
    // - Fetch should complete (reading from local committed data)
    // - OR fetch should return partial data (acceptable)
    // - Client should retry and succeed with new leader

    // Phase 3: Verify Consistency
    // - All fetched data should match original
    // - No duplicate or missing messages
}
```

#### Test 2.4: Concurrent Fetches from Multiple Replicas
```rust
#[tokio::test]
async fn test_concurrent_fetches_from_replicas() {
    // Setup:
    // - 3-node cluster
    // - Produce 10,000 messages
    // - Wait for full replication

    // Test:
    // - Start 10 concurrent consumers
    // - 5 fetch from leader, 5 fetch from followers
    // - All fetch the same offset range [0, 10000)

    // Verify:
    // - All consumers get identical data
    // - No CRC errors
    // - Load is balanced (followers serve ~5 consumers each)
}
```

#### Test 2.5: Network Partition Handling
```rust
#[tokio::test]
async fn test_network_partition() {
    // Setup:
    // - 3-node cluster (A, B, C)
    // - Produce 500 messages (committed)

    // Phase 1: Create Partition
    // - Partition: [A, B] vs [C]
    // - A or B becomes leader (has majority)
    // - Produce 500 more messages (committed in [A,B])

    // Phase 2: Fetch from Minority (C)
    // - Try to fetch offset 600 from C
    // - Expect: Empty (not committed, lost leadership)

    // Phase 3: Heal Partition
    // - Reconnect C to cluster
    // - C catches up via Raft replication
    // - Retry fetch from C
    // - Expect: Success
}
```

### 3. Performance Tests

Location: `tests/integration/raft_fetch_performance.rs`

#### Test 3.1: Read Scalability Benchmark
```rust
#[tokio::test]
async fn benchmark_read_scalability() {
    // Setup:
    // - 3-node cluster
    // - Pre-populate 1M messages

    // Benchmark 1: All reads from leader
    // - 100 concurrent consumers, all fetch from leader
    // - Measure: throughput (msg/s), latency (p50, p95, p99)

    // Benchmark 2: Reads distributed across followers
    // - 100 concurrent consumers, evenly distributed
    // - Measure: throughput (msg/s), latency (p50, p95, p99)

    // Expected Result:
    // - Benchmark 2 should have 3x throughput of Benchmark 1
    // - Latency should be similar (committed data)
}
```

#### Test 3.2: Follower Lag Impact
```rust
#[tokio::test]
async fn benchmark_follower_lag_impact() {
    // Setup:
    // - 3-node cluster
    // - Continuous write load (1000 msg/s)

    // Benchmark:
    // - Introduce network delay to follower (10ms, 50ms, 100ms)
    // - Measure fetch latency from lagging follower

    // Verify:
    // - Fetch latency increases with follower lag
    // - Fetches still succeed (reading committed data)
    // - No data loss or corruption
}
```

### 4. Client Compatibility Tests

Location: `tests/integration/raft_client_compat.rs`

#### Test 4.1: kafka-python Client
```python
# test_kafka_python_follower_reads.py

def test_produce_and_consume_via_follower():
    # Setup
    producer = KafkaProducer(bootstrap_servers=['node1:9092'])
    consumer = KafkaConsumer(
        'test-topic',
        bootstrap_servers=['node2:9092'],  # Connect to follower
        auto_offset_reset='earliest'
    )

    # Produce
    for i in range(1000):
        producer.send('test-topic', f'message-{i}'.encode())
    producer.flush()

    # Consume from follower
    messages = []
    for msg in consumer:
        messages.append(msg.value)
        if len(messages) >= 1000:
            break

    # Verify
    assert len(messages) == 1000
    assert messages[0] == b'message-0'
    assert messages[999] == b'message-999'
```

#### Test 4.2: Java kafka-console-consumer
```bash
#!/bin/bash
# test_java_console_consumer.sh

# Start 3-node cluster
./start_cluster.sh

# Produce to leader
kafka-console-producer --bootstrap-server node1:9092 --topic test-topic < messages.txt

# Consume from follower (should work without CRC errors)
kafka-console-consumer --bootstrap-server node2:9092 --topic test-topic --from-beginning --max-messages 1000 > output.txt

# Verify
diff messages.txt output.txt
```

#### Test 4.3: confluent-kafka (C/C++ client)
```python
# test_confluent_kafka_follower.py

from confluent_kafka import Producer, Consumer

def test_confluent_kafka_raft_cluster():
    # Producer to leader
    producer = Producer({'bootstrap.servers': 'node1:9092'})
    for i in range(1000):
        producer.produce('test-topic', f'message-{i}'.encode())
    producer.flush()

    # Consumer from follower
    consumer = Consumer({
        'bootstrap.servers': 'node2:9092',
        'group.id': 'test-group',
        'auto.offset.reset': 'earliest'
    })
    consumer.subscribe(['test-topic'])

    messages = []
    for _ in range(1000):
        msg = consumer.poll(timeout=10.0)
        if msg is None:
            break
        if msg.error():
            raise Exception(f"Consumer error: {msg.error()}")
        messages.append(msg.value())

    assert len(messages) == 1000
```

## Test Matrix

| Test ID | Scenario | Leader Fetch | Follower Fetch | Expected Result |
|---------|----------|--------------|----------------|-----------------|
| 1.1 | Committed data | ✅ Success | ✅ Success | Data matches |
| 1.2 | Uncommitted data | ✅ Success | ⏳ Empty (wait) | Follower returns empty |
| 1.3 | Leader-only mode | ✅ Success | ❌ NOT_LEADER | Error with redirect |
| 2.1 | 3-node normal | ✅ Success | ✅ Success | Load balanced |
| 2.2 | Follower lagging | ✅ Success | ⏳ Empty (lag) | Catch up later |
| 2.3 | Leader failover | 🔄 New leader | ✅ Success | Seamless transition |
| 2.4 | Concurrent reads | ✅ Success | ✅ Success | Linear scalability |
| 2.5 | Network partition | ✅ Majority | ❌ Minority | Minority can't serve |

## Success Criteria

### Functional
- ✅ All unit tests pass
- ✅ All integration tests pass
- ✅ Follower reads return correct data
- ✅ No CRC errors with real Kafka clients
- ✅ Leader failover is handled gracefully

### Performance
- ✅ Follower reads have <100ms additional latency vs leader reads
- ✅ Read throughput scales linearly with replica count (3 nodes = 3x capacity)
- ✅ No memory leaks or resource exhaustion under load

### Consistency
- ✅ Followers only serve committed data
- ✅ No stale reads beyond committed offset
- ✅ Monotonic reads (never see data go backwards)
- ✅ No duplicate or missing messages

## Test Environment

### Local Development
```bash
# 3-node cluster on localhost
cargo build --release --bin chronik-server

# Terminal 1: Node 1 (leader)
CHRONIK_NODE_ID=1 \
CHRONIK_KAFKA_PORT=9092 \
CHRONIK_RAFT_ADDR=127.0.0.1:19092 \
./target/release/chronik-server --raft standalone

# Terminal 2: Node 2 (follower)
CHRONIK_NODE_ID=2 \
CHRONIK_KAFKA_PORT=9093 \
CHRONIK_RAFT_ADDR=127.0.0.1:19093 \
./target/release/chronik-server --raft standalone

# Terminal 3: Node 3 (follower)
CHRONIK_NODE_ID=3 \
CHRONIK_KAFKA_PORT=9094 \
CHRONIK_RAFT_ADDR=127.0.0.1:19094 \
./target/release/chronik-server --raft standalone
```

### CI/CD (GitHub Actions)
```yaml
# .github/workflows/raft-fetch-tests.yml
name: Raft Fetch Integration Tests

on: [push, pull_request]

jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v3
      - uses: actions-rs/toolchain@v1
        with:
          toolchain: stable

      - name: Build
        run: cargo build --release --bin chronik-server

      - name: Run Raft Fetch Tests
        run: cargo test --test raft_fetch_follower_reads -- --nocapture

      - name: Run Performance Benchmarks
        run: cargo test --test raft_fetch_performance -- --nocapture
```

## Debugging Tips

### Enable Raft Debug Logs
```bash
RUST_LOG=chronik_server::fetch_handler=debug,chronik_raft=debug cargo run --bin chronik-server
```

### Check Replica State
```rust
// In fetch_handler.rs
if let Some(ref raft_manager) = self.raft_manager {
    let replica = raft_manager.get_replica(topic, partition)?;
    debug!(
        "Replica state: is_leader={}, committed_index={}, last_applied={}",
        replica.is_leader(),
        replica.committed_index(),
        replica.last_applied()
    );
}
```

### Monitor Follower Lag
```bash
# Add Prometheus metric
chronik_raft_follower_lag_seconds{topic="test-topic",partition="0"} 0.042
```

## Known Limitations (Phase 1)

1. **No Read-Your-Writes Guarantee**: Producing to leader and immediately consuming from follower may not see the write (bounded lag)
   - Workaround: Use sticky sessions or read from same node
   - Future: Implement client-side tracking

2. **No Wait for Commit**: If follower doesn't have committed data, returns empty immediately (no polling)
   - Workaround: Client retries
   - Future: Implement async commit notification

3. **No Linearizable Reads**: Followers may serve slightly stale data (bounded by commit lag)
   - Acceptable for streaming use cases
   - Use leader-only mode if strict consistency required

## Future Enhancements (Phase 2)

1. **Commit Wait with Polling**: Wait for Raft commit with timeout
2. **Read-Your-Writes**: Track client sessions and enforce consistency
3. **Follower Lag Metrics**: Expose lag per partition via Prometheus
4. **Adaptive Preferred Replica**: Dynamically route clients to least-loaded replica
5. **Follower Read Lease**: Reduce commit checks with bounded staleness leases

## References

- Design Document: `FETCH_HANDLER_RAFT_DESIGN.md`
- Kafka KIP-392: Fetch from Followers
- Raft Paper: Section 8 (Client Interaction)
- Chronik Raft Implementation: `crates/chronik-server/src/raft_integration.rs`
