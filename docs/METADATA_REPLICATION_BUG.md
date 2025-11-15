# Metadata WAL Replication Bug - Investigation Report

## Date: 2025-11-13 (Session 7-8)

## Executive Summary

**Issue**: Metadata WAL replication infrastructure exists and is properly wired. Events are emitted correctly on the Raft leader (Node 1). `WalReplicationManager.replicate_partition()` IS called but does NOT actually send frames for `__chronik_metadata` topic to followers. This causes followers to lack partition replica information, leading to ISR quorum timeouts with acks=all under high concurrency (128 threads).

**Impact**:
- Low concurrency (1 thread): Works fine (16ms completion) - discovery loop completes in time
- High concurrency (128 threads): All produce requests timeout after 5 seconds - requests arrive before metadata replication completes

**Status (Session 8)**: DEBUG logging enabled. Confirmed `replicate_partition()` IS called for metadata commands but frames are NOT sent. Need to investigate internal logic of `replicate_partition()` to find why sends are silently skipped.

## Problem Chain

### What's Working ✅

1. **Metadata WAL Replication Infrastructure**: Fully implemented and wired
   - `MetadataWalReplicator` exists and event listener is running
   - `MetadataEventBus` properly emits events when partitions are created
   - Event listener receives `AssignPartition` events: `📥 RECV: AssignPartition event for bench-test-0 => [1, 2, 3]`
   - `handle_event()` calls `replicate()` method (line 192 in metadata_wal_replication.rs)

2. **Follower Registration**: `__chronik_metadata` topic has registered followers
   - Node 2 (leader) registered: `["localhost:9291", "localhost:9293"]` (Nodes 1 and 3)
   - Registration happens immediately at startup in integrated_server.rs line 551-556

3. **WalReceiver Metadata Handling**: Followers have code to apply metadata commands
   - Special handling for `__chronik_metadata` topic at wal_replication.rs line 1393-1414
   - `handle_metadata_wal_record()` deserializes and applies commands to Raft state (line 1538-1587)

4. **TCP Connections**: Connections between nodes are established
   - Node 2 connected to Node 1 at `21:30:43.285` (365ms after startup)
   - Node 2 connected to Node 3 at `21:30:51.294` (8 seconds after startup)

### What's NOT Working ❌

1. **Metadata Frames Not Sent**: Node 2 (leader) never sends metadata frames to followers
   - Checked logs: `grep "Sent WAL record to" node2.log` returns NO results
   - No "Failed to send WAL record" errors either
   - This means `WalReplicationManager.replicate_partition()` is called but doesn't actually send data

2. **Followers Don't Receive Metadata**: Node 1 and Node 3 never receive `AssignPartition` commands
   - Checked Node 1 logs: NO "METADATA✓ Replicated" messages for bench-test partitions
   - Checked Node 1 logs: NO "Applied metadata event" messages for bench-test
   - Node 1 metadata store shows: `assignments_count=0` for bench-test topic

3. **Discovery Loop Insufficient**: 1-second interval not fast enough for 128 concurrent threads
   - Discovery loop runs every 1 second (wal_replication.rs line 790-872)
   - With 128 threads producing immediately, requests arrive within milliseconds of topic creation
   - By the time discovery completes (1000ms), produce requests have already timed out (5000ms)

## Root Cause Analysis

### The Bug Location

**File**: `crates/chronik-server/src/metadata_wal_replication.rs`
**Method**: `replicate()` at lines 81-115

```rust
pub async fn replicate(&self, cmd: &MetadataCommand, offset: i64) -> Result<()> {
    // Serialize command to bytes
    let data = bincode::serialize(cmd)
        .context("Failed to serialize metadata command for replication")?;

    debug!(
        "Replicating metadata command to followers (offset={}): {:?}",
        offset,
        cmd
    );

    // Use existing WalReplicationManager with special topic name
    self.replication_mgr.replicate_partition(
        self.wal.topic_name().to_string(),  // "__chronik_metadata"
        self.wal.partition(),                // 0
        offset,
        offset,                              // leader_high_watermark
        data,                                // Vec<u8>
    ).await;

    debug!(
        "Metadata command queued for replication (topic='{}', partition={}, offset={})",
        self.wal.topic_name(),
        self.wal.partition(),
        offset
    );

    Ok(())
}
```

**Problem**:
- Method calls `replication_mgr.replicate_partition()` (line 99-105)
- Method returns `Ok(())` with NO errors
- BUT: No frames are actually sent to followers (verified by log analysis)
- Debug messages at lines 86-90 and 107-112 are NOT logged (log level is INFO, not DEBUG)

### Hypothesis

`WalReplicationManager.replicate_partition()` may be:

1. **Checking partition_followers map**: If map is empty or doesn't have `__chronik_metadata-0` entry, it may silently skip sending
2. **Fire-and-forget async**: Returns immediately without waiting, so errors are never propagated
3. **Connection issue**: TCP connections exist but sender workers may not be spawned for metadata topic
4. **Missing sender workers**: Each partition needs a sender worker spawned, but metadata topic may not have one

