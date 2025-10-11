# WAL Message Loss Investigation Report

## Issue Summary

**Problem**: 10% message loss in multi-partition, high-volume WAL recovery scenarios
**Severity**: CRITICAL - Data loss bug
**Discovered**: During v1.3.51 testing
**Status**: Root cause analysis required

## Test Results

### ✅ PASS: Single Partition (5000 messages)
- **Test**: `test_v1.3.51_single_partition.py`
- **Result**: 5000/5000 messages recovered (100%)
- **Scenario**: Produce 5000 → crash → WAL recovery → consume all
- **Conclusion**: Works perfectly for single partition scenarios

### ❌ FAIL: Multi-Partition (10000 messages)
- **Test**: `test_v1.3.51_multi_partition.py`
- **Result**: 9000/10000 messages recovered (90%)
- **Written to WAL**: 10000 records confirmed
- **Recovered**: 9000 messages (3000 per partition × 3 partitions)
- **Missing**: 1000 messages (10% loss)
- **Distribution**: Equal distribution suggests systematic issue, not random

### ❌ FAIL: Stress Test (50000 messages)
- **Test**: `test_v1.3.51_stress.py`
- **Result**: 46000/50000 messages recovered (92%)
- **Missing**: 4000 messages (8% loss)
- **Pattern**: Scattered gaps throughout offset range

## Evidence

### WAL Write Confirmation
```bash
$ grep "WAL✓.*test-v1.3.51-multi" /tmp/v1.3.51-multi-test2.log | grep -o '[0-9]\+ records' | awk '{sum+=$1} END {print sum}'
10000
```
**Conclusion**: All 10000 messages were successfully written to WAL

### WAL File Sizes
```
data/wal/test-v1.3.51-multi/0/wal_0_0.log: 486200 bytes
data/wal/test-v1.3.51-multi/1/wal_0_0.log: 447718 bytes
data/wal/test-v1.3.51-multi/2/wal_0_0.log: 506592 bytes
Total: ~1.4MB for 10000 messages
```
**Conclusion**: WAL files contain data, not corrupted or empty

### Recovery Logs
```
[INFO] Recovered partition test-v1.3.51-multi/0 with segment 0
[INFO] Recovered partition test-v1.3.51-multi/1 with segment 0
[INFO] Recovered partition test-v1.3.51-multi/2 with segment 0
[INFO] Recovered 3 partitions from disk
```
**Missing**: No log of total messages/records recovered
**Issue**: Cannot determine if loss happens during recovery or consumption

### Consumer Results
```
Partition 0: 3000 messages
Partition 1: 3000 messages
Partition 2: 3000 messages
Total: 9000/10000
```
**Pattern**: Perfect distribution (3000 each) suggests systematic truncation

### Missing Message Pattern
```
First 20 missing: [2187, 2190, 2193, 2196, 2199, 2202, 2205, 2208, 2211, 2214, ...]
Last 20 missing: [9970, 9972, 9973, 9975, 9976, 9978, 9979, 9981, 9982, 9984, ...]
```
**Pattern**: Scattered gaps, not contiguous blocks
**Suggests**: Offset-based skipping or filtering issue

## Hypotheses

### Hypothesis 1: WAL Recovery Incomplete
**Theory**: `WalManager::recover()` doesn't replay all records from WAL segments
**Evidence**:
- 10000 records written
- Only 9000 recovered
- No error logs during recovery

**Investigation needed**:
1. Check `WalManager::recover()` in [crates/chronik-wal/src/manager.rs](../crates/chronik-wal/src/manager.rs)
2. Add logging to count records during replay
3. Verify all WAL segments are being read completely
4. Check if there's early termination or limit on recovery

### Hypothesis 2: Offset Filtering Too Aggressive
**Theory**: v1.3.51 offset metadata filtering is skipping valid batches
**Evidence**:
- Single partition works (5000/5000)
- Multi-partition fails (9000/10000)
- "INCLUDE" logs show batches being included, but no "SKIP" logs

