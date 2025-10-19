# Raft Verification & Hardening Guide

This guide covers the comprehensive test suite for validating Chronik's Raft clustering implementation.

## Overview

The verification suite consists of three critical test categories:

1. **End-to-End Failure Recovery** - Tests node failures, recovery, and data consistency
2. **Leader Failover** - Tests leader election timing and continuous operation during failover
3. **Performance Benchmarking** - Compares Raft cluster performance vs standalone mode

## Quick Start

Run the complete verification suite:

```bash
./run_raft_verification_suite.sh
```

This will:
- Build the server with Raft support
- Run all three test suites sequentially
- Generate detailed logs for each test
- Provide a final pass/fail report

## Prerequisites

### Required Software

```bash
# Rust toolchain
rustc --version  # Should be 1.70+

# Python 3 with required packages
pip3 install kafka-python psutil
```

### System Resources

- **CPU**: 4+ cores recommended
- **Memory**: 8GB+ RAM (3 nodes × ~500MB each)
- **Disk**: 5GB free space for test data
- **Network**: Ports 9092-9094 (Kafka), 9192-9194 (Raft) must be available

## Test Suite Details

### Test 1: End-to-End Failure Recovery

**Script**: `test_raft_e2e_failures.py`

**What it tests**:
1. 3-node cluster formation and quorum establishment
2. Message production to leader with replication
3. Follower node failure (Node 3) - verify quorum maintained
4. Leader node failure (Node 1) - verify new leader election
5. Node recovery and cluster re-formation
6. Message consumption from all nodes
7. Data consistency verification

**Success Criteria**:
- Cluster maintains quorum with 2/3 nodes
- New leader elected within 15 seconds
- All nodes recover successfully
- At least 95% of messages are recovered
- No data corruption detected

**Expected Output**:
```
STEP 1: Clean environment and start 3-node cluster
[INFO] Starting Node 1...
[INFO] Starting Node 2...
[INFO] Starting Node 3...
[SUCCESS] Cluster is ready!

STEP 2: Produce 1000 messages to cluster
[SUCCESS] Produced 1000 messages, 0 failures

STEP 3: Kill Node 3 (follower)
[SUCCESS] Cluster maintains quorum with 2/3 nodes!

STEP 4: Kill Node 1 (assumed leader)
[SUCCESS] New leader elected and operational!

STEP 5: Recover Node 3 and Node 1
[SUCCESS] Cluster fully recovered!

STEP 6: Consume all messages
[SUCCESS] ✅ Raft cluster survived node failures and recovered!
```

**Run individually**:
```bash
python3 test_raft_e2e_failures.py
```

**Logs**: `node1_e2e.log`, `node2_e2e.log`, `node3_e2e.log`

---

### Test 2: Leader Failover

**Script**: `test_raft_leader_failover.py`

**What it tests**:
1. 3-node cluster formation
2. Leader identification
3. Continuous message production (30 seconds)
4. Leader killed abruptly mid-stream (simulating crash)
5. New leader election timing
6. Message loss measurement
7. Throughput impact measurement

**Success Criteria**:
- Leader failover completes in < 15 seconds
- Message success rate > 80% during failover
- No data corruption
- Producer recovers automatically after failover

**Key Metrics**:
- **Failover Time**: Time from leader kill to new leader operational
- **Messages Sent**: Total messages attempted
- **Messages Acked**: Successfully acknowledged messages
- **Success Rate**: Acked / Sent percentage

**Expected Output**:
```
📍 Identified leader: Node 1
Before failover: 500 messages acked

💥 INITIATING LEADER FAILOVER

✅ New leader elected! Recovery time: 8.34s

FAILOVER TEST RESULTS
Messages Sent:    3000
Messages Acked:   2847
Messages Failed:  153
Leader Failover Time: 8.34s
Success Rate:     94.9%
Messages Consumed: 2847

✅ Leader failover test PASSED!
```

**Run individually**:
```bash
python3 test_raft_leader_failover.py
```

**Logs**: `node1_failover.log`, `node2_failover.log`, `node3_failover.log`

---

### Test 3: Performance Benchmark

**Script**: `benchmark_raft_vs_standalone.py`

**What it tests**:
1. **Standalone Mode**:
   - Single node, WAL-only (no Raft)
   - 10,000 messages (1KB each)
   - Measure throughput, latency, resource usage

2. **Raft Cluster Mode**:
   - 3 nodes with quorum writes
   - Same 10,000 messages
   - Measure throughput, latency, resource usage

3. **Comparison**:
   - Throughput overhead (%)
   - Latency overhead (%)
   - Memory footprint

**Success Criteria**:
- Raft throughput within 50% of standalone
- Raft p99 latency < 3× standalone
- No crashes or errors during benchmark

**Key Metrics**:
- **Produce Throughput**: Messages per second
- **Consume Throughput**: Messages per second
- **Latency**: p50, p95, p99 in milliseconds
- **CPU Usage**: Percentage per node
- **Memory**: MB per node

**Expected Output**:
```
BENCHMARKING STANDALONE MODE
Standalone Results:
  Produce Throughput: 5234 msg/s
  Consume Throughput: 8765 msg/s
  Latency (p50): 2.34 ms
  Latency (p95): 8.91 ms
  Latency (p99): 15.67 ms
  CPU Usage: 45.2%
  Memory: 234 MB

BENCHMARKING RAFT CLUSTER MODE (3 nodes)
Raft Cluster Results:
  Produce Throughput: 3128 msg/s
  Consume Throughput: 7234 msg/s
  Latency (p50): 4.12 ms
  Latency (p95): 18.34 ms
  Latency (p99): 32.45 ms
  CPU Usage (avg per node): 38.7%
  Memory (avg per node): 312 MB
  Total Memory (all nodes): 936 MB

PERFORMANCE COMPARISON
Produce Throughput (msg/s):
  Standalone: 5234.00
  Raft:       3128.00
  Difference: -40.2%

Latency p99 (ms):
  Standalone: 15.67
  Raft:       32.45
  Difference: +107.1%

SUMMARY:
  Raft Throughput Overhead: 40.2%
  Raft Latency Overhead: 107.1%

✅ Raft performance is acceptable
```

