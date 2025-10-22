# Chronik Raft Clustering - Testing Guide

This guide describes the comprehensive test suite for Chronik's Raft clustering implementation.

## Overview

Chronik implements Raft consensus for multi-node replication (3+ nodes). The test suite validates:
- Leader election and failover
- Data replication and consistency
- Network partition handling
- Split brain prevention
- Node recovery and rejoin
- Failure scenario handling

## Prerequisites

### Required Software

1. **Chronik Server** (built in release mode):
   ```bash
   cargo build --release --bin chronik-server
   ```

2. **Toxiproxy** (for network chaos testing):
   ```bash
   # macOS
   brew install toxiproxy

   # Or download from https://github.com/Shopify/toxiproxy
   ```

3. **Python Dependencies**:
   ```bash
   pip3 install kafka-python requests
   ```

### Cluster Setup

**CRITICAL**: Single-node Raft is NOT supported (provides zero benefit). Minimum 3 nodes required for quorum.

#### Manual Cluster Setup

Use the provided script to start a 3-node cluster:

```bash
./test_cluster_manual.sh
```

This starts:
- **Node 1**: Kafka 9092, Raft 9192
- **Node 2**: Kafka 9093, Raft 9193
- **Node 3**: Kafka 9094, Raft 9194

Each node logs to `test-cluster-data/node{1,2,3}.log`

#### Verify Cluster

```bash
# Check all nodes are alive
lsof -i :9092
lsof -i :9093
lsof -i :9094

# Test connectivity
kafka-topics --list --bootstrap-server localhost:9092
kafka-topics --list --bootstrap-server localhost:9093
kafka-topics --list --bootstrap-server localhost:9094
```

#### Stop Cluster

```bash
pkill -f chronik-server
# Or use the stop script
./test_cluster_manual.sh stop
```

## Test Suites

### 1. Network Chaos Testing (`test_network_chaos.py`)

Tests cluster behavior under adverse network conditions using Toxiproxy.

**Prerequisites**:
- Toxiproxy running on port 8474
- 3-node cluster running
- Toxiproxy proxies configured (see script)

**Tests**:
1. **Network Latency Injection** - Add 100ms latency to Raft communication
2. **Network Partition** - Isolate node from cluster (zero bandwidth)
3. **Packet Loss Tolerance** - 20% packet loss on Raft port
4. **Slow Connection Close** - 5s delay on connection close

**Run**:
```bash
# Start Toxiproxy
toxiproxy-server &

# Start cluster with Toxiproxy proxies
./test_cluster_manual.sh

# Run chaos tests
python3 test_network_chaos.py
```

**Expected Results**:
- Cluster should tolerate latency, packet loss, slow close
- Partition test shows majority continues operating
- No split brain under any scenario

### 2. Failure Scenario Testing (`test_raft_failure_scenarios.py`)

Tests critical failure scenarios for Raft cluster.

**Prerequisites**:
- 3-node cluster running
- Manual restart capability between tests

**Tests**:

#### Test 1: Leader Failure and Re-election
- Kills leader node (node1)
- Verifies remaining nodes elect new leader
- Verifies cluster accepts writes after election
- **Pass Criteria**: New leader elected, writes accepted

#### Test 2: Minority Partition (1 node isolated)
- Isolates 1 node from cluster
- Verifies majority (2 nodes) continue operating
- **Pass Criteria**: Majority continues accepting writes

#### Test 3: Split Brain Prevention
- Creates 2 separate minorities (1 node each)
- Verifies neither minority elects leader
- Verifies no writes accepted without quorum
- **Pass Criteria**: No writes accepted (prevents split brain)

#### Test 4: Data Consistency After Partition
- Partitions minority
- Produces to majority
- Heals partition
- Verifies all nodes have consistent data
- **Pass Criteria**: Nodes have same message count (±5)

#### Test 5: Cascading Failures
- Kills nodes one by one
- Verifies cluster operates until quorum lost
- Verifies cluster stops after quorum loss
- **Pass Criteria**: Works with 2/3, fails with 1/3

**Run**:
```bash
# Start cluster
./test_cluster_manual.sh

# Run failure tests (interactive - requires manual restarts)
python3 test_raft_failure_scenarios.py
```

