# Watermark Overwrite Bug - Root Cause Found

## Date: 2025-11-15

## Executive Summary

**Root Cause IDENTIFIED**: Watermarks are being unconditionally overwritten when clients retry produce requests, causing watermarks to reset backward instead of monotonically increasing.

**Bug**: `partition_state.high_watermark.store(new_watermark)` - unconditional write
**Fix**: `partition_state.high_watermark.fetch_max(new_watermark)` - only update if new value is higher

**Impact**: 12-24% message loss in large batch consumption due to watermark regressing when retries occur.

---

## Evidence from Debug Logs

### Partition 1 (Node 2) - Showing Watermark Resets

```
🎯 PRODUCE REQUEST: partition=1, acks=1, base_offset=0, last_offset=214
🔥 WATERMARK UPDATE [acks=1]: partition=1, old=0, new=215

🎯 PRODUCE REQUEST: partition=1, acks=1, base_offset=215, last_offset=429
🔥 WATERMARK UPDATE [acks=1]: partition=1, old=215, new=430

🎯 PRODUCE REQUEST: partition=1, acks=1, base_offset=0, last_offset=214  ← RETRY!
🔥 WATERMARK UPDATE [acks=1]: partition=1, old=0, new=215  ← OVERWRITE BACK TO 215!

🎯 PRODUCE REQUEST: partition=1, acks=1, base_offset=215, last_offset=429  ← RETRY!
🔥 WATERMARK UPDATE [acks=1]: partition=1, old=215, new=430

🎯 PRODUCE REQUEST: partition=1, acks=1, base_offset=0, last_offset=214  ← RETRY AGAIN!
🔥 WATERMARK UPDATE [acks=1]: partition=1, old=0, new=215  ← OVERWRITE BACK TO 215 AGAIN!
```

**Pattern**: Client retries earlier batches, and watermark gets reset backward from 430→215.

### Partition 2 (Node 3) - Same Pattern

```
🎯 PRODUCE REQUEST: partition=2, base_offset=0, last_offset=214
🔥 WATERMARK UPDATE: partition=2, old=0, new=215

🎯 PRODUCE REQUEST: partition=2, base_offset=0, last_offset=214  ← DUPLICATE
🔥 WATERMARK UPDATE: partition=2, old=0, new=215  ← STAYS AT 215

🎯 PRODUCE REQUEST: partition=2, base_offset=0, last_offset=166  ← DIFFERENT BATCH SIZE
🔥 WATERMARK UPDATE: partition=2, old=0, new=167  ← OVERWRITES TO 167

🎯 PRODUCE REQUEST: partition=2, base_offset=0, last_offset=214  ← RETRY ORIGINAL
🔥 WATERMARK UPDATE: partition=2, old=0, new=215  ← BACK TO 215
```

### Partition 0 (Node 1) - Works Correctly (No Retries)

```
🎯 PRODUCE REQUEST: partition=0, base_offset=0, last_offset=162
🔥 WATERMARK UPDATE: partition=0, old=0, new=163

🎯 PRODUCE REQUEST: partition=0, base_offset=163, last_offset=377
🔥 WATERMARK UPDATE: partition=0, old=163, new=378

🎯 PRODUCE REQUEST: partition=0, base_offset=378, last_offset=592
🔥 WATERMARK UPDATE: partition=0, old=378, new=593
...
```

**Pattern**: Partition 0 has NO retries, so watermark advances monotonically: 163→378→593→808→1023→...→1662.

---

## Root Cause Analysis

### Why Retries Happen

Kafka clients retry produce requests for various reasons:
1. **Timeout**: Client didn't get response fast enough
2. **Transient errors**: Network issues, broker unavailable
3. **Leadership changes**: Partition leader changed mid-request
4. **Idempotent producer**: Retries by default for exactly-once semantics

### Why Watermark Overwrites Cause Message Loss

1. **Producer sends batch 0-214** → Watermark advances to 215
2. **Producer sends batch 215-429** → Watermark advances to 430
3. **Consumer fetches** up to offset 430 (**sees 430 messages**)
4. **Producer RETRIES batch 0-214** → Watermark OVERWRITES to 215
5. **Consumer fetches again** → Server says high_watermark=215, consumer already at 430 → **NO NEW DATA**
6. **Consumer times out** → Only 215 messages consumed, **215 messages lost** (from 215-429)

### Code Bug Location

