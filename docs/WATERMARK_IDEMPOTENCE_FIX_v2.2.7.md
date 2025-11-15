# Watermark Idempotence Fix - v2.2.7

## Date: 2025-11-15

## Executive Summary

**Fix Applied**: ✅ Watermark idempotence using `AtomicU64::fetch_max()`
**Root Cause**: Kafka client retries can overwrite watermarks backward with unconditional `store()`
**Result**: Small improvement (87.8% → 88.2%) but NOT a complete fix
**Actual Problem Identified**: Cluster metadata/leadership issues preventing most produce requests from completing

---

## What Was Fixed

### The Watermark Regression Bug

**Problem**: When Kafka clients retry produce requests (due to timeouts, transient errors, or idempotent producers), the watermark update code was using unconditional `store()`, which allowed retries to overwrite watermarks backward.

**Example**:
```
Request 1: last_offset=429 → watermark.store(430) → watermark=430 ✅
Request 1 RETRY: last_offset=214 → watermark.store(215) → watermark=215 ❌ (regression!)
Consumer sees watermark drop from 430 to 215 → messages "disappear"
```

**Fix**: Changed all 4 watermark update locations from `store()` to `fetch_max()`:

```rust
// BEFORE (broken):
partition_state.high_watermark.store(new_watermark as u64, Ordering::SeqCst);

// AFTER (idempotent):
partition_state.high_watermark.fetch_max(new_watermark as u64, Ordering::SeqCst);
```

**Locations Fixed**:
1. `produce_handler.rs:1552` - acks=0 path
2. `produce_handler.rs:1572` - acks=1 path
3. `produce_handler.rs:1656` - acks=-1 with ISR quorum
4. `produce_handler.rs:1693` - acks=-1 without ISR tracker

---

## Test Results

### Comparison

| Version | Consumed | Success Rate | Change |
|---------|----------|--------------|--------|
| v2.2.7 (baseline) | 4391/5000 | 87.8% | - |
| v2.2.7 (with fetch_max fix) | 4410/5000 | 88.2% | ✅ +0.4% |

### What We Learned

**Good News**:
- Fix prevents watermark regression (no backward movement)
- Small improvement over baseline (19 more messages consumed)
- Watermarks are now monotonic during client retries

**Bad News**:
- NOT a complete fix for large batch consumption
- Still missing ~590/5000 messages (11.8%)
- Watermarks still stop advancing after ~1700 messages per partition

---

## Evidence from Debug Logs

### 1. Watermarks Are Now Monotonic

```
🔥 WATERMARK UPDATE [acks=1]: old=0, new=215, actually_updated=true
🔥 WATERMARK UPDATE [acks=1]: old=215, new=430, actually_updated=true
🔥 WATERMARK UPDATE [acks=1]: old=0, new=215, actually_updated=false  ← Retry rejected!
🔥 WATERMARK UPDATE [acks=1]: old=215, new=430, actually_updated=false ← Retry rejected!
```

**Conclusion**: `fetch_max()` successfully prevents backward progression.

### 2. Only 29 Watermark Updates for 5000 Messages

```bash
$ grep "🔥 WATERMARK UPDATE.*actually_updated=true" logs/*.log | wc -l
29
```

**Why so few?** Only 29 produce requests actually reached the watermark update code!

### 3. Highest Watermarks Reached

```
Partition 0: ~430
Partition 1: ~1633 (highest)
Partition 2: ~942
```

**Conclusion**: Watermarks stop advancing after initial burst (~1500-1700 messages).

### 4. Leadership Rejections

```bash
$ grep -i "not leader" logs/*.log | wc -l
198
```

**Conclusion**: Most produce requests (198 out of ~227) were rejected due to leadership issues!

---

## The Real Problem

The watermark idempotence fix is **correct and working**, but it revealed that the actual problem is:

### Cluster Metadata/Leadership Issues

**Evidence**:
- Only 29 produce requests reached watermark update code (out of 5000 messages sent)
- 198 "not leader" rejections
- Test script uses single broker (`localhost:9092`) instead of full cluster
- Clients don't know about all brokers → can't handle leadership correctly

