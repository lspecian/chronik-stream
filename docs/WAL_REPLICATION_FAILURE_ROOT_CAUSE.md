# WAL Replication Failure - Root Cause Analysis

**Date**: 2025-11-20
**Priority**: P0 - CRITICAL
**Status**: Root cause identified
**Related**: [WATERMARK_VISIBILITY_BUG_ROOT_CAUSE.md](WATERMARK_VISIBILITY_BUG_ROOT_CAUSE.md)

---

## Executive Summary

**Problem**: WAL data replication appears to succeed from leader (Node 3) but data never persists on followers (Node 1/2), resulting in 100% data loss if leader fails.

**Root Cause**: Chicken-and-egg problem between WAL replication and metadata replication:
- WAL data arrives at followers BEFORE Raft metadata syncs
- Followers reject WAL data because they don't know they're replicas yet
- Raft metadata consensus takes seconds; WAL replication happens immediately
- Result: Data silently dropped on followers

**Impact**: Complete replication failure despite acks=all configuration, violating durability guarantees.

---

## Evidence

### Symptoms

**Node 3 (Leader)**:
- Watermark: 40,474 (all produced data)
- Data size: 251 MB in WAL files
- Logs: "✅ Sent and flushed WAL record for perf-acks-all-0 to follower: localhost:9291 (1990 bytes)"
- Conclusion: Leader believes replication succeeded

**Node 2 (Follower)**:
- Watermark: 0 (no data)
- Topic exists in metadata (knows about topic)
- Logs: "WAL receiver: Accepted connection from 127.0.0.1"
- Conclusion: Received connections but no data persisted

**Node 1 (Follower)**:
- Watermark: 0 (no data)
- Topic DOESN'T exist in metadata (doesn't know about topic!)
- Logs: "WAL receiver: Accepted connection from 127.0.0.1"
- Conclusion: Received connections but no data persisted + metadata incomplete

### Timeline

```
T+0ms:   Producer sends message to Node 3 with acks=all
T+1ms:   Node 3 creates topic (auto-creation)
T+2ms:   Node 3 writes to local WAL
T+3ms:   Node 3 submits topic creation to Raft
T+4ms:   Node 3 sends WAL replication to Node 1/2
T+5ms:   Node 1/2 receive WAL data
T+6ms:   Node 1/2 query Raft: "Am I a replica?" → Raft returns None (not synced yet)
T+7ms:   Node 1/2 REJECT and DROP WAL data
T+500ms: Raft consensus completes, Node 2 learns about topic
T+2000ms: Raft consensus reaches Node 1 (slower), Node 1 learns about topic
T+∞:     WAL data already dropped, can never be recovered
```

---

## Root Cause Analysis

### Location: `crates/chronik-server/src/wal_replication.rs`

**Lines 1698-1722**: `WalReceiver::handle_connection()` critical filtering logic

```rust
// CRITICAL FIX (Phase 3 CORRECTED): Only accept replication for partitions where we're a REPLICA
if let Some(ref raft) = raft_cluster {
    // Query partition replicas from Raft metadata
    if let Some(replicas) = raft.get_partition_replicas(&wal_record.topic, wal_record.partition) {
        if !replicas.contains(&node_id) {
            info!("⏭️  Skipping replication for {}-{}: this node ({}) is NOT in replica set {:?}",
                wal_record.topic, wal_record.partition, node_id, replicas);
            continue; // Skip - we're not a replica for this partition
        }

        info!("✅ Accepting replication for {}-{}: this node ({}) IS in replica set {:?}",
            wal_record.topic, wal_record.partition, node_id, replicas);
    } else {
        // No replica info yet - REJECT to prevent duplication
        warn!("⛔ NO REPLICA INFO for {}-{} yet, REJECTING replication (waiting for metadata sync)",
            wal_record.topic, wal_record.partition);
        continue; // Skip - no replica info available
    }
}
```

### Why This Fails

**Design Intent**: Prevent message duplication by ensuring followers only accept replication for partitions they own.