**File**: `crates/chronik-server/src/produce_handler.rs`

**All 4 watermark update paths have the same bug**:

#### acks=0 path (line ~1544-1545):
```rust
// BUG: Unconditional write
partition_state.high_watermark.store(new_watermark as u64, Ordering::SeqCst);
```

####acks=1 path (line ~1563-1564):
```rust
// BUG: Unconditional write
partition_state.high_watermark.store(new_watermark as u64, Ordering::SeqCst);
```

#### acks=-1 with ISR quorum (line ~1646-1647):
```rust
// BUG: Unconditional write
partition_state.high_watermark.store(new_watermark as u64, Ordering::SeqCst);
```

#### acks=-1 without ISR tracker (line ~1684-1685):
```rust
// BUG: Unconditional write
partition_state.high_watermark.store(new_watermark as u64, Ordering::SeqCst);
```

---

## The Fix

### Use AtomicU64::fetch_max Instead of store

**Current (WRONG)**:
```rust
partition_state.high_watermark.store(new_watermark as u64, Ordering::SeqCst);
```

**Fixed (CORRECT)**:
```rust
partition_state.high_watermark.fetch_max(new_watermark as u64, Ordering::SeqCst);
```

**Why fetch_max?**
- Only updates if new_watermark > current watermark
- Prevents backward progression
- Atomic operation (lock-free, thread-safe)
- Idempotent (safe to call multiple times with same value)

### Example Behavior After Fix

**Before (with retries)**:
```
Batch 0-214   → watermark: 0 → 215  ✅ Updated
Batch 215-429 → watermark: 215 → 430  ✅ Updated
Batch 0-214 (retry) → watermark: 430 → 215  ❌ OVERWROTE (BUG!)
```

**After (with retries)**:
```
Batch 0-214   → watermark: 0 → 215  ✅ Updated (215 > 0)
Batch 215-429 → watermark: 215 → 430  ✅ Updated (430 > 215)
Batch 0-214 (retry) → watermark: 430 → 430  ✅ NO CHANGE (215 < 430, ignored)
```

---

## Code Changes Required

Apply fix to all 4 watermark update locations:

### 1. acks=0 path (~line 1544)

```diff
- partition_state.high_watermark.store(new_watermark as u64, Ordering::SeqCst);
+ partition_state.high_watermark.fetch_max(new_watermark as u64, Ordering::SeqCst);
```

### 2. acks=1 path (~line 1563)

```diff
- partition_state.high_watermark.store(new_watermark as u64, Ordering::SeqCst);
+ partition_state.high_watermark.fetch_max(new_watermark as u64, Ordering::SeqCst);
```

### 3. acks=-1 with ISR quorum (~line 1646)

```diff
- partition_state.high_watermark.store(new_watermark as u64, Ordering::SeqCst);
+ partition_state.high_watermark.fetch_max(new_watermark as u64, Ordering::SeqCst);
```

### 4. acks=-1 without ISR tracker (~line 1684)

```diff
- partition_state.high_watermark.store(new_watermark as u64, Ordering::SeqCst);
+ partition_state.high_watermark.fetch_max(new_watermark as u64, Ordering::SeqCst);
```

---

## Testing Plan

### 1. Unit Test for Retry Behavior

```rust
#[tokio::test]
async fn test_watermark_idempotence_with_retries() {
    // Produce batch 0-214 → watermark = 215
    // Produce batch 215-429 → watermark = 430
    // Retry batch 0-214 → watermark should STAY at 430 (not regress to 215)

    let handler = create_test_handler().await;

    // First produce
    handler.produce(..., base_offset=0, last_offset=214).await;
    assert_eq!(get_watermark(), 215);

    // Second produce
    handler.produce(..., base_offset=215, last_offset=429).await;
    assert_eq!(get_watermark(), 430);

    // Retry first produce (should NOT regress)
    handler.produce(..., base_offset=0, last_offset=214).await;
    assert_eq!(get_watermark(), 430);  // ← This will FAIL before fix, PASS after
}
```

### 2. Integration Test - Large Batch Consumption

**Expected result after fix**:
- Consumed: **5000/5000 (100%)** ← UP from 87.8%
- Missing: **0 messages (0%)** ← DOWN from 12.2%

```bash
# Rebuild with fix
cargo build --release --bin chronik-server

# Restart cluster
./tests/cluster/stop.sh && ./tests/cluster/start.sh

# Run large batch test
python3 /tmp/debug_large_batch_consume.py

# Should now see:
# Consumed:     5000/5000 (100%)
# Missing:      0 messages (0%)
```

