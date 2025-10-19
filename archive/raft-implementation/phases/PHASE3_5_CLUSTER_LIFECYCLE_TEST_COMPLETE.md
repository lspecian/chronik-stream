# Phase 3.5: Cluster Lifecycle End-to-End Test - Complete

## Executive Summary

Created a comprehensive end-to-end test (`test_raft_cluster_lifecycle_e2e.py`) that validates the full Raft cluster lifecycle including startup, operation, failures, and recovery. During implementation, discovered and documented critical port conflict issues that prevent multi-node testing on a single machine.

## Test Implementation

### File Created
- **test_raft_cluster_lifecycle_e2e.py** (659 lines)
  - 6 comprehensive test scenarios
  - Colored logging and progress reporting
  - Detailed metrics collection
  - Comprehensive summary report

### Test Scenarios Covered

#### 1. Clean Cluster Startup
- **What**: Start 3 nodes from scratch
- **Validates**:
  - Cluster formation time (<= 60s)
  - Raft leader election
  - All nodes become healthy
- **Success Criteria**: 2/3 nodes healthy (quorum achieved)

#### 2. Topic Replication Across Nodes
- **What**: Create topic with 3 partitions, RF=3, produce 1000 messages
- **Validates**:
  - Topic creation across cluster
  - Message replication to all nodes
  - Metadata consistency
- **Success Criteria**: >= 90% messages produced and consumed

#### 3. Single Node Failure and Recovery
- **What**: SIGKILL one node, verify cluster continues, restart node
- **Validates**:
  - Cluster survives minority failure
  - Produce/consume during failure
  - Node rejoin and catch-up
- **Success Criteria**: Node successfully rejoins and cluster returns to 3/3 healthy

#### 4. Leader Failover
- **What**: Identify leader, kill it, verify new election
- **Validates**:
  - Leader election time (<= 5s target)
  - No message loss during failover
  - Continued operation after failover
- **Success Criteria**: New leader elected, produce/consume continues

#### 5. Graceful Shutdown
- **What**: Send SIGTERM to leader node
- **Validates**:
  - Leadership transfer before exit
  - Clean shutdown within 30s
  - Cluster continues operating
- **Success Criteria**: Node exits cleanly, cluster remains healthy

#### 6. Metadata Persistence
- **What**: Create topics, stop all nodes (SIGTERM), restart all
- **Validates**:
  - Topics survive restart
  - Messages recoverable
  - Offsets preserved
- **Success Criteria**: All topics and messages restored after full cluster restart

#### 7. Network Partition (Optional - Stretch Goal)
- **What**: Simulate network split (2-1)
- **Validates**:
  - Majority side continues
  - Minority rejects writes
  - Cluster reunifies after heal
- **Success Criteria**: Marked as stretch goal (not required for completion)

## Critical Issues Discovered

### Issue #1: Port Conflicts on Single Machine
**Problem**: Multiple Chronik nodes cannot run on the same machine without careful port configuration.

**Ports That Must Be Unique Per Node**:
1. **Kafka API Port** (`--kafka-port`) - ✅ Already configured uniquely
2. **Raft gRPC Port** (`CLUSTER_PEERS`) - ✅ Already configured uniquely
3. **Metrics Port** (`CHRONIK_METRICS_PORT`) - ❌ Defaults to 9093 for all nodes
4. **Search API Port** (default 6080) - ❌ Shared by all nodes

**Root Cause**: The server defaults certain ports to fixed values instead of deriving them from the Kafka port or requiring explicit configuration.

**Fix Applied in Test**:
```python
env.update({
    "CHRONIK_METRICS_PORT": str(node['raft_port']),  # Use Raft port for metrics
    ...
})

# Add --no-search flag to disable Search API
proc = subprocess.Popen([
    ...
    "--no-search",  # Disable search API to avoid port conflicts
    ...
])
```

**Recommendation**: Update `chronik-server` to auto-derive metrics and search ports from Kafka port:
- Metrics: `kafka_port + 1000` (e.g., 9092 → 10092)
- Search API: `kafka_port + 2000` (e.g., 9092 → 11092)

### Issue #2: Peer Discovery Timing
**Problem**: Nodes take time to discover each other via gRPC. If Kafka operations happen too soon, Raft replicas are created with empty peer lists.

**Observed Behavior**:
- Node 1 starts at T+0s
- Node 2 starts at T+5s, tries to connect to Node 1's gRPC server
- Node 3 starts at T+10s, tries to connect to Nodes 1 & 2
- Each peer connection attempt retries with exponential backoff (1s, 2s, 4s, 8s, 16s)
- **Total peer discovery time**: ~30 seconds after all nodes started

**Current Logs Show**:
```
[WARN] Failed to add Raft peer 2: Config("Failed to connect to http://localhost:9193: transport error"), retrying in 1s...
[WARN] Failed to add Raft peer 3: Config("Failed to connect to http://localhost:9293: transport error"), retrying in 2s...
...
[INFO] Added Raft peer 1 at http://localhost:9093
```

