# Cluster Status Report - v2.2.8

**Date**: 2025-11-16
**Investigation**: Performance testing and cluster health check
**Status**: CORRECTED - Original diagnosis was incorrect

---

## Executive Summary

### ORIGINAL DIAGNOSIS (INCORRECT ❌)
- Suspected: Election storm preventing all operations
- Suspected: Missing Raft WAL files
- Conclusion: WAL persistence bug

### ACTUAL STATUS (CORRECTED ✅)
- **Raft cluster**: ✅ FUNCTIONAL (Node 2 is stable leader)
- **WAL persistence**: ✅ WORKING (all nodes have Raft logs)
- **Metadata replication**: ✅ WORKING (offset 15,893+)
- **Existing topics**: ✅ WORKING (produce/consume functional)
- **Topic auto-creation**: ❌ **BROKEN** (5s timeout)

---

## What Actually Happened

### Timeline of Misdiagnosis

**Nov 15 21:27** - Election storm occurred during cluster startup
- Lasted ~13 seconds (term 6 → term 15)
- Cluster stabilized on its own
- Last election: Nov 15 21:27:20 (term 15)

**Nov 16 11:00-14:00** - Investigation period
- Initial tests hung due to topic creation timeout
- Misinterpreted old logs as current state
- Assumed cluster was broken

**Nov 16 14:00** - Discovery of actual state
- Cluster IS functional (has been for 17+ hours)
- Raft logs DO exist on all nodes
- Metadata IS replicating successfully
- Problem is **topic auto-creation**, not Raft

---

## Current Cluster State

### Raft Consensus ✅
```
Leader: Node 2 (192.168.1.7)
Term: 15 (stable since Nov 15 21:27:20)
Uptime: 17+ hours without election
```

### WAL Status ✅
```
Node 1: /home/ubuntu/chronik-data/wal/__meta/__raft_metadata/0/
        - wal_0_0.log (377K)
        - wal_0_1.log (282K)
        - wal_0_2.log (76K)
        Total: 760K

Node 2: /home/ubuntu/chronik-data/wal/__meta/__raft_metadata/0/
        - wal_0_0.log (21K)
        - wal_0_1.log (3.6K)
        Total: 40K

Node 3: /home/ubuntu/chronik-data/wal/__meta/__raft_metadata/0/
        - wal_0_0.log (21K)
        - wal_0_1.log (128K)
        Total: 168K
```

### Metadata Replication ✅
```
Topic: __chronik_metadata-0
Leader: Node 2
Followers: Nodes 1, 3
Current offset: 15,893+
Replication: Active (data flowing)
```

---

## Critical Issue: Topic Auto-Creation

### The Bug ❌

When attempting to create new topics via Raft metadata store:

```
ERROR: Topic 'perf-test-acks1-async' creation timed out after 5000ms
Failed to auto-create topic: NotFound("Topic ... not found after timeout")
```

### Impact

- ✅ **Existing topics work** (can produce/consume)
- ❌ **New topics fail** (5s timeout, metadata not created)
- ✅ **Raft is healthy** (leader elected, logs persisting)
- ❌ **Raft metadata operations timeout** (topic creation specifically)

### Evidence

From logs:
```
[2025-11-16T14:35:41] Auto-creating topic 'perf-test-acks1-async'
[2025-11-16T14:35:46] WARN: Topic creation timed out after 5000ms
[2025-11-16T14:35:46] ERROR: Failed to auto-create topic
```

This is a **Raft metadata store bug**, not a WAL or election issue.

---

## Performance Testing Results

### Test 1: Synchronous (Blocking)
```python
for i in range(50000):
    future = producer.send(topic, value=message)
    future.get(timeout=10)  # ← BLOCKS!
```

**Results:**
- Throughput: **67 msg/s** ❌
- Latency p99: 18.98ms ✅
- Problem: Serialized sends (waits for each ACK)

