# Chronik Stream Clustering Completion Roadmap

**Target Version**: v2.0.0 GA
**Current Status**: 75-80% Complete (16,500+ lines implemented)
**Timeline**: 3 weeks to production-ready
**Last Updated**: 2025-10-19

---

## Executive Summary

The Chronik clustering implementation is substantially further along than originally documented. Most of the core Raft infrastructure (Phases 1-3) is complete, with significant progress on production features (Phase 4) and advanced features (Phase 5).

**What's Left**: Integration, testing, hardening, and documentation.

**Critical Path**: Server integration → Integration tests → E2E validation → Chaos testing → Documentation → GA release

---

## Implementation Status Overview

| Phase | Plan Status | Actual Status | Completion | Tasks Remaining |
|-------|-------------|---------------|------------|-----------------|
| **Phase 1**: Raft Foundation | 🔴 Not Started | ✅ Complete | 100% | 0 |
| **Phase 2**: Multi-Partition | 🔴 Not Started | ✅ Complete | 100% | 0 |
| **Phase 3**: Cluster Membership | 🔴 Not Started | 🟡 Nearly Complete | 85% | 2 |
| **Phase 4**: Production Features | 🔴 Not Started | 🟡 Nearly Complete | 80% | 3 |
| **Phase 5**: Advanced Features | 🔴 Not Started | 🟡 Partial | 60% | 4 |
| **Testing & Validation** | 🔴 Not Started | 🟡 Partial | 40% | 5 |
| **Documentation** | 🔴 Not Started | 🔴 Not Started | 10% | 6 |
| **Overall Progress** | 0% | **~75%** | 75% | **20 tasks** |

---

## Week 1: Critical Path (Integration & Core Testing)

**Goal**: Wire up cluster mode, verify it works end-to-end with Kafka clients

### Day 1-2: Server Integration & Initial Testing

#### Task 1.1: Complete Raft-Server Integration
- **Priority**: 🔴 CRITICAL
- **Status**: ⬜ Not Started
- **Assignee**: TBD
- **Estimate**: 4 hours
- **Blockers**: None
- **Dependencies**: None

**Subtasks**:
- [ ] Wire `RaftGroupManager` from `raft_cluster.rs` to `IntegratedKafkaServer`
- [ ] Modify `IntegratedKafkaServer::new()` to accept optional `RaftGroupManager`
- [ ] Update `ProduceHandler` to use Raft when `raft_manager` is present
- [ ] Update `FetchHandler` to query Raft leadership before serving
- [ ] Add feature gate checks (`#[cfg(feature = "raft")]`)

**Files to Modify**:
- [crates/chronik-server/src/raft_cluster.rs](crates/chronik-server/src/raft_cluster.rs)
- [crates/chronik-server/src/integrated_server.rs](crates/chronik-server/src/integrated_server.rs)
- [crates/chronik-server/src/produce_handler.rs](crates/chronik-server/src/produce_handler.rs)
- [crates/chronik-server/src/fetch_handler.rs](crates/chronik-server/src/fetch_handler.rs)

**Acceptance Criteria**:
```bash
# Start 3-node cluster locally
cargo run --features raft --bin chronik-server -- --node-id 1 --advertised-addr localhost:9092 standalone --raft
cargo run --features raft --bin chronik-server -- --node-id 2 --advertised-addr localhost:9093 standalone --raft
cargo run --features raft --bin chronik-server -- --node-id 3 --advertised-addr localhost:9094 standalone --raft

# Verify cluster forms (check logs for leader election)
```

**Success Metrics**:
- ✅ Cluster starts without errors
- ✅ Leader election completes within 5 seconds
- ✅ All nodes report healthy via logs

---

#### Task 1.2: Run Single-Partition Integration Test
- **Priority**: 🔴 CRITICAL
- **Status**: ⬜ Not Started
- **Assignee**: TBD
- **Estimate**: 2 hours
- **Blockers**: Task 1.1
- **Dependencies**: Server integration complete

**Subtasks**:
- [ ] Run `raft_single_partition` test with `--ignored` flag
- [ ] Fix any compilation errors or test failures
- [ ] Document any environment setup required
- [ ] Update test if APIs changed

**Command**:
```bash
RUST_LOG=debug cargo test --features raft --test raft_single_partition -- --ignored --nocapture --test-threads=1
```

**Acceptance Criteria**:
- ✅ Test passes without panics
- ✅ Leader election works
- ✅ Message replication verified (3 nodes receive same data)

---

#### Task 1.3: Run Multi-Partition Integration Test
- **Priority**: 🔴 CRITICAL
- **Status**: ⬜ Not Started
- **Assignee**: TBD
- **Estimate**: 2 hours
- **Blockers**: Task 1.2
- **Dependencies**: Single-partition test passing

**Subtasks**:
- [ ] Run `raft_multi_partition` test
- [ ] Verify multiple Raft groups operate independently
- [ ] Test partition-specific leader failures
- [ ] Document any issues found

**Command**:
```bash
RUST_LOG=debug cargo test --features raft --test raft_multi_partition -- --ignored --nocapture --test-threads=1
```

**Acceptance Criteria**:
- ✅ Multiple partitions each have independent leaders
- ✅ Failure in partition 0 doesn't affect partition 1
- ✅ All partitions replicate correctly

---

### Day 3: End-to-End Cluster Testing

#### Task 1.4: E2E Test with Kafka Python Client
- **Priority**: 🔴 CRITICAL
- **Status**: ⬜ Not Started
- **Assignee**: TBD
- **Estimate**: 4 hours
- **Blockers**: Task 1.1, 1.2, 1.3
- **Dependencies**: Cluster mode fully working

**Subtasks**:
- [ ] Start 3-node cluster (nodes on ports 9092, 9093, 9094)
- [ ] Create test topic with 3 partitions, replication factor 3
- [ ] Produce 10,000 messages via kafka-python
- [ ] Consume all messages and verify count
- [ ] Kill leader node (SIGKILL), verify re-election
- [ ] Resume producing, verify zero message loss
- [ ] Document test steps and results

**Test Script** (create as `test_cluster_e2e.py`):
```python
from kafka import KafkaProducer, KafkaConsumer, KafkaAdminClient
from kafka.admin import NewTopic
import time

# Create topic
admin = KafkaAdminClient(bootstrap_servers='localhost:9092')
topic = NewTopic(name='test-cluster', num_partitions=3, replication_factor=3)
admin.create_topics([topic])
time.sleep(2)

# Produce 10K messages
producer = KafkaProducer(bootstrap_servers='localhost:9092,localhost:9093,localhost:9094')
for i in range(10000):
    producer.send('test-cluster', f"message-{i}".encode())
producer.flush()
print("Produced 10,000 messages")

# Consume and verify
consumer = KafkaConsumer('test-cluster', bootstrap_servers='localhost:9092', auto_offset_reset='earliest')
count = 0
for msg in consumer:
    count += 1
    if count >= 10000:
        break

print(f"Consumed {count} messages (expected 10000)")
assert count == 10000, "Message loss detected!"
```

**Acceptance Criteria**:
- ✅ Topic creation replicates to all nodes
- ✅ 10,000 messages produced successfully
- ✅ 10,000 messages consumed (zero loss)
- ✅ Leader failover completes in < 5 seconds
- ✅ Cluster continues operating after failure

---

#### Task 1.5: Run Cluster Bootstrap Test
- **Priority**: 🟡 HIGH
- **Status**: ⬜ Not Started
- **Assignee**: TBD
- **Estimate**: 2 hours
- **Blockers**: Task 1.4
- **Dependencies**: E2E test passing

**Subtasks**:
- [ ] Run `raft_cluster_bootstrap` test
- [ ] Verify bootstrap with different node start orders
- [ ] Test quorum detection (cluster waits for 2/3 nodes)
- [ ] Document bootstrap behavior

**Command**:
```bash
cargo test --features raft --test raft_cluster_bootstrap -- --ignored --nocapture
```

**Acceptance Criteria**:
- ✅ Cluster forms when 2/3 nodes available
- ✅ Cluster waits if only 1/3 nodes started
- ✅ Late-joining nodes catch up automatically

---

### Day 4-5: Full Integration Test Suite

#### Task 1.6: Run All Raft Integration Tests
- **Priority**: 🟡 HIGH
- **Status**: ⬜ Not Started
- **Assignee**: TBD
- **Estimate**: 1 day
- **Blockers**: Task 1.5
- **Dependencies**: All previous tests passing

**Test Suite**:
```bash
# Run ALL Raft integration tests
cargo test --features raft --workspace --test '*raft*' -- --ignored --nocapture --test-threads=1
```

**Tests to Run** (in order):
1. ✅ `raft_single_partition.rs` - Basic leader election
2. ✅ `raft_multi_partition.rs` - Multiple Raft groups
3. ✅ `raft_cluster_bootstrap.rs` - Bootstrap scenarios
4. ✅ `raft_cluster_integration.rs` - Multi-node coordination
5. ✅ `raft_cluster_e2e.rs` - Full cluster test
6. ✅ `raft_produce_path_test.rs` - Producer integration
7. ✅ `raft_network_test.rs` - Network layer
8. ✅ `raft_snapshot_test.rs` - Snapshot creation
9. ✅ `raft_phase4_integration.rs` - Production-like test
10. ✅ `wal_raft_storage.rs` - WAL backend for Raft log