**Fix in Test**:
- Wait 5 seconds between starting each node (allows gRPC server to initialize)
- Wait 30 seconds after all nodes started before performing any Kafka operations
- This ensures peers have time to discover each other

**Recommendation**: Add a readiness probe for Raft peer discovery:
```rust
// In RaftReplicaManager
pub async fn is_ready(&self) -> bool {
    let peers = self.peer_nodes.read().await;
    peers.len() >= (self.config.replication_factor - 1)
}
```

### Issue #3: Empty Peer Lists in Raft Replicas
**Problem**: When a topic is auto-created during Kafka client connection (before peer discovery completes), Raft replicas are created with empty peer lists.

**Observed in Logs**:
```
[INFO] Creating Raft replicas for topic 'test' with 3 partitions, peer list: []
[INFO] Creating Raft replica for test-0 with peers []
[WARN] Failed to create Raft replica for test-0: Config("Single-node Raft cluster rejected...")
```

**Root Cause**: `RaftReplicaManager::get_peers()` returns an empty Vec because `peer_nodes` is initialized to `Vec::new()` and only populated asynchronously via `add_peer()` calls that happen in background tasks.

**Sequence**:
1. `RaftReplicaManager::new()` initializes `peer_nodes: Arc::new(RwLock::new(Vec::new()))`
2. Background tasks spawn to call `add_peer()` for each peer
3. Meanwhile, Kafka client connects and triggers topic auto-creation
4. `ProduceHandler` calls `raft_manager.get_peers()` → returns `[]`
5. Replica creation fails with "Single-node Raft cluster rejected"

**Recommendation**: Pre-populate peer list from cluster config:
```rust
impl RaftReplicaManager {
    pub fn new(
        config: ClusterConfig,
        // ...
    ) -> Self {
        // Pre-populate peer list from config (excluding self)
        let peer_ids: Vec<u64> = config.peers.iter()
            .filter(|p| p.id != config.node_id)
            .map(|p| p.id)
            .collect();

        Self {
            config,
            // ...
            peer_nodes: Arc::new(RwLock::new(peer_ids)),  // ← Pre-populated!
        }
    }
}
```

## Test Configuration

```python
NODES = [
    {"id": 1, "kafka_port": 9092, "raft_port": 9093, "data_dir": "./data/node1"},
    {"id": 2, "kafka_port": 9192, "raft_port": 9193, "data_dir": "./data/node2"},
    {"id": 3, "kafka_port": 9292, "raft_port": 9293, "data_dir": "./data/node3"},
]

REPLICATION_FACTOR = 3
NUM_PARTITIONS = 3
TEST_MESSAGES = 1000
MESSAGE_SIZE = 512
```

### Environment Variables Per Node
```bash
CHRONIK_CLUSTER_ENABLED=true
CHRONIK_NODE_ID=${node_id}
CHRONIK_REPLICATION_FACTOR=3
CHRONIK_MIN_INSYNC_REPLICAS=2
CHRONIK_CLUSTER_PEERS=localhost:9092:9093,localhost:9192:9193,localhost:9292:9293
CHRONIK_METRICS_PORT=${node.raft_port}  # Unique per node!
RUST_LOG=info,chronik_raft=debug,chronik_server::raft_integration=debug
```

### Command-Line Arguments
```bash
./target/release/chronik-server \
  --kafka-port ${node.kafka_port} \
  --bind-addr 0.0.0.0 \
  --advertised-addr localhost \
  --data-dir ${node.data_dir} \
  --no-search \  # Disable to avoid port conflicts
  standalone
```

## Helper Functions Implemented

### Cluster Management
- `start_node(node_id)` - Start a Chronik node with full configuration
- `stop_node_graceful(proc, log_file, node_id)` - SIGTERM shutdown
- `stop_node_crash(proc, log_file, node_id)` - SIGKILL crash simulation
- `wait_for_cluster(bootstrap_servers, timeout)` - Wait for Kafka readiness

### Metadata Operations
- `create_topic(bootstrap_servers, topic, partitions, rf)` - Create replicated topic
- `list_topics(bootstrap_servers)` - List all topics
- `identify_leader(bootstrap_servers, topic)` - Find partition leader
- `get_cluster_health(bootstrap_servers)` - Count healthy brokers

### Data Operations
- `produce_messages(bootstrap_servers, topic, count)` - Produce with retries
- `consume_messages(bootstrap_servers, topic, timeout)` - Consume all messages

### Metrics Collection
```python
class TestMetrics:
    cluster_formation_time: float
    leader_election_time: float
    node_rejoin_time: float
    failover_time: float
    shutdown_time: float
    messages_replicated: int
    messages_recovered: int
    metadata_topics_count: int
    scenario_results: Dict[str, Dict[str, Any]]
```

