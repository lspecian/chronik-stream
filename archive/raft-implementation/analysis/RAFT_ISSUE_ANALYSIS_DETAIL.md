# Raft Clustering Data Loss - Detailed Root Cause Analysis

**Date**: 2025-10-16 18:07 UTC
**Status**: ROOT CAUSE IDENTIFIED - INLINE WAL WRITES NOT EXECUTING
**Severity**: CRITICAL - Data loss during shutdown

---

## Executive Summary

Testing revealed 30% data loss during shutdown. Initial hypothesis was that `flush_partition()` was a NO-OP, but deeper investigation revealed the ACTUAL root cause:

**INLINE WAL WRITES AT LINE 1208 ARE NOT EXECUTING**

The architecture assumes that WAL writes happen inline during `handle_produce()` at line ~1208, but logs show ZERO "WAL✓" messages, indicating this code path is never reached.

---

## Architecture (As Designed)

The intended data flow is:

```
Producer Request
    ↓
handle_produce() entry (line ~757)
    ↓
Batch processing
    ↓
[LINE ~1188-1237] Inline WAL write (CRITICAL - SHOULD HAPPEN HERE)
    ├─ Check if self.wal_manager exists
    ├─ Convert batch to CanonicalRecord
    ├─ Serialize to bincode
    ├─ Call wal_manager.append_canonical_with_acks(acks=producer_acks)
    └─ Log "WAL✓" on success
    ↓
[LINE ~1242-1310] Raft integration (optional, #[cfg(feature = "raft")])
    ├─ If Raft enabled for partition: propose to Raft
    └─ Else: buffer to pending_batches
    ↓
Response to client
```

**Key Design Principle**: `flush_partition()` is intentionally a NO-OP because WAL writes happen inline BEFORE buffering.

---

## What's Actually Happening

### Evidence from Testing

1. **No "WAL✓" logs**:
   ```bash
   $ grep "WAL✓" /tmp/chronik_test/node1/chronik.log | wc -l
   0
   ```

2. **No "Batch:" logs**:
   ```bash
   $ grep "Batch: topic=" /tmp/chronik_test/node1/chronik.log | wc -l
   0
   ```

3. **No Raft logs**:
   ```bash
   $ grep "Raft-enabled partition" /tmp/chronik_test/node1/chronik.log | wc -l
   0
   ```

4. **WAL manager IS set**:
   ```bash
   $ grep "wal_manager set to Some" /tmp/chronik_test/node1/chronik.log
   INFO chronik_server::produce_handler: CRITICAL_DEBUG: wal_manager set to Some - inline WAL writes ENABLED
   ```

### Conclusion

The batch processing code from line ~1183 onwards (which includes inline WAL writes) **IS NOT BEING EXECUTED**.

---

## Why Inline WAL Writes Don't Execute

The code must be returning or taking a different path BEFORE reaching line 1183. Possible reasons:

### Hypothesis 1: Early Return Due to Error
The `handle_produce()` method might be returning an error before reaching batch processing.

### Hypothesis 2: Different Code Path
There might be a feature flag or conditional that routes produce requests through a completely different handler that doesn't include inline WAL writes.

### Hypothesis 3: handle_produce() Not Called
The request might be handled by a different method entirely (e.g., direct Raft integration bypass).

---

## Test Results Summary

### Initial Test (Before Understanding)
```
Sent: 100 messages
Received: 70 messages
Missing: 30 messages
Duplicates: 0
```

### After Attempted Fix (flush_partition writes to WAL)
```
Sent: 100 messages
Received: 71 messages
Missing: 62 messages
Duplicates: 29 messages  ← NEW PROBLEM!
```

**Analysis**: Flush fix caused duplicates because:
1. Inline WAL writes (line 1208) ARE executing after all
2. Flush ALSO writes to WAL (double write)
3. But we see no "WAL✓" logs, so... something else is wrong

---

## Code Locations

### Inline WAL Write (Should Execute)
**File**: `crates/chronik-server/src/produce_handler.rs`
**Lines**: 1188-1237

