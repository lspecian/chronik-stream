# Network Chaos Testing Guide

This guide covers network fault injection testing for Chronik using Toxiproxy.

## Overview

Chronik uses **Toxiproxy** for network chaos engineering to validate cluster behavior under adverse network conditions. This enables testing of:

- Network partitions (split-brain scenarios)
- Network latency and jitter
- Packet loss
- Slow connection close
- Bandwidth limitations

## Architecture

### Proxy Topology

Toxiproxy sits between Kafka clients and the Chronik cluster:

```
┌─────────────────────────────────────────────────────────────┐
│                     Toxiproxy Layer                          │
│  (Fault Injection: latency, partition, packet loss, etc.)   │
├─────────────────────────────────────────────────────────────┤
│                                                               │
│  Kafka Proxies (Client → Server):                           │
│    Node 1: :19092 → :9092                                    │
│    Node 2: :19093 → :9093                                    │
│    Node 3: :19094 → :9094                                    │
│                                                               │
│  Raft Proxies (Inter-node Communication):                   │
│    Node 1: :15001 → :5001                                    │
│    Node 2: :15002 → :5002                                    │
│    Node 3: :15003 → :5003                                    │
│                                                               │
└─────────────────────────────────────────────────────────────┘
                              ↓
┌─────────────────────────────────────────────────────────────┐
│                   Chronik Cluster (Actual)                   │
│                                                               │
│  Node 1: Kafka :9092, Raft :5001, Metrics :9101             │
│  Node 2: Kafka :9093, Raft :5002, Metrics :9102             │
│  Node 3: Kafka :9094, Raft :5003, Metrics :9103             │
│                                                               │
└─────────────────────────────────────────────────────────────┘
```

### Key Concepts

- **Kafka Proxies**: Used for client-to-server communication (produce/fetch)
- **Raft Proxies**: Used for inter-node Raft consensus traffic
- **Toxics**: Fault injection primitives (latency, bandwidth, packet loss, etc.)
- **Streams**: Direction of fault (upstream = client→server, downstream = server→client)

## Quick Start

### 1. Install Toxiproxy

```bash
# macOS
brew install toxiproxy

# Verify installation
which toxiproxy-server
which toxiproxy-cli
```

### 2. Start Toxiproxy Infrastructure

```bash
# Start Toxiproxy server and create all proxies
./test_toxiproxy_setup.sh

# Keep server running (press Ctrl+C to stop)
./test_toxiproxy_setup.sh start
```

This creates 6 proxies:
- 3 Kafka proxies (ports 19092-19094)
- 3 Raft proxies (ports 15001-15003)

### 3. Start Chronik Cluster

In separate terminals, start the 3-node cluster normally:

```bash
# Terminal 1 - Node 1
cargo run --features raft --release --bin chronik-server -- \
  --node-id 1 \
  --advertised-addr localhost:9092 \
  standalone --raft

# Terminal 2 - Node 2
cargo run --features raft --release --bin chronik-server -- \
  --node-id 2 \
  --advertised-addr localhost:9093 \
  standalone --raft

# Terminal 3 - Node 3
cargo run --features raft --release --bin chronik-server -- \
  --node-id 3 \
  --advertised-addr localhost:9094 \
  standalone --raft
```

**IMPORTANT**: The cluster runs on standard ports (9092-9094). Toxiproxy proxies to these ports.

### 4. Run Chaos Tests

```bash
# Run all network chaos tests
./test_network_chaos.py

# Tests connect to proxied ports (19092-19094)
```

## Available Toxics

### 1. Latency

Add network delay to simulate slow networks or WAN latency.

```bash
# Add 100ms latency to node 1 Kafka traffic
toxiproxy-cli toxic add chronik-node1-kafka -t latency -a latency=100

# Add latency with jitter (100ms ± 20ms)
toxiproxy-cli toxic add chronik-node1-kafka -t latency -a latency=100 -a jitter=20

# Remove latency toxic
toxiproxy-cli toxic remove chronik-node1-kafka -n latency_downstream
```

