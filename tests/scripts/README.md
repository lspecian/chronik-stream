# Chronik Test Scripts

This directory contains integration and end-to-end test scripts for Chronik's Raft clustering and layered storage features.

## Prerequisites

```bash
# Build the server first
cargo build --release --bin chronik-server --features raft

# Install Python dependencies
pip3 install kafka-python
```

## Layered Storage Tests

### `test_layered_storage_e2e.sh`
**End-to-end test for layered storage with Raft clustering**

Tests the complete 3-tier storage architecture (WAL → S3 segments → Tantivy indexes).

**Configuration:**
- 3-node Raft cluster
- 1MB WAL rotation threshold (triggers sealing quickly)
- Hetzner S3 credentials from `.env.hetzner`

**What it tests:**
- ✅ WAL writes and sealing
- ✅ Raw segment upload to S3
- ✅ Tantivy index creation and upload
- ✅ WalIndexer background processing
- ✅ Local WAL deletion after upload

**Usage:**
```bash
# Requires .env.hetzner with S3 credentials
./test_layered_storage_e2e.sh
```

**Expected output:**
- All 3 nodes seal segments when reaching 1MB
- Raw segments uploaded to S3 (~1MB each)
- Tantivy indexes uploaded to S3 (~13KB each)
- Local WAL files deleted after successful upload

### `test_layered_storage_cluster.py`
**Python-based layered storage test**

Verifies data persistence and retrieval across cluster nodes.

**Usage:**
```bash
python3 test_layered_storage_cluster.py
```

## Raft Cluster Tests

### Basic Cluster Operations

#### `test_cluster_kafka_python.py`
**Basic produce/consume test with kafka-python client**

**Usage:**
```bash
# Start cluster first (see test_cluster_manual.sh)
python3 test_cluster_kafka_python.py
```

#### `test_simple_topic_create.py`
**Topic creation and metadata synchronization**

Tests topic creation across cluster nodes.

#### `test_simple_leader_wait.py`
**Leader election and stabilization**

Waits for cluster to elect leaders for all partitions.

### Cluster Management Scripts

#### `test_cluster_manual.sh`
**Manual 3-node cluster startup**

Starts a 3-node Raft cluster with proper peer configuration.

**Usage:**
```bash
./test_cluster_manual.sh
```

**Ports:**
- Node 1: Kafka 9092, Raft 5001, Metrics 9101
- Node 2: Kafka 9093, Raft 5002, Metrics 9102
- Node 3: Kafka 9094, Raft 5003, Metrics 9103

#### `test_debug_cluster.sh`
**Cluster startup with debug logging**

Same as `test_cluster_manual.sh` but with verbose Raft logging.

**Usage:**
```bash
RUST_LOG=debug ./test_debug_cluster.sh
```

### Failure Scenario Tests

#### `test_leader_kill_recovery.py`
**Leader failure and re-election**

**Scenario:**
1. Starts 3-node cluster
2. Kills current leader
3. Verifies cluster elects new leader
4. Verifies produce/consume still works
5. Restarts killed node
6. Verifies it rejoins cluster

#### `test_cascading_failure.py`
**Multiple sequential node failures**

**Scenario:**
1. Kill node 1 → verify cluster survives
2. Kill node 2 → verify cluster loses quorum
3. Restart nodes → verify recovery

#### `test_cascading_simple.py`
**Simplified cascading failure test**

Less aggressive version of `test_cascading_failure.py`.

#### `test_raft_failure_scenarios.py`
**Comprehensive failure scenarios**

Tests:
- Single node failure
- Minority failure (1/3 nodes)
- Majority failure (2/3 nodes)
- Network partition simulation
- Leader isolation

#### `test_network_chaos.py`
**Network chaos engineering**

Requires Toxiproxy for network simulation.

**Setup:**
```bash
./test_toxiproxy_setup.sh
```

**Scenarios:**
- Latency injection
- Packet loss
- Connection timeout
- Network partition

### Replication and Recovery Tests

#### `test_replica_creation.py`
**Raft replication verification**

Tests:
- Partition replica creation
- Raft log synchronization
- Multi-partition replication

#### `test_raft_recovery.py`
**Crash recovery and WAL replay**

**Scenario:**
1. Start cluster, produce messages
2. Crash node (SIGKILL)
3. Restart node
4. Verify WAL recovery
5. Verify data integrity

#### `test_raft_partition.py`
**Partition tolerance testing**

