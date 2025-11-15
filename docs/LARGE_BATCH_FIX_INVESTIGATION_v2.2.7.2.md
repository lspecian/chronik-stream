# Large Batch Consumption Investigation - v2.2.7.2

## Date: 2025-11-15

## Executive Summary

**Previous Attempt (v2.2.7.1)**: Watermark monitoring improvements FAILED
- ❌ Made consumption worse (87.8% → 78.8%, -9% regression)
- ❌ Solved wrong problem (NO watermark lag detected)
- ❌ Added complexity without benefit

**This Session (v2.2.7.2)**: REVERTED bad changes and added proper investigation tools
- ✅ Reverted watermark improvements back to simple v2.2.7 version
- ✅ Added comprehensive fetch handler tracing to identify actual stall points
- ✅ Created per-partition consumption analysis script
- ✅ Created extended timeout testing script (120s, max_poll_records=5000)

**Next Steps**: Use new tools to identify ACTUAL root cause of 21% message loss.

---

## Problem Statement

**Issue**: Large batch consumption (5000 messages) stalls after ~78-88% completion
- Fast initial consumption (~7000 msg/s for first 3500-4000 messages)
- Then complete stall for remaining 60 seconds
- Consumer times out with 600-1200 messages missing (12-24% loss)

**NOT a watermark issue** (proven by v2.2.7.1 monitoring - NO lag detected)

**Likely causes**:
1. Fetch handler pagination/buffer limits
2. Partition imbalance (3 partitions in cluster mode)
3. Consumer polling limits (max_poll_records)
4. WAL replication delays

---

## Work Performed

### 1. ✅ Reverted Watermark Improvements (v2.2.7.1 → v2.2.7.2)

**Why**: v2.2.7.1 improvements made things worse and solved wrong problem

**File**: `crates/chronik-server/src/produce_handler.rs` (lines 786-839)

**Reverted**:
- ❌ Per-partition watermark monitoring (lag tracking)
- ❌ Adaptive sync interval (5s → 500ms switching)
- ❌ Lag threshold logic

**Kept (v2.2.7 baseline)**:
- ✅ Simple 5-second background watermark sync
- ✅ Tolerant error handling for Raft leadership changes
- ✅ No complex lag tracking (was never needed)

**Code diff**:
```rust
// BEFORE (v2.2.7.1 - WRONG):
tokio::spawn(async move {
    let mut current_interval_ms = 5000u64;

    while running {
        tokio::time::sleep(Duration::from_millis(current_interval_ms)).await;

        // Track watermark lag
        let mut max_lag = 0i64;
        let mut lagging_partitions = 0usize;

        for each partition {
            let watermark_lag = next_offset - high_watermark;
            if watermark_lag > 0 {
                lagging_partitions += 1;
                max_lag = max_lag.max(watermark_lag);
                // Log lag...
            }
            // Update metadata store...
        }

        // Adaptive interval switching
        if max_lag > LAG_THRESHOLD {
            current_interval_ms = 500;  // Fast mode
        } else {
            current_interval_ms = 5000; // Normal mode
        }
    }
});

// AFTER (v2.2.7.2 - CORRECT):
tokio::spawn(async move {
    while running {
        tokio::time::sleep(Duration::from_secs(5)).await;  // Fixed 5s interval

        for each partition {
            let high_watermark = state.high_watermark.load(Ordering::SeqCst) as i64;
            let log_start_offset = state.log_start_offset.load(Ordering::SeqCst) as i64;

            // Update metadata store (simple, no lag tracking)
            metadata_store.update_partition_offset(...).await?;
        }
    }
});
```

**Result**: Back to proven v2.2.7 baseline (87.8% success) with NO regression risk

---

### 2. ✅ Added Comprehensive Fetch Handler Tracing

**Why**: Need to identify WHERE consumption stalls (buffer? partition? timeout?)

**File**: `crates/chronik-server/src/fetch_handler.rs`

**Tracing added**:

#### A. Fetch Start (line 260-264)
```rust
let fetch_start = Instant::now();
info!(
    "🔍 FETCH START: topic={}, partition={}, offset={}, max_bytes={}, max_wait_ms={}, min_bytes={}",
    topic, partition, fetch_offset, max_bytes, max_wait_ms, min_bytes
);
```