**Use Cases**:
- Test producer/consumer timeout handling
- Verify Raft heartbeat tolerance
- Simulate cross-region replication

### 2. Bandwidth Limit

Throttle bandwidth to simulate slow links or network congestion.

```bash
# Limit node 2 to 1KB/s (essentially a partition)
toxiproxy-cli toxic add chronik-node2-raft -t bandwidth -a rate=1024

# Complete partition (zero bandwidth)
toxiproxy-cli toxic add chronik-node2-raft -t bandwidth -a rate=0
```

**Use Cases**:
- Network partition testing (rate=0)
- Slow network testing (low rate)
- Leader election during partition

### 3. Slow Close

Delay connection close to test resource cleanup.

```bash
# Delay close by 5 seconds
toxiproxy-cli toxic add chronik-node3-raft -t slow_close -a delay=5000
```

**Use Cases**:
- Connection pool exhaustion
- Resource leak detection
- Graceful shutdown testing

### 4. Timeout

Close connections after a timeout to simulate network failures.

```bash
# Close connections after 10 seconds
toxiproxy-cli toxic add chronik-node1-kafka -t timeout -a timeout=10000
```

**Use Cases**:
- Client reconnection logic
- Connection timeout handling
- Stale connection cleanup

### 5. Slicer

Slice TCP packets to simulate packet fragmentation.

```bash
# Slice packets to 100 bytes with 10µs delay
toxiproxy-cli toxic add chronik-node1-kafka -t slicer -a average_size=100 -a size_variation=50 -a delay=10
```

**Use Cases**:
- TCP reassembly issues
- Small packet handling
- Protocol framing bugs

## Test Scenarios

### Scenario 1: Network Latency Injection

**Goal**: Verify cluster works with high-latency networks

```bash
# Add 200ms latency to all Kafka proxies
toxiproxy-cli toxic add chronik-node1-kafka -t latency -a latency=200
toxiproxy-cli toxic add chronik-node2-kafka -t latency -a latency=200
toxiproxy-cli toxic add chronik-node3-kafka -t latency -a latency=200

# Produce/consume messages (should work but be slower)
python3 test_cluster_kafka_python.py

# Cleanup
toxiproxy-cli toxic remove chronik-node1-kafka -n latency_downstream
toxiproxy-cli toxic remove chronik-node2-kafka -n latency_downstream
toxiproxy-cli toxic remove chronik-node3-kafka -n latency_downstream
```

**Expected Behavior**:
- Messages still produced/consumed successfully
- Higher latency (p99 > 200ms)
- No message loss

### Scenario 2: Network Partition (Split Brain)

**Goal**: Test Raft behavior when a node is partitioned from the cluster

```bash
# Partition node 2 from Raft cluster (zero bandwidth)
toxiproxy-cli toxic add chronik-node2-raft -t bandwidth -a rate=0

# Wait 10 seconds for leader election
sleep 10

# Produce messages (should work, leader is node 1 or 3)
python3 test_cluster_kafka_python.py

# Heal partition
toxiproxy-cli toxic remove chronik-node2-raft -n bandwidth_downstream

# Wait for node 2 to rejoin
sleep 5

# Consume messages (should get all messages including from partition period)
```

**Expected Behavior**:
- Cluster maintains quorum (2/3 nodes)
- Messages produced to leader node
- Node 2 catches up after partition heals
- No message loss

### Scenario 3: Packet Loss

**Goal**: Test Raft tolerance to packet loss

```bash
# Add 20% packet loss to node 1 Raft traffic
# Note: Toxiproxy doesn't have a dedicated packet loss toxic
# Use latency toxic with toxicity=0.2 (drops 20% of packets)
curl -X POST http://localhost:8474/proxies/chronik-node1-raft/toxics \
  -H "Content-Type: application/json" \
  -d '{"name":"packet_loss","type":"latency","toxicity":0.2,"attributes":{"latency":0}}'

# Produce messages
python3 test_cluster_kafka_python.py

# Remove toxic
curl -X DELETE http://localhost:8474/proxies/chronik-node1-raft/toxics/packet_loss
```