**Subtasks**:
- [ ] Run each test individually
- [ ] Document any failures with logs
- [ ] Fix critical failures (P0: cluster doesn't start)
- [ ] File issues for non-critical failures
- [ ] Update test expectations if APIs changed

**Acceptance Criteria**:
- ✅ At least 8/10 tests pass (80% pass rate)
- ✅ All P0 failures fixed
- ✅ Failures documented with repro steps

---

### Week 1 Deliverables

**By End of Week 1**:
- ✅ Cluster mode fully integrated with server
- ✅ Core integration tests passing (single-partition, multi-partition)
- ✅ E2E test with kafka-python successful
- ✅ Bootstrap and full test suite executed
- ✅ Known issues documented

**Exit Criteria**:
- Cluster can start, elect leaders, replicate data
- Kafka clients can produce/consume in cluster mode
- Basic failure recovery works (leader re-election)

---

## Week 2: Production Hardening (Testing & Metrics)

**Goal**: Validate failure scenarios, verify metrics, test S3 integration

### Day 6-7: Chaos Testing with Fault Injection

#### Task 2.1: Set Up Toxiproxy Infrastructure
- **Priority**: 🟡 HIGH
- **Status**: ⬜ Not Started
- **Assignee**: TBD
- **Estimate**: 4 hours
- **Blockers**: None
- **Dependencies**: Docker

**Subtasks**:
- [ ] Install Toxiproxy (`docker run -d -p 8474:8474 shopify/toxiproxy`)
- [ ] Create proxies for 3 Chronik nodes
- [ ] Configure port mapping (9090→9092, 9091→9093, 9092→9094)
- [ ] Verify proxies forward traffic correctly
- [ ] Document Toxiproxy setup

**Setup Script** (`scripts/toxiproxy_setup.sh`):
```bash
#!/bin/bash
# Start Toxiproxy
docker run -d --name toxiproxy \
  -p 8474:8474 \
  -p 9090-9094:9090-9094 \
  shopify/toxiproxy

# Wait for Toxiproxy to start
sleep 2

# Create proxies for 3 Chronik nodes
toxiproxy-cli create chronik1 -l 0.0.0.0:9090 -u localhost:9092
toxiproxy-cli create chronik2 -l 0.0.0.0:9091 -u localhost:9093
toxiproxy-cli create chronik3 -l 0.0.0.0:9092 -u localhost:9094

echo "Toxiproxy ready - nodes accessible on ports 9090, 9091, 9092"
```

**Acceptance Criteria**:
- ✅ Toxiproxy container running
- ✅ All 3 proxies created
- ✅ Traffic flows through proxies (test with telnet)

---

#### Task 2.2: Network Partition Test
- **Priority**: 🟡 HIGH
- **Status**: ⬜ Not Started
- **Assignee**: TBD
- **Estimate**: 4 hours
- **Blockers**: Task 2.1
- **Dependencies**: Toxiproxy setup

**Subtasks**:
- [ ] Start 3-node cluster via Toxiproxy
- [ ] Identify current leader
- [ ] Isolate leader from followers (timeout toxic)
- [ ] Verify followers elect new leader
- [ ] Remove toxic, verify old leader becomes follower
- [ ] Verify cluster continues operating

**Test Script** (`test_network_partition.py`):
```python
import subprocess
import time
from kafka import KafkaProducer

# Start cluster via Toxiproxy
producer = KafkaProducer(bootstrap_servers='localhost:9090,localhost:9091,localhost:9092')

# Produce some messages
for i in range(100):
    producer.send('test', f"before-partition-{i}".encode())
producer.flush()

# Isolate Node 1 (leader)
subprocess.run(['toxiproxy-cli', 'toxic', 'add', 'chronik1', '-t', 'timeout', '-a', 'timeout=0'])
print("Node 1 isolated - waiting for re-election...")
time.sleep(5)

# Continue producing (should go to new leader)
for i in range(100):
    producer.send('test', f"during-partition-{i}".encode())
producer.flush()

# Remove partition
subprocess.run(['toxiproxy-cli', 'toxic', 'remove', 'chronik1', '-n', 'timeout'])
print("Partition healed - old leader should rejoin as follower")
time.sleep(5)

# Final produce
for i in range(100):
    producer.send('test', f"after-partition-{i}".encode())
producer.flush()

print("Test complete - verify 300 messages in topic")
```

**Acceptance Criteria**:
- ✅ Followers detect leader failure within 3 seconds
- ✅ New leader elected within 5 seconds
- ✅ No message loss during partition
- ✅ Old leader rejoins as follower
- ✅ All 300 messages consumed successfully

---

#### Task 2.3: Leader Kill and Recovery Test
- **Priority**: 🟡 HIGH
- **Status**: ⬜ Not Started
- **Assignee**: TBD
- **Estimate**: 2 hours
- **Blockers**: Task 2.2
- **Dependencies**: Network partition test passing

**Subtasks**:
- [ ] Start 3-node cluster
- [ ] Produce 1000 messages
- [ ] Kill leader with SIGKILL (not graceful)
- [ ] Verify new leader elected
- [ ] Restart killed node, verify it rejoins
- [ ] Consume all messages, verify zero loss

**Test Script** (`test_leader_kill.sh`):
```bash
#!/bin/bash
set -e

# Start cluster in background
cargo run --features raft --bin chronik-server -- --node-id 1 --advertised-addr localhost:9092 standalone --raft &
PID1=$!
cargo run --features raft --bin chronik-server -- --node-id 2 --advertised-addr localhost:9093 standalone --raft &
PID2=$!
cargo run --features raft --bin chronik-server -- --node-id 3 --advertised-addr localhost:9094 standalone --raft &
PID3=$!

sleep 10  # Wait for cluster formation

# Produce messages
python3 - <<EOF
from kafka import KafkaProducer
producer = KafkaProducer(bootstrap_servers='localhost:9092')
for i in range(1000):
    producer.send('test', f"msg-{i}".encode())
producer.flush()
print("Produced 1000 messages")
EOF

# Kill leader (assume Node 1 for simplicity, check logs for actual leader)
kill -9 $PID1
echo "Killed leader - waiting for re-election..."
sleep 5

# Restart killed node
cargo run --features raft --bin chronik-server -- --node-id 1 --advertised-addr localhost:9092 standalone --raft &
PID1_NEW=$!
sleep 5

# Consume and verify
python3 - <<EOF
from kafka import KafkaConsumer
consumer = KafkaConsumer('test', bootstrap_servers='localhost:9092', auto_offset_reset='earliest')
count = sum(1 for _ in consumer)
print(f"Consumed {count} messages")
assert count == 1000, f"Expected 1000, got {count}"
EOF

# Cleanup
kill $PID1_NEW $PID2 $PID3
echo "Test passed!"
```

**Acceptance Criteria**:
- ✅ Leader kill doesn't cause cluster failure
- ✅ New leader elected in < 5 seconds
- ✅ Killed node rejoins as follower
- ✅ Zero message loss (1000 produced, 1000 consumed)

---

#### Task 2.4: Cascading Failure Test
- **Priority**: 🟢 MEDIUM
- **Status**: ⬜ Not Started
- **Assignee**: TBD
- **Estimate**: 2 hours
- **Blockers**: Task 2.3
- **Dependencies**: Leader kill test passing

**Subtasks**:
- [ ] Start 3-node cluster
- [ ] Kill 2 nodes (lose quorum)
- [ ] Verify cluster becomes unavailable
- [ ] Verify no data corruption
- [ ] Restart killed nodes
- [ ] Verify cluster recovers
- [ ] Verify data integrity

**Test Script** (`test_cascading_failure.sh`):
```bash
#!/bin/bash
set -e

# Start cluster
cargo run --features raft --bin chronik-server -- --node-id 1 --advertised-addr localhost:9092 standalone --raft &
PID1=$!
cargo run --features raft --bin chronik-server -- --node-id 2 --advertised-addr localhost:9093 standalone --raft &
PID2=$!
cargo run --features raft --bin chronik-server -- --node-id 3 --advertised-addr localhost:9094 standalone --raft &
PID3=$!

sleep 10

# Produce initial batch
python3 -c "
from kafka import KafkaProducer
p = KafkaProducer(bootstrap_servers='localhost:9092')
for i in range(500):
    p.send('test', f'msg-{i}'.encode())
p.flush()
"

# Kill 2 nodes (quorum lost)
kill -9 $PID1 $PID2
echo "Killed 2 nodes - cluster should be unavailable"
sleep 5

# Try to produce (should fail or timeout)
timeout 10s python3 -c "
from kafka import KafkaProducer
p = KafkaProducer(bootstrap_servers='localhost:9092')
try:
    p.send('test', b'should-fail')
    p.flush(timeout=5)
    print('ERROR: Produce succeeded without quorum!')
    exit(1)
except Exception as e:
    print(f'Expected failure: {e}')
" || echo "Produce correctly failed without quorum"

# Restart killed nodes
cargo run --features raft --bin chronik-server -- --node-id 1 --advertised-addr localhost:9092 standalone --raft &
PID1=$!
cargo run --features raft --bin chronik-server -- --node-id 2 --advertised-addr localhost:9093 standalone --raft &
PID2=$!
sleep 10

# Verify recovery and data
python3 -c "
from kafka import KafkaConsumer
c = KafkaConsumer('test', bootstrap_servers='localhost:9092', auto_offset_reset='earliest')
count = sum(1 for _ in c)
print(f'Consumed {count} messages after recovery')
assert count == 500, f'Expected 500, got {count}'
"

# Cleanup
kill $PID1 $PID2 $PID3 2>/dev/null || true
echo "Cascading failure test passed!"
```

**Acceptance Criteria**:
- ✅ Cluster unavailable when 2/3 nodes down
- ✅ Produce operations fail gracefully (timeout, not corrupt)
- ✅ Cluster recovers when quorum restored
- ✅ All 500 original messages preserved (no corruption)

---

### Day 8-9: Metrics & Monitoring Validation

#### Task 2.5: Verify Raft Metrics Exposed
- **Priority**: 🟢 MEDIUM
- **Status**: ⬜ Not Started
- **Assignee**: TBD
- **Estimate**: 4 hours
- **Blockers**: None (can run in parallel)
- **Dependencies**: Cluster running

**Subtasks**:
- [ ] Start 3-node cluster
- [ ] Scrape `/metrics` endpoint from each node
- [ ] Verify all expected Raft metrics present
- [ ] Produce messages, verify metrics update
- [ ] Kill leader, verify election metrics increment
- [ ] Document any missing metrics

**Expected Metrics** (from [chronik-monitoring/src/metrics.rs](crates/chronik-monitoring/src/metrics.rs)):
```
# Core Raft Health
chronik_raft_leader_count{node_id="1"} 2
chronik_raft_follower_count{node_id="1"} 4
chronik_raft_isr_size{topic="orders", partition="0"} 3

# Replication Lag
chronik_raft_follower_lag_entries{partition="0", follower="2"} 15
chronik_raft_follower_lag_ms{partition="0", follower="2"} 45

# Elections
chronik_raft_election_count{partition="0"} 2
chronik_raft_election_latency_ms{partition="0", quantile="0.99"} 1200

# Commit Latency
chronik_raft_commit_latency_ms{quantile="0.5"} 8
chronik_raft_commit_latency_ms{quantile="0.99"} 35

# Snapshots
chronik_raft_snapshot_count{partition="0"} 5
chronik_raft_snapshot_size_bytes{partition="0"} 67108864
```

**Test Script** (`verify_metrics.sh`):
```bash
#!/bin/bash

# Scrape metrics from Node 1
curl -s http://localhost:8080/metrics | grep chronik_raft

# Check for required metrics
REQUIRED_METRICS=(
    "chronik_raft_leader_count"
    "chronik_raft_follower_lag_entries"
    "chronik_raft_isr_size"
    "chronik_raft_election_count"
    "chronik_raft_commit_latency_ms"
)

for metric in "${REQUIRED_METRICS[@]}"; do
    if curl -s http://localhost:8080/metrics | grep -q "$metric"; then
        echo "✅ $metric found"
    else
        echo "❌ $metric MISSING"
    fi
done
```

**Acceptance Criteria**:
- ✅ All required metrics exposed
- ✅ Metrics update in real-time during operations
- ✅ Election count increments on leader failure
- ✅ Commit latency shows realistic values (< 100ms p99)

---

#### Task 2.6: ISR Tracking Validation
- **Priority**: 🟢 MEDIUM
- **Status**: ⬜ Not Started
- **Assignee**: TBD
- **Estimate**: 4 hours
- **Blockers**: Task 2.5
- **Dependencies**: Metrics verified

**Subtasks**:
- [ ] Start 3-node cluster
- [ ] Verify all nodes in ISR initially
- [ ] Add 500ms latency to one follower (Toxiproxy)
- [ ] Verify follower removed from ISR after threshold
- [ ] Remove latency
- [ ] Verify follower re-added to ISR
- [ ] Test produce with `min.insync.replicas=2`

**Test Script** (`test_isr_tracking.py`):
```python
import subprocess
import time
from kafka import KafkaProducer, KafkaAdminClient

# Start cluster (assume already running)
admin = KafkaAdminClient(bootstrap_servers='localhost:9092')

# Check initial ISR (all 3 nodes)
# TODO: Add API to query ISR status

# Add latency to Node 3 (simulate slow follower)
subprocess.run(['toxiproxy-cli', 'toxic', 'add', 'chronik3', '-t', 'latency', '-a', 'latency=500'])
print("Added 500ms latency to Node 3")

# Wait for ISR shrink (default threshold: 10s lag)
time.sleep(15)

# Verify ISR size reduced to 2 (check metrics)
import requests
metrics = requests.get('http://localhost:8080/metrics').text
if 'chronik_raft_isr_size{partition="0"} 2' in metrics:
    print("✅ Node 3 removed from ISR")
else:
    print("❌ ISR did not shrink")

# Remove latency
subprocess.run(['toxiproxy-cli', 'toxic', 'remove', 'chronik3', '-n', 'latency'])
time.sleep(10)

# Verify ISR expanded back to 3
metrics = requests.get('http://localhost:8080/metrics').text
if 'chronik_raft_isr_size{partition="0"} 3' in metrics:
    print("✅ Node 3 re-added to ISR")
else:
    print("❌ ISR did not expand")
```

**Acceptance Criteria**:
- ✅ Slow follower removed from ISR after lag threshold (10s)
- ✅ Fast follower re-added to ISR when caught up
- ✅ ISR size metric reflects changes
- ✅ Produce fails if ISR < `min.insync.replicas`

---

### Day 10: S3 Snapshot Integration

#### Task 2.7: S3 Snapshot Upload/Download Test
- **Priority**: 🟡 HIGH
- **Status**: ⬜ Not Started
- **Assignee**: TBD
- **Estimate**: 4 hours
- **Blockers**: None (can run in parallel)
- **Dependencies**: S3/MinIO available

**Subtasks**:
- [ ] Start MinIO container (local S3-compatible storage)
- [ ] Configure Chronik to use MinIO for snapshots
- [ ] Start 3-node cluster, produce 100K messages
- [ ] Trigger snapshot creation (via log size threshold)
- [ ] Verify snapshot uploaded to MinIO
- [ ] Start 4th node, verify it bootstraps from snapshot
- [ ] Verify 4th node catches up and joins cluster

**Setup Script** (`scripts/start_minio.sh`):
```bash
#!/bin/bash
docker run -d --name minio \
  -p 9000:9000 \
  -p 9001:9001 \
  -e MINIO_ROOT_USER=minioadmin \
  -e MINIO_ROOT_PASSWORD=minioadmin \
  minio/minio server /data --console-address ":9001"

# Wait for MinIO
sleep 5

# Create bucket
docker exec minio mc alias set local http://localhost:9000 minioadmin minioadmin
docker exec minio mc mb local/chronik-snapshots

echo "MinIO ready at http://localhost:9000 (user: minioadmin, pass: minioadmin)"
```

**Configuration** (`chronik-s3-test.toml`):
```toml
[object_store]
backend = "s3"
s3_endpoint = "http://localhost:9000"
s3_bucket = "chronik-snapshots"
s3_access_key = "minioadmin"
s3_secret_key = "minioadmin"
s3_path_style = true
s3_disable_ssl = true

[raft.snapshots]
max_log_size_mb = 10  # Trigger snapshot at 10MB for testing
min_interval_entries = 1000
```

**Test Script** (`test_s3_snapshot.sh`):
```bash
#!/bin/bash
set -e

# Start MinIO
./scripts/start_minio.sh

# Start 3-node cluster with S3 config
cargo run --features raft --bin chronik-server -- --config chronik-s3-test.toml --node-id 1 --advertised-addr localhost:9092 standalone --raft &
PID1=$!
cargo run --features raft --bin chronik-server -- --config chronik-s3-test.toml --node-id 2 --advertised-addr localhost:9093 standalone --raft &
PID2=$!
cargo run --features raft --bin chronik-server -- --config chronik-s3-test.toml --node-id 3 --advertised-addr localhost:9094 standalone --raft &
PID3=$!

sleep 10

# Produce 100K messages to trigger snapshot (10MB+)
python3 -c "
from kafka import KafkaProducer
p = KafkaProducer(bootstrap_servers='localhost:9092')
for i in range(100000):
    p.send('test', f'message-{i}'.encode() * 100)  # ~10KB per message
p.flush()
print('Produced 100K messages')
"

# Wait for snapshot creation and upload
sleep 30

# Verify snapshot in MinIO
docker exec minio mc ls local/chronik-snapshots/
if docker exec minio mc ls local/chronik-snapshots/ | grep -q "snapshot"; then
    echo "✅ Snapshot found in MinIO"
else
    echo "❌ Snapshot NOT found in MinIO"
    exit 1
fi

# Start 4th node (should bootstrap from S3 snapshot)
cargo run --features raft --bin chronik-server -- --config chronik-s3-test.toml --node-id 4 --advertised-addr localhost:9095 standalone --raft &
PID4=$!
sleep 20

# Verify 4th node joined and can serve data
python3 -c "
from kafka import KafkaConsumer
c = KafkaConsumer('test', bootstrap_servers='localhost:9095', auto_offset_reset='earliest', consumer_timeout_ms=10000)
count = sum(1 for _ in c)
print(f'Node 4 served {count} messages')
assert count > 0, 'Node 4 did not bootstrap correctly'
"

# Cleanup
kill $PID1 $PID2 $PID3 $PID4
docker stop minio && docker rm minio
echo "S3 snapshot test passed!"
```

**Acceptance Criteria**:
- ✅ Snapshot created when WAL exceeds 10MB
- ✅ Snapshot uploaded to MinIO successfully
- ✅ New node downloads snapshot on startup
- ✅ New node catches up and can serve data
- ✅ Snapshot compressed (< 50% of raw WAL size)

---

### Week 2 Deliverables

**By End of Week 2**:
- ✅ Chaos testing framework operational (Toxiproxy)
- ✅ Network partition, leader kill, cascading failure tests passing
- ✅ All Raft metrics verified and accurate
- ✅ ISR tracking working correctly (shrink/expand)
- ✅ S3 snapshot integration tested (MinIO)

**Exit Criteria**:
- Cluster survives all failure scenarios
- Metrics dashboards show accurate data
- New nodes can bootstrap from S3

---

## Week 3: Advanced Features & Documentation

**Goal**: Complete Phase 5 features, comprehensive documentation, prepare for GA

### Day 11-12: DNS Discovery Implementation

#### Task 3.1: Implement DNS SRV Record Querying
- **Priority**: 🟢 MEDIUM
- **Status**: ⬜ Not Started
- **Assignee**: TBD
- **Estimate**: 6 hours
- **Blockers**: None
- **Dependencies**: None (can run in parallel)

**Subtasks**:
- [ ] Add `trust-dns-resolver` dependency to `chronik-config`
- [ ] Implement DNS SRV lookup in `cluster.rs`
- [ ] Parse SRV records into peer list (hostname, port, priority)
- [ ] Add fallback to static config if DNS fails
- [ ] Handle DNS changes (periodic re-query)
- [ ] Add configuration option: `discovery.mode = "dns"`

**Files to Modify**:
- [crates/chronik-config/src/cluster.rs](crates/chronik-config/src/cluster.rs)
- `Cargo.toml` (add `trust-dns-resolver = "0.23"`)

**Code Skeleton**:
```rust
// In cluster.rs
use trust_dns_resolver::TokioAsyncResolver;

pub async fn discover_peers_from_dns(service: &str) -> Result<Vec<NodeConfig>> {
    let resolver = TokioAsyncResolver::tokio_from_system_conf()?;
    let srv_records = resolver.srv_lookup(service).await?;

    let mut peers = Vec::new();
    for srv in srv_records.iter() {
        let node = NodeConfig {
            node_id: hash_hostname(srv.target().to_string()), // deterministic ID
            address: srv.target().to_string(),
            port: srv.port(),
        };
        peers.push(node);
    }
    Ok(peers)
}
```

**Acceptance Criteria**:
- ✅ DNS SRV records parsed correctly
- ✅ Peer list updated when DNS changes
- ✅ Fallback to static config if DNS unavailable
- ✅ Works with Kubernetes headless service pattern

---

#### Task 3.2: Test DNS Discovery with Kubernetes Simulation
- **Priority**: 🟢 MEDIUM
- **Status**: ⬜ Not Started
- **Assignee**: TBD
- **Estimate**: 4 hours
- **Blockers**: Task 3.1
- **Dependencies**: DNS implementation complete

**Subtasks**:
- [ ] Set up local DNS server (dnsmasq or CoreDNS)
- [ ] Configure SRV records for `chronik-headless.default.svc.cluster.local`
- [ ] Start cluster using DNS discovery
- [ ] Verify all nodes discovered
- [ ] Add new node to DNS, verify cluster detects it
- [ ] Document DNS setup for Kubernetes

**DNS Configuration** (example for dnsmasq):
```
# /etc/dnsmasq.d/chronik.conf
srv-host=_chronik._tcp.chronik-headless.default.svc.cluster.local,chronik-0.chronik-headless.default.svc.cluster.local,9092,0,0
srv-host=_chronik._tcp.chronik-headless.default.svc.cluster.local,chronik-1.chronik-headless.default.svc.cluster.local,9092,0,0
srv-host=_chronik._tcp.chronik-headless.default.svc.cluster.local,chronik-2.chronik-headless.default.svc.cluster.local,9092,0,0
```

**Chronik Config**:
```toml
[cluster.discovery]
mode = "dns"
dns_service = "_chronik._tcp.chronik-headless.default.svc.cluster.local"
refresh_interval_secs = 30
```

**Acceptance Criteria**:
- ✅ Cluster discovers all nodes via DNS
- ✅ New node automatically joins when DNS updated
- ✅ Works with standard Kubernetes headless service

---

### Day 12-13: Rebalancer Integration

#### Task 3.3: Wire Rebalancer to Cluster Coordinator
- **Priority**: 🟢 MEDIUM
- **Status**: ⬜ Not Started
- **Assignee**: TBD
- **Estimate**: 4 hours
- **Blockers**: None
- **Dependencies**: Rebalancer code already exists

**Subtasks**:
- [ ] Integrate `PartitionRebalancer` with `ClusterCoordinator`
- [ ] Start background rebalance check loop (every 60s)
- [ ] Trigger leadership transfer when imbalance detected
- [ ] Update partition assignment in metadata
- [ ] Add metrics for rebalancing activity

**Files to Modify**:
- [crates/chronik-raft/src/cluster_coordinator.rs](crates/chronik-raft/src/cluster_coordinator.rs)
- [crates/chronik-raft/src/rebalancer.rs](crates/chronik-raft/src/rebalancer.rs)

**Code Integration**:
```rust
// In cluster_coordinator.rs
use crate::rebalancer::PartitionRebalancer;

pub struct ClusterCoordinator {
    // ... existing fields
    rebalancer: Arc<PartitionRebalancer>,
}

impl ClusterCoordinator {
    pub async fn start_rebalance_loop(&self) {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        loop {
            interval.tick().await;
            if let Some(plan) = self.rebalancer.check_imbalance().await {
                info!("Rebalancing needed: {:?}", plan);
                self.execute_rebalance_plan(plan).await;
            }
        }
    }
}
```

**Acceptance Criteria**:
- ✅ Rebalancer detects imbalance (node has 2x partitions vs. average)
- ✅ Automatic leadership transfer initiated
- ✅ Partition assignment updated in metadata
- ✅ Zero downtime during rebalance

---

#### Task 3.4: Test Rebalancer with Node Addition
- **Priority**: 🟢 MEDIUM
- **Status**: ⬜ Not Started
- **Assignee**: TBD
- **Estimate**: 4 hours
- **Blockers**: Task 3.3
- **Dependencies**: Rebalancer integration

**Subtasks**:
- [ ] Start 3-node cluster with 9 partitions (3 per node)
- [ ] Add 4th node to cluster
- [ ] Wait for rebalancer to detect imbalance
- [ ] Verify partitions redistribute (2-3 per node)
- [ ] Produce/consume during rebalance (verify zero downtime)
- [ ] Document rebalancing behavior

**Test Script** (`test_rebalancer.sh`):
```bash
#!/bin/bash
set -e

# Start 3-node cluster
# ... (nodes 1-3 on ports 9092-9094)

# Create topic with 9 partitions
python3 -c "
from kafka.admin import KafkaAdminClient, NewTopic
admin = KafkaAdminClient(bootstrap_servers='localhost:9092')
topic = NewTopic('test', num_partitions=9, replication_factor=3)
admin.create_topics([topic])
print('Created topic with 9 partitions')
"

# Verify initial distribution (3 partitions per node)
# ... check metrics or metadata

# Add 4th node
cargo run --features raft --bin chronik-server -- --node-id 4 --advertised-addr localhost:9095 standalone --raft &
PID4=$!
echo "Added 4th node - waiting for rebalance..."
sleep 60

# Verify new distribution (2-3 partitions per node)
# ... check metrics

# Produce during rebalance (should not fail)
python3 -c "
from kafka import KafkaProducer
p = KafkaProducer(bootstrap_servers='localhost:9092')
for i in range(1000):
    p.send('test', f'msg-{i}'.encode())
p.flush()
print('Produced 1000 messages during rebalance')
"

# Cleanup
kill $PID4
echo "Rebalancer test passed!"
```

**Acceptance Criteria**:
- ✅ Imbalance detected after 4th node added
- ✅ Partitions redistribute automatically
- ✅ Produce/consume continue during rebalance
- ✅ Final distribution balanced (2-3 per node)

---

### Day 14-15: Documentation Sprint

#### Task 3.5: Write Deployment Guide
- **Priority**: 🔴 CRITICAL (for GA)
- **Status**: ⬜ Not Started
- **Assignee**: TBD
- **Estimate**: 8 hours
- **Blockers**: None
- **Dependencies**: All features working

**Subtasks**:
- [ ] Document single-node deployment (standalone mode)
- [ ] Document 3-node cluster deployment (static config)
- [ ] Document 5-node cluster deployment (DNS discovery)
- [ ] Kubernetes deployment example (StatefulSet + headless service)
- [ ] Docker Compose example
- [ ] Troubleshooting section (common issues)
- [ ] Performance tuning guide

**Document Outline** (`docs/RAFT_DEPLOYMENT_GUIDE.md`):
```markdown
# Chronik Stream Raft Deployment Guide

## Table of Contents
1. Overview
2. Single-Node Deployment (Development)
3. 3-Node Cluster Deployment (Production)
4. 5-Node Cluster Deployment (High Availability)
5. Kubernetes Deployment
6. Docker Compose Deployment
7. Configuration Reference
8. Performance Tuning
9. Troubleshooting

## 1. Overview
Chronik Stream supports both standalone (single-node) and clustered (multi-node with Raft) deployments...

## 2. Single-Node Deployment
For development or low-traffic scenarios...
```bash
cargo run --bin chronik-server -- --advertised-addr localhost standalone
```

## 3. 3-Node Cluster Deployment
Production-ready setup with quorum-based replication...
```bash
# Node 1
cargo run --features raft --bin chronik-server -- \
  --config chronik-cluster.toml \
  --node-id 1 \
  --advertised-addr node1.example.com:9092 \
  standalone --raft

# Node 2
cargo run --features raft --bin chronik-server -- \
  --config chronik-cluster.toml \
  --node-id 2 \
  --advertised-addr node2.example.com:9092 \
  standalone --raft

# Node 3
cargo run --features raft --bin chronik-server -- \
  --config chronik-cluster.toml \
  --node-id 3 \
  --advertised-addr node3.example.com:9092 \
  standalone --raft
```

**Configuration** (`chronik-cluster.toml`):
```toml
[cluster]
enabled = true
replication_factor = 3
min_insync_replicas = 2

[cluster.peers]
nodes = [
  { id = 1, addr = "node1.example.com:9092" },
  { id = 2, addr = "node2.example.com:9092" },
  { id = 3, addr = "node3.example.com:9092" },
]

[raft]
election_timeout_ms = 1000
heartbeat_interval_ms = 100
snapshot_max_log_size_mb = 64
```

## 4. Kubernetes Deployment
See [kubernetes/chronik-statefulset.yaml](../kubernetes/chronik-statefulset.yaml)...

## 5. Troubleshooting
### Cluster Won't Form
- Check network connectivity between nodes
- Verify all nodes have same cluster config
- Check logs for Raft errors

### Leader Election Timeout
- Increase `election_timeout_ms` for high-latency networks
- Verify clocks synchronized (NTP)
...
```

**Acceptance Criteria**:
- ✅ Covers all deployment scenarios (single, 3-node, 5-node, K8s)
- ✅ Includes configuration examples
- ✅ Troubleshooting section with common issues
- ✅ Performance tuning recommendations

---

#### Task 3.6: Write Configuration Reference
- **Priority**: 🔴 CRITICAL (for GA)
- **Status**: ⬜ Not Started
- **Assignee**: TBD
- **Estimate**: 6 hours
- **Blockers**: None
- **Dependencies**: All configuration options finalized

**Subtasks**:
- [ ] Document all `[cluster]` options
- [ ] Document all `[raft]` options
- [ ] Document all `[raft.snapshots]` options
- [ ] Document environment variable overrides
- [ ] Provide examples for each option
- [ ] Add validation rules and defaults

**Document Template** (`docs/RAFT_CONFIGURATION_REFERENCE.md`):
```markdown
# Chronik Stream Raft Configuration Reference

## Cluster Configuration

### `[cluster]`
General cluster settings.

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `enabled` | bool | `false` | Enable Raft clustering mode |
| `node_id` | u64 | required | Unique node identifier (1-N) |
| `replication_factor` | u32 | `3` | Number of replicas per partition |
| `min_insync_replicas` | u32 | `2` | Minimum in-sync replicas for produce |

**Example**:
```toml
[cluster]
enabled = true
node_id = 1
replication_factor = 3
min_insync_replicas = 2
```

### `[cluster.peers]`
Static peer discovery.

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `nodes` | array | `[]` | List of peer nodes |

**Example**:
```toml
[cluster.peers]
nodes = [
  { id = 1, addr = "10.0.1.10:9092" },
  { id = 2, addr = "10.0.1.11:9092" },
  { id = 3, addr = "10.0.1.12:9092" },
]
```

### `[cluster.discovery]`
Dynamic peer discovery (DNS).

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `mode` | string | `"static"` | Discovery mode: `static` or `dns` |
| `dns_service` | string | - | DNS SRV service name (e.g., `_chronik._tcp.svc.local`) |
| `refresh_interval_secs` | u64 | `30` | DNS re-query interval |

**Example**:
```toml
[cluster.discovery]
mode = "dns"
dns_service = "_chronik._tcp.chronik-headless.default.svc.cluster.local"
refresh_interval_secs = 30
```

## Raft Configuration

### `[raft]`
Core Raft protocol settings.

| Option | Type | Default | Description | Tuning Guidance |
|--------|------|---------|-------------|-----------------|
| `election_timeout_ms` | u64 | `1000` | Election timeout (ms) | LAN: 500-1000ms, WAN: 2000-5000ms |
| `heartbeat_interval_ms` | u64 | `100` | Heartbeat interval (ms) | Should be < election_timeout/10 |
| `rpc_timeout_ms` | u64 | `5000` | RPC timeout (ms) | Increase for slow networks |
| `max_inflight_messages` | usize | `256` | Max concurrent Raft messages | Higher = more bandwidth |

**Example (LAN)**:
```toml
[raft]
election_timeout_ms = 1000
heartbeat_interval_ms = 100
rpc_timeout_ms = 5000
```

**Example (WAN/Multi-DC)**:
```toml
[raft]
election_timeout_ms = 5000  # 5x LAN
heartbeat_interval_ms = 500
rpc_timeout_ms = 15000
```

### `[raft.snapshots]`
Snapshot configuration (log compaction).

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `max_log_size_mb` | u64 | `64` | Trigger snapshot at this WAL size |
| `min_interval_entries` | u64 | `10000` | Minimum entries between snapshots |
| `compression` | string | `"zstd"` | Compression algorithm: `none`, `gzip`, `zstd` |

**Example**:
```toml
[raft.snapshots]
max_log_size_mb = 64
min_interval_entries = 10000
compression = "zstd"
```

### `[raft.advanced]`
Advanced Raft tuning (use with caution).

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `replica_lag_max_entries` | u64 | `1000` | Remove from ISR if lagging > N entries |
| `replica_lag_max_ms` | u64 | `10000` | Remove from ISR if lagging > N ms |
| `enable_leadership_transfer` | bool | `true` | Transfer leadership on graceful shutdown |
| `batch_append_entries` | bool | `true` | Batch Raft messages for efficiency |

**Example**:
```toml
[raft.advanced]
replica_lag_max_entries = 1000
replica_lag_max_ms = 10000
enable_leadership_transfer = true
```

## Environment Variables

All TOML options can be overridden via environment variables:

| Environment Variable | TOML Equivalent | Example |
|---------------------|-----------------|---------|
| `CHRONIK_CLUSTER_ENABLED` | `cluster.enabled` | `true` |
| `CHRONIK_NODE_ID` | `cluster.node_id` | `1` |
| `CHRONIK_RAFT_ELECTION_TIMEOUT_MS` | `raft.election_timeout_ms` | `1000` |
| `CHRONIK_CLUSTER_PEERS` | `cluster.peers.nodes` | `10.0.1.10:9092,10.0.1.11:9092` |

**Example**:
```bash
CHRONIK_CLUSTER_ENABLED=true \
CHRONIK_NODE_ID=1 \
CHRONIK_RAFT_ELECTION_TIMEOUT_MS=1000 \
cargo run --features raft --bin chronik-server -- standalone --raft
```

## Configuration Validation

Chronik validates configuration on startup:
- `replication_factor` must be odd (3, 5, 7)
- `min_insync_replicas` must be ≤ `replication_factor`
- `heartbeat_interval_ms` must be < `election_timeout_ms`
- `node_id` must be unique in cluster

Invalid configurations will fail with error message.
```

**Acceptance Criteria**:
- ✅ All configuration options documented
- ✅ Includes default values and validation rules
- ✅ Provides examples for each option
- ✅ Covers environment variable overrides

---

#### Task 3.7: Write Troubleshooting Guide
- **Priority**: 🔴 CRITICAL (for GA)
- **Status**: ⬜ Not Started
- **Assignee**: TBD
- **Estimate**: 6 hours
- **Blockers**: Tasks 1-3 (need real issues from testing)
- **Dependencies**: All testing complete

**Subtasks**:
- [ ] Document common cluster formation issues
- [ ] Document leader election problems
- [ ] Document replication lag issues
- [ ] Document snapshot/bootstrap failures
- [ ] Add log examples for each issue
- [ ] Provide resolution steps

**Document Template** (`docs/RAFT_TROUBLESHOOTING.md`):
```markdown
# Chronik Stream Raft Troubleshooting Guide

## Table of Contents
1. Cluster Formation Issues
2. Leader Election Problems
3. Replication Lag
4. Snapshot Failures
5. ISR Issues
6. Performance Problems
7. Data Corruption

---

## 1. Cluster Formation Issues

### Problem: Cluster Won't Form (Nodes Don't Connect)

**Symptoms**:
- Logs show: `Waiting for quorum... (1/3 nodes)`
- Nodes don't discover each other
- Cluster never elects leader

**Root Causes**:
1. Network connectivity issues between nodes
2. Incorrect peer configuration
3. Firewall blocking Raft ports
4. Mismatched cluster IDs

**Resolution Steps**:

**Step 1: Verify network connectivity**
```bash
# From each node, test connectivity to peers
telnet node2.example.com 9092
telnet node3.example.com 9092
```

**Step 2: Check peer configuration**
```bash
# Verify all nodes have identical cluster.peers configuration
grep -A 5 "cluster.peers" chronik-cluster.toml
```

**Step 3: Check Raft gRPC port**
Raft uses Kafka port + 100 for gRPC (e.g., 9092 → 9192)
```bash
# Verify Raft port is open
netstat -tuln | grep 9192
```

**Step 4: Check logs for errors**
```bash
RUST_LOG=chronik_raft=debug cargo run --bin chronik-server
# Look for: "Failed to connect to peer", "Connection refused"
```

**Expected Logs (Success)**:
```
INFO chronik_raft::cluster_coordinator - Peer node2 connected
INFO chronik_raft::cluster_coordinator - Peer node3 connected
INFO chronik_raft::replica - Quorum reached, starting leader election
INFO chronik_raft::replica - Node 1 elected as leader
```

---

### Problem: Cluster Forms But No Leader Elected

**Symptoms**:
- All nodes connected but stuck in "Candidate" state
- Logs show: `Election timeout, starting new election`
- Never reaches "Leader" state

**Root Causes**:
1. Clock skew between nodes (> 500ms)
2. Election timeout too low for network latency
3. Split vote (2-node clusters don't work)

**Resolution**:

**Step 1: Verify clock synchronization**
```bash
# Check NTP status on all nodes
timedatectl status
# Or
ntpq -p

# Clock skew should be < 100ms
```

**Step 2: Increase election timeout**
```toml
[raft]
election_timeout_ms = 2000  # Increase from default 1000ms
```

**Step 3: Ensure odd number of nodes**
Raft requires majority quorum. 2-node clusters can't elect leader (2/2 = no majority).
```
✅ 3 nodes: Quorum = 2 (can lose 1 node)
✅ 5 nodes: Quorum = 3 (can lose 2 nodes)
❌ 2 nodes: Quorum = 2 (can't lose any - split brain)
```

---

## 2. Replication Lag

### Problem: Follower Lag Increasing

**Symptoms**:
- Metrics show: `chronik_raft_follower_lag_entries{follower="2"} 10000+`
- Follower removed from ISR
- Produce operations slow or fail

**Root Causes**:
1. Slow network between leader and follower
2. Follower overloaded (high CPU/disk I/O)
3. Large batch sizes overwhelming follower

**Resolution**:

**Step 1: Check network latency**
```bash
# From leader to follower
ping -c 10 node2.example.com
# Should be < 50ms for LAN

# Check bandwidth
iperf3 -c node2.example.com
```

**Step 2: Check follower resource usage**
```bash
# On follower node
top  # Check CPU usage
iostat -x 1  # Check disk I/O
```

**Step 3: Reduce batch size**
```toml
[raft]
max_append_entries_size = 524288  # Reduce from 1MB to 512KB
```

**Step 4: Increase ISR lag threshold**
```toml
[raft.advanced]
replica_lag_max_entries = 5000   # Increase from 1000
replica_lag_max_ms = 30000       # Increase from 10s to 30s
```

---

## 3. Snapshot Failures

### Problem: Snapshot Creation Fails

**Symptoms**:
- Logs show: `Failed to create snapshot: ...`
- WAL grows unbounded (never compacted)
- New nodes can't bootstrap

**Root Causes**:
1. Insufficient disk space
2. S3 credentials invalid
3. Snapshot compression failure

**Resolution**:

**Step 1: Check disk space**
```bash
df -h /data  # Ensure > 20% free space
```

**Step 2: Verify S3 configuration**
```bash
# Test S3 access manually
aws s3 ls s3://chronik-snapshots --endpoint-url http://minio:9000
```

**Step 3: Check snapshot logs**
```bash
RUST_LOG=chronik_raft::snapshot=debug cargo run --bin chronik-server
# Look for specific error (e.g., "Access Denied", "Disk full")
```

**Step 4: Disable compression temporarily**
```toml
[raft.snapshots]
compression = "none"  # Disable to isolate compression issues
```

---

## 4. Performance Problems

### Problem: High Commit Latency (> 100ms)

**Symptoms**:
- Metrics show: `chronik_raft_commit_latency_ms{quantile="0.99"} 500+`
- Producer operations slow
- Users complain about latency

**Root Causes**:
1. Network latency between nodes (WAN deployment)
2. Disk I/O bottleneck (slow fsync)
3. Too many partitions per node

**Resolution**:

**Step 1: Profile network latency**
```bash
# Measure inter-node latency
ping -c 100 node2.example.com | tail -1
# Should be < 10ms for LAN, < 100ms for WAN
```

**Step 2: Tune for WAN**
```toml
[raft]
election_timeout_ms = 5000     # 5x LAN
heartbeat_interval_ms = 500
rpc_timeout_ms = 15000
```

**Step 3: Optimize WAL fsync**
```toml
[wal]
group_commit_interval_ms = 10  # Batch commits
max_batch_size = 100           # Larger batches
```

**Step 4: Reduce partition count per node**
```
Too many partitions = too many Raft groups = high overhead
Recommendation: < 100 partitions per node
```

---

## 5. Data Corruption

### Problem: WAL Checksum Errors

**Symptoms**:
- Logs show: `WAL checksum mismatch`
- Node fails to start after crash
- Data loss detected

**THIS SHOULD NEVER HAPPEN** - contact Chronik developers immediately.

**Emergency Recovery**:
```bash
# Backup corrupted WAL
cp -r ./data/wal ./data/wal.backup

# Try to recover from S3 snapshot
rm -rf ./data/wal
# Restart node (will download snapshot from S3)
cargo run --features raft --bin chronik-server -- ...
```

**Prevention**:
- Use reliable storage (EBS with provisioned IOPS, not instance storage)
- Enable S3 snapshots for disaster recovery
- Test backups regularly

---

## 6. Debugging Tools

### Enable Debug Logging
```bash
RUST_LOG=chronik_raft=debug,chronik_server=info cargo run --bin chronik-server
```

### Check Metrics
```bash
curl http://localhost:8080/metrics | grep chronik_raft
```

### Dump Raft State (future feature)
```bash
chronik-server raft status --node-id 1
# Output: Leader, Follower, or Candidate
# Current term: 5
# Commit index: 12345
# ISR: [1, 2, 3]
```

---

## 7. Getting Help

If you encounter issues not covered here:
1. Check logs with `RUST_LOG=debug`
2. Collect metrics from `/metrics` endpoint
3. File an issue at https://github.com/your-repo/chronik-stream/issues
4. Include: logs, metrics, configuration, repro steps
```

**Acceptance Criteria**:
- ✅ Covers all common issues from testing
- ✅ Includes log examples for each problem
- ✅ Provides clear resolution steps
- ✅ Links to relevant configuration options

---

#### Task 3.8: Write Migration Guide (v1.3 → v2.0)
- **Priority**: 🟡 HIGH
- **Status**: ⬜ Not Started
- **Assignee**: TBD
- **Estimate**: 4 hours
- **Blockers**: None
- **Dependencies**: v2.0 features finalized

**Subtasks**:
- [ ] Document breaking changes (if any)
- [ ] Provide migration path for existing deployments
- [ ] WAL format changes (V2 → V3 if applicable)
- [ ] Configuration migration (old → new format)
- [ ] Rollback procedure (v2.0 → v1.3 if needed)

**Document Template** (`docs/MIGRATION_GUIDE_V2.md`):
```markdown
# Migration Guide: Chronik v1.3.x → v2.0.0

## Overview

Chronik v2.0.0 introduces Raft-based clustering for multi-node deployments. This guide helps you migrate from standalone (v1.3.x) to clustered (v2.0.0) mode.

**Key Changes**:
- ✅ Backward compatible: v1.3.x standalone mode still works
- ✅ New cluster mode (`--raft` flag) for multi-node deployments
- ✅ WAL format unchanged (no data migration needed)
- ⚠️ New configuration options for clustering

---

## Migration Paths

### Path 1: Stay on Standalone (No Migration Needed)

If you don't need multi-node replication, continue using standalone mode:
```bash
# v1.3.x (old)
cargo run --bin chronik-server -- standalone

# v2.0.0 (new - identical)
cargo run --bin chronik-server -- standalone
```

**No changes required**. Your existing data and configuration work as-is.

---

### Path 2: Migrate to 3-Node Cluster

**Prerequisites**:
- 3 servers (physical or VMs)
- Network connectivity between nodes (ports 9092, 9192)
- Existing v1.3.x data to preserve

**Step-by-Step Migration**:

**Step 1: Backup existing data**
```bash
# On existing v1.3.x node
tar czf chronik-backup-$(date +%F).tar.gz ./data/
```

**Step 2: Stop v1.3.x server**
```bash
pkill chronik-server
```

**Step 3: Upgrade binary to v2.0.0**
```bash
cargo build --release --features raft --bin chronik-server
```

**Step 4: Create cluster configuration**
Create `chronik-cluster.toml`:
```toml
[cluster]
enabled = true
replication_factor = 3
min_insync_replicas = 2

[cluster.peers]
nodes = [
  { id = 1, addr = "node1.example.com:9092" },
  { id = 2, addr = "node2.example.com:9092" },
  { id = 3, addr = "node3.example.com:9092" },
]

[raft]
election_timeout_ms = 1000
heartbeat_interval_ms = 100
snapshot_max_log_size_mb = 64
```

**Step 5: Start first node with existing data**
```bash
# On node1 (has existing data)
./target/release/chronik-server \
  --config chronik-cluster.toml \
  --node-id 1 \
  --advertised-addr node1.example.com:9092 \
  standalone --raft
```

**Step 6: Start remaining nodes (empty data)**
```bash
# On node2
./target/release/chronik-server \
  --config chronik-cluster.toml \
  --node-id 2 \
  --advertised-addr node2.example.com:9092 \
  standalone --raft

# On node3
./target/release/chronik-server \
  --config chronik-cluster.toml \
  --node-id 3 \
  --advertised-addr node3.example.com:9092 \
  standalone --raft
```

**Step 7: Verify cluster formation**
```bash
# Check logs on all nodes
# Should see: "Node 1 elected as leader"

# Produce test message
kafka-console-producer --bootstrap-server node1.example.com:9092 --topic test
> hello cluster

# Consume from any node
kafka-console-consumer --bootstrap-server node2.example.com:9092 --topic test --from-beginning
# Should see: hello cluster
```

**Step 8: Verify replication**
```bash
# Check metrics on all nodes
curl http://node1.example.com:8080/metrics | grep chronik_raft_isr_size
# Should show: chronik_raft_isr_size{partition="0"} 3
```

---

## Configuration Changes

### New Configuration Options (v2.0)

| Option | Purpose | Default | Required for Clustering |
|--------|---------|---------|------------------------|
| `cluster.enabled` | Enable clustering | `false` | ✅ YES |
| `cluster.peers.nodes` | Peer list | `[]` | ✅ YES |
| `raft.election_timeout_ms` | Election timeout | `1000` | ⚠️ Optional (tune for network) |
| `raft.snapshots.max_log_size_mb` | Snapshot trigger | `64` | ⚠️ Optional |

### Deprecated Options

**None**. All v1.3.x options still work in v2.0.0.

---

## Breaking Changes

### None

v2.0.0 is **fully backward compatible** with v1.3.x for standalone mode.

**New features** (Raft clustering) are opt-in via `--raft` flag.

---

## Rollback Procedure

If you need to rollback from v2.0.0 to v1.3.x:

**Step 1: Stop v2.0.0 cluster**
```bash
pkill chronik-server
```

**Step 2: Restore v1.3.x binary**
```bash
# Use backup or rebuild v1.3.x
git checkout v1.3.65
cargo build --release --bin chronik-server
```

**Step 3: Start in standalone mode**
```bash
./target/release/chronik-server standalone
```

**Step 4: Verify data intact**
```bash
kafka-console-consumer --bootstrap-server localhost:9092 --topic test --from-beginning
# Should see all messages
```

**Data Safety**: WAL format unchanged, no data loss.

---

## Frequently Asked Questions

### Q: Can I run a 2-node cluster?
**A**: No. Raft requires odd number of nodes (3, 5, 7) for quorum. 2-node clusters can't elect leader.

### Q: Can I mix v1.3.x and v2.0.0 nodes in a cluster?
**A**: No. All nodes must be v2.0.0 when using `--raft` mode.

### Q: Do I need to migrate data when upgrading?
**A**: No. WAL format unchanged. Existing data works as-is.

### Q: What happens to my existing topics/partitions?
**A**: Preserved. Cluster mode replicates them across nodes.

### Q: Can I downgrade from cluster to standalone?
**A**: Yes. Stop cluster, start one node in standalone mode. Other nodes' data is redundant.

---

## Need Help?

- Deployment Guide: [RAFT_DEPLOYMENT_GUIDE.md](RAFT_DEPLOYMENT_GUIDE.md)
- Troubleshooting: [RAFT_TROUBLESHOOTING.md](RAFT_TROUBLESHOOTING.md)
- Configuration Reference: [RAFT_CONFIGURATION_REFERENCE.md](RAFT_CONFIGURATION_REFERENCE.md)
- File an issue: https://github.com/your-repo/chronik-stream/issues
```

**Acceptance Criteria**:
- ✅ Clear migration path for existing deployments
- ✅ Step-by-step instructions
- ✅ Rollback procedure documented
- ✅ FAQ answers common questions

---

### Week 3 Deliverables

**By End of Week 3**:
- ✅ DNS discovery implemented and tested
- ✅ Rebalancer integrated and working
- ✅ Complete deployment guide
- ✅ Complete configuration reference
- ✅ Complete troubleshooting guide
- ✅ Complete migration guide

**Exit Criteria**:
- All Phase 5 features working
- Documentation comprehensive enough for GA release
- Users can deploy without hand-holding

---

## Week 4 (Buffer): Final Testing & Release Prep

**Goal**: Polish, final testing, prepare for GA release

### Day 16-17: Performance Benchmarking

#### Task 4.1: Throughput Benchmark (Cluster vs. Standalone)
- **Priority**: 🟢 MEDIUM
- **Status**: ⬜ Not Started
- **Assignee**: TBD
- **Estimate**: 4 hours
- **Blockers**: None
- **Dependencies**: Cluster fully working

**Subtasks**:
- [ ] Benchmark standalone mode (baseline)
- [ ] Benchmark 3-node cluster (with replication)
- [ ] Measure throughput (messages/sec)
- [ ] Measure latency (p50, p95, p99)
- [ ] Document overhead of replication
- [ ] Publish results

**Benchmark Script** (`benchmark_raft_vs_standalone.py`):
```python
import time
from kafka import KafkaProducer, KafkaConsumer
import statistics

def benchmark_produce(bootstrap_servers, num_messages=100000):
    producer = KafkaProducer(bootstrap_servers=bootstrap_servers)

    start = time.time()
    latencies = []

    for i in range(num_messages):
        msg_start = time.time()
        future = producer.send('bench-topic', f"message-{i}".encode())
        future.get(timeout=10)  # Wait for ack
        latencies.append((time.time() - msg_start) * 1000)  # ms

    producer.flush()
    elapsed = time.time() - start

    throughput = num_messages / elapsed
    p50 = statistics.median(latencies)
    p95 = statistics.quantiles(latencies, n=20)[18]  # 95th percentile
    p99 = statistics.quantiles(latencies, n=100)[98]  # 99th percentile

    return {
        'throughput': throughput,
        'latency_p50': p50,
        'latency_p95': p95,
        'latency_p99': p99,
    }

# Benchmark standalone
print("Benchmarking standalone mode...")
standalone_results = benchmark_produce('localhost:9092')
print(f"Standalone: {standalone_results['throughput']:.0f} msg/s, "
      f"p99 latency: {standalone_results['latency_p99']:.2f}ms")

# Benchmark cluster
print("Benchmarking 3-node cluster...")
cluster_results = benchmark_produce('localhost:9092,localhost:9093,localhost:9094')
print(f"Cluster: {cluster_results['throughput']:.0f} msg/s, "
      f"p99 latency: {cluster_results['latency_p99']:.2f}ms")

# Calculate overhead
overhead_pct = ((standalone_results['throughput'] - cluster_results['throughput']) /
                standalone_results['throughput']) * 100
print(f"\nReplication overhead: {overhead_pct:.1f}%")
```

**Target Metrics**:
- ✅ Cluster throughput ≥ 80% of standalone
- ✅ Cluster p99 latency < 50ms
- ✅ Replication overhead < 30%

---

#### Task 4.2: Load Test (Sustained 100K msg/s)
- **Priority**: 🟢 MEDIUM
- **Status**: ⬜ Not Started
- **Assignee**: TBD
- **Estimate**: 4 hours
- **Blockers**: Task 4.1
- **Dependencies**: Benchmark infrastructure

**Subtasks**:
- [ ] Run 1-hour sustained load test (100K msg/s)
- [ ] Monitor memory usage (check for leaks)
- [ ] Monitor CPU usage (check for hotspots)
- [ ] Monitor disk I/O (check for bottlenecks)
- [ ] Verify zero message loss
- [ ] Document resource requirements

**Load Test Script** (`load_test_sustained.py`):
```python
import time
import threading
from kafka import KafkaProducer

def produce_worker(worker_id, bootstrap_servers, duration_secs=3600):
    producer = KafkaProducer(bootstrap_servers=bootstrap_servers, linger_ms=10)

    start = time.time()
    count = 0

    while time.time() - start < duration_secs:
        producer.send('load-test', f"worker-{worker_id}-msg-{count}".encode())
        count += 1

        if count % 1000 == 0:
            producer.flush()

    producer.flush()
    return count

# Start 10 producer threads (10K msg/s each = 100K total)
threads = []
for i in range(10):
    t = threading.Thread(target=produce_worker, args=(i, 'localhost:9092,localhost:9093,localhost:9094', 3600))
    t.start()
    threads.append(t)

print("Load test running for 1 hour (100K msg/s)...")
print("Monitor with: watch -n 1 'curl -s http://localhost:8080/metrics | grep chronik_raft'")

for t in threads:
    t.join()

print("Load test complete!")
```

**Monitoring Checklist**:
- [ ] Memory usage stable (no leaks)
- [ ] CPU usage < 80% per node
- [ ] Disk I/O < 80% capacity
- [ ] Replication lag < 100ms p99
- [ ] No errors in logs

**Acceptance Criteria**:
- ✅ Cluster handles 100K msg/s for 1 hour
- ✅ Memory usage stable (< 5% growth per hour)
- ✅ Zero message loss
- ✅ All metrics healthy

---

### Day 18-19: Final Integration Testing

#### Task 4.3: Multi-Client Compatibility Test
- **Priority**: 🟡 HIGH
- **Status**: ⬜ Not Started
- **Assignee**: TBD
- **Estimate**: 4 hours
- **Blockers**: None
- **Dependencies**: Cluster working

**Subtasks**:
- [ ] Test with kafka-python (already done in Week 1)
- [ ] Test with confluent-kafka-python
- [ ] Test with Java Kafka client (kafka-console-producer/consumer)
- [ ] Test with KSQLDB (if installed)
- [ ] Test with Apache Flink (if available)
- [ ] Document any compatibility issues

**Test Matrix**:
| Client | Version | Status | Notes |
|--------|---------|--------|-------|
| kafka-python | 2.0.2 | ⬜ | Standard Python client |
| confluent-kafka-python | 2.3.0 | ⬜ | Confluent-supported client |
| kafka-console-producer | 3.6.0 | ⬜ | Java CLI tools |
| KSQLDB | 0.29.0 | ⬜ | SQL engine (if installed) |
| Apache Flink | 1.18.0 | ⬜ | Stream processing (optional) |

**Test Script Template**:
```bash
# Test with confluent-kafka-python
pip install confluent-kafka

python3 - <<EOF
from confluent_kafka import Producer, Consumer

# Produce
p = Producer({'bootstrap.servers': 'localhost:9092,localhost:9093,localhost:9094'})
for i in range(100):
    p.produce('test', f'msg-{i}'.encode())
p.flush()
print('✅ confluent-kafka-python: Produce OK')

# Consume
c = Consumer({
    'bootstrap.servers': 'localhost:9092',
    'group.id': 'test-group',
    'auto.offset.reset': 'earliest',
})
c.subscribe(['test'])
msgs = []
while len(msgs) < 100:
    msg = c.poll(timeout=5.0)
    if msg:
        msgs.append(msg)
print(f'✅ confluent-kafka-python: Consumed {len(msgs)} messages')
EOF

# Test with Java kafka-console-producer (if installed)
if command -v kafka-console-producer &> /dev/null; then
    echo "test message" | kafka-console-producer --bootstrap-server localhost:9092 --topic test
    kafka-console-consumer --bootstrap-server localhost:9092 --topic test --from-beginning --max-messages 1
    echo "✅ Java kafka-console-* tools: OK"
else
    echo "⚠️ Java Kafka tools not installed (skipping)"
fi
```

**Acceptance Criteria**:
- ✅ kafka-python works (already verified)
- ✅ confluent-kafka-python works
- ✅ Java Kafka clients work (if available)
- ✅ No protocol errors with any client

---

#### Task 4.4: Graceful Shutdown Test
- **Priority**: 🟡 HIGH
- **Status**: ⬜ Not Started
- **Assignee**: TBD
- **Estimate**: 2 hours
- **Blockers**: None
- **Dependencies**: Shutdown handler implemented (Phase 4.2)

**Subtasks**:
- [ ] Start 3-node cluster
- [ ] Identify leader node
- [ ] Send SIGTERM to leader (graceful shutdown)
- [ ] Verify leadership transfer to follower
- [ ] Verify no message loss during shutdown
- [ ] Verify cluster continues operating
- [ ] Document shutdown behavior

**Test Script** (`test_graceful_shutdown.sh`):
```bash
#!/bin/bash
set -e

# Start 3-node cluster
cargo run --features raft --bin chronik-server -- --node-id 1 --advertised-addr localhost:9092 standalone --raft &
PID1=$!
cargo run --features raft --bin chronik-server -- --node-id 2 --advertised-addr localhost:9093 standalone --raft &
PID2=$!
cargo run --features raft --bin chronik-server -- --node-id 3 --advertised-addr localhost:9094 standalone --raft &
PID3=$!

sleep 10

# Produce initial messages
python3 -c "
from kafka import KafkaProducer
p = KafkaProducer(bootstrap_servers='localhost:9092')
for i in range(500):
    p.send('test', f'before-shutdown-{i}'.encode())
p.flush()
print('Produced 500 messages before shutdown')
"

# Graceful shutdown of leader (assume Node 1, check logs for actual leader)
echo "Sending SIGTERM to leader (Node 1)..."
kill -TERM $PID1

# Wait for leadership transfer (should be < 5 seconds)
sleep 5

# Continue producing (should go to new leader)
python3 -c "
from kafka import KafkaProducer
p = KafkaProducer(bootstrap_servers='localhost:9092,localhost:9093,localhost:9094')
for i in range(500):
    p.send('test', f'after-shutdown-{i}'.encode())
p.flush()
print('Produced 500 messages after shutdown')
"

# Verify all 1000 messages consumed
python3 -c "
from kafka import KafkaConsumer
c = KafkaConsumer('test', bootstrap_servers='localhost:9092', auto_offset_reset='earliest', consumer_timeout_ms=10000)
count = sum(1 for _ in c)
print(f'Consumed {count} messages')
assert count == 1000, f'Expected 1000, got {count}'
"

# Cleanup
kill $PID2 $PID3 2>/dev/null || true
echo "✅ Graceful shutdown test passed!"
```

**Acceptance Criteria**:
- ✅ SIGTERM triggers leadership transfer
- ✅ New leader elected in < 5 seconds
- ✅ Old leader shuts down cleanly
- ✅ Zero message loss (1000 produced, 1000 consumed)
- ✅ Cluster continues operating

---

### Day 20: Release Preparation

#### Task 4.5: Create Release Notes
- **Priority**: 🔴 CRITICAL
- **Status**: ⬜ Not Started
- **Assignee**: TBD
- **Estimate**: 4 hours
- **Blockers**: All testing complete
- **Dependencies**: All features finalized

**Subtasks**:
- [ ] Summarize new features (Raft clustering, ISR, snapshots, etc.)
- [ ] List breaking changes (if any)
- [ ] Document upgrade path (v1.3 → v2.0)
- [ ] Include performance benchmarks
- [ ] Add known limitations
- [ ] Link to documentation

**Release Notes Template** (`CHANGELOG.md` entry):
```markdown
# v2.0.0 - Raft Clustering GA (2025-11-XX)

## 🎉 Major Features

### Raft-Based Clustering
Transform Chronik from single-node to distributed multi-node cluster with:
- **Replication**: 3x redundancy (configurable)
- **High Availability**: Automatic leader election per partition
- **Strong Consistency**: Raft consensus for data and metadata
- **Zero Data Loss**: Quorum-based commits with ISR tracking

**Usage**:
```bash
# Start 3-node cluster
cargo run --features raft --bin chronik-server -- --node-id 1 --advertised-addr node1:9092 standalone --raft
cargo run --features raft --bin chronik-server -- --node-id 2 --advertised-addr node2:9092 standalone --raft
cargo run --features raft --bin chronik-server -- --node-id 3 --advertised-addr node3:9092 standalone --raft
```

**Configuration**:
```toml
[cluster]
enabled = true
replication_factor = 3
min_insync_replicas = 2

[cluster.peers]
nodes = [
  { id = 1, addr = "node1:9092" },
  { id = 2, addr = "node2:9092" },
  { id = 3, addr = "node3:9092" },
]
```

See [Deployment Guide](docs/RAFT_DEPLOYMENT_GUIDE.md) for details.

---

## ✨ New Features

### Production Features
- **ISR Tracking**: Automatic in-sync replica tracking with dynamic shrink/expand ([#123](link))
- **Graceful Shutdown**: Leadership transfer on SIGTERM for zero-downtime deployments ([#124](link))
- **S3 Snapshots**: New nodes bootstrap from S3 snapshots for fast catchup ([#125](link))
- **Replication Metrics**: Comprehensive Prometheus metrics for monitoring ([#126](link))
  - `chronik_raft_leader_count`, `chronik_raft_follower_lag`, `chronik_raft_isr_size`, etc.

### Advanced Features
- **DNS Discovery**: Dynamic peer discovery via DNS SRV records (Kubernetes-ready) ([#127](link))
- **Partition Rebalancing**: Automatic load balancing when nodes added/removed ([#128](link))
- **Multi-DC Replication**: Cross-region replication with WAN-optimized timeouts ([#129](link))
- **Lease-Based Reads**: Linearizable reads without Raft consensus ([#130](link))

---

## 📊 Performance

Benchmarks (3-node cluster, RF=3):
- **Throughput**: 85K msg/s (85% of standalone)
- **Latency (p99)**: 35ms commit latency
- **Leader Election**: < 3 seconds
- **Replication Lag**: < 50ms p99

See [Performance Guide](docs/RAFT_PERFORMANCE.md) for tuning.

---

## 🔄 Upgrade Guide

**From v1.3.x to v2.0.0**:
- ✅ **Backward Compatible**: Standalone mode unchanged
- ✅ **No Data Migration**: WAL format unchanged
- ⚠️ **New Clustering Features**: Opt-in via `--raft` flag

See [Migration Guide](docs/MIGRATION_GUIDE_V2.md) for step-by-step instructions.

---

## 📚 Documentation

New documentation:
- [Deployment Guide](docs/RAFT_DEPLOYMENT_GUIDE.md) - Single-node, 3-node, 5-node, Kubernetes
- [Configuration Reference](docs/RAFT_CONFIGURATION_REFERENCE.md) - All Raft options
- [Troubleshooting Guide](docs/RAFT_TROUBLESHOOTING.md) - Common issues and solutions
- [Migration Guide](docs/MIGRATION_GUIDE_V2.md) - Upgrade from v1.3.x

---

## ⚠️ Breaking Changes

**None**. v2.0.0 is fully backward compatible with v1.3.x for standalone mode.

---

## 🐛 Bug Fixes

- Fixed CRC validation for Raft-replicated messages ([#131](link))
- Fixed ISR tracking with slow followers ([#132](link))
- Fixed snapshot upload to S3 with large WALs ([#133](link))

---

## 🔬 Known Limitations

- DNS discovery requires Rust 1.75+ (async DNS resolver)
- Maximum 100 partitions per node recommended (higher = performance degradation)
- 2-node clusters not supported (Raft requires odd numbers: 3, 5, 7)

---

## 🙏 Acknowledgments

Special thanks to:
- TiKV team for [`raft-rs`](https://github.com/tikv/raft-rs)
- Kafka community for protocol specifications
- Early testers for feedback and bug reports

---

## 📦 Installation

**Cargo**:
```bash
cargo install chronik-server --features raft --version 2.0.0
```

**Docker**:
```bash
docker pull chronik/chronik-stream:v2.0.0
```

**From Source**:
```bash
git clone https://github.com/your-repo/chronik-stream.git
cd chronik-stream
git checkout v2.0.0
cargo build --release --features raft --bin chronik-server
```

---

**Full Changelog**: v1.3.65...v2.0.0
```

**Acceptance Criteria**:
- ✅ All features summarized
- ✅ Upgrade path documented
- ✅ Performance benchmarks included
- ✅ Links to documentation

---

#### Task 4.6: Tag v2.0.0 Release
- **Priority**: 🔴 CRITICAL
- **Status**: ⬜ Not Started
- **Assignee**: TBD
- **Estimate**: 1 hour
- **Blockers**: All testing complete, release notes written
- **Dependencies**: Everything

**Subtasks**:
- [ ] Update `Cargo.toml` version to `2.0.0`
- [ ] Update `CHANGELOG.md` with v2.0.0 entry
- [ ] Commit changes: `git commit -m "chore: Bump version to v2.0.0"`
- [ ] Create Git tag: `git tag -a v2.0.0 -m "Release v2.0.0 - Raft Clustering GA"`
- [ ] Push tag: `git push origin v2.0.0`
- [ ] Create GitHub release with release notes

**Checklist Before Tagging**:
- [ ] All tests passing (`cargo test --features raft --workspace`)
- [ ] Documentation complete (deployment, config, troubleshooting, migration)
- [ ] Benchmarks run and results documented
- [ ] Release notes written
- [ ] No known critical bugs

**GitHub Release Steps**:
1. Go to https://github.com/your-repo/chronik-stream/releases/new
2. Tag: `v2.0.0`
3. Title: `v2.0.0 - Raft Clustering GA`
4. Description: Paste release notes from `CHANGELOG.md`
5. Upload binaries (if built): `chronik-server-linux-amd64`, `chronik-server-darwin-arm64`
6. Mark as "Latest release"
7. Publish!

**Acceptance Criteria**:
- ✅ Git tag created
- ✅ GitHub release published
- ✅ Release notes visible
- ✅ Binaries available (if applicable)

---

### Week 4 Deliverables

**By End of Week 4**:
- ✅ Performance benchmarks published
- ✅ Load testing complete (1 hour @ 100K msg/s)
- ✅ Multi-client compatibility verified
- ✅ Graceful shutdown tested
- ✅ Release notes written
- ✅ v2.0.0 tagged and published

**Exit Criteria**:
- Ready for GA announcement
- Users can download and deploy v2.0.0

---

## Post-GA: Future Work (v2.1.0+)

### Not in Scope for v2.0.0 GA

These features can be deferred to v2.1.0 or later:

1. **Jepsen Testing** (Phase 5 stretch goal)
   - Formal verification of linearizability
   - Requires significant setup and expertise
   - Recommendation: Partner with Jepsen experts or defer to v2.2.0

2. **Rolling Upgrade Testing** (Phase 5.3)
   - Framework exists but not tested in production
   - Defer to v2.1.0 after real-world deployments

3. **Multi-DC Testing** (Phase 5.4)
   - Code complete but never tested with real cross-region setup
   - Defer to v2.1.0 when users request it

4. **Advanced Observability**
   - Distributed tracing (OpenTelemetry)
   - Real-time cluster topology visualization
   - Defer to v2.1.0

5. **Operational CLI Enhancements**
   - `chronik-server raft status` - Show cluster state
   - `chronik-server raft step-down` - Manual leadership transfer
   - `chronik-server raft rebalance` - Force rebalancing
   - Defer to v2.1.0

---

## Success Metrics (v2.0.0 GA)

### Technical Metrics

- [x] **Phase Completion**:
  - [x] Phase 1 (Raft Foundation): 100%
  - [x] Phase 2 (Multi-Partition): 100%
  - [x] Phase 3 (Cluster Membership): 85%+
  - [x] Phase 4 (Production Features): 80%+
  - [ ] Phase 5 (Advanced Features): 60%+ (DNS + rebalancer)

- [ ] **Test Coverage**:
  - [ ] All integration tests passing (10/10)
  - [ ] Chaos tests passing (network partition, leader kill, cascading failure)
  - [ ] E2E test with Kafka clients passing
  - [ ] Load test (1 hour @ 100K msg/s) successful

- [ ] **Performance**:
  - [ ] Cluster throughput ≥ 80% of standalone
  - [ ] Commit latency p99 < 50ms
  - [ ] Leader election < 3 seconds

- [ ] **Stability**:
  - [ ] Zero memory leaks (1-hour load test)
  - [ ] Zero message loss in all failure scenarios
  - [ ] Graceful shutdown works

### Operational Metrics

- [ ] **Documentation**:
  - [ ] Deployment guide complete
  - [ ] Configuration reference complete
  - [ ] Troubleshooting guide complete
  - [ ] Migration guide complete

- [ ] **Release**:
  - [ ] Git tag v2.0.0 created
  - [ ] GitHub release published
  - [ ] Release notes written
  - [ ] Binaries available

### User Metrics (Post-GA)

- [ ] **Adoption**:
  - [ ] 5+ early adopters testing v2.0.0
  - [ ] No critical bugs reported in first week
  - [ ] Positive feedback from users

- [ ] **Support**:
  - [ ] < 1 day response time on issues
  - [ ] All P0 bugs fixed within 48 hours
  - [ ] Documentation covers 90% of user questions

---

## Risk Register

### High Risks

| Risk | Impact | Likelihood | Mitigation | Owner |
|------|--------|------------|------------|-------|
| **Integration tests fail** | High (blocks GA) | Medium | Run early (Week 1), fix as found | TBD |
| **Performance below target** | Medium (bad reputation) | Low | Benchmark early, optimize if needed | TBD |
| **Data loss in edge case** | High (critical bug) | Low | Extensive chaos testing, multiple reviews | TBD |
| **Docs incomplete** | Medium (poor UX) | Medium | Start docs in Week 3, review before GA | TBD |

### Medium Risks

| Risk | Impact | Likelihood | Mitigation | Owner |
|------|--------|------------|------------|-------|
| **DNS discovery broken** | Low (optional feature) | Medium | Test with real DNS server, fallback to static | TBD |
| **S3 snapshot slow** | Low (new nodes rare) | Medium | Test with MinIO, document expected times | TBD |
| **Memory leak under load** | High (production issue) | Low | 1-hour load test, profiling | TBD |

### Low Risks

| Risk | Impact | Likelihood | Mitigation | Owner |
|------|--------|------------|------------|-------|
| **Rebalancer too aggressive** | Low (tune threshold) | Low | Conservative defaults, configurable | TBD |
| **Metrics incomplete** | Low (fix in patch) | Low | Review metrics early, add missing ones | TBD |

---

## Timeline Summary

| Week | Focus | Deliverables | Status |
|------|-------|--------------|--------|
| **Week 1** | Integration & Core Testing | Server integration, E2E tests passing | ⬜ Not Started |
| **Week 2** | Production Hardening | Chaos testing, metrics, S3 integration | ⬜ Not Started |
| **Week 3** | Advanced Features & Docs | DNS discovery, rebalancer, full documentation | ⬜ Not Started |
| **Week 4** | Final Testing & Release | Benchmarks, multi-client tests, v2.0.0 GA | ⬜ Not Started |

**Total Timeline**: 3-4 weeks to v2.0.0 GA

---

## Progress Tracking

**Overall Completion**: 75% (implementation) → Target: 100% (tested, documented, released)

**Remaining Work**: 20 tasks across 4 weeks

### Week 1 Tasks (6 tasks)
- [ ] Task 1.1: Complete Raft-Server Integration (4 hours) - 🔴 CRITICAL
- [ ] Task 1.2: Run Single-Partition Integration Test (2 hours) - 🔴 CRITICAL
- [ ] Task 1.3: Run Multi-Partition Integration Test (2 hours) - 🔴 CRITICAL
- [ ] Task 1.4: E2E Test with Kafka Python Client (4 hours) - 🔴 CRITICAL
- [ ] Task 1.5: Run Cluster Bootstrap Test (2 hours) - 🟡 HIGH
- [ ] Task 1.6: Run All Raft Integration Tests (1 day) - 🟡 HIGH

### Week 2 Tasks (6 tasks)
- [ ] Task 2.1: Set Up Toxiproxy Infrastructure (4 hours) - 🟡 HIGH
- [ ] Task 2.2: Network Partition Test (4 hours) - 🟡 HIGH
- [ ] Task 2.3: Leader Kill and Recovery Test (2 hours) - 🟡 HIGH
- [ ] Task 2.4: Cascading Failure Test (2 hours) - 🟢 MEDIUM
- [ ] Task 2.5: Verify Raft Metrics Exposed (4 hours) - 🟢 MEDIUM
- [ ] Task 2.6: ISR Tracking Validation (4 hours) - 🟢 MEDIUM
- [ ] Task 2.7: S3 Snapshot Upload/Download Test (4 hours) - 🟡 HIGH

### Week 3 Tasks (5 tasks)
- [ ] Task 3.1: Implement DNS SRV Record Querying (6 hours) - 🟢 MEDIUM
- [ ] Task 3.2: Test DNS Discovery with K8s Simulation (4 hours) - 🟢 MEDIUM
- [ ] Task 3.3: Wire Rebalancer to Cluster Coordinator (4 hours) - 🟢 MEDIUM
- [ ] Task 3.4: Test Rebalancer with Node Addition (4 hours) - 🟢 MEDIUM
- [ ] Task 3.5: Write Deployment Guide (8 hours) - 🔴 CRITICAL
- [ ] Task 3.6: Write Configuration Reference (6 hours) - 🔴 CRITICAL
- [ ] Task 3.7: Write Troubleshooting Guide (6 hours) - 🔴 CRITICAL
- [ ] Task 3.8: Write Migration Guide (4 hours) - 🟡 HIGH

### Week 4 Tasks (5 tasks)
- [ ] Task 4.1: Throughput Benchmark (4 hours) - 🟢 MEDIUM
- [ ] Task 4.2: Load Test Sustained 100K msg/s (4 hours) - 🟢 MEDIUM
- [ ] Task 4.3: Multi-Client Compatibility Test (4 hours) - 🟡 HIGH
- [ ] Task 4.4: Graceful Shutdown Test (2 hours) - 🟡 HIGH
- [ ] Task 4.5: Create Release Notes (4 hours) - 🔴 CRITICAL
- [ ] Task 4.6: Tag v2.0.0 Release (1 hour) - 🔴 CRITICAL

---

## Next Actions (Start Immediately)

**This Week (Week 1)**:
1. **Task 1.1** - Complete server integration (4 hours) - UNBLOCK everything else
2. **Task 1.2** - Run single-partition test (2 hours) - Verify basics work
3. **Task 1.4** - E2E with kafka-python (4 hours) - Prove Kafka compatibility

**By End of Week**:
- ✅ Cluster mode fully operational
- ✅ Basic tests passing
- ✅ Ready for chaos testing (Week 2)

---

## Communication Plan

**Weekly Updates** (every Friday):
- Progress on tasks (completed, in-progress, blocked)
- Test results (pass/fail, known issues)
- Updated timeline (on track / delayed)
- Blockers and help needed

**Stakeholder Demo** (end of Week 2):
- Live cluster demo (3 nodes)
- Leader failover demonstration
- Metrics dashboard walkthrough

**GA Announcement** (end of Week 4):
- Blog post: "Chronik v2.0.0 - Production-Ready Raft Clustering"
- Twitter/social media announcement
- Kafka community channels (reddit, discord)

---

## Contact & Support

**Project Lead**: TBD
**Release Manager**: TBD
**Documentation Lead**: TBD

**Issue Tracker**: https://github.com/your-repo/chronik-stream/issues
**Discussions**: https://github.com/your-repo/chronik-stream/discussions

---

**Document Version**: 1.0
**Created**: 2025-10-19
**Last Updated**: 2025-10-19
**Next Review**: 2025-10-26 (end of Week 1)