### Evidence from Logs

**Node 1 (follower) attempting to replicate metadata locally** (should NOT happen - only leader should replicate):
```
21:30:43.195120 [DEBUG] 🔍 replicate_partition ENTRY: __chronik_metadata-0 offset=0 data_len=30
21:30:43.195125 [DEBUG] No replicas found for __chronik_metadata-0, falling back to replicate_serialized
```

This suggests `WalReplicationManager` is checking for registered followers and finding NONE, even though we registered them!

**Node 2 (leader) has registered followers**:
```
21:30:43.285013 [INFO] ✓ Registered immediate followers for __chronik_metadata-0: ["localhost:9291", "localhost:9293"] (was: None)
```

**Node 2 metadata events are emitted**:
```
21:31:27.143311 [INFO] 📥 RECV: AssignPartition event for bench-test-0 => [1, 2, 3], offset=282
21:31:27.144716 [INFO] 📥 RECV: AssignPartition event for bench-test-1 => [2, 3, 1], offset=284
21:31:27.146183 [INFO] 📥 RECV: AssignPartition event for bench-test-2 => [3, 1, 2], offset=286
```

**But NO replication attempts logged on Node 2**:
```
# This should show "Sent WAL record to localhost:9291" but shows NOTHING:
grep "Sent WAL record to\|Failed to send" node2.log | wc -l
0
```

## Session 8 Findings (DEBUG Logging Enabled)

### What We Discovered

1. **✅ Node 1 is the Raft leader** - Confirmed via logs: `Initializing Raft partition metadata for topic 'bench-test' (3 partitions) - this node is Raft leader`

2. **✅ Events ARE emitted on Node 1**:
   ```
   21:53:13.532426 📥 RECV: AssignPartition event for bench-test-0 => [1, 2, 3], offset=172
   21:53:13.533336 📥 RECV: SetPartitionLeader event for bench-test-0 => 1, offset=173
   21:53:13.534247 📥 RECV: AssignPartition event for bench-test-1 => [2, 3, 1], offset=174
   ```

3. **✅ `replicate_partition()` IS called**:
   ```
   21:53:13.532446 [DEBUG] 🔍 replicate_partition ENTRY: __chronik_metadata-0 offset=172 data_len=58
   21:53:13.533350 [DEBUG] 🔍 replicate_partition ENTRY: __chronik_metadata-0 offset=173 data_len=34
   21:53:13.534265 [DEBUG] 🔍 replicate_partition ENTRY: __chronik_metadata-0 offset=174 data_len=58
   ```

4. **❌ BUT: Frames are NOT sent** - NO "Sent WAL record to" messages in Node 1 logs for `__chronik_metadata` topic

### Root Cause Narrowed Down

The problem is INSIDE `WalReplicationManager.replicate_partition()` implementation. The method:
- DOES get called (confirmed via DEBUG logs)
- DOES enter the function (`replicate_partition ENTRY` message)
- Does NOT actually send frames to followers (no "Sent WAL record" messages)
- Must be silently skipping the send operation

**Hypothesis**: The method is likely checking the `partition_followers` map for `__chronik_metadata-0` and finding no entries, causing it to skip sending frames.

### Next Steps for Debugging

**CRITICAL NEXT STEP**: Investigate `WalReplicationManager.replicate_partition()` implementation to find what condition causes it to skip sending. Likely candidates:
1. Checking `partition_followers.get((__chronik_metadata, 0))` and finding None
2. Missing sender worker for metadata topic/partition
3. Connection manager not handling metadata topic correctly

### 2. Check Debug Messages

After running with DEBUG logging, check for these specific messages:

**On Node 2 (leader)**:
```bash
grep "Replicating metadata command to followers" tests/cluster/logs/node2.log
grep "Metadata command queued for replication" tests/cluster/logs/node2.log
grep "🔍 replicate_partition ENTRY.*__chronik_metadata" tests/cluster/logs/node2.log
grep "partition_followers.*__chronik_metadata" tests/cluster/logs/node2.log
```

**Expected to find**:
- "Replicating metadata command to followers" - confirms replicate() was called
- "Metadata command queued for replication" - confirms replicate() completed
- "🔍 replicate_partition ENTRY" - confirms WalReplicationManager method entry
- Sender worker messages for `__chronik_metadata` topic

### 3. Verify Sender Workers

Check if sender workers are spawned for metadata topic:

```bash
grep "Spawning sender worker\|sender worker.*__chronik_metadata" tests/cluster/logs/node2.log
```

### 4. Check Connection Manager

Verify connection manager is handling metadata topic correctly:

```bash
grep "connection_manager.*__chronik_metadata\|Connecting to follower" tests/cluster/logs/node2.log
```

### 5. Investigate replicate_partition() Logic

Review `WalReplicationManager.replicate_partition()` implementation to understand why it's not sending frames:

- Check if it validates topic name before sending
- Check if it requires partition_followers entry before sending
- Check if sender workers exist for the topic/partition
- Check if there's a special case that skips metadata replication