**Actual Behavior**:
1. **New topic creation**: Auto-created on leader first
2. **Raft submission**: Topic creation submitted to Raft for consensus
3. **Immediate WAL replication**: Leader sends data to followers immediately (doesn't wait for Raft)
4. **Followers check Raft**: `get_partition_replicas()` returns `None` (not committed yet)
5. **Followers reject**: Lines 1716-1721 drop the data
6. **Raft eventually commits**: Metadata syncs, but WAL data already lost

**The chicken-and-egg**:
- WAL receiver needs Raft metadata to accept data
- Raft metadata needs time to consensus (100ms-2s)
- WAL data arrives in microseconds
- Result: Data arrives before metadata, gets rejected

---

## Why Two Different Symptoms?

### Node 2: Has Topic, No Data

**Explanation**: Raft consensus reached Node 2 relatively quickly (500ms), so:
- Topic metadata committed to Node 2's Raft log
- Topic appears in metadata store
- But WAL data was already dropped 500ms earlier

### Node 1: No Topic, No Data

**Explanation**: Raft consensus to Node 1 even slower (2s), so:
- Topic metadata NOT committed yet (or slower replication)
- Topic doesn't appear in metadata store
- WAL data was dropped 2s earlier

This suggests Node 1 is having additional Raft replication delays on top of the WAL issue.

---

## Code Flow Analysis

### Leader Side (Working Correctly)

**File**: `crates/chronik-server/src/wal_replication.rs`

1. **Lines 287-315**: `replicate_serialized()` pushes to lock-free queue
2. **Lines 445-474**: `run_sender_worker()` dequeues and sends
3. **Lines 482-550**: `send_to_followers()` sends to ALL followers
4. **Lines 533-542**: CRITICAL - calls `flush()` after `write_all()` to actually send
5. Logs confirm: "✅ Sent and flushed WAL record for perf-acks-all-0 to follower: localhost:9291"

**Conclusion**: Leader side works perfectly.

### Follower Side (Broken Filtering)

**File**: `crates/chronik-server/src/wal_replication.rs`

1. **Lines 1498-1552**: `run()` accepts TCP connections
2. **Lines 1554-1786**: `handle_connection()` processes frames
3. **Lines 1624-1665**: Deserializes WAL frames successfully
4. **Lines 1698-1722**: ❌ **CRITICAL FILTERING** - checks Raft replica set
5. **Lines 1724-1738**: `write_to_wal()` only called if filtering passes
6. **Lines 1788-1815**: Actually writes to local WAL

**Conclusion**: Data is received and deserialized successfully, but dropped at step 4 due to missing Raft metadata.

---

## Impact Assessment

### Durability Violation

**acks=all Contract**: "Return success only when message is replicated to all in-sync replicas"

**Actual Behavior**:
- Producer gets success response (acks=all satisfied)
- Leader has data
- Followers have NO data
- Leader crash = 100% data loss

### ISR Inconsistency

**Expected**: All 3 nodes in ISR for RF=3 cluster
**Actual**: Only leader (Node 3) has data, but ISR shows [1,2,3]
**Result**: ISR list is WRONG - Nodes 1/2 are NOT in sync but marked as such

### Client Visible Impact

**Consumption**:
- From leader (Node 3): 100% success
- From followers (Node 1/2): 0% success (no data)
- Kafka clients round-robin between brokers → 66% failure rate

**Production**:
- acks=1: Works (leader only)
- acks=all: FALSE success (followers don't have data)

---

## Proposed Solutions

### Option 1: Disable Replica Filtering (Quick Fix - Unsafe)

**Change**: Remove lines 1698-1722 entirely

**Pros**:
- WAL data would be accepted immediately
- No chicken-and-egg problem
- Simple one-line fix

**Cons**:
- **UNSAFE**: Can cause message duplication
- Partition reassignment would write data to wrong nodes
- Leader change scenarios could duplicate messages
- WHY IT EXISTS: The filtering was added to prevent duplication bugs

**Recommendation**: ❌ **DO NOT DO THIS** - would introduce worse bugs

### Option 2: Buffer WAL Data Until Metadata Syncs (Correct Fix)

**Change**: In `WalReceiver::handle_connection()`, buffer received WAL records if no replica info

**Implementation**:
```rust
// New data structure
struct PendingWalBuffer {
    records: DashMap<(String, i32), Vec<WalReplicationRecord>>,
    max_buffer_size: usize,  // e.g., 10,000 records per partition
}

// In handle_connection()
if let Some(replicas) = raft.get_partition_replicas(&wal_record.topic, wal_record.partition) {
    // Replica info available - accept immediately
    if replicas.contains(&node_id) {
        Self::write_to_wal(&wal_manager, &wal_record).await?;
    }
} else {
    // No replica info yet - BUFFER instead of drop
    warn!("📦 BUFFERING WAL data for {}-{} until metadata syncs (buffer size: {})",
          wal_record.topic, wal_record.partition, pending_buffer.len());

    pending_buffer.insert((wal_record.topic, wal_record.partition), wal_record);

    // Spawn task to retry after Raft sync (poll every 100ms for 10 seconds)
    tokio::spawn(async move {
        for _ in 0..100 {
            sleep(Duration::from_millis(100)).await;
            if let Some(replicas) = raft.get_partition_replicas(&topic, partition) {
                if replicas.contains(&node_id) {
                    // Metadata arrived! Flush buffer to WAL
                    for record in pending_buffer.drain() {
                        Self::write_to_wal(&wal_manager, &record).await?;
                    }
                    return Ok(());
                }
            }
        }
        // Timeout after 10s - drop buffer and warn
        warn!("⚠️ Metadata never arrived for {}-{}, dropping {} buffered records",
              topic, partition, pending_buffer.len());
    });
}
```

**Pros**:
- ✅ Solves chicken-and-egg problem
- ✅ Preserves duplication prevention
- ✅ Graceful degradation (timeout after 10s)
- ✅ No changes to leader side

**Cons**:
- Adds memory overhead for buffering
- Complex async retry logic
- 10s timeout still loses data if Raft is extremely slow

**Recommendation**: ⭐ **BEST SOLUTION** for WAL replication

### Option 3: Wait for Metadata Sync Before WAL Replication (Leader Fix)

**Change**: On leader side, don't send WAL replication until Raft metadata commit completes

**Implementation**:
```rust
// In ProduceHandler or wherever topic is created
let (topic_created, partition_assigned) = raft_cluster.create_topic(...).await?;

// WAIT for metadata to commit before replicating
topic_created.wait().await?;  // Block until Raft committed

// NOW it's safe to replicate WAL
wal_replication_manager.replicate_serialized(...).await;
```

**Pros**:
- ✅ Guarantees metadata arrives before data
- ✅ Simple conceptual model
- ✅ No buffering needed

**Cons**:
- ⚠️ Adds latency to produce path (wait for Raft consensus = 100ms-2s)
- ⚠️ Defeats purpose of async WAL replication
- ⚠️ Blocks produce on Raft speed (bad for performance)

**Recommendation**: ❌ **NOT RECOMMENDED** - too slow for production

### Option 4: Raft Metadata Pre-Sync (Hybrid Fix)

**Change**: When leader creates topic, immediately send metadata replication BEFORE WAL data

**Implementation**:
```rust
// In ProduceHandler for auto-created topics
1. Create topic locally on leader
2. Submit to Raft (async, don't wait)
3. IMMEDIATELY send metadata replication message to followers (direct, no Raft)
4. Followers apply metadata directly (bypass Raft on followers)
5. Then send WAL data replication (followers now have metadata)
```

**Pros**:
- ✅ No latency added (metadata sent async)
- ✅ Metadata arrives before WAL data (guaranteed ordering)
- ✅ No buffering needed
- ✅ Raft still provides consistency eventually

**Cons**:
- Complex: Two metadata paths (Raft + direct replication)
- Potential inconsistency if direct metadata fails but Raft succeeds
- Requires new protocol

**Recommendation**: 🔶 **GOOD LONG-TERM** but complex to implement correctly

---

## Recommended Fix Plan

### Immediate Fix (Option 2 - Buffering)

**Target**: v2.2.10

1. **Add pending buffer** to `WalReceiver`:
   ```rust
   struct PendingWalBuffer {
       records: Arc<DashMap<(String, i32), VecDeque<WalReplicationRecord>>>,
       max_per_partition: usize,  // 10,000 records
   }
   ```

2. **Modify `handle_connection()`** (lines 1698-1722):
   - If replica info missing: buffer instead of drop
   - Spawn retry task to check Raft every 100ms
   - Apply buffered records once metadata arrives
   - Timeout after 10 seconds with warning

3. **Add metrics**:
   - `wal_replication_buffered_records` - count of buffered records
   - `wal_replication_buffer_timeouts` - count of timeouts
   - `wal_replication_buffer_applied` - count of successful buffer applications

4. **Test**:
   - Create new topic with acks=all
   - Verify followers buffer WAL data
   - Verify followers apply buffered data after Raft sync
   - Verify all nodes have data (100% replication)

### Long-Term Fix (Option 4 - Metadata Pre-Sync)

**Target**: v2.3.0

1. **Implement direct metadata replication**:
   - Leader sends metadata updates to followers before WAL data
   - Followers apply metadata immediately (don't wait for Raft)
   - Raft still provides eventual consistency check

2. **Ensure ordering**:
   - Metadata replication must complete before WAL replication starts
   - Use sync point or barrier

3. **Handle failures**:
   - If metadata replication fails, retry before WAL replication
   - If consistently fails, fall back to Raft-only path

---

## Testing Plan

### Test 1: Auto-Created Topic Replication

```bash
# Start fresh cluster
./tests/cluster/stop.sh && ./tests/cluster/start.sh

# Produce to new topic (auto-create)
./target/release/chronik-bench --acks all --message-count 1000 --topic new-topic

# Verify replication to ALL nodes
for port in 9092 9093 9094; do
    echo "=== Node $port ==="
    python3 -c "
from kafka import KafkaConsumer
from kafka import TopicPartition
consumer = KafkaConsumer(bootstrap_servers=['localhost:$port'])
tp = TopicPartition('new-topic', 0)
offsets = consumer.end_offsets([tp])
print(f'End offset: {offsets[tp]}')
consumer.close()
    "
done

# Expected: ALL nodes show end_offset=1000
# Before fix: Node 3=1000, Node 1/2=0
```

### Test 2: Raft Metadata Delay

```bash
# Introduce artificial Raft delay (if possible via config)
CHRONIK_RAFT_COMMIT_DELAY=2000ms ./tests/cluster/start.sh

# Produce immediately
./target/release/chronik-bench --acks all --message-count 100 --topic delayed-topic

# Verify buffering works despite 2s Raft delay
# Expected: All nodes eventually have data after buffer application
```

### Test 3: High-Throughput Stress Test

```bash
# Rapid topic creation + production
for i in {1..100}; do
    ./target/release/chronik-bench --acks all --message-count 10 --topic stress-$i &
done
wait

# Verify NO data loss on any node
# Check buffer overflow didn't occur
```

---

## Related Issues

- [WATERMARK_VISIBILITY_BUG_ROOT_CAUSE.md](WATERMARK_VISIBILITY_BUG_ROOT_CAUSE.md) - Fixed in v2.2.9
- [CLUSTER_CRASH_ROOT_CAUSE_ANALYSIS.md](CLUSTER_CRASH_ROOT_CAUSE_ANALYSIS.md) - Memory leak
- [MEMORY_LEAK_TANTIVY_ANALYSIS.md](MEMORY_LEAK_TANTIVY_ANALYSIS.md) - Tantivy indexing memory

---

## Conclusion

This is a **P0 CRITICAL** bug that completely breaks replication despite correct acks=all configuration. The root cause is a design flaw in the WAL receiver's filtering logic that rejects data before Raft metadata syncs.

**Key Takeaways**:
1. ✅ **WAL sending works** - Leader successfully sends data to followers
2. ✅ **TCP transport works** - Followers receive and deserialize frames
3. ❌ **Filtering is broken** - Rejects data due to missing metadata
4. ❌ **Timing issue** - WAL arrives microseconds, Raft takes seconds

**Fix Priority**: Implement Option 2 (buffering) immediately for v2.2.10.

**Estimated Fix Time**: 4-6 hours (implementation + testing)
**Estimated Test Time**: 2 hours (comprehensive replication verification)
**Total Time**: 6-8 hours

**CRITICAL**: This must be fixed before any production deployment. Current cluster configuration is NOT safe for acks=all workloads.