**Expected Behavior**:
- Raft retries lost messages
- Cluster remains available
- Possible increased latency due to retries

### Scenario 4: Cascading Failure

**Goal**: Test cluster behavior when 2/3 nodes fail (quorum loss)

```bash
# Partition nodes 2 and 3 from cluster
toxiproxy-cli toxic add chronik-node2-raft -t bandwidth -a rate=0
toxiproxy-cli toxic add chronik-node3-raft -t bandwidth -a rate=0

# Wait 10 seconds
sleep 10

# Try to produce messages (should fail - no quorum)
python3 test_cluster_kafka_python.py

# Expected: Produce failures (NotEnoughReplicasException or timeout)

# Heal one node (restore quorum)
toxiproxy-cli toxic remove chronik-node2-raft -n bandwidth_downstream

# Wait 10 seconds for quorum
sleep 10

# Produce messages (should work now)
python3 test_cluster_kafka_python.py

# Cleanup
toxiproxy-cli toxic remove chronik-node3-raft -n bandwidth_downstream
```

**Expected Behavior**:
- Cluster becomes unavailable when quorum lost (2/3 nodes down)
- Cluster recovers when quorum restored
- No message loss for committed messages

## Automated Test Suite

The `test_network_chaos.py` script runs 4 comprehensive chaos tests:

### Test 1: Network Latency Injection
- Measures baseline throughput
- Adds 100ms latency to node 1
- Verifies messages still produced/consumed
- Validates no message loss

### Test 2: Network Partition and Recovery
- Partitions node 2 from Raft cluster
- Produces messages during partition
- Heals network
- Verifies cluster recovers and all messages consumed

### Test 3: Packet Loss Tolerance
- Adds 20% packet loss to node 1
- Produces 100 messages
- Verifies messages arrive despite packet loss

### Test 4: Slow Connection Close
- Adds 5s delay to connection close
- Verifies cluster handles slow closes gracefully
- Checks for resource leaks

### Running the Test Suite

```bash
# Start Toxiproxy infrastructure
./test_toxiproxy_setup.sh start &

# Start Chronik cluster (3 nodes)
./test_cluster_manual.sh start

# Run chaos tests
./test_network_chaos.py

# Cleanup
./test_cluster_manual.sh stop
./test_toxiproxy_setup.sh stop
```

## Monitoring During Chaos Tests

### Prometheus Metrics

Key metrics to monitor during chaos testing:

```
# Raft metrics
chronik_raft_leader_changes_total         # Leader election count
chronik_raft_proposal_failures_total      # Failed proposals
chronik_raft_message_send_errors_total    # Network send errors
chronik_raft_heartbeat_timeout_total      # Heartbeat timeouts

# Kafka metrics
chronik_produce_requests_total{status="error"}  # Produce failures
chronik_fetch_requests_total{status="error"}    # Fetch failures
chronik_cluster_partitions_offline              # Unavailable partitions
```

### Check Prometheus Metrics

```bash
# Node 1 metrics
curl -s http://localhost:9101/metrics | grep chronik_raft

# Node 2 metrics
curl -s http://localhost:9102/metrics | grep chronik_raft

# Node 3 metrics
curl -s http://localhost:9103/metrics | grep chronik_raft
```

### Toxiproxy Status

```bash
# List all proxies and active toxics
toxiproxy-cli list

# Get JSON details
curl -s http://localhost:8474/proxies | python3 -m json.tool

# Check specific proxy
curl -s http://localhost:8474/proxies/chronik-node1-kafka | python3 -m json.tool
```

## Troubleshooting

### Toxiproxy Not Starting

```bash
# Check if port 8474 is already in use
lsof -i :8474

# Kill existing Toxiproxy server
pkill toxiproxy-server

# Restart
./test_toxiproxy_setup.sh
```

### Proxies Not Created