**Note**: This test suite requires manual cluster restarts between tests. Follow on-screen prompts.

### 3. Recovery Testing (`test_raft_recovery.py`)

Tests node recovery, catch-up, and rejoin scenarios.

**Prerequisites**:
- 3-node cluster running
- Manual restart capability

**Tests**:

#### Test 1: Graceful Shutdown and Rejoin
- Gracefully stops 1 node (SIGTERM)
- Produces messages while down
- Restarts node
- Verifies node catches up
- **Pass Criteria**: Node has all messages (including while-down)

#### Test 2: Crash Recovery (SIGKILL)
- Hard kills 1 node (SIGKILL)
- Produces messages
- Restarts node (WAL recovery)
- Verifies WAL recovery works
- **Pass Criteria**: Node recovers and has messages

#### Test 3: Stale Node Rejoin
- Stops node for extended period
- Produces large volume (250+ messages)
- Restarts stale node
- Verifies catch-up from far behind
- **Pass Criteria**: Node catches up to 80%+ of messages

#### Test 4: Rolling Restart
- Restarts nodes one by one
- Produces messages during each restart
- Verifies quorum maintained throughout
- Verifies consistency after all restarts
- **Pass Criteria**: All nodes have same data (±10 messages)

#### Test 5: Concurrent Failures and Recovery
- Kills 2 nodes simultaneously (no quorum)
- Verifies writes rejected
- Restarts 1 node (restore quorum)
- Verifies cluster recovers
- **Pass Criteria**: Rejects writes without quorum, accepts after restore

**Run**:
```bash
# Start cluster
./test_cluster_manual.sh

# Run recovery tests (interactive)
python3 test_raft_recovery.py
```

**Note**: Requires manual node restarts. The script provides commands to run.

## Interpreting Test Results

### Success Criteria

**Network Chaos Tests**:
- ✓ 3/4 or 4/4 tests pass
- Cluster must survive partition and recover
- No split brain under any condition

**Failure Scenario Tests**:
- ✓ Leader election works (Test 1)
- ✓ Majority continues (Test 2)
- ✓ Split brain prevented (Test 3)
- ✓ Data consistent (Test 4)
- ✓ Quorum enforced (Test 5)

**Recovery Tests**:
- ✓ Graceful rejoin works (Test 1)
- ✓ WAL recovery works (Test 2)
- ✓ Stale catch-up works (Test 3)
- ✓ Rolling restart maintains quorum (Test 4)
- ✓ Concurrent failure recovery (Test 5)

### Common Issues

#### Producer Timeouts
**Symptom**: Many failed sends, timeouts
**Cause**: Producer timeout too short for Raft replication
**Fix**: Increase `request_timeout_ms` in producer config

#### No Quorum Errors
**Symptom**: Writes rejected, "no quorum" errors
**Cause**: Less than 2/3 nodes alive
**Fix**: Ensure at least 2 nodes running

#### Split Brain
**Symptom**: Different nodes accept different writes
**Cause**: Bug in leader election or quorum check
**Fix**: CRITICAL bug - investigate Raft implementation

#### Failed Recovery
**Symptom**: Node can't rejoin after restart
**Cause**: WAL corruption, network issue, or incompatible state
**Fix**: Check WAL recovery logs, verify network connectivity

## Debugging

### Enable Raft Debug Logging

```bash
RUST_LOG=chronik_raft=debug,chronik_server::raft_cluster=debug \
  cargo run --bin chronik-server -- --advertised-addr localhost:9092 --raft standalone
```

### Monitor Raft Metrics

```bash
# Prometheus metrics at :9090
curl http://localhost:9090/metrics | grep raft

# Key metrics:
# - raft_leader_election_count
# - raft_log_replication_lag
# - raft_quorum_status
```

### Check Raft State

```bash
# View Raft state in data directory
ls -la test-cluster-data/node1/raft/
ls -la test-cluster-data/node2/raft/
ls -la test-cluster-data/node3/raft/

# Check logs
tail -f test-cluster-data/node1.log
tail -f test-cluster-data/node2.log
tail -f test-cluster-data/node3.log
```

### Network Debugging