### Test 2: Asynchronous (Pipelined)
```python
futures = []
for i in range(50000):
    future = producer.send(topic, value=message)
    futures.append(future)
# Wait for all at end
for f in futures:
    f.get()
```

**Results:**
- Throughput: **13,402 msg/s** ✅ (67% of target)
- Latency p99: 754.84ms (acceptable)
- Bandwidth: 3.27 MB/s
- Total time: 3.73s

**Comparison:**
- Sync: 67 msg/s
- Async: 13,402 msg/s
- Improvement: **200x faster**

### Target Analysis
```
Target: 20,000 msg/s
Achieved: 13,402 msg/s
Gap: -6,598 msg/s (33% shortfall)
```

**Why we're not hitting 20k:**
1. Raft replication overhead (3-way replication)
2. WAL fsync latency (durability guarantee)
3. Network latency between physical machines
4. Possible topic auto-creation blocking metadata updates

---

## Root Causes

### 1. Raft Metadata Store Timeout (CRITICAL)

**Problem**: `create_topic()` operations via Raft consensus time out after 5 seconds.

**Symptoms:**
- New topic creation fails
- Metadata requests hang
- Auto-create operations fail

**Impact**: Cannot create new topics dynamically

**Fix needed**: Investigate why Raft metadata proposals time out

### 2. Original Diagnosis Error

**What went wrong:**
1. Saw old election logs from 17 hours ago
2. Assumed cluster was currently broken
3. Didn't verify actual current state
4. Claimed WAL files didn't exist (they did!)

**Lesson**: Always verify current state, not just logs

---

## Corrected Facts

### What's TRUE ✅
- Raft cluster is stable (Node 2 leader for 17+ hours)
- WAL files exist on all nodes
- Metadata replication works
- Existing topics can produce/consume
- Async performance: 13,402 msg/s (good, not great)

### What's FALSE ❌
- ~~Election storm preventing operations~~ (was temporary, 17 hours ago)
- ~~No Raft WAL directory~~ (exists on all 3 nodes)
- ~~WAL writes claim success but don't persist~~ (they do persist)
- ~~Cluster completely non-functional~~ (works with existing topics)

---

## Next Steps

### Immediate (Fix Topic Creation)
1. Investigate Raft metadata store timeout logic
2. Check why `create_topic()` proposals don't complete
3. Verify Raft commit index advancement
4. Test topic creation with debug logging

### Performance Optimization (Hit 20k Target)
1. Profile WAL fsync latency
2. Optimize batch commit sizes
3. Tune Raft replication parameters
4. Consider increasing `max_in_flight_requests`

### Documentation
1. ~~Delete incorrect RAFT_ELECTION_STORM_BUG.md~~ (or mark as incorrect)
2. Document topic auto-creation bug
3. Update performance baselines
4. Add cluster health check procedures

---

## Lessons Learned

1. **Verify current state** - Don't assume based on old logs
2. **Test with existing resources** - New topic creation was the blocker
3. **Async vs Sync matters** - 200x performance difference
4. **Cluster can appear healthy but have specific bugs** - Raft works, topic creation doesn't

---

## Reproduction (Topic Creation Bug)

```bash
# This WILL fail:
python3 -c "
from kafka import KafkaProducer
producer = KafkaProducer(
    bootstrap_servers=['192.168.1.5:29092'],
    acks=1
)
# Tries to auto-create topic → 5s timeout
producer.send('new-topic-test', b'test')
producer.flush()
"

# This WILL work:
python3 -c "
from kafka import KafkaProducer
producer = KafkaProducer(
    bootstrap_servers=['192.168.1.5:29092'],
    acks=1
)
# Uses existing topic → succeeds
producer.send('perf-test-acks1', b'test')
producer.flush()
"
```

---

## Status: CLUSTER FUNCTIONAL WITH LIMITATIONS

- ✅ Can produce/consume on existing topics
- ✅ Raft consensus is stable
- ✅ WAL persistence works
- ❌ Cannot create new topics (Raft metadata timeout)
- ⚠️  Performance: 13k msg/s (67% of 20k target)