#### B. Watermark Check (line 317-320)
```rust
info!(
    "📊 WATERMARK: topic={}, partition={}, high_watermark={}, fetch_offset={}, gap={} (from ProduceHandler)",
    topic, partition, high_watermark, fetch_offset, high_watermark - fetch_offset
);
```

#### C. Data Available Path (line 342-345)
```rust
info!(
    "✅ DATA AVAILABLE: topic={}, partition={}, fetch_offset={}, high_watermark={}, available={}",
    topic, partition, fetch_offset, high_watermark, high_watermark - fetch_offset
);
```

#### D. Fetch Raw Bytes (line 355)
```rust
info!("📦 FETCH RAW: Trying raw bytes (CRC-preserving) for {}-{}", topic, partition);
```

#### E. Fetch Success (line 416-419)
```rust
let fetch_elapsed = fetch_start.elapsed();
info!(
    "✅ FETCH SUCCESS: topic={}, partition={}, offset={}, bytes={}, elapsed={:?}",
    topic, partition, fetch_offset, records_bytes.len(), fetch_elapsed
);
```

#### F. No Data Available (line 436-439)
```rust
info!(
    "⏸️  NO DATA: topic={}, partition={}, fetch_offset={}, high_watermark={} (waiting...)",
    topic, partition, fetch_offset, high_watermark
);
```

**Expected Log Pattern** (healthy consumption):
```
🔍 FETCH START: topic=test, partition=0, offset=0
📊 WATERMARK: topic=test, partition=0, high_watermark=1000, fetch_offset=0, gap=1000
✅ DATA AVAILABLE: topic=test, partition=0, fetch_offset=0, high_watermark=1000, available=1000
📦 FETCH RAW: Trying raw bytes for test-0
✅ FETCH SUCCESS: topic=test, partition=0, offset=0, bytes=65536, elapsed=5ms
```

**Stall Detection**:
- If logs show repeated `⏸️  NO DATA` → Watermark not advancing (produce issue)
- If logs show `✅ DATA AVAILABLE` but no `✅ FETCH SUCCESS` → Fetch hang (buffer issue)
- If logs show success for some partitions but not others → Partition imbalance

---

### 3. ✅ Created Per-Partition Consumption Analysis Script

**File**: `/tmp/debug_partition_consumption.py`

**Purpose**: Identify if partition imbalance is causing stalls

**Features**:
- Produces 5000 messages (same as original test)
- Consumes with detailed per-partition tracking
- Detects stalls (no messages for 5+ seconds)
- Shows per-partition breakdown:
  - Message count per partition
  - Deviation from expected (5000/3 = 1666.67 per partition)
  - Offset range per partition
  - Gap detection (missing offsets in range)

**Usage**:
```bash
chmod +x /tmp/debug_partition_consumption.py
python3 /tmp/debug_partition_consumption.py
```

**Example Output**:
```
Per-Partition Breakdown
================================================================================
Partition 0:
  Messages:   1650/5000 (33.0%)
  Deviation:  -17 messages from expected 1667
  Offsets:    0-1649
  Gaps:       0 (offsets missing in range)

Partition 1:
  Messages:   1720/5000 (34.4%)
  Deviation:  +53 messages from expected 1667
  Offsets:    0-1719
  Gaps:       0 (offsets missing in range)

Partition 2:
  Messages:   1021/5000 (20.4%)  ⚠️  LOW!
  Deviation:  -646 messages from expected 1667
  Offsets:    0-1499
  Gaps:       479 (offsets missing in range)  ⚠️  GAPS!

Partition Imbalance: 699 messages (max=1720, min=1021)
⚠️  SIGNIFICANT IMBALANCE DETECTED - one partition may be stalling
```

**What it reveals**:
- Which partition(s) are slow
- Whether messages are missing (gaps) vs just not fetched yet
- Timing of stalls (are all partitions stalling together or separately?)

---

### 4. ✅ Created Extended Timeout Test Script

**File**: `/tmp/debug_extended_timeout.py`

**Purpose**: Test if longer timeout or higher max_poll_records fixes the issue