```bash
# Check Toxiproxy proxies
curl http://localhost:8474/proxies

# Check node connectivity
nc -zv localhost 9092
nc -zv localhost 9192  # Raft port

# Monitor network traffic
tcpdump -i lo0 port 9192  # Raft traffic
```

## Performance Benchmarking

### Raft Replication Latency

```bash
# Measure produce latency with Raft replication
python3 test_parallel_stress.py

# Expected latency with 3-node Raft:
# - p50: < 50ms
# - p95: < 200ms
# - p99: < 500ms
```

### Throughput Testing

```bash
# Measure throughput under different loads
for msgs in 1000 5000 10000; do
  echo "Testing $msgs messages..."
  python3 test_simple_topic_create.py --messages $msgs
done
```

### Leader Election Time

Measure time to elect new leader after failure:

```bash
# Kill leader and measure recovery
time ./test_leader_kill_recovery.py
# Expected: < 10 seconds for election + stabilization
```

## CI/CD Integration

### GitHub Actions Workflow

```yaml
name: Raft Cluster Tests

on: [push, pull_request]

jobs:
  raft-tests:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v3

      - name: Install Toxiproxy
        run: |
          wget https://github.com/Shopify/toxiproxy/releases/download/v2.5.0/toxiproxy-server-linux-amd64
          chmod +x toxiproxy-server-linux-amd64
          ./toxiproxy-server-linux-amd64 &

      - name: Build Chronik
        run: cargo build --release --bin chronik-server

      - name: Start Cluster
        run: ./test_cluster_manual.sh

      - name: Run Network Chaos Tests
        run: python3 test_network_chaos.py

      - name: Cleanup
        run: pkill -f chronik-server
```

## Best Practices

### Testing Guidelines

1. **Always test with 3+ nodes** - Single-node Raft is rejected
2. **Test failure scenarios** - Not just happy path
3. **Verify consistency** - Check all nodes after recovery
4. **Monitor metrics** - Use Prometheus for insights
5. **Test network issues** - Use Toxiproxy for real-world conditions

### Development Workflow

1. Make Raft changes
2. Build release binary
3. Run failure scenario tests
4. Run recovery tests
5. Run network chaos tests
6. Verify metrics and logs
7. Only commit if all tests pass

### Production Readiness Checklist

- [ ] All failure scenario tests pass
- [ ] All recovery tests pass
- [ ] Network chaos tests pass (3/4 or better)
- [ ] Leader election < 10 seconds
- [ ] No split brain under any condition
- [ ] WAL recovery works after crashes
- [ ] Stale nodes can catch up
- [ ] Metrics show healthy replication
- [ ] Logs show no errors under normal load

## Troubleshooting Guide

### Issue: Tests Hang

**Symptom**: Test scripts hang indefinitely
**Cause**: Dead node blocking producer/consumer
**Fix**:
```bash
# Kill all nodes and restart
pkill -f chronik-server
./test_cluster_manual.sh
```

### Issue: Inconsistent Data

**Symptom**: Nodes have different message counts
**Cause**: Raft replication lag or bug
**Fix**:
```bash
# Check Raft logs for replication errors
grep -i "replication" test-cluster-data/node*.log

# Verify quorum
grep -i "quorum" test-cluster-data/node*.log
```

### Issue: No Leader Elected

**Symptom**: All writes fail, no leader
**Cause**: Network partition, configuration error, or Raft bug
**Fix**:
```bash
# Check Raft configuration
grep -i "cluster" test-cluster-data/node*.log

# Verify all nodes can communicate
for port in 9192 9193 9194; do nc -zv localhost $port; done
```

## Further Reading

- [Raft Paper](https://raft.github.io/raft.pdf) - Original Raft consensus algorithm
- [Chronik Raft Implementation](../crates/chronik-raft/README.md) - Implementation details
- [Chronik Clustering Architecture](./CLUSTERING_ARCHITECTURE.md) - High-level design
- [WAL Recovery Guide](../crates/chronik-wal/README.md) - WAL recovery details

## Contributing

When adding new Raft features:

1. Add corresponding tests to appropriate test suite
2. Update this guide with new test descriptions
3. Verify all existing tests still pass
4. Add metrics for new functionality
5. Document expected behavior and failure modes

## License

Same as Chronik Stream - MIT or Apache 2.0 (your choice)