**Run individually**:
```bash
python3 benchmark_raft_vs_standalone.py
```

**Logs**: `standalone_perf.log`, `node1_perf.log`, `node2_perf.log`, `node3_perf.log`

---

## Understanding the Results

### Expected Performance Impact

Raft consensus adds overhead due to:
1. **Network round-trips** for quorum writes (2/3 nodes must ack)
2. **Log replication** to multiple nodes
3. **Leader election** overhead during failures
4. **Raft state machine** processing

**Typical overhead**:
- **Throughput**: 30-50% reduction vs standalone
- **Latency**: 2-3× increase vs standalone
- **Memory**: 3× total (3 nodes vs 1)

This is **expected and acceptable** for the benefits Raft provides:
- ✅ Fault tolerance (survives minority node failures)
- ✅ Strong consistency (linearizable reads/writes)
- ✅ Automatic failover (no manual intervention)
- ✅ Zero data loss (quorum-based replication)

### Failure Scenarios

#### Follower Failure
- **Impact**: Minimal (quorum maintained with 2/3 nodes)
- **Recovery**: Automatic when node restarts
- **Expected**: < 1 second disruption

#### Leader Failure
- **Impact**: Moderate (new leader election required)
- **Recovery**: Automatic election within 5-15 seconds
- **Expected**: Brief producer timeout, then automatic recovery

#### Majority Failure
- **Impact**: Cluster unavailable (no quorum)
- **Recovery**: Manual (restart failed nodes)
- **Expected**: Cluster stops accepting writes until quorum restored

### Interpreting Logs

#### Good Signs (Expected):
```
Clustering enabled with 3 peers, creating RaftReplicaManager
Added peer 2 to RaftReplicaManager
Creating replica for raft-test-topic-0
Proposing conf change: AddNode(2)
Conf change applied: voters=[1, 2]
Raft tick loop starting for partition raft-test-topic-0
```

#### Warning Signs (Investigate):
```
Failed to connect to peer X
Raft tick timeout exceeded
No leader elected after 30s
Quorum lost: 1/3 nodes available
```

#### Critical Issues (Fix Required):
```
Raft node panicked
Checksum mismatch in WAL
Failed to apply committed entry
Split brain detected
```

## Troubleshooting

### Test Failures

#### Cluster Won't Start
```bash
# Check if ports are in use
lsof -i :9092
lsof -i :9093
lsof -i :9094

# Check logs for errors
tail -f node1_*.log
```

#### Quorum Not Formed
```bash
# Verify cluster configuration
grep "CHRONIK_CLUSTER" node1_*.log
grep "Added peer" node1_*.log

# Check network connectivity
nc -zv localhost 9192
nc -zv localhost 9193
nc -zv localhost 9194
```

#### Messages Not Replicated
```bash
# Check Raft state
grep "Conf change applied" node*.log
grep "Committed entry" node*.log

# Verify min_insync_replicas
grep "min_insync_replicas" node*.log
```

### Performance Issues

#### Low Throughput
- Check CPU usage (`top`, `htop`)
- Check disk I/O (`iostat`)
- Increase `batch_size` in producer
- Reduce `min_insync_replicas` (reduces durability)

#### High Latency
- Check network latency between nodes (`ping`)
- Reduce `request_timeout_ms`
- Use `acks=1` instead of `acks=all` (reduces durability)

#### Out of Memory
- Reduce buffer sizes (`buffer_memory`)
- Increase `wal_rotation_size` to reduce segments
- Reduce retention period

## Next Steps

After all tests pass:

1. **Stress Testing**
   - Run longer duration tests (hours/days)
   - Test with higher message rates
   - Test with larger message sizes

2. **Chaos Engineering**
   - Random node kills
   - Network partitions
   - Disk full scenarios
   - Clock skew testing

3. **Production Deployment**
   - Set up monitoring (Prometheus/Grafana)
   - Configure alerts (leader changes, quorum loss)
   - Document runbooks for common failures

## Configuration Tuning

### For Low Latency
```bash
CHRONIK_PRODUCE_PROFILE=low-latency
CHRONIK_WAL_PROFILE=low
CHRONIK_MIN_INSYNC_REPLICAS=1  # Sacrifice durability
```

### For High Throughput
```bash
CHRONIK_PRODUCE_PROFILE=high-throughput
CHRONIK_WAL_PROFILE=high
CHRONIK_WAL_ROTATION_SIZE=1GB
```

### For Maximum Durability
```bash
CHRONIK_MIN_INSYNC_REPLICAS=3  # All replicas must ack
CHRONIK_REPLICATION_FACTOR=5   # 5 nodes total
CHRONIK_WAL_PROFILE=ultra
```

## Conclusion

This verification suite provides comprehensive testing of Chronik's Raft implementation:

✅ **Fault Tolerance**: Survives node failures
✅ **Consistency**: Zero data loss with quorum writes
✅ **Performance**: Acceptable overhead for benefits gained
✅ **Recovery**: Automatic failover and healing

If all tests pass, the Raft cluster is **production-ready**.
