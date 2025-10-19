# Chronik Raft Clustering - Comprehensive Chaos Testing Report

**Date**: 2025-10-16
**Build**: chronik-server v1.3.65 with raft feature
**Test Framework**: Custom Python chaos testing suite with kafka-python client
**Duration**: ~5 minutes
**Test Scope**: Single node, 3-node cluster, crash scenarios, rolling restarts

---

## Executive Summary

✗ **COMPREHENSIVE TESTING COMPLETED - CRITICAL ISSUES DISCOVERED**

The Raft clustering implementation was subjected to comprehensive chaos testing including:
- Single-node basic operations
- 3-node cluster formation
- Leader crash scenarios
- Follower crash and rejoin
- Rolling restarts under load

**Result**: Multiple critical issues discovered that prevent production deployment.

---

## Test Results Overview

| Test # | Test Name | Result | Issue |
|--------|-----------|--------|-------|
| 1 | Single Node Basic Operations | ✗ FAILED | Data consistency failure (36 missing/duplicate messages) |
| 2 | 3-Node Cluster Formation | ✗ FAILED | Protocol version negotiation failure on nodes 2 & 3 |
| 3 | Leader Crash During Produce | ✗ FAILED | Unable to connect to nodes 2 & 3, protocol errors |
| 4 | Follower Crash and Rejoin | ✗ FAILED | Same protocol negotiation issues |
| 5 | Rolling Restarts Under Load | ✗ FAILED | Same protocol negotiation issues |

**Overall Test Success Rate**: 0/5 (0%)

---

## Critical Issue #1: Data Consistency Failure (Single Node)

### Symptom
```
TEST 1: Single Node Basic Operations
- Produced: 100 unique messages
- Consumed: 64 unique messages
- Missing: 36 messages
- Duplicates: 36 messages
```

### Analysis
On a **single-node deployment**, which should be the simplest and most reliable scenario, we observed:
- **36% message loss** - Messages sent but not received
- **36% duplication** - Same messages received multiple times
- Message IDs that went missing: `[132, 134, 136, 140, 16, 148, 150, 24, 152, 154, ...]`

### Root Cause Hypothesis
1. **WAL/Storage Layer Issue**: Messages may not be properly persisted or indexed
2. **Consumer Group State**: Possible offset tracking issues in the FileMetadataStore
3. **Message IDs**: The missing IDs suggest a test script issue (IDs > 100 when only 100 messages sent)
4. **State Reuse**: The test may be reusing state from previous tests due to improper cleanup

### Impact
- **CRITICAL**: Single-node deployments cannot guarantee data durability
- Violates the "zero message loss guarantee" claim
- Makes the system unsuitable for production use until fixed

---

## Critical Issue #2: Protocol Version Negotiation Failure

### Symptom
Nodes 2 and 3 fail to negotiate Kafka protocol version with clients:

```
ERROR: socket disconnected
INFO: Broker is not v(0, 10) -- it did not recognize ApiVersionRequest_v0
INFO: Broker is not v(0, 9) -- it did not recognize ListGroupsRequest_v0
INFO: Broker is not v(0, 8, 2) -- it did not recognize GroupCoordinatorRequest_v0
INFO: Broker is not v(0, 8, 1) -- it did not recognize OffsetFetchRequest_v0
INFO: Broker is not v(0, 8, 0) -- it did not recognize MetadataRequest_v0
ERROR: UnrecognizedBrokerVersion
```

### Analysis
- Node 1 (port 9092) successfully negotiates and identifies as Kafka 2.5.0
- Nodes 2 and 3 (ports 9093, 9094) **immediately disconnect** on any Kafka protocol request
- The client exhausts all version fallback attempts (0.10 → 0.9 → 0.8.2 → 0.8.1 → 0.8.0)
- Final result: **UnrecognizedBrokerVersion** error