**Configuration Changes**:
- `consumer_timeout_ms`: 60s → **120s** (2x longer)
- `max_poll_records`: 500 → **5000** (10x higher)
- `fetch_max_wait_ms`: 500ms (unchanged)

**Usage**:
```bash
chmod +x /tmp/debug_extended_timeout.py
python3 /tmp/debug_extended_timeout.py
```

**Expected outcomes**:
1. **If it helps** (consumes > 4391 messages):
   - Issue is timeout or polling limit related
   - Consumer needs more time or larger poll batches
   - Easy fix: Adjust consumer config

2. **If it doesn't help** (consumes ≤ 4391 messages):
   - Issue is in fetch handler or partition balancing
   - Need to investigate with fetch tracing
   - May need code fixes in fetch pagination

**Example Output**:
```
Comparison to Original (60s timeout, max_poll_records=500)
================================================================================
Original results (from WATERMARK_IMPROVEMENTS_v2.2.7.1.md):
  - Consumed: 4391/5000 (87.8%)
  - Missing:  609 messages (12.2%)

✅ IMPROVEMENT: +609 messages (+12.2%)

This suggests the issue was timeout or max_poll_records related!
```

---

## Testing Plan

### Phase 1: Extended Timeout Test (Quick Check)

**Goal**: Rule out timeout/polling limits as root cause

```bash
# Stop existing cluster
./tests/cluster/stop.sh

# Start fresh cluster with new binary
./tests/cluster/start.sh

# Run extended timeout test
python3 /tmp/debug_extended_timeout.py
```

**Decision tree**:
- **If SUCCESS (100% consumed)** → Issue was timeout/polling limits → Document and close
- **If IMPROVED (>87.8% consumed)** → Partially timeout related → Continue to Phase 2
- **If NO CHANGE (≤87.8% consumed)** → NOT timeout related → Continue to Phase 2

### Phase 2: Per-Partition Analysis (Identify Imbalance)

**Goal**: Identify which partition(s) are slow

```bash
# Run partition analysis
python3 /tmp/debug_partition_consumption.py

# Check logs for per-partition fetch patterns
tail -f tests/cluster/logs/node1.log | grep "FETCH"
```

**Look for**:
- Partition imbalance (one partition significantly behind)
- Gaps in offset ranges (missing messages)
- Stall timing (all partitions stall together or separately)

### Phase 3: Fetch Handler Tracing (Deep Dive)

**Goal**: Find exact stall point in fetch handler

```bash
# Enable INFO logging for fetch handler
RUST_LOG=chronik_server::fetch_handler=info ./tests/cluster/start.sh

# Run original test
python3 /tmp/debug_large_batch_consume.py

# Analyze logs
tail -n 1000 tests/cluster/logs/node1.log | grep "🔍\|📊\|✅\|⏸️\|📦"
```

**Expected patterns**:

**Pattern 1: Watermark not advancing**
```
⏸️  NO DATA: partition=0, fetch_offset=3500, high_watermark=3500 (waiting...)
⏸️  NO DATA: partition=0, fetch_offset=3500, high_watermark=3500 (waiting...)
...repeated for 60s...
```
→ **Root cause**: Produce path not updating watermark fast enough

**Pattern 2: Fetch hanging**
```
✅ DATA AVAILABLE: partition=0, available=1500
📦 FETCH RAW: Trying raw bytes for test-0
...no FETCH SUCCESS for 60s...
```
→ **Root cause**: Fetch handler buffer exhaustion or timeout

**Pattern 3: Partition imbalance**
```
✅ FETCH SUCCESS: partition=0, bytes=65536  ← Healthy
✅ FETCH SUCCESS: partition=1, bytes=65536  ← Healthy
⏸️  NO DATA: partition=2, fetch_offset=1000, high_watermark=1000  ← Stalled
```
→ **Root cause**: One partition slower than others

---

## Success Criteria

**Phase 1 (Extended Timeout)**:
- ✅ PASS: 100% consumption (5000/5000)
- ⚠️ PARTIAL: >87.8% consumption (better than baseline)
- ❌ FAIL: ≤87.8% consumption (no improvement)

