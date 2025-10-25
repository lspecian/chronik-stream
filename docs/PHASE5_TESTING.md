# Phase 5: Comprehensive Testing

**Date**: 2025-10-24
**Status**: 🔄 IN PROGRESS - Running comprehensive test suite
**Duration**: ~10 minutes (4 tests × 2-5 min each)

---

## Test Suite Overview

**Test Script**: [tests/phase5_comprehensive_test.sh](../tests/phase5_comprehensive_test.sh)

### Test 1: Cluster Formation & Stability (5 min)
**Validates**: Phase 1 (Config fixes)
**What it tests**:
- Starts 3-node Raft cluster
- Creates test topic with 3 partitions, replication-factor 3
- Monitors for 5 minutes
- Counts total elections
- Tracks max term number
- Counts "lower term" errors

**Success Criteria**:
- ✅ Total elections ≤ 30
- ✅ Max term number ≤ 5
- ✅ Lower term errors < 100

---

### Test 2: Java Client Compatibility (2 min)
**Validates**: Phase 4 (Metadata sync retry)
**What it tests**:
- Uses kafka-console-producer to send 10 messages
- Uses kafka-console-consumer to read messages
- Checks metadata sync logs
- Verifies no client errors

**Success Criteria**:
- ✅ Producer succeeds (10 messages sent)
- ✅ Consumer receives all 10 messages
- ✅ No errors in client logs
- ✅ Metadata syncs within 5 retry attempts

---

### Test 3: High Write Load Stability (2 min)
**Validates**: Phase 2 (Non-blocking ready)
**What it tests**:
- Uses kafka-producer-perf-test
- Sends 10,000 messages at 5,000 msg/sec
- Monitors elections before and after load
- Checks for election churn during writes

**Success Criteria**:
- ✅ No new elections during load (or ≤3)
- ✅ Achieves target throughput
- ✅ No crashes or errors

---

### Test 4: Deserialization Error Check (1 min)
**Validates**: Phase 3 (Comprehensive logging)
**What it tests**:
- Scans logs for deserialization failures
- Checks data flow consistency (PRODUCE → REPLICA → STATE_MACHINE)
- Verifies Phase 3 logging is working

**Success Criteria**:
- ✅ Zero deserialization errors
- ✅ Data flow tracked through all stages
- ✅ No data corruption

---

## How to Run Tests

### Automated (Recommended)

```bash
# Run full test suite (interactive, ~10 minutes)
./tests/phase5_comprehensive_test.sh
```

The script will:
1. Start 3-node cluster
2. Run all 4 tests sequentially
3. Show real-time progress
4. Pause between tests (press Enter to continue)
5. Display final pass/fail summary
6. Keep cluster running for manual inspection

### Manual Testing

If you prefer to run tests manually:

```bash
# Build first
cargo build --release --bin chronik-server --features raft

# Start Node 1
RUST_LOG=info ./target/release/chronik-server \
  --kafka-port 9092 --advertised-addr localhost \
  --data-dir ./data-node1 \
  --cluster-config ./config/cluster-node1.toml \
  all > node1.log 2>&1 &

# Start Node 2
RUST_LOG=info ./target/release/chronik-server \
  --kafka-port 9093 --advertised-addr localhost \
  --data-dir ./data-node2 \
  --cluster-config ./config/cluster-node2.toml \
  all > node2.log 2>&1 &

# Start Node 3
RUST_LOG=info ./target/release/chronik-server \
  --kafka-port 9094 --advertised-addr localhost \
  --data-dir ./data-node3 \
  --cluster-config ./config/cluster-node3.toml \
  all > node3.log 2>&1 &

# Wait 15 seconds
sleep 15

# Create topic
kafka-topics --bootstrap-server localhost:9092 --create \
  --topic test --partitions 3 --replication-factor 3

# Monitor elections
watch -n 10 'grep "Became Raft leader" node*.log | wc -l'

# Produce/consume
kafka-console-producer --bootstrap-server localhost:9092 --topic test
kafka-console-consumer --bootstrap-server localhost:9092 --topic test --from-beginning

# Check logs
grep "Deserialization FAILED" node*.log
grep "Updated partition assignment" node*.log
```

---

## Expected Results

### If All Tests Pass ✅

```
╔════════════════════════════════════════════════════════════════╗
║  ✅ ALL TESTS PASSED!                                          ║
║                                                                ║
║  Phases 1-4 fixes validated successfully.                     ║
║  Chronik v2.0.0 is READY FOR GA RELEASE!                      ║
╚════════════════════════════════════════════════════════════════╝

Tests Passed: 4/4
Tests Failed: 0/4

Next Steps:
1. Tag release: git tag v2.0.0
2. Update CHANGELOG.md
3. Push: git push && git push --tags
4. Announce: v2.0.0 GA Release
```

### If Tests Fail ❌

```
╔════════════════════════════════════════════════════════════════╗
║  ❌ TESTS FAILED (X/4)                                         ║
║                                                                ║
║  Review logs in ./phase5_test_logs/                           ║
║  Use Phase 3 diagnostic logging for root cause analysis       ║
╚════════════════════════════════════════════════════════════════╝

Tests Passed: X/4
Tests Failed: X/4

Logs saved to: ./phase5_test_logs/
  - node1.log, node2.log, node3.log
  - producer.log, consumer.log, perf-test.log
```

---

## Troubleshooting

### Test 1 Fails (Election Churn)

**Symptom**: More than 30 elections in 5 minutes

