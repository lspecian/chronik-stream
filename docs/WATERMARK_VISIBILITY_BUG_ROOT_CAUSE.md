# High Watermark Visibility Bug - Root Cause Analysis

**Date**: 2025-11-20
**Priority**: P0 - CRITICAL
**Status**: Root cause identified, fix required
**Version**: v2.2.9+

---

## Executive Summary

**Problem**: ProduceHandler writes 1.2M messages to WAL but consumers see 0 messages (100% data loss). High watermarks show as 0 even though data exists.

**Root Cause**: ListOffsets handler calculates high watermarks from **segments** instead of reading from **metadata store's partition_offsets**.

**Impact**: All produced messages invisible to consumers until WAL flushes to segments (can be minutes or never in some workloads).

---

## Evidence

### Test Scenario
```bash
# Benchmark produces 1.2M messages successfully
./target/release/chronik-bench --concurrency 128 --duration 30s --acks 1
# Reports: 1,247,197 messages produced @ 41,573 msg/sec

# Consumer sees ZERO messages
python3 -c "from kafka import KafkaConsumer; ..."
# Returns: 0 messages, end_offsets={partition=0: 0, partition=1: 0, partition=2: 0}
```

### Data Verification
```bash
# Data exists in WAL files
$ ls -lh tests/cluster/data/node2/wal/perf-test-clean/*/wal_*.log
-rw-r--r-- 1 251M wal_0_0.log  # Partition 0
-rw-r--r-- 1 251M wal_1_0.log  # Partition 1
-rw-r--r-- 1 251M wal_2_0.log  # Partition 2
# Total: 753 MB of data written to WAL

# But segments don't exist yet
$ ls tests/cluster/data/node2/segments/perf-test-clean/
# Empty - no segments flushed
```

### Watermark State
```bash
# ProduceHandler logs show watermarks updated
2025-11-20T... WARN 🔥 WATERMARK UPDATE [acks=1]: topic=perf-test-clean, partition=0,
  old=0, new=415724, last_offset=415723, actually_updated=true

2025-11-20T... WARN 📡 Emitted HighWatermarkUpdated: perf-test-clean-0 => 415724, subscribers=1

# But ListOffsets returns 0
2025-11-20T... INFO ListOffsets: Returning high watermark 0 for perf-test-clean-0
```

---

## Root Cause Analysis

### Write Path (ProduceHandler) - CORRECT

**File**: `crates/chronik-server/src/produce_handler.rs`

**Flow for acks=1** (lines 1789-1801):
```rust
// 1. Update in-memory high watermark
let new_watermark = (last_offset + 1) as i64;
partition_state.high_watermark.fetch_max(new_watermark as u64, Ordering::SeqCst);

warn!("🔥 WATERMARK UPDATE [acks=1]: topic={}, partition={}, new={}",
      topic, partition, new_watermark);

// 2. Emit watermark event
self.emit_watermark_event(topic, partition, new_watermark);
```

**Event Emission** (lines 750-762):
```rust
fn emit_watermark_event(&self, topic: &str, partition: i32, offset: i64) {
    if let Some(ref event_bus) = self.event_bus {
        let subscriber_count = event_bus.publish(MetadataEvent::HighWatermarkUpdated {
            topic: topic.to_string(),
            partition,
            offset,
        });
        warn!("📡 Emitted HighWatermarkUpdated: {}-{} => {}, subscribers={}",
              topic, partition, offset, subscriber_count);
    }
}
```

**Event Handler** (WalMetadataStore, lines 133-143):
```rust
MetadataEventPayload::HighWatermarkUpdated { topic, partition, new_watermark } => {
    let mut offsets = self.partition_offsets.write().await;
    let key = (topic.clone(), *partition as u32);
    if let Some((hwm, _lso)) = offsets.get_mut(&key) {
        *hwm = *new_watermark;  // ✅ Updates partition_offsets HashMap
    } else {
        offsets.insert(key, (*new_watermark, 0));
    }
    Ok(())
}
```

**Result**: `partition_offsets` HashMap contains correct watermarks (e.g., 415724 for partition 0).

---

### Read Path (ListOffsets Handler) - BROKEN

**File**: `crates/chronik-protocol/src/handler.rs`

**Current Implementation** (lines 4829-4848):
```rust
let high_watermark = if let Some(ref metadata_store) = self.metadata_store {
    // ❌ BUG: Gets segments instead of partition offsets!
    match metadata_store.list_segments(&topic.name, Some(partition.partition_index as u32)).await {
        Ok(segments) => {
            // Calculate high watermark from segment end_offset
            segments.iter()
                .map(|s| s.end_offset + 1)
                .max()
                .unwrap_or(0)  // ❌ Returns 0 when no segments exist
        }
        Err(e) => {
            tracing::warn!("Failed to get segments for {}-{}: {}",
                topic.name, partition.partition_index, e);
            0
        }
    }
} else {
    0
};

tracing::info!("ListOffsets: Returning high watermark {} for {}-{}",
    high_watermark, topic.name, partition.partition_index);
```

**Why This Fails**:
1. Data written to WAL first (for durability)
2. Segments created later via background flush (can be 30s-5min delay)
3. `list_segments()` returns empty list → watermark calculated as 0
4. Consumers see 0 even though data exists in WAL

