# Release Notes - Chronik Stream v1.3.43

**Release Date:** 2025-01-08
**Type:** Bug Fix (Critical)

## Overview

v1.3.43 fixes a critical race condition in the WAL write path that caused message loss when the background flush task cleared `pending_batches` before the WAL could write them. This release ensures zero message loss by deferring the clearing of pending batches until after successful WAL persistence.

## Critical Fixes

### 1. **CRITICAL: Fixed Race Condition in WAL Write Path**

**Issue:** Multi-batch produce requests lost data due to background flush task clearing `pending_batches` before WalProduceHandler could read them for WAL writing.

**Root Cause:**
```
1. Batch 0 produced → added to pending_batches → WAL writes it
2. Background flush task wakes up → calls flush_partition → CLEARS pending_batches
3. Batch 1 produced → added to pending_batches
4. Background flush task runs immediately → CLEARS pending_batches before WAL write
5. WalProduceHandler tries to read pending_batches → EMPTY!
```

**Solution:**
- **Modified `produce_handler.rs` flush_partition (lines 1358-1378)**:
  - Removed `pending.clear()` from `flush_partition` NO-OP
  - Added comment explaining why clearing must NOT happen during background flush

- **Added `produce_handler.rs` clear_pending_batches method (lines 382-399)**:
  ```rust
  pub async fn clear_pending_batches(&self, topic: &str, partition: i32) -> Result<()>
  ```
  - New method to explicitly clear pending batches after WAL write
  - Prevents duplicate WAL writes and memory leaks

- **Modified `wal_integration.rs` handle_produce (lines 280-286)**:
  - Added loop to clear `pending_batches` AFTER successful WAL write
  - Ensures batches remain available for WAL write

**Files Modified:**
- `crates/chronik-server/src/produce_handler.rs`
- `crates/chronik-server/src/wal_integration.rs`

**Impact:** Ensures zero message loss in multi-batch scenarios by guaranteeing all batches are written to WAL before being cleared from memory.

## Testing

### Comprehensive Integration Tests (All Passed)

```
✅ Test 1: Basic Produce/Consume - 50/50 messages
✅ Test 2: Offset Continuity - 30 messages, continuous offsets 0-29 (single partition)
✅ Test 3: Multi-Partition - 15 messages across 3 partitions
✅ Test 4: Consumer Groups - Offset commit/fetch working
```

### Test Script Updates

- **Fixed Test 2 (Offset Continuity)**: Modified to produce to single partition (partition 0) to get true sequential offsets 0-29, matching correct Kafka per-partition offset semantics.

## Technical Details

### WAL Write Flow (Corrected)

```
1. ProduceHandler processes request → assigns offsets → re-encodes batches
2. Batches added to pending_batches in-memory buffer
3. ProduceHandler returns response
4. WalProduceHandler reads pending_batches → writes to WAL
5. WalProduceHandler calls clear_pending_batches → clears memory
6. Background flush task (if triggered) → NO-OP, does not clear pending_batches
```

### Key Architectural Principle

**CRITICAL:** `pending_batches` must remain available until after WAL write completes. The clearing responsibility moved from the background flush task to the WAL write path.

## Migration Notes

No migration required. This is a transparent bug fix with no API or configuration changes.

## Known Limitations

- Consumer group offset fetch behavior with `auto_offset_reset='earliest'` may vary with some Kafka client libraries (kafka-python quirk, not a server issue)

## Contributors

- Chronik Stream Development Team

## Upgrade Path

From v1.3.42:
1. Replace binary with v1.3.43
2. Restart server
3. No data migration needed

## Related Issues

- Fixes multi-batch WAL write race condition (v1.3.42 regression)
- Completes WAL refactor Phase 8 integration testing

---

**Previous Release:** [v1.3.42](RELEASE_NOTES_v1.3.42.md)
**Next Release:** TBD