**Root Cause Investigation**:
```bash
# Check if Phase 1 config was applied
grep "election_timeout_ms" target/release/chronik-server --version

# Check actual timeout values in logs
grep "Raft configuration" phase5_test_logs/node*.log

# Look for specific failure patterns
grep "election timeout" phase5_test_logs/node*.log
```

**Possible Causes**:
- Phase 1 config not applied (revert and rebuild)
- Network issues causing message delays
- State machine still blocking (Phase 2 issue)

### Test 2 Fails (Java Client Issues)

**Symptom**: Producer/consumer errors

**Root Cause Investigation**:
```bash
# Check metadata sync retries
grep "Updated partition assignment" phase5_test_logs/node*.log | grep "attempt"

# Check for sync failures
grep "CRITICAL: Partition assignment" phase5_test_logs/node*.log

# Check client error messages
cat phase5_test_logs/producer.log
cat phase5_test_logs/consumer.log
```

**Possible Causes**:
- Phase 4 retry logic not working
- __meta Raft group not forming
- Metadata inconsistency across nodes

### Test 3 Fails (Load Churn)

**Symptom**: Elections occur during high write load

**Root Cause Investigation**:
```bash
# Check if heartbeats were blocked
grep "ready_non_blocking" phase5_test_logs/node*.log

# Check for long apply times
grep "Applying.*committed entries" phase5_test_logs/node*.log

# Look for blocking patterns
grep "blocked\|waiting" phase5_test_logs/node*.log
```

**Possible Causes**:
- Phase 2 non-blocking not applied
- State machine still doing synchronous apply
- Tick loop using old `ready()` method

### Test 4 Fails (Deserialization Errors)

**Symptom**: Deserialization failures in logs

**Root Cause Investigation**:
```bash
# Get full error details with hex dumps
grep -A 5 "Deserialization FAILED" phase5_test_logs/node*.log

# Check data sizes
grep "PRODUCE: Proposing.*bytes" phase5_test_logs/node*.log
grep "REPLICA: propose.*data.len=" phase5_test_logs/node*.log
grep "STATE_MACHINE: apply.*data.len=" phase5_test_logs/node*.log

# Compare to see where data gets corrupted
```

**Possible Causes**:
- Data corruption in Raft log storage
- Bincode version mismatch
- Empty data being proposed
- Log compaction corrupting entries

---

## Quick Reference Commands

### Monitor Live Progress

```bash
# Watch election count
watch -n 5 'grep "Became Raft leader" phase5_test_logs/node*.log | wc -l'

# Watch for errors
tail -f phase5_test_logs/node*.log | grep -i "error\|fail\|critical"

# Check cluster health
ps aux | grep chronik-server | grep -v grep | wc -l  # Should be 3
```

### Check Specific Metrics

```bash
# Election statistics
grep "Became Raft leader" phase5_test_logs/node*.log | \
  awk '{print $NF}' | sort | uniq -c

# Term distribution
grep "Became Raft leader" phase5_test_logs/node*.log | \
  grep -oE "term=[0-9]+" | cut -d= -f2 | sort -n | uniq -c

# Metadata sync success rate
grep "Updated partition assignment" phase5_test_logs/node*.log | \
  grep -oE "attempt [0-9]+" | cut -d' ' -f2 | sort | uniq -c
```

### Manual Cleanup

```bash
# Kill all chronik-server processes
pkill -f chronik-server

# Clean test data
rm -rf phase5_test_logs phase5_data
```

---

## Test Log Locations

After test completion, logs are saved to:

```
phase5_test_logs/
├── node1.log          # Node 1 server logs
├── node2.log          # Node 2 server logs
├── node3.log          # Node 3 server logs
├── producer.log       # Java producer test output
├── consumer.log       # Java consumer test output
└── perf-test.log      # Performance test results
```

**Important Logs to Check**:
- Node logs: Election events, deserialization errors, metadata sync
- Producer log: Write failures, connection issues
- Consumer log: Read failures, CRC errors
- Perf log: Throughput, latency, error rates

---

## Next Steps Based on Results

### All Tests Pass ✅

1. **Tag v2.0.0**:
   ```bash
   git add -A
   git commit -m "fix: Complete Raft stability fixes (Phases 1-4) - v2.0.0"
   git tag -a v2.0.0 -m "v2.0.0 GA - Production-ready multi-node Raft clustering"
   git push origin main --tags
   ```

2. **Update Documentation**:
   - [x] Update CLAUDE.md version to v2.0.0
   - [x] Update CHANGELOG.md with Phase 1-4 changes
   - [x] Update README.md clustering status

3. **Announce Release**:
   - Create GitHub release with summary
   - Update docs with new stable version
   - Notify users of GA release

### Some Tests Fail ❌

1. **Analyze Failures**:
   - Use Phase 3 diagnostic logging
   - Review specific test failure patterns
   - Check COMPREHENSIVE_RAFT_FIX_PLAN.md for additional insights

2. **Fix Forward** (per CLAUDE.md policy):
   - Identify root cause from logs
   - Implement targeted fix
   - Tag as v2.0.1 (not v2.0.0)
   - Re-run tests

3. **Document Learnings**:
   - Add findings to COMPREHENSIVE_RAFT_FIX_PLAN.md
   - Update fix documentation
   - Share insights for future work

---

**Status**: 🔄 Test suite running...

**Check progress**: `tail -f phase5_test_logs/node1.log`

**Monitor elections**: `watch -n 5 'grep "Became Raft leader" phase5_test_logs/node*.log | wc -l'`