**Correct Implementation Should Be** (using MetadataStore trait line 241):
```rust
async fn get_partition_offset(&self, topic: &str, partition: u32)
    -> Result<Option<(i64, i64)>>; // Returns (high_watermark, log_start_offset)
```

**Fixed code**:
```rust
let high_watermark = if let Some(ref metadata_store) = self.metadata_store {
    // ✅ FIX: Get watermark from partition_offsets (updated by ProduceHandler)
    match metadata_store.get_partition_offset(&topic.name, partition.partition_index as u32).await {
        Ok(Some((hwm, _lso))) => {
            hwm  // ✅ Returns actual watermark from metadata store
        }
        Ok(None) => {
            0  // Partition doesn't exist yet
        }
        Err(e) => {
            tracing::warn!("Failed to get partition offset for {}-{}: {}",
                topic.name, partition.partition_index, e);
            0
        }
    }
} else {
    0
};
```

---

## Timeline of Discovery

1. **Benchmark Test**: Produced 1.2M messages successfully (benchmark reported success)
2. **Consumption Failed**: Consumer end_offsets returned all zeros
3. **Data Verification**: Confirmed 753 MB in WAL files, no segments yet
4. **ProduceHandler Traced**: Found watermark updates + event emissions working correctly
5. **WalMetadataStore Traced**: Confirmed event handler updating partition_offsets
6. **ListOffsets Traced**: **Found bug - reading from segments instead of partition_offsets!**

---

## Impact Assessment

### Severity: P0 - CRITICAL

**Data Loss**:
- 100% of produced messages invisible to consumers until WAL flush
- WAL flush can take minutes or never happen for low-throughput topics
- Appears as silent data loss to users

**Affected Operations**:
- `consumer.end_offsets()` - Returns 0
- `consumer.position()` - Returns 0
- Kafka Fetch requests - No messages returned
- ListOffsets API - Returns 0

**Not Affected**:
- ProduceRequest - Works correctly, data persisted to WAL
- Producer acknowledgments - Correct offsets returned
- Data durability - WAL ensures no data loss on crash
- Consumption after segment flush - Would work correctly (once segments exist)

### When Bug Manifests

**Always Fails**:
- Fresh topics with no segments yet
- High-throughput benchmarks (produce faster than segment flush)
- Low-throughput topics (WAL never seals to trigger segment flush)

**Eventually Works**:
- After WAL flush creates segments (30s-5min delay depending on config)
- This explains why some tests pass - they wait long enough for segments

---

## Fix Required

**File**: `crates/chronik-protocol/src/handler.rs`
**Function**: `handle_list_offsets()` (lines 4829-4848)

**Change**:
```diff
-match metadata_store.list_segments(&topic.name, Some(partition.partition_index as u32)).await {
-    Ok(segments) => {
-        segments.iter()
-            .map(|s| s.end_offset + 1)
-            .max()
-            .unwrap_or(0)
-    }
+match metadata_store.get_partition_offset(&topic.name, partition.partition_index as u32).await {
+    Ok(Some((hwm, _lso))) => {
+        hwm
+    }
+    Ok(None) => {
+        0  // Partition not created yet
+    }
```

**Rationale**:
- `get_partition_offset()` reads from `partition_offsets` HashMap
- `partition_offsets` is updated immediately by ProduceHandler via event bus
- No dependency on segment flush timing
- Source of truth for high watermarks

---

## Testing Plan

### Test 1: Immediate Consumption (Before Fix - FAILS)
```bash
# Produce messages
./target/release/chronik-bench --message-count 1000 --acks 1

# Immediately consume (don't wait for segment flush)
python3 -c "from kafka import KafkaConsumer; ..."
# Expected (before fix): 0 messages ❌
# Expected (after fix): 1000 messages ✅
```

### Test 2: High-Throughput Benchmark (Before Fix - FAILS)
```bash
# Produce 1M messages fast (faster than segment flush)
./target/release/chronik-bench --concurrency 128 --duration 30s

# Consume immediately
python3 consume_all.py
# Expected (before fix): 0 messages ❌
# Expected (after fix): ~1M messages ✅
```

### Test 3: End-to-End Latency (Before Fix - HIGH)
```bash
# Produce one message
echo "test" | kafka-console-producer --topic test

# Consume immediately (measure latency)
kafka-console-consumer --topic test --from-beginning --max-messages 1
# Expected (before fix): Timeout or 30s-5min delay ❌
# Expected (after fix): < 100ms ✅
```

---

## Related Issues

**Similar Historical Bugs**:
- v2.2.7: High watermark replication lag (fixed via event bus)
- v2.2.6: Watermark idempotence issues (fixed via fetch_max)
- v2.2.5: Watermark overwrite bugs (fixed via last-write-wins)

**Root Architectural Issue**:
- Chronik has **two sources of truth** for high watermarks:
  1. `partition_offsets` in WalMetadataStore (updated by ProduceHandler)
  2. Segment metadata (updated during WAL flush)

- ListOffsets was reading from (2) instead of (1)
- Correct answer: Always read from (1) - it's the authoritative source

---

## Conclusion

This is a **critical P0 bug** causing 100% data loss for all consumption until segments are flushed. The fix is simple (change 1 line) but the impact is severe.

**Estimated Fix Time**: 5 minutes (change + compile)
**Estimated Test Time**: 10 minutes (verify with benchmark + consumption)
**Estimated Total Time**: 15 minutes

**Priority**: Fix immediately, this blocks all real-world usage.
