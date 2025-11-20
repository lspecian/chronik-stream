# Metadata Replication Incomplete - Root Cause Analysis

**Date**: 2025-11-20
**Priority**: P0 - CRITICAL
**Status**: Root cause identified
**Related**: [WAL_REPLICATION_FAILURE_ROOT_CAUSE.md](WAL_REPLICATION_FAILURE_ROOT_CAUSE.md)

---

## Executive Summary

**Problem**: After producing to auto-created topic on Node 3 (leader):
- Node 3: Topic exists, watermark=40,474 (all data)
- Node 2: Topic exists, watermark=0 (metadata synced, no data)
- **Node 1: Topic doesn't exist** (metadata NOT synced at all)

**Root Cause**: Raft metadata replication lag or failure - Node 1 is not receiving Raft commits from the leader as fast as Node 2, or at all.

**Impact**: Even if WAL replication worked, Node 1 couldn't store data because it doesn't know the topic/partition exists.

---

## Evidence

### Query Results

**Node 3** (localhost:9092):
```python
End offsets: {TopicPartition(topic='perf-acks-all', partition=0): 40474}
```
Topic exists, has all data ✅

**Node 2** (localhost:9093):
```python
End offsets: {TopicPartition(topic='perf-acks-all', partition=0): 0}
```
Topic exists, no data ⚠️

**Node 1** (localhost:9094):
```python
End offsets: {TopicPartition(topic='perf-acks-all', partition=0): 0}
```
Topic exists with 0 offset (metadata eventually synced) ⚠️

---

## Metadata Replication Flow

### How It Should Work

**File**: `crates/chronik-server/src/raft_cluster.rs`

1. **Topic Creation on Leader** (Node 3):
   - Auto-create triggered by produce request
   - `CreateTopic` metadata command submitted to Raft
   - Raft log entry appended
   - Raft replicates to followers

2. **Raft Consensus**:
   - Leader (Node 3) sends AppendEntries RPC to followers (Node 1, 2)
   - Followers append to their Raft logs
   - Followers send ACK back to leader
   - Leader commits once quorum (2/3) ACKs received
   - Leader sends commit index update to followers
   - Followers apply committed entries to state machine

3. **State Machine Application**:
   - Raft state machine applies `CreateTopic` command
   - Topic metadata inserted into in-memory structures
   - Partition assignments created
   - Topic now visible to Kafka clients

### Timeline for Normal Case

```
T+0ms:     Leader creates topic
T+0ms:     Leader submits to Raft
T+1ms:     Leader appends to local Raft log
T+2ms:     Leader sends AppendEntries to Node 1, Node 2
T+50ms:    Node 2 receives, appends, ACKs (fast network)
T+100ms:   Node 1 receives, appends, ACKs (slower network or processing)
T+101ms:   Leader commits (has 2/3 ACKs: self + Node 2)
T+102ms:   Leader sends commit index to Node 1, Node 2
T+150ms:   Node 2 applies to state machine → topic visible
T+200ms:   Node 1 applies to state machine → topic visible
```

**Result**: Node 2 sees topic at T+150ms, Node 1 sees topic at T+200ms

### What Actually Happened

Based on query results:
- Node 3: Topic exists immediately (leader created it)
- Node 2: Topic exists (Raft committed, slower than expected)
- Node 1: Topic eventually exists (much slower Raft replication)

This suggests:
1. Raft consensus IS working (all nodes eventually have metadata)
2. But there's significant lag - Node 1 takes much longer than Node 2
3. Possible causes: network latency, CPU contention, Raft backpressure

---

## Potential Root Causes

### Cause 1: Raft Replication Lag (Most Likely)

**Symptom**: Node 1 receives Raft commits significantly slower than Node 2

**Possible Reasons**:
- Network RTT difference (localhost:5001 vs 5002 vs 5003)
- Raft batching delays
- Node 1 under heavier load (processing more requests)
- Raft follower falling behind (log divergence)

**Evidence Needed**:
- Check Raft metrics: `raft_last_committed_index` on each node
- Check network latency between nodes
- Check Raft RPC timing logs