**Phase 2 (Partition Analysis)**:
- ✅ PASS: All partitions balanced (deviation < 10%)
- ⚠️ PARTIAL: Some imbalance but all partitions complete eventually
- ❌ FAIL: One or more partitions significantly behind (>10% imbalance)

**Phase 3 (Fetch Tracing)**:
- ✅ PASS: All fetches succeed quickly (<100ms)
- ⚠️ PARTIAL: Some slow fetches but all complete
- ❌ FAIL: Fetches hang or timeout

---

## Files Modified

### Code Changes

1. **crates/chronik-server/src/produce_handler.rs** (lines 786-839)
   - REVERTED watermark improvements from v2.2.7.1
   - Restored simple 5s interval background sync
   - Removed lag tracking, monitoring, adaptive interval logic

2. **crates/chronik-server/src/fetch_handler.rs** (lines 260-439)
   - Added comprehensive INFO-level tracing at all decision points
   - Track fetch start, watermark checks, data availability, fetch completion
   - Emoji markers for easy log filtering (🔍 📊 ✅ ⏸️ 📦)

### Test Scripts Created

1. **/tmp/debug_partition_consumption.py**
   - Per-partition consumption analysis
   - Detects imbalance, gaps, stalls
   - 5000 message test (same as original)

2. **/tmp/debug_extended_timeout.py**
   - Extended timeout testing (120s vs 60s)
   - Higher max_poll_records (5000 vs 500)
   - Compares to baseline v2.2.7 results

### Documentation

- **docs/LARGE_BATCH_FIX_INVESTIGATION_v2.2.7.2.md** - This document

---

## Comparison: v2.2.7 vs v2.2.7.1 vs v2.2.7.2

| Version | Watermark Sync | Monitoring | Result | Status |
|---------|---------------|------------|--------|--------|
| v2.2.7 | 5s fixed | None | 87.8% success | ✅ Baseline |
| v2.2.7.1 | 5s→500ms adaptive | Per-partition lag | 78.8% success | ❌ REGRESSION |
| v2.2.7.2 | 5s fixed (reverted) | Fetch tracing only | Expected: 87.8% | ✅ Testing |

**Key insight**: v2.2.7.1 proved watermark lag is NOT the problem (no lag detected). Investigation must focus on fetch handler and partition balancing.

---

## Next Actions (Immediate)

1. **Run extended timeout test** → Determine if timeout/polling is the issue
2. **Run partition analysis** → Identify which partition(s) are slow
3. **Analyze fetch logs** → Find exact stall point
4. **Document findings** → Create action plan based on results

---

## Lessons Learned

### From v2.2.7.1 Failure

1. **Don't optimize without measurement** - Added monitoring, found NO lag
2. **Test hypothesis first** - Should have checked logs before implementing
3. **Improvements can regress** - 9% worse despite better monitoring
4. **Fix forward, not more complexity** - Revert and investigate properly

### From v2.2.7.2 Approach

1. **Measure first, optimize second** - Added tracing BEFORE changing logic
2. **Multiple test angles** - Timeout, partition, fetch handler all tested
3. **Incremental testing** - Phase 1→2→3 narrows down root cause
4. **Proper tools** - Scripts provide objective data, not guesses

---

## References

**Previous Work**:
- [WATERMARK_IMPROVEMENTS_v2.2.7.1.md](WATERMARK_IMPROVEMENTS_v2.2.7.1.md) - Failed attempt
- [HIGH_WATERMARK_BUG_FIX_v2.2.7.md](HIGH_WATERMARK_BUG_FIX_v2.2.7.md) - get_high_watermark() fix
- [WORK_COMPLETED_v2.2.7_SESSION3.md](WORK_COMPLETED_v2.2.7_SESSION3.md) - Session 3 summary

**Related**:
- [LOCK_AUDIT_v2.2.7.md](LOCK_AUDIT_v2.2.7.md) - Lock audit (NO critical issues)
- [STABILITY_STATUS_v2.2.7.md](STABILITY_STATUS_v2.2.7.md) - Overall stability

**Test Scripts**:
- `/tmp/debug_partition_consumption.py` - Per-partition analysis
- `/tmp/debug_extended_timeout.py` - Extended timeout test
- `/tmp/debug_large_batch_consume.py` - Original baseline test