## Temporary Workaround

Until the WalReplicationManager bug is fixed, add a 1-second sleep after topic creation to allow discovery loop to complete:

**File**: `crates/chronik-server/src/produce_handler.rs`
**Location**: After partition creation completes (around line 2600)

```rust
// v2.2.7 TEMPORARY: Wait for discovery loop to complete
// TODO: Remove once metadata WAL replication is fixed
if self.cluster_config.is_some() {
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
    info!("Waited 1s for partition discovery/metadata replication");
}
```

This ensures:
- Discovery loop has time to run (every 1 second)
- Followers have partition replica information
- Produce requests don't arrive before metadata is available

## Related Files

### Key Implementation Files

1. **crates/chronik-server/src/metadata_wal_replication.rs** (lines 81-115)
   - `MetadataWalReplicator.replicate()` method
   - Calls `WalReplicationManager.replicate_partition()` but frames not sent

2. **crates/chronik-server/src/wal_replication.rs**
   - Line 290: `register_partition_followers()` - registers followers for topics
   - Line 790-872: Discovery loop - runs every 1 second
   - Line 1393-1414: Special handling for `__chronik_metadata` topic in WalReceiver
   - Line 1538-1587: `handle_metadata_wal_record()` - applies metadata commands on followers

3. **crates/chronik-server/src/integrated_server.rs** (lines 544-557)
   - Registers followers for `__chronik_metadata` topic immediately at startup

4. **crates/chronik-server/src/raft_metadata_store.rs** (lines 1144-1198)
   - `assign_partition_with_full_replicas()` - emits events for metadata replication

### Test Files

1. **tests/cluster/bench_test.py**
   - 128 concurrent threads, 256-byte messages, 30 seconds
   - Reproduces the timeout issue consistently

2. **tests/cluster/test_acks_all_fix.py**
   - 1 thread, simple test
   - Works fine (16ms completion)

## Architecture Context

### Two Replication Systems

Chronik has TWO DISTINCT replication systems:

1. **Raft Consensus** (synchronous, for cluster membership)
   - Used for: Node addition/removal, leader election
   - Method: `propose()` - blocks until majority ACK
   - Latency: 10-50ms
   - **WARNING**: Never call from request handlers!

2. **WAL-Based Replication** (asynchronous, for data + metadata)
   - Used for: Message data, partition assignments, ISR updates
   - Method: Fire-and-forget async replication
   - Latency: 1-2ms
   - Safe to use in hot path
   - **BUG**: Metadata replication not actually sending frames!

The fix must use #2 (WAL-based) for metadata, not #1 (Raft consensus).

### Metadata Replication Flow (INTENDED)

```
Leader (Node 2):
1. RaftMetadataStore.assign_partition_with_full_replicas() called
2. Emits MetadataEvent::AssignPartition via EventBus
3. MetadataWalReplicator event listener receives event
4. Calls replicate() with MetadataCommand
5. Calls WalReplicationManager.replicate_partition()
6. Serializes frame and queues for sender worker
7. Sender worker sends frame via TCP to followers

Followers (Node 1, Node 3):
8. WalReceiver receives frame for __chronik_metadata topic
9. Detects special topic name, calls handle_metadata_wal_record()
10. Deserializes MetadataCommand from frame data
11. Applies command to local Raft state via apply_metadata_command_direct()
12. Updates partition_assignments map with replica information
13. Discovery loop sees new assignments, registers followers for data replication
14. Produce requests can now replicate data to followers
```

### Current Broken Flow

Steps 1-5 work correctly. **Step 6 FAILS** - frames are never queued for sending. Steps 7-14 never happen because followers never receive the metadata.

## Success Criteria

The bug is fixed when:

1. ✅ Node 2 logs show: "Sent WAL record to localhost:9291" for `__chronik_metadata` topic
2. ✅ Node 1 logs show: "METADATA✓ Replicated: __chronik_metadata-0" for AssignPartition commands
3. ✅ Node 1 metadata store shows: `assignments_count=3` for bench-test topic (not 0)
4. ✅ Node 1 logs show: "✅ Accepting replication for bench-test-0: this node (1) IS in replica set [1, 2, 3]"
5. ✅ Benchmark test (128 threads) completes without timeouts in < 30 seconds

## Prompt for Next Session

```
Continue investigating the metadata WAL replication bug from Session 7.

Key findings so far:
1. Metadata WAL replication infrastructure exists and is properly wired
2. Events are emitted and received correctly
3. Followers are registered for __chronik_metadata topic
4. BUT: WalReplicationManager.replicate_partition() does NOT actually send frames to followers

Next steps:
1. Enable DEBUG logging for metadata_wal_replication and wal_replication modules
2. Rebuild and restart cluster with DEBUG logging
3. Run bench_test.py and analyze detailed logs
4. Find why replicate_partition() is not sending frames for __chronik_metadata topic
5. Fix the sender worker logic or connection manager issue
6. Test that metadata frames are actually sent and received

See docs/METADATA_REPLICATION_BUG.md for complete investigation details.
```