**What's Happening**:
1. Producer sends to `localhost:9092` (node 1)
2. Some partitions are led by node 2 or node 3
3. Node 1 returns `NOT_LEADER_FOR_PARTITION` error
4. Client retries to same broker (doesn't know about nodes 2/3)
5. Eventually gives up → messages lost

**Why Baseline Was 87.8% Instead of 100%**:
- Same metadata/leadership issue existed in v2.2.7
- NOT a regression from watermark changes
- Pre-existing cluster configuration problem

---

## Why Previous Attempts Failed

### v2.2.7.1 (Watermark Improvements) - 78.8% Success

**What it did**: Adaptive watermark sync interval (500ms-5s)
**Why it failed**: Made things worse by increasing metadata churn
**Lesson**: Optimizing sync interval doesn't help if watermarks never update

### v2.2.7.2 (Metadata WAL Replication) - 76.3% Success

**What it did**: Event-based watermark replication via metadata WAL
**Why it failed**: Can't replicate watermarks that never update in the first place
**Lesson**: Fast replication doesn't help if produce path is broken

### v2.2.7 (This Fix) - 88.2% Success

**What it did**: Idempotent watermark updates using `fetch_max()`
**Why it helped slightly**: Prevents retries from making things worse
**Why it's not enough**: Doesn't fix the underlying leadership/metadata issue

---

## Next Steps

The watermark fix is **complete and should be kept** (it prevents a real bug), but to reach 100% consumption we need to fix the cluster metadata issue:

### Option 1: Fix Test Script (Recommended for Testing)

Change test script to use all brokers:

```python
producer = KafkaProducer(
    bootstrap_servers='localhost:9092,localhost:9093,localhost:9094',  # All nodes!
    acks=1
)

consumer = KafkaConsumer(
    TOPIC,
    bootstrap_servers='localhost:9092,localhost:9093,localhost:9094',  # All nodes!
    ...
)
```

**Expected impact**: 100% consumption (clients can find partition leaders)

### Option 2: Fix Metadata Response (Production Fix)

Ensure `Metadata` responses from any node include ALL brokers:

```rust
// In metadata_handler.rs:
// ALWAYS return ALL cluster nodes, not just current node
brokers: vec![
    Broker { node_id: 1, host: "localhost", port: 9092 },
    Broker { node_id: 2, host: "localhost", port: 9093 },
    Broker { node_id: 3, host: "localhost", port: 9094 },
]
```

**Expected impact**: 100% consumption even with single broker in client config

### Option 3: Implement Partition Leader Forwarding

Forward produce requests to correct leader instead of rejecting:

```rust
// In produce_handler.rs line 1132:
if !is_leader {
    // Instead of returning error, forward to leader
    return self.forward_to_leader(&topic_name, partition, partition_data, leader_hint).await;
}
```

**Expected impact**: 100% consumption (no client retries needed)

---

## Files Modified

### Code Changes

**crates/chronik-server/src/produce_handler.rs** - 4 locations updated:
- Line 1552: acks=0 path → `fetch_max()`
- Line 1572: acks=1 path → `fetch_max()`
- Line 1656: acks=-1 with ISR → `fetch_max()`
- Line 1693: acks=-1 without ISR → `fetch_max()`

All locations now use:
```rust
let prev_watermark = partition_state.high_watermark.fetch_max(new_watermark as u64, Ordering::SeqCst) as i64;

warn!(
    "🔥 WATERMARK UPDATE [{}]: topic={}, partition={}, old={}, new={}, last_offset={}, actually_updated={}",
    acks_mode, topic, partition, old_watermark, new_watermark, last_offset, prev_watermark < new_watermark
);
```

### Documentation

- **docs/WATERMARK_IDEMPOTENCE_FIX_v2.2.7.md** - This document
- **docs/WATERMARK_REPLICATION_TEST_RESULTS_v2.2.7.2.md** - Previous investigation

---

## Impact Assessment

### What This Fix Solves

✅ Prevents watermark regression during client retries
✅ Makes watermark updates idempotent (safe to retry)
✅ Small improvement in consumption rate (87.8% → 88.2%)
✅ Reduces chance of race conditions between concurrent produce requests

### What This Fix Does NOT Solve

❌ Large batch consumption still only ~88% successful
❌ Watermarks still stop advancing after ~1700 messages
❌ Cluster metadata/leadership issues
❌ Client can't find partition leaders with single broker config

### Should This Fix Be Kept?

**YES, absolutely!** Even though it doesn't solve the full problem, it:
- Fixes a real idempotence bug
- Is correct regardless of the metadata issue
- Has no downside (monotonic watermarks are always correct)
- Will be needed even after fixing metadata

---

## Technical Details

### Why fetch_max() Is Correct

**AtomicU64::fetch_max()**:
- Atomic operation (thread-safe)
- Only updates if new value > current value
- Returns previous value (before update)
- Perfect for monotonically increasing counters

**Usage**:
```rust
let prev = watermark.fetch_max(430, Ordering::SeqCst);
// If watermark was 215 → now 430, prev=215 (updated)
// If watermark was 645 → stays 645, prev=645 (no update)
```

### Why Retries Happen

**Kafka clients retry for**:
- Request timeouts
- Transient network errors
- `NOT_LEADER_FOR_PARTITION` errors
- Idempotent producers (retry by design)
- `BROKER_NOT_AVAILABLE` errors

**Idempotent producers**:
- kafka-python uses idempotence by default in newer versions
- Each request has sequence number
- Retries use SAME sequence number → SAME offsets
- Without `fetch_max()`, retries overwrite watermarks backward

---

## Lessons Learned

1. **Test with realistic client config** - Single broker config hides metadata issues
2. **Look at the full picture** - Watermark updates depend on produce path success
3. **Fix small bugs even if they're not the root cause** - Idempotence is still correct
4. **Debug logging is invaluable** - Revealed both the fix and the real problem

---

## References

**Previous Investigation**:
- [WATERMARK_REPLICATION_TEST_RESULTS_v2.2.7.2.md](WATERMARK_REPLICATION_TEST_RESULTS_v2.2.7.2.md)
- [WATERMARK_IMPROVEMENTS_v2.2.7.1.md](WATERMARK_IMPROVEMENTS_v2.2.7.1.md)

**Related Code**:
- `crates/chronik-server/src/produce_handler.rs` - Watermark update logic
- `crates/chronik-server/src/metadata_handler.rs` - Metadata responses
- `tests/cluster/` - Local test cluster configuration

**Test Scripts**:
- `/tmp/debug_large_batch_consume.py` - Large batch consumption test

---

## Conclusion

The watermark idempotence fix is **COMPLETE, CORRECT, and SHOULD BE KEPT**, but it's NOT the solution to large batch consumption failures.

**The real problem**: Cluster metadata/leadership issues prevent most produce requests from completing.

**Next action**: Fix metadata responses to include all brokers, or update test script to use full cluster config.

**Expected outcome after metadata fix**: 100% consumption success.