Simulates network partitions and verifies cluster behavior.

### Performance Tests

#### `test_parallel_stress.py`
**Concurrent produce/consume load test**

**Configuration:**
- Multiple producer threads
- Multiple consumer threads
- High message throughput

**Usage:**
```bash
python3 test_parallel_stress.py --producers 10 --consumers 5 --messages 100000
```

#### `test_all_nodes_fetch.py`
**Fetch consistency across all nodes**

Verifies all nodes return identical data for the same partition.

## Helper Scripts

### `test_toxiproxy_setup.sh`
**Install and configure Toxiproxy for network chaos tests**

Installs Toxiproxy and creates proxies for all cluster nodes.

**Usage:**
```bash
./test_toxiproxy_setup.sh
```

## Running Tests

### Quick Start: Basic Cluster Test
```bash
# 1. Start cluster
./test_cluster_manual.sh

# 2. Wait for stabilization (15 seconds)
sleep 15

# 3. Test basic produce/consume
python3 test_simple_topic_create.py
python3 test_cluster_kafka_python.py

# 4. Cleanup
pkill -f 'chronik-server.*909[2-4]'
```

### Full Test Suite
```bash
# Run all failure scenario tests
for test in test_leader_kill_recovery.py test_cascading_simple.py test_raft_recovery.py; do
    echo "Running $test..."
    python3 "$test"
    sleep 5
done
```

### Layered Storage Test with S3
```bash
# Requires .env.hetzner with Hetzner S3 credentials
./test_layered_storage_e2e.sh
```

## Debugging Tips

### Check Cluster Status
```bash
# View logs
tail -f test-cluster-data/node1/node1.log

# Check Raft metrics
curl http://localhost:9101/metrics | grep raft
```

### Check S3 Uploads
```bash
# View WalIndexer logs
grep -i "walindexer\|sealed\|uploaded" test-cluster-data/node1/node1.log

# Check local object store
ls -lh test-cluster-data/node1/object_store/
```

### Common Issues

**Issue: "Connection refused"**
- Cluster not started or ports in use
- Check with: `lsof -i :9092`

**Issue: "No sealed segments to index"**
- WAL rotation threshold not reached
- Produce more data (> 1MB per partition)

**Issue: "Election timeout"**
- Cluster not stabilized yet
- Wait 15-30 seconds after startup

**Issue: "S3 upload failed"**
- Check `.env.hetzner` credentials
- Verify network connectivity
- Check logs: `RUST_LOG=chronik_storage::wal_indexer=debug`

## Environment Variables

### Cluster Configuration
- `CHRONIK_DATA_DIR` - Data directory (default: `./test-cluster-data`)
- `CHRONIK_KAFKA_PORT` - Kafka port (default: 9092)
- `CHRONIK_RAFT_ADDR` - Raft listen address (default: 0.0.0.0:5001)

### S3 Configuration (for layered storage tests)
- `OBJECT_STORE_BACKEND` - Backend type (s3, gcs, azure, local)
- `S3_ENDPOINT` - S3 endpoint URL
- `S3_REGION` - S3 region
- `S3_BUCKET` - S3 bucket name
- `S3_ACCESS_KEY` - S3 access key
- `S3_SECRET_KEY` - S3 secret key
- `S3_PREFIX` - Key prefix for all objects

### Logging
- `RUST_LOG=debug` - Enable debug logging
- `RUST_LOG=chronik_raft=trace` - Trace-level Raft logs
- `RUST_LOG=chronik_storage::wal_indexer=debug` - WalIndexer debug logs

## Test Coverage

| Feature | Test Script |
|---------|------------|
| Basic produce/consume | `test_cluster_kafka_python.py` |
| Leader election | `test_simple_leader_wait.py` |
| Leader failure | `test_leader_kill_recovery.py` |
| Cascading failures | `test_cascading_failure.py` |
| Crash recovery | `test_raft_recovery.py` |
| Replication | `test_replica_creation.py` |
| Network chaos | `test_network_chaos.py` |
| Layered storage | `test_layered_storage_e2e.sh` |
| Multi-tier fetch | `test_all_nodes_fetch.py` |
| Stress testing | `test_parallel_stress.py` |

## Contributing

When adding new test scripts:
1. Follow naming convention: `test_<feature>_<scenario>.{py,sh}`
2. Add prerequisites section if needed
3. Document expected behavior
4. Include cleanup steps
5. Update this README with test description