### Root Cause Hypothesis
1. **Server Binding Issue**: Nodes 2 & 3 may not be properly listening on their Kafka ports
2. **Protocol Handler Not Initialized**: The IntegratedKafkaServer may not be fully initialized in cluster mode
3. **Config Mismatch**: The raft-cluster mode may not be properly starting the Kafka server
4. **Port Conflicts**: Possible port binding conflicts or firewall issues

### Impact
- **CRITICAL**: Multi-node clusters are completely non-functional
- Clients cannot produce or consume from any node except node 1
- Raft replication cannot be tested as clients fail to connect

---

## Critical Issue #3: Raft Peer Connection Failure

### Symptom
From node logs (`/tmp/chronik_test/node1/chronik.log`):

```
INFO chronik_raft::client: Adding peer 2 at http://localhost:5002
Error: Configuration error: Failed to connect to http://localhost:5002: transport error
```

### Analysis
- Node 1 (bootstrap node) starts successfully
- When it attempts to add peers (nodes 2 & 3 via gRPC), connection fails
- This happens even though nodes 2 & 3 should be running
- The error occurs during startup, before the Kafka server is ready

### Root Cause Hypothesis
1. **Peer Not Ready**: Node 1 tries to connect to peers before they've started their gRPC servers
2. **gRPC Server Not Started**: The raft-cluster.rs code has a TODO comment that says "Raft gRPC service would be started here in a production implementation"
3. **No gRPC Service**: The RaftService implementation is incomplete or not wired up

### Impact
- **CRITICAL**: Raft cluster formation is impossible
- No log replication can occur
- Cluster bootstrap fails immediately
- This explains why the subsequent tests all fail

---

## Additional Observations

### 1. Test 1 Message ID Issue
The test reports missing message IDs like `132, 134, 136`, but the test only sends 100 messages (IDs 0-99). This suggests:
- **Test Script Bug**: The message ID tracking is cumulative across test runs
- **Data Not Cleaned**: `/tmp/chronik_test` directory is not properly cleaned between tests
- This makes the first test failure less clear than it appears

### 2. Nodes Crash Immediately After Start
From the system reminders, we see repeated node crashes:
```
[2025-10-16T15:38:10] Node 1 starts
Error: Configuration error: Failed to connect to http://localhost:5002: transport error
```

This suggests the `add_peer()` call is **blocking and failing**, which prevents the server from fully initializing.

### 3. No Actual Raft Testing Occurred
Because of the peer connection failures and protocol negotiation issues, **none of the actual Raft functionality was tested**:
- ✗ Leader election - not tested
- ✗ Log replication - not tested
- ✗ Failover - not tested
- ✗ Split-brain scenarios - not tested
- ✗ Network partitions - not tested

---

## Recommended Fixes (Priority Order)

### Priority 1: Fix Raft Peer Connection Logic
**File**: `crates/chronik-server/src/raft_cluster.rs`

**Issue**: The code attempts to add peers synchronously during startup, causing a blocking error:
```rust
for (peer_id, peer_addr) in config.peers {
    raft_manager.add_peer(peer_id, peer_url).await?;  // <-- BLOCKS HERE
}
```

**Fix**:
1. Make peer addition **non-blocking** and **asynchronous**
2. Implement **retry logic** with exponential backoff
3. Allow the server to start even if peers are not yet reachable
4. Add peers in a **background task** that retries periodically

**Example**:
```rust
// Start peer discovery in background
tokio::spawn(async move {
    for (peer_id, peer_addr) in config.peers {
        loop {
            match raft_manager.add_peer(peer_id, peer_url).await {
                Ok(_) => {
                    info!("Successfully added peer {}", peer_id);
                    break;
                }
                Err(e) => {
                    warn!("Failed to add peer {}: {:?}, retrying in 5s", peer_id, e);
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
    }
});
```

### Priority 2: Fix Protocol Version Negotiation
**File**: `crates/chronik-server/src/raft_cluster.rs`

**Issue**: Nodes 2 & 3 don't respond to Kafka protocol requests

**Debugging Steps**:
1. Add logging to verify `IntegratedKafkaServer::run()` is called
2. Check if the server is actually binding to the correct ports
3. Verify the Kafka protocol handler is initialized
4. Test with `telnet localhost 9093` to see if the port is open

