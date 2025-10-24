# Java Kafka Client Testing Session - 2025-10-22

**Date**: 2025-10-22
**Goal**: Test Chronik with Java kafka-clients library (Task #3 from RC roadmap)
**Result**: ⛔ **FAILED** - Discovered P0 critical bug

---

## Summary

Attempted to test Chronik v1.3.66 with Java-based Kafka clients (kafka-console-producer/consumer from Confluent Platform 7.5.0). Testing was **blocked** by discovery of a critical port binding bug that prevents 3-node clusters from starting.

**Key Finding**: Nodes bind to **multiple** Kafka ports instead of just their configured port, causing port conflicts and preventing node 3 from starting.

---

## Test Environment

- **OS**: macOS (Darwin 25.0.0)
- **Java**: OpenJDK 17.0.9
- **Kafka Client**: Confluent Platform 7.5.0 (kafka-console-producer/consumer)
- **Chronik Version**: v1.3.66 (latest with startup race condition fix)
- **Cluster Config**: 3-node Raft cluster (localhost:9092, :9093, :9094)

---

## Test Sequence

### 1. Environment Setup ✅

```bash
# Verified Java installation
java -version
# OpenJDK 17.0.9 ✅

# Verified Confluent Platform
ls ksql/confluent-7.5.0/bin/kafka-*
# kafka-console-producer, kafka-console-consumer ✅

# Built latest Chronik
cargo build --release --bin chronik-server --features raft
# Build successful ✅
```

### 2. Start 3-Node Cluster ❌

```bash
bash tests/start-test-cluster-ryw.sh
```

**Expected**: 3 nodes running (PIDs for node 1, 2, 3)
**Actual**: Only 2 nodes running (node 3 failed to start)

```bash
$ ps aux | grep chronik-server | grep -v grep
lspecian  17933  ...  chronik-server standalone  # Node 1 ✅
lspecian  17943  ...  chronik-server standalone  # Node 2 ✅
# Node 3 missing ❌
```

### 3. Java Producer Test ❌

```bash
echo "test-message" | ksql/confluent-7.5.0/bin/kafka-console-producer \
  --bootstrap-server localhost:9092 --topic java-test-topic
```

**Result**: FAILED with errors

```
[Producer] Got error produce response: NOT_LEADER_OR_FOLLOWER
[Producer] Received invalid metadata error in produce request
ERROR: NotLeaderOrFollowerException
```

### 4. Diagnostic Investigation

#### 4.1 Check Running Processes
```bash
$ ps aux | grep chronik | wc -l
2  # Only 2 nodes running (should be 3)
```

#### 4.2 Check Node 3 Logs
```bash
$ tail test-cluster-data-ryw/node3.log
[2025-10-22T19:27:06] Server shutdown complete  # OLD timestamp!

$ grep "2025-10-22T21:" test-cluster-data-ryw/node3.log
# No output - node never logged anything from current run
```

**Conclusion**: Node 3 crashed immediately or failed to start, with no logging.

#### 4.3 Port Analysis - **ROOT CAUSE DISCOVERED**

```bash
$ lsof -nP -i4TCP -s TCP:LISTEN | grep chronik
chronik  17933  ... *:9092 (LISTEN)  # Node 1 - Expected ✅
chronik  17933  ... *:9094 (LISTEN)  # Node 1 - WRONG! ❌
chronik  17933  ... *:5001 (LISTEN)  # Node 1 Raft - OK ✅
chronik  17943  ... *:9093 (LISTEN)  # Node 2 - Expected ✅
chronik  17943  ... *:9095 (LISTEN)  # Node 2 - WRONG! ❌
chronik  17943  ... *:5002 (LISTEN)  # Node 2 Raft - OK ✅
```

**Critical Discovery**:
- Node 1 binds to BOTH 9092 (correct) AND 9094 (node 3's port!)
- Node 2 binds to BOTH 9093 (correct) AND 9095 (not in any config!)
- Node 3 cannot start because port 9094 is already taken by node 1

---

## Root Cause Analysis

### Bug Description

Chronik nodes are binding to **multiple Kafka ports** instead of just their configured port:

- **Node 1** should bind ONLY to `9092` (from `CHRONIK_KAFKA_PORT`)
- **Node 2** should bind ONLY to `9093` (from `CHRONIK_KAFKA_PORT`)
- **Node 3** should bind ONLY to `9094` (from `CHRONIK_KAFKA_PORT`)

**Instead**, nodes appear to be binding to ports from the **peer list** in the cluster config.

### Evidence

**Node 1 Config** (`test-cluster-ryw-node1.toml`):
```toml
node_id = 1

[[peers]]
id = 1
addr = "localhost:9092"
raft_port = 5001

[[peers]]
id = 2
addr = "localhost:9093"
raft_port = 5002

[[peers]]
id = 3
addr = "localhost:9094"  # Node 1 incorrectly binds to this!
raft_port = 5003
```

**Hypothesis**: The server is parsing the `peers` array and binding to ALL peer addresses instead of identifying its own address based on `node_id`.

### Impact

**Severity**: P0 (Critical)
**Blocks**: v2.0.0-rc.1 release
**Affects**:
- ❌ 3-node clusters cannot start
- ❌ Java client testing blocked
- ❌ Docker testing blocked
- ❌ All Raft clustering features non-functional

---

## Files to Investigate

Based on startup flow:

1. **`crates/chronik-server/src/main.rs`**
   - Line ~750: Cluster config loading
   - Line ~837-878: Server initialization
   - Look for port binding logic

2. **`crates/chronik-server/src/integrated_server.rs`**
   - Line ~197-241: `IntegratedKafkaServer::new_with_raft()`
   - Check how Kafka listener is created

3. **`crates/chronik-server/src/kafka_handler.rs`**
   - Server bind address logic

---

## Next Steps

### Immediate (P0 - Must Fix Before RC)

1. **⚠️ Debug Port Binding Logic**
   - Add debug logging to identify where multiple ports are bound
   - Check cluster config parsing
   - Check server initialization

2. **⚠️ Fix Bug**
   - Ensure server binds ONLY to `CHRONIK_KAFKA_PORT` env var
   - OR derive port from `peers[node_id].addr` correctly
   - Verify no iteration over ALL peers for binding

3. **✅ Verify Fix**
   ```bash
   # Clean restart
   pkill -9 chronik-server
   rm -rf test-cluster-data-ryw
   bash tests/start-test-cluster-ryw.sh

   # Verify 3 nodes running
   ps aux | grep chronik | wc -l
   # Expected: 3

   # Verify correct port bindings
   lsof -i :9092  # Should ONLY be node 1
   lsof -i :9093  # Should ONLY be node 2
   lsof -i :9094  # Should ONLY be node 3
   ```

4. **✅ Retry Java Client Testing**
   ```bash
   # Produce
   echo "test" | ksql/confluent-7.5.0/bin/kafka-console-producer \
     --bootstrap-server localhost:9092 --topic test

   # Consume
   timeout 5 ksql/confluent-7.5.0/bin/kafka-console-consumer \
     --bootstrap-server localhost:9092 --topic test --from-beginning
   ```

### After Bug Fix

- [ ] Complete Java client validation
- [ ] Docker deployment testing
- [ ] Update CHANGELOG for v2.0.0-rc.1
- [ ] Re-assess RC release readiness

---

## Lessons Learned

1. **Always check process health BEFORE testing**: Should have verified all 3 nodes running before attempting producer test

2. **Port conflicts are silent killers**: Node 3 crashed with no error logging, making diagnosis difficult

3. **Multi-node testing reveals bugs**: Single-node testing wouldn't have caught this port binding issue

4. **Java clients are strict**: kafka-console-producer immediately failed with protocol errors, making the cluster issue obvious

---

## Documentation Created

- `docs/CRITICAL_NODE3_CRASH.md` - Detailed bug report
- `docs/testing/JAVA_CLIENT_TESTING_2025-10-22.md` - This file
- Updated `docs/ROADMAP_v2.x.md` - Marked tasks complete/blocked

---

## Status

**Java Client Testing**: ⛔ **BLOCKED** - Cannot proceed until port binding bug is fixed

**RC Release**: ⛔ **BLOCKED** - P0 bug must be resolved first

**Next Session**: Fix port binding bug, then retry Java testing

---

**Tested By**: Claude Code
**Session Duration**: ~30 minutes
**Bugs Found**: 1 critical (P0)
**Tests Passed**: 0 (blocked)
**Tests Failed**: 2 (cluster startup, Java producer)