### 3. Verify Watermark Monotonicity in Logs

```bash
# Check watermark updates for partition 1
grep "🔥 WATERMARK UPDATE.*partition=1" tests/cluster/logs/*.log

# Should see watermarks ALWAYS increasing or staying the same (never decreasing):
# old=0, new=215
# old=215, new=430
# old=430, new=430  ← Retry, ignored (not old=430, new=215!)
# old=430, new=645
# ...
```

---

## Why Previous Attempts Failed

### v2.2.7.1: Watermark Monitoring Improvements

**Approach**: Faster background watermark sync (500ms-5s adaptive)
**Result**: 78.8% success (**-9% WORSE**)
**Why failed**: Faster sync doesn't help if watermarks regress due to retries

### v2.2.7.2: Metadata WAL Event Replication

**Approach**: Replicate watermarks via metadata WAL events
**Result**: 76.3% success (**-11.5% WORSE**)
**Why failed**: Replicating more watermark updates actually made it WORSE because more retry events got replicated, causing more resets!

### This Fix: Watermark Idempotence

**Approach**: Use fetch_max to prevent backward watermark progression
**Expected result**: 100% success (**+12.2% BETTER**)
**Why it will work**: Retries can't cause watermark regression anymore

---

## Impact Analysis

### Performance Impact

**None** - `fetch_max` is the same cost as `store` (both atomic operations)

### Behavioral Changes

**Before fix**:
- Watermark can regress backward during retries
- Consumers see watermark "jump around"
- Some messages become unavailable intermittently

**After fix**:
- Watermark monotonically increases (or stays same)
- Consumers always see consistent or advancing watermark
- All messages remain available once written

### Kafka Compatibility

**fetch_max behavior matches Kafka semantics**:
- Kafka high watermark is monotonic (never decreases)
- Retries/duplicates don't affect watermark
- Idempotent producer guarantees exactly-once without watermark regression

---

## Lessons Learned

### 1. Measure Before Optimizing

Added comprehensive debug logging FIRST to identify the actual bug:
```rust
warn!("🔥 WATERMARK UPDATE [acks=1]: topic={}, partition={}, old={}, new={}, last_offset={}",
      topic, partition, old_watermark, new_watermark, last_offset);
```

This revealed the watermark resets that previous attempts missed.

### 2. Look for Retries in Distributed Systems

Retries are NORMAL in Kafka:
- Network timeouts
- Leadership changes
- Idempotent producers
- Transient errors

Code must handle retries gracefully (idempotent operations).

### 3. Atomic Operations Matter

Using `store()` for monotonic values is a bug. Always use:
- `fetch_max()` for high watermarks (monotonically increasing)
- `fetch_min()` for low watermarks/offsets (monotonically decreasing)
- `compare_exchange()` for complex state transitions

### 4. Test with Real Clients

Real Kafka clients (kafka-python) send retries under load.
Unit tests with perfect conditions don't reveal these bugs.
Always test with actual client libraries at scale.

---

## Next Steps

1. **Apply fix** to all 4 watermark update locations
2. **Rebuild** and test with large batch consumption
3. **Verify** 100% consumption success
4. **Document** in changelog as "Critical bug fix: Watermark idempotence"
5. **Add** unit test for retry behavior

---

## References

**Debug Logs**:
- `tests/cluster/logs/node1.log` - Partition 0 (no retries, works correctly)
- `tests/cluster/logs/node2.log` - Partition 1 (retries, shows bug)
- `tests/cluster/logs/node3.log` - Partition 2 (retries, shows bug)

**Test Scripts**:
- `/tmp/debug_large_batch_consume.py` - 5000 message test
- `/tmp/debug_partition_consumption.py` - Per-partition analysis

**Previous Investigation Docs**:
- [WATERMARK_REPLICATION_TEST_RESULTS_v2.2.7.2.md](WATERMARK_REPLICATION_TEST_RESULTS_v2.2.7.2.md)
- [LARGE_BATCH_FIX_INVESTIGATION_v2.2.7.2.md](LARGE_BATCH_FIX_INVESTIGATION_v2.2.7.2.md)

**Code Files**:
- `crates/chronik-server/src/produce_handler.rs` - Lines 1544, 1563, 1646, 1684