```bash
# Check Toxiproxy logs
tail -f /tmp/toxiproxy.log

# Manually create proxy
curl -X POST http://localhost:8474/proxies \
  -H "Content-Type: application/json" \
  -d '{"name":"test","listen":"127.0.0.1:19999","upstream":"127.0.0.1:9092","enabled":true}'

# List proxies
curl -s http://localhost:8474/proxies
```

### Clients Can't Connect Through Proxy

```bash
# Verify Chronik cluster is running on actual ports
lsof -i :9092
lsof -i :9093
lsof -i :9094

# Verify proxies are listening
lsof -i :19092
lsof -i :19093
lsof -i :19094

# Test proxy connection
nc -zv localhost 19092
```

### Toxic Not Working

```bash
# List all toxics for a proxy
curl -s http://localhost:8474/proxies/chronik-node1-kafka/toxics

# Remove all toxics from proxy
curl -X POST http://localhost:8474/proxies/chronik-node1-kafka/toxics | \
  jq -r '.[].name' | \
  xargs -I {} curl -X DELETE http://localhost:8474/proxies/chronik-node1-kafka/toxics/{}

# Re-add toxic
toxiproxy-cli toxic add chronik-node1-kafka -t latency -a latency=100
```

## Best Practices

### 1. Always Reset Toxics Between Tests

```bash
# Python
toxi = ToxiproxyClient()
toxi.reset_all_proxies()

# Bash
toxiproxy-cli toxic remove chronik-node1-kafka -n latency_downstream
```

### 2. Use Named Toxics

```bash
# Good - named toxic, easy to remove
toxiproxy-cli toxic add chronik-node1-kafka -t latency -n test1_latency -a latency=100

# Remove by name
toxiproxy-cli toxic remove chronik-node1-kafka -n test1_latency
```

### 3. Monitor Metrics During Tests

Always check Prometheus metrics to validate cluster behavior:

```bash
# Check for errors
curl -s http://localhost:9101/metrics | grep _error

# Check Raft leader changes
curl -s http://localhost:9101/metrics | grep leader_changes
```

### 4. Test Recovery, Not Just Failure

Always test that the cluster recovers after faults are removed:

```python
# Add fault
toxi.add_latency("node1", 200)

# Test during fault
produce_messages()

# Remove fault
toxi.reset_proxy("chronik-node1-kafka")

# Test after recovery - CRITICAL!
produce_messages()
consume_messages()
```

### 5. Use Realistic Fault Magnitudes

Don't test with extreme values that would never happen in production:

```bash
# Good - realistic WAN latency
toxiproxy-cli toxic add chronik-node1-kafka -t latency -a latency=100

# Bad - unrealistic 60 second latency
toxiproxy-cli toxic add chronik-node1-kafka -t latency -a latency=60000
```

## Integration with CI/CD

### GitHub Actions Workflow

```yaml
name: Chaos Testing

on: [pull_request]

jobs:
  chaos-test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v3

      - name: Install Toxiproxy
        run: |
          wget https://github.com/Shopify/toxiproxy/releases/download/v2.5.0/toxiproxy-server-linux-amd64
          chmod +x toxiproxy-server-linux-amd64
          sudo mv toxiproxy-server-linux-amd64 /usr/local/bin/toxiproxy-server

      - name: Start Toxiproxy
        run: ./test_toxiproxy_setup.sh &

      - name: Build Chronik
        run: cargo build --features raft --release

      - name: Start Cluster
        run: ./test_cluster_manual.sh start

      - name: Run Chaos Tests
        run: ./test_network_chaos.py

      - name: Cleanup
        run: |
          ./test_cluster_manual.sh stop
          ./test_toxiproxy_setup.sh stop
```

## References

- [Toxiproxy GitHub](https://github.com/Shopify/toxiproxy)
- [Toxiproxy Toxics Documentation](https://github.com/Shopify/toxiproxy#toxics)
- [Chronik Clustering Guide](../CLUSTERING_TRACKER.md)
- [Raft Consensus Algorithm](https://raft.github.io/)

---

**Last Updated**: 2025-10-21
**Version**: v2.0.0-rc1