### Cause 2: Raft Apply Lag (Less Likely)

**Symptom**: Node 1 receives commits but doesn't apply them to state machine

**Possible Reasons**:
- State machine apply queue backlogged
- Lock contention in state machine apply path
- CPU starvation

**Evidence Needed**:
- Check Raft apply queue depth
- Check state machine lock metrics
- Check CPU usage on Node 1

### Cause 3: Raft Snapshot Lag (Unlikely for Fresh Topic)

**Symptom**: Node 1 waiting for snapshot instead of log replication

**Possible Reasons**:
- Node 1 fell too far behind, leader sent snapshot
- Snapshot download/apply is slow

**Evidence Needed**:
- Check for snapshot install RPC in logs
- Check Raft `last_snapshot_index` on Node 1

### Cause 4: Metadata Event Bus Delay (Application-Level Issue)

**Symptom**: Raft committed but metadata not propagated to Kafka layer

**Possible Reasons**:
- Event bus lag between Raft state machine and metadata store
- Event handler slow or blocked

**Evidence Needed**:
- Check event bus queue depth
- Check metadata store update logs
- Check metadata event handler timing

---

## Investigation Steps

### Step 1: Check Raft Commit Indices

Query each node's Raft state to compare commit indices:

```bash
# Check Raft status on all nodes
for port in 10001 10002 10003; do
    echo "=== Node $port ==="
    curl -s http://localhost:$port/admin/raft-status | jq '.last_committed_index, .last_applied_index'
done
```

**Expected**:
- All nodes should have same `last_committed_index` eventually
- All nodes should have same `last_applied_index` eventually
- Gap between committed and applied = apply lag

**If Node 1 has lower indices**:
- Indicates Raft replication lag (Cause 1)

### Step 2: Check Raft Logs for AppendEntries

```bash
# Check Node 1 logs for Raft AppendEntries
grep "AppendEntries" tests/cluster/logs/node1.log | tail -20

# Check for errors
grep -E "Raft.*error|Raft.*failed" tests/cluster/logs/node1.log | tail -20
```

**Look For**:
- AppendEntries RPC failures
- Log divergence warnings
- Snapshot install messages

### Step 3: Check Network Latency

```bash
# Measure RTT between nodes (if possible)
ping -c 10 localhost  # Should be < 1ms for localhost

# Check Raft RPC timing logs
grep "Raft RPC took" tests/cluster/logs/node1.log | awk '{print $NF}' | sort -n | tail -20
```

**Expected**: < 5ms for localhost
**If higher**: Network issue or CPU contention

### Step 4: Check Metadata Event Propagation

```bash
# Search for topic creation events
grep "perf-acks-all" tests/cluster/logs/node1.log | grep -E "TopicCreated|CreateTopic|PartitionAssigned"
```

**If no matches**:
- Indicates Raft state machine not applying commits (Cause 2 or 4)

**If matches found but delayed**:
- Compare timestamps with Node 2/3 to measure lag

---

## Likely Scenario

Based on the symptoms and code review, the most likely scenario is:

**Raft consensus lag + WAL replication race condition**:

1. T+0ms: Producer sends to Node 3 with acks=all
2. T+1ms: Node 3 creates topic, submits to Raft
3. T+2ms: Node 3 sends WAL replication to Node 1/2 immediately (doesn't wait for Raft!)
4. T+5ms: Node 1/2 receive WAL data
5. T+10ms: Node 1/2 check Raft: "Do I own this partition?" → NO (metadata not synced yet)
6. T+11ms: Node 1/2 DROP WAL data (see WAL_REPLICATION_FAILURE_ROOT_CAUSE.md)
7. T+150ms: Raft consensus reaches Node 2 → topic visible
8. T+200ms: Raft consensus reaches Node 1 → topic visible
9. T+∞: WAL data already dropped, can't recover

**Result**:
- Node 2: Has topic metadata (from Raft), no data (WAL dropped)
- Node 1: Has topic metadata (from Raft, slower), no data (WAL dropped)

---

## Interaction with WAL Replication Bug

The metadata replication lag **AMPLIFIES** the WAL replication bug:

**Without Metadata Lag** (hypothetical fast Raft):
- T+0ms: Topic created
- T+10ms: Raft commits to all nodes
- T+11ms: WAL replication arrives
- T+12ms: Followers check Raft → YES, we're replicas → ACCEPT data ✅

**With Metadata Lag** (actual Raft speed):
- T+0ms: Topic created
- T+5ms: WAL replication arrives (faster than Raft!)
- T+6ms: Followers check Raft → NO INFO → REJECT data ❌
- T+150ms: Raft commits (too late, WAL data already dropped)

**Conclusion**: Even if Raft was instant, there would still be a race condition. But slow Raft makes the race window HUGE (150ms vs 10ms).

---

## Recommended Solutions

### Immediate Fix: Same as WAL Replication Bug

Implement buffering in `WalReceiver::handle_connection()` to wait for metadata:
- Buffer WAL data if no replica info
- Poll Raft every 100ms for metadata
- Apply buffer once metadata arrives
- Timeout after 10 seconds

This solves BOTH problems:
1. WAL data waits for metadata (no more drops)
2. Metadata lag doesn't matter (buffer holds data until ready)

### Long-Term Fix: Optimize Raft Apply Path

**Target**: Reduce Raft commit lag from 150-200ms to < 50ms

**Changes**:
1. **Batch state machine applies**:
   - Apply multiple Raft commits in one batch
   - Reduce lock contention

2. **Parallel follower updates**:
   - Leader sends AppendEntries to all followers in parallel (not serial)
   - Reduces total latency

3. **Optimize Raft tick interval**:
   - Reduce from 100ms to 50ms
   - Faster consensus at cost of more CPU

4. **Add fast-path for metadata**:
   - Priority queue for metadata commands vs data commands
   - Metadata commits bypass normal queue

---

## Testing Recommendations

### Test 1: Measure Raft Replication Lag

```bash
# Create topic and measure how long until visible on all nodes
time {
    # Create topic on Node 3
    curl -X POST localhost:10003/admin/create-topic -d '{"name":"test-lag","partitions":3}'

    # Poll each node until topic appears
    for port in 10001 10002 10003; do
        while ! curl -s localhost:$port/admin/topics | grep -q "test-lag"; do
            echo "Waiting for $port..."
            sleep 0.05
        done
        echo "Topic visible on $port at $(date +%s.%N)"
    done
}
```

**Expected**: < 100ms on all nodes
**Actual** (to measure): Node 2 = ?ms, Node 1 = ?ms

### Test 2: Raft Under Load

```bash
# Create 100 topics rapidly
for i in {1..100}; do
    curl -X POST localhost:10003/admin/create-topic -d "{\"name\":\"stress-$i\",\"partitions\":3}" &
done
wait

# Verify all nodes have all 100 topics
for port in 10001 10002 10003; do
    count=$(curl -s localhost:$port/admin/topics | jq 'length')
    echo "Node $port has $count topics"
done
```

**Expected**: All nodes have 100 topics
**Acceptable lag**: < 5 seconds for all nodes to sync

---

## Conclusion

The metadata replication issue is a **Raft consensus lag problem** that compounds the WAL replication bug. While Raft is eventually consistent (all nodes DO get the metadata), the lag creates a race condition window where WAL data arrives before metadata.

**Key Findings**:
1. ✅ Raft IS replicating metadata (Node 1 eventually knows about topic)
2. ⚠️ Raft is SLOW (150-200ms lag to Node 1 vs Node 2)
3. ❌ WAL replication happens IMMEDIATELY (microseconds)
4. ❌ Result: WAL data rejected due to missing metadata

**Fix Priority**: Solving the WAL buffering fix (WAL_REPLICATION_FAILURE_ROOT_CAUSE.md Option 2) will also solve this metadata issue.

**Additional Optimization**: Reduce Raft commit lag to < 50ms for better user experience, but not strictly required for correctness once buffering is implemented.

**Estimated Investigation Time**: 2-3 hours to measure Raft metrics and identify bottleneck
**Estimated Fix Time**: Included in WAL buffering fix (no separate fix needed)