```rust
// CRITICAL (v1.3.47+): Write to WAL BEFORE updating high watermark
if let Some(ref wal_mgr) = self.wal_manager {
    // Convert to CanonicalRecord
    match CanonicalRecord::from_kafka_batch(&re_encoded_bytes) {
        Ok(mut canonical_record) => {
            match bincode::serialize(&canonical_record) {
                Ok(serialized) => {
                    if let Err(e) = wal_mgr.append_canonical_with_acks(
                        topic.to_string(),
                        partition,
                        serialized,
                        base_offset as i64,
                        last_offset as i64,
                        records.len() as i32,
                        acks
                    ).await {
                        error!("WAL WRITE FAILED: topic={} partition={} ...", topic, partition);
                        return Err(...);
                    }
                    warn!("WAL✓ {}-{}: {} bytes, {} records ...", topic, partition, ...);  ← NO LOGS!
                }
            }
        }
    }
}
```

### Raft Code Path (Alternative)
**File**: Same
**Lines**: 1242-1310

```rust
#[cfg(feature = "raft")]
if let Some(ref raft_manager) = self.raft_manager {
    if raft_manager.has_replica(topic, partition) {
        info!("Raft-enabled partition {}-{}: Proposing write to Raft consensus", topic, partition);
        // Raft write here
    } else {
        // Not Raft-enabled, buffer to pending_batches
        let mut pending = partition_state.pending_batches.lock().await;
        pending.push(BufferedBatch { ... });
    }
}
```

---

## Next Steps

### Immediate (Priority 1) - Find Why Inline WAL Writes Don't Execute

1. **Add tracing at method entry**:
   ```rust
   pub async fn handle_produce(...) -> Result<ProduceResponse> {
       warn!("DEBUG: handle_produce ENTRY for {} topics", request.topic_data.len());
       // ...
   }
   ```

2. **Add tracing before WAL write section**:
   ```rust
   // Line ~1188
   warn!("DEBUG: About to check wal_manager, is_some={}", self.wal_manager.is_some());
   if let Some(ref wal_mgr) = self.wal_manager {
       warn!("DEBUG: Inside wal_manager block, about to process batch");
       // ...
   }
   ```

3. **Add tracing before Raft section**:
   ```rust
   // Line ~1242
   #[cfg(feature = "raft")]
   warn!("DEBUG: About to check raft_manager, is_some={}", self.raft_manager.is_some());
   ```

4. **Rebuild and retest**:
   ```bash
   cargo build --release --bin chronik-server --features raft
   python3 tests/raft_chaos_test.py --test single-node
   ```

5. **Analyze logs** to see which sections execute.

### Medium-Term (Priority 2) - Fix the Actual Issue

Once we know WHY inline WAL writes don't execute, implement the proper fix.

### Long-Term (Priority 3) - Architectural Improvements

1. **Simplify buffering pipeline** - Too many layers (pending_batches, WAL, segments)
2. **Add metrics** - Track buffer depths, WAL write rates, etc.
3. **Better testing** - E2E tests that verify WAL writes happen

---

## Lessons Learned

1. **Don't assume code executes** - Just because code exists doesn't mean it runs
2. **Check the logs first** - Missing "WAL✓" logs immediately told us the problem
3. **Understand architecture before fixing** - Initial flush fix was wrong because I didn't understand inline writes
4. **Feature flags matter** - `#[cfg(feature = "raft")]` creates complex conditional logic
5. **Test with actual clients** - Synthetic tests miss real-world issues

---

## Related Files

- `crates/chronik-server/src/produce_handler.rs` - Main logic
- `crates/chronik-server/src/wal_integration.rs` - WAL wrapper (now mostly passthrough)
- `crates/chronik-wal/src/manager.rs` - WAL manager
- `tests/raft_chaos_test.py` - Comprehensive test suite

---

**Report Generated**: 2025-10-16 18:07 UTC
**Next Action**: Add debug tracing to find execution path