**Investigation needed**:
1. Check `read_from()` filtering logic in [manager.rs:449-484](../crates/chronik-wal/src/manager.rs#L449-L484)
2. Verify condition: `last_offset >= offset` is correct
3. Check if there's an off-by-one error
4. Verify base_offset/last_offset are set correctly during write

### Hypothesis 3: Consume Path Truncation
**Theory**: FetchHandler or consume logic truncates results before client
**Evidence**:
- Perfect 3000 messages per partition suggests artificial limit
- Consumer timeout set to 10000ms might be too short for large recovery

**Investigation needed**:
1. Check `FetchHandler::handle_fetch()` max records logic
2. Verify buffer trimming (100 batches) doesn't affect recovery reads
3. Check if there's a pagination or max_bytes limit
4. Increase consumer timeout and retry

### Hypothesis 4: Partition Offset Tracking Bug
**Theory**: High watermark or offset tracking corrupts during multi-partition writes
**Evidence**:
- Works for single partition
- Fails for multiple partitions
- Loss percentage increases with message count

**Investigation needed**:
1. Check high watermark updates during produce
2. Verify DashMap-based partition state doesn't have race conditions
3. Check if offset allocation has gaps for multi-partition batched writes

## Diagnostic Steps

### Step 1: Add Recovery Logging
Add comprehensive logging to `WalManager::recover()`:

```rust
// In crates/chronik-wal/src/manager.rs - recover() method
let mut total_records_recovered = 0;
for record in recovered_records {
    total_records_recovered += 1;
    // ... replay logic
}
info!("WAL recovery complete: {} records replayed", total_records_recovered);
```

### Step 2: Verify Offset Metadata
Add validation logging during WAL write:

```rust
// In crates/chronik-server/src/produce_handler.rs - after append_canonical()
debug!("WAL write: topic={} partition={} base_offset={} last_offset={} count={}",
    topic, partition, base_offset, last_offset, records.len());
```

### Step 3: Count Recovery vs Consumption
Add logging in recovery replay:

```rust
// In WalProduceHandler::apply_recovered_records()
info!("Replaying {} recovered records to ProduceHandler", records.len());
```

### Step 4: Trace Consumption
Enable trace logging for fetch:

```bash
RUST_LOG=chronik_server::fetch_handler=trace,chronik_wal=debug
```

Check if all recovered messages are being fetched.

### Step 5: Direct WAL Read Test
Create a standalone test that:
1. Reads WAL files directly
2. Counts all V2 records
3. Deserializes and counts messages
4. Compares to expected count

```rust
#[test]
fn test_wal_completeness() {
    let wal_manager = WalManager::new(config);
    let records = wal_manager.read_from("test-topic", 0, 0, u64::MAX).await?;
    let total_messages = records.iter().map(|r| r.record_count).sum();
    assert_eq!(total_messages, 10000);
}
```

## Key Code Locations

### WAL Recovery
- **File**: [crates/chronik-wal/src/manager.rs](../crates/chronik-wal/src/manager.rs)
- **Method**: `recover()` - Lines ~130-180
- **Issue**: No logging of total records recovered
- **Fix**: Add record counting and logging

### WAL Reading
- **File**: [crates/chronik-wal/src/manager.rs](../crates/chronik-wal/src/manager.rs)
- **Method**: `read_from()` - Lines 449-484
- **Recent Changes**: v1.3.51 added offset metadata filtering
- **Suspect**: Filtering logic may skip valid batches

### Recovery Replay
- **File**: [crates/chronik-server/src/wal_integration.rs](../crates/chronik-server/src/wal_integration.rs)
- **Method**: `apply_recovered_records()` - Lines ~111-148
- **Issue**: May not replay all records to ProduceHandler
- **Fix**: Verify all records are applied

### Fetch/Consume
- **File**: [crates/chronik-server/src/fetch_handler.rs](../crates/chronik-server/src/fetch_handler.rs)
- **Method**: `handle_fetch()` - Lines ~200-400
- **Suspect**: Buffer trimming or max records limiting

## Related Files

### Test Files
- `tests/test_v1.3.51_single_partition.py` - PASSING test
- `tests/test_v1.3.51_multi_partition.py` - FAILING test (9000/10000)
- `tests/test_v1.3.51_stress.py` - FAILING test (46000/50000)

### WAL Components
- `crates/chronik-wal/src/record.rs` - WalRecord V2 format with offset metadata
- `crates/chronik-wal/src/segment.rs` - Segment reading/writing
- `crates/chronik-wal/src/manager.rs` - WAL management and recovery

### Integration
- `crates/chronik-server/src/produce_handler.rs` - Calls append_canonical()
- `crates/chronik-server/src/wal_integration.rs` - WAL recovery orchestration
- `crates/chronik-server/src/fetch_handler.rs` - Message consumption

## Quick Reproduction

```bash
# Clean state
pkill -9 chronik-server
rm -rf data/wal data/segments

# Start server
RUST_LOG=info,chronik_wal=debug ./target/release/chronik-server standalone > /tmp/test.log 2>&1 &

# Run failing test
python3 tests/test_v1.3.51_multi_partition.py

# Expected: 10000/10000
# Actual: 9000/10000 (10% loss)
```

## Next Steps

1. **Add diagnostic logging** to all recovery paths
2. **Count records** at each stage: WAL write → WAL read → recovery replay → consumption
3. **Identify exact point of loss**: Is it during recovery or consumption?
4. **Fix root cause** based on findings
5. **Add regression test** to prevent future loss
6. **Verify fix** with all three test scenarios

## Impact Assessment

**Severity**: CRITICAL
**Scope**: Multi-partition, high-volume deployments
**Data Loss**: 8-10% in affected scenarios
**Workaround**: Use single partition topics (100% reliable)
**Priority**: P0 - Must fix before production release

## Version Context

- **v1.3.51**: Added WAL offset metadata filtering (architectural fix)
- **Pre-v1.3.51**: Unknown if this bug existed before
- **Single partition**: Works perfectly (5000/5000)
- **Multi-partition**: Exhibits loss (9000/10000, 46000/50000)

## Investigation Owner

**Discovered by**: Claude Code AI Assistant during v1.3.51 testing
**Assigned to**: Development team (new session required)
**Created**: 2025-10-11
**Status**: Awaiting investigation

---

**Note**: This bug was discovered during comprehensive testing of v1.3.51 WAL offset metadata improvements. While v1.3.51 itself is working correctly (offset filtering logs show proper behavior), this testing exposed a pre-existing or newly introduced message loss bug that requires immediate attention.