**Potential Fix**:
- Ensure `IntegratedKafkaServer::new_with_raft()` is called correctly
- Verify `server.run(bind_addr).await` is actually executed
- Check for errors in the server startup that are being silently ignored

### Priority 3: Fix Data Consistency in Single-Node Mode
**File**: Multiple - storage, WAL, metadata layers

**Debugging Steps**:
1. Clean test data between runs: `rm -rf /tmp/chronik_test` in test script
2. Fix message ID tracking in test script to reset between tests
3. Add detailed logging to ProduceHandler and FetchHandler
4. Verify WAL writes are actually persisted
5. Check consumer group offset tracking

**Potential Fixes**:
- Ensure `flush()` is called on storage layers before shutdown
- Verify segment rotation doesn't lose in-flight messages
- Check that consumer group offsets are correctly committed

### Priority 4: Implement gRPC Raft Service
**File**: `crates/chronik-server/src/raft_cluster.rs`

**Issue**: The code currently has a placeholder:
```rust
// Note: Raft gRPC service would be started here in a production implementation
info!("Raft replicas created (TODO: implement gRPC server)");
```

**Fix**:
1. Uncomment and fix the gRPC server code
2. Ensure `chronik-raft` has a working `RaftService` implementation
3. Wire up the service to handle `AppendEntries`, `RequestVote`, etc.
4. Test with direct gRPC calls

---

## Test Environment Details

### Build Configuration
```bash
cargo build --release --bin chronik-server --features raft
- Warnings: 150 warnings (mostly unused variables/imports)
- Errors: 0 (build successful)
- Features: search=true, backup=true, dynamic-config=false
```

### Test Cluster Configuration
```
Node 1: kafka=9092, raft=5001, bootstrap=true
Node 2: kafka=9093, raft=5002
Node 3: kafka=9094, raft=5003
Data: /tmp/chronik_test/node{1,2,3}
Metadata: FileMetadataStore
```

### Kafka Client Configuration
```python
KafkaProducer(
    bootstrap_servers=[node.bootstrap_server],
    acks='all',  # Wait for all replicas
    retries=3,
    max_in_flight_requests_per_connection=1
)

KafkaConsumer(
    auto_offset_reset='earliest',
    group_id='chaos-test-consumer'
)
```

---

## Conclusion

The comprehensive chaos testing successfully identified **multiple critical blockers** that prevent the Raft clustering feature from being production-ready:

1. ✗ **Raft peer discovery fails** → Cluster cannot form
2. ✗ **Protocol negotiation fails on non-bootstrap nodes** → Multi-node unusable
3. ✗ **Data consistency issues even in single-node** → Basic durability broken
4. ✗ **No actual Raft functionality tested** → Consensus, replication, failover all untested

### Status: NOT PRODUCTION READY

**Next Steps**:
1. Implement the Priority 1-4 fixes above
2. Re-run the comprehensive chaos test suite
3. Add additional tests for:
   - Network partitions (iptables-based)
   - Disk failures (fsync errors)
   - Slow followers (latency injection)
   - Byzantine scenarios (corrupted messages)

### Estimated Work Required
- **High Priority Fixes**: 2-3 days
- **Additional Testing**: 1-2 days
- **Production Hardening**: 1 week

---

## Lessons Learned

1. **Test Early, Test Often**: Attempting to claim "ALL INTEGRATION WORK COMPLETE!" without running a single test was a critical mistake
2. **Real Clients Matter**: Testing with actual Kafka clients (kafka-python) immediately exposed protocol issues that unit tests missed
3. **Chaos Testing Works**: This comprehensive test suite found issues in < 5 minutes that could have caused data loss in production
4. **Never Skip E2E Tests**: The "produce → consume" roundtrip test is mandatory before any release

---

**Test Report Generated**: 2025-10-16 17:39 UTC
**Test Framework**: tests/raft_chaos_test.py
**Full Test Output**: Available in background Bash output (4f370d)