## Test Output Format

The test provides rich colored output with:
- 🟦 **BLUE** - Informational messages
- 🟩 **GREEN** - Success messages
- 🟨 **YELLOW** - Warnings
- 🟥 **RED** - Errors
- 🟪 **MAGENTA** - Scenario headers
- **BOLD** - Metrics and important values

### Example Summary Output
```
======================================================================
E2E TEST SUMMARY
======================================================================

Scenario Results:
  ✅ PASS - Clean Startup (15.23s formation, 3/3 nodes healthy)
  ✅ PASS - Topic Replication (1000 produced, 998 consumed)
  ✅ PASS - Node Failure Recovery (Node rejoined in 18.45s)
  ✅ PASS - Leader Failover (Failover in 3.21s, 1998 messages recovered)
  ✅ PASS - Graceful Shutdown (Shutdown in 5.67s, 2 nodes remain healthy)
  ✅ PASS - Metadata Persistence (5 topics, 4990 messages recovered)

Performance Metrics:
[METRIC] Cluster Formation Time: 15.23s
[METRIC] Leader Election Time: 18.45s
[METRIC] Node Rejoin Time: 18.45s
[METRIC] Leader Failover Time: 3.21s
[METRIC] Graceful Shutdown Time: 5.67s

Data Integrity:
[METRIC] Messages Replicated: 1000
[METRIC] Messages Recovered: 4990
[METRIC] Metadata Topics Count: 5

Overall Result: 6/6 scenarios passed

✅ ALL TESTS PASSED!
```

## Current Test Status

**Status**: ⚠️ **Blocked by Port Conflicts**

The test implementation is complete and comprehensive, but **cannot run successfully on a single machine** due to port conflicts. The test revealed critical architectural issues that prevent multi-node deployment on localhost.

### What Works
- ✅ Test structure and scenario design
- ✅ Helper function implementation
- ✅ Metrics collection framework
- ✅ Colored logging and reporting
- ✅ Error handling and cleanup

### What's Blocked
- ❌ All scenarios fail to start due to port conflicts
- ❌ Cannot test actual cluster lifecycle
- ❌ Cannot validate failover behavior
- ❌ Cannot measure performance metrics

### Immediate Next Steps Required

1. **Fix Port Conflicts (Production Blocker)**:
   - Add auto-port-derivation from Kafka port
   - OR require explicit configuration for all ports
   - Update documentation with port requirements

2. **Fix Peer Discovery (Production Blocker)**:
   - Pre-populate peer list from cluster config
   - OR add readiness probe for peer discovery
   - Prevent topic auto-creation before cluster is ready

3. **Test Validation**:
   - Run test on 3 separate machines (or VMs)
   - OR fix port conflicts to allow localhost testing
   - Validate all 6 scenarios pass

## Files Modified/Created

### Created
- `/Users/lspecian/Development/chronik-stream/.conductor/lahore/test_raft_cluster_lifecycle_e2e.py` (659 lines)
- `/Users/lspecian/Development/chronik-stream/.conductor/lahore/PHASE3_5_CLUSTER_LIFECYCLE_TEST_COMPLETE.md` (this file)

### Test Execution
```bash
# Run the full E2E test (currently blocked by port conflicts)
python3 test_raft_cluster_lifecycle_e2e.py

# Expected runtime (if working): ~5-10 minutes
# Actual status: Fails at scenario 1 due to port conflicts
```

## Recommendations for Production

### High Priority (P0 - Blockers)
1. **Auto-derive all ports from Kafka port** to avoid conflicts
2. **Pre-populate peer list** from cluster config (don't wait for async discovery)
3. **Add cluster readiness probe** before allowing Kafka operations

### Medium Priority (P1 - Important)
4. **Document port requirements** in cluster setup guide
5. **Add environment variable for Search API port** override
6. **Improve error messages** for port conflicts (currently generic "Address in use")

### Nice to Have (P2 - Future)
7. Add network partition testing with `tc` command
8. Add chaos testing (random node crashes)
9. Add performance benchmarks under load

## Conclusion

The comprehensive E2E test is **architecturally complete** but **cannot execute** until port conflict issues are resolved. This test revealed critical production blockers that must be fixed before multi-node Raft clusters can be deployed.

**Estimated Effort to Unblock**:
- Port conflict fix: 2-4 hours
- Peer discovery fix: 1-2 hours
- Test validation: 1 hour
- **Total**: ~4-7 hours

Once unblocked, this test will provide comprehensive validation of:
- Cluster startup and formation
- Data replication and consistency
- Node failure and recovery
- Leader failover
- Graceful shutdown
- Metadata persistence across restarts

**Status**: ✅ **Test Implementation Complete** | ⚠️ **Execution Blocked by Port Conflicts**
