# CRITICAL ISSUE: Node 3 Silent Crash on Startup

**Date**: 2025-10-22
**Severity**: P0 (CRITICAL - Blocks RC Release)
**Status**: ⛔ ACTIVE - Requires immediate fix

---

## Summary

During Java kafka-clients testing, discovered that **node 3 fails to start** in the 3-node test cluster. The node crashes silently with no log output.

## Evidence

### Test Command
```bash
bash tests/start-test-cluster-ryw.sh
```

**Expected**: 3 nodes running
**Actual**: Only 2 nodes running (nodes 1 and 2)

### Process Check
```bash
$ ps aux | grep chronik-server | grep -v grep
lspecian  17933  ... chronik-server standalone  # Node 1 ✅
lspecian  17943  ... chronik-server standalone  # Node 2 ✅
# Node 3 (PID 17953) missing ❌
```

### Log Evidence
```bash
$ ls -lh test-cluster-data-ryw/node3.log
-rw-r--r--  1 lspecian  staff   451K Oct 22 21:27 node3.log

$ tail -1 test-cluster-data-ryw/node3.log
[2025-10-22T19:27:06] Server shutdown complete  # OLD timestamp (2 hours ago!)

$ grep "2025-10-22T21:" test-cluster-data-ryw/node3.log
# No output - node never logged anything from current run
```

## Impact

**Cluster Stability**: ⛔ BROKEN
- Cannot form 3-node quorum (only 2 nodes)
- Raft requires 3+ nodes for fault tolerance
- Leader election may fail or take excessive time

**Java Client Testing**: ⛔ BLOCKED
- Java kafka-console-producer gets `NOT_LEADER_OR_FOLLOWER` errors
- Topics cannot be created (no quorum)
- Produce/consume operations fail

**RC Release**: ⛔ BLOCKED
- v2.0.0-rc.1 cannot be released with cluster instability
- Violates "MUST HAVE" criteria: "Build succeeds on all platforms"

## Root Cause

**Unknown** - Node 3 crashes/exits immediately after start with no error logging.

### Hypotheses

1. **Port Conflict**: Node 3 uses ports 9094 (Kafka) and 5003 (Raft gRPC)
   - Maybe port 5003 is already in use?
   - Script shows "PID 17953" was assigned but process doesn't exist

2. **Configuration Error**: `test-cluster-ryw-node3.toml` may have invalid config
   - Need to inspect config file

3. **Panic/Crash Before Logging**: Rust panic before logger initialization
   - Would explain absence of logs
   - Check if crash report in system logs

4. **Data Directory Corruption**: Old WAL data from previous runs
   - Node 3 data: `test-cluster-data-ryw/node3/`
   - Should be cleaned by script but maybe not working

## Diagnostic Steps

### 1. Check Port Availability
```bash
lsof -i :9094  # Kafka port for node 3
lsof -i :5003  # Raft gRPC port for node 3
```

### 2. Inspect Node 3 Config
```bash
cat tests/cluster-configs/test-cluster-ryw-node3.toml
```

### 3. Manual Start (Foreground)
```bash
# Start node 3 in foreground to see immediate errors
RUST_LOG=debug \
CHRONIK_CLUSTER_CONFIG=tests/cluster-configs/test-cluster-ryw-node3.toml \
CHRONIK_DATA_DIR=test-cluster-data-ryw/node3 \
CHRONIK_ADVERTISED_ADDR=localhost \
CHRONIK_KAFKA_PORT=9094 \
./target/release/chronik-server standalone
```

### 4. Check System Crash Logs
```bash
# macOS crash reports
ls -lt ~/Library/Logs/DiagnosticReports/ | grep chronik | head -5
```

### 5. Clean Data and Retry
```bash
# Completely clean and restart
pkill -9 chronik-server
rm -rf test-cluster-data-ryw
bash tests/start-test-cluster-ryw.sh
```

## Workaround

**None** - 3-node cluster is required for Raft quorum. Cannot proceed with 2 nodes.

## Next Steps

1. ⚠️ **IMMEDIATE**: Run diagnostic steps above to identify root cause
2. ⚠️ **URGENT**: Fix whatever is causing node 3 to crash
3. ✅ **VERIFY**: Restart cluster and confirm all 3 nodes run for 60+ seconds
4. ✅ **TEST**: Retry Java kafka-clients testing
5. ✅ **DOCUMENT**: Update KNOWN_ISSUES and CHANGELOG

## Timeline

- **21:26**: Started cluster for Java testing
- **21:28**: Java producer fails with `NOT_LEADER_OR_FOLLOWER`
- **21:29**: Discovered node 3 not running
- **21:30**: Confirmed silent crash (no logs)
- **21:31**: Created this issue document

---

**Reporter**: Claude Code (during Java client testing)
**Assignee**: Needs immediate investigation
**Priority**: P0 (Blocks RC release)
**ETA for Fix**: Unknown (depends on root cause)

---

## Related Files

- Start script: `tests/start-test-cluster-ryw.sh`
- Config: `tests/cluster-configs/test-cluster-ryw-node3.toml`
- Logs: `test-cluster-data-ryw/node3.log`
- Binary: `target/release/chronik-server`

## Test to Verify Fix

```bash
# Clean start
pkill -9 chronik-server
rm -rf test-cluster-data-ryw
bash tests/start-test-cluster-ryw.sh

# Wait for startup
sleep 15

# Verify all 3 nodes running
ps aux | grep chronik-server | grep -v grep | wc -l
# Expected: 3

# Test Java client
echo "test-message" | ksql/confluent-7.5.0/bin/kafka-console-producer \
  --bootstrap-server localhost:9092 --topic test-topic
# Expected: No errors, message produced

# Consume
timeout 5 ksql/confluent-7.5.0/bin/kafka-console-consumer \
  --bootstrap-server localhost:9092 --topic test-topic --from-beginning
# Expected: "test-message" displayed
```
