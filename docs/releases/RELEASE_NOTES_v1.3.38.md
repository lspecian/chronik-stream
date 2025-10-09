# Release Notes - v1.3.38

**Release Date:** 2025-10-08
**Type:** Critical Bug Fixes
**Status:** PARTIAL - WAL Recovery Parsing Fixed, Consumption Requires Refactor Completion

---

## 🎯 Summary

v1.3.38 fixes **CRITICAL** WAL V2 record parsing bugs that prevented proper crash recovery. However, full end-to-end consumption after recovery requires completing the layered storage refactor (tracked in REFACTOR_IMPLEMENTATION_TRACKER.md).

---

## ✅ Fixed Issues

### 1. WAL V2 Parser Cursor Advancement Bug (CRITICAL)

**Problem**: WAL recovery was only parsing 1 record out of N records in the WAL file.

**Root Cause**: Parser was using stored `length` field for cursor advancement instead of tracking actual bytes consumed from the reader.

**Fix**: Changed cursor advancement to use `rdr.position()` to track exact bytes read.

**Impact**: WAL recovery now correctly parses ALL V2 records.

**Files Changed**:
- `crates/chronik-wal/src/manager.rs` (lines 401-467)

**Test Evidence**:
```
Before: "Found 1 V2 records in active segment file"
After:  "Found 10 V2 records in active segment file"
        "Successfully recovered 10 batches for partition"
```

---

### 2. WAL Flush Overwrite Bug (CRITICAL)

**Problem**: `flush_all()` was OVERWRITING the WAL file instead of APPENDING, causing message loss.

**Root Cause**: Used `tokio::fs::write()` which truncates the file.

**Fix**: Changed to `OpenOptions::new().append(true).open()` with proper fsync.

**Impact**: WAL now correctly accumulates records across multiple flushes.

**Files Changed**:
- `crates/chronik-wal/src/manager.rs` (lines 617-630)

---

### 3. Buffer Not Cleared After Flush

**Problem**: In-memory buffer wasn't cleared after successful flush to disk, causing duplicate writes.

**Fix**: Added `buffer.clear()` after successful fsync.

**Files Changed**:
- `crates/chronik-wal/src/manager.rs` (lines 636-641)

---

### 4. Segment Registration for Recovered Batches

**Problem**: Segments with WAL-recovered batches weren't being registered in metadata because they had 0 indexed records.

**Root Cause**: Segment registration checked `record_count > 0` but recovered batches skip indexing.

**Fix**: Changed condition to `record_count > 0 || has_raw_kafka` to register segments with raw Kafka data.

**Impact**: FetchHandler can now discover segments created from recovered batches.

**Files Changed**:
- `crates/chronik-storage/src/segment_writer.rs` (line 260)

---

### 5. Updated CLAUDE.md Work Ethic Rules

**Added**:
- Rule 7: ABSOLUTE OWNERSHIP - Never say "beyond scope" or "not my responsibility"
- Rule 8: END-TO-END VERIFICATION - Fixes must be verified with real client testing

---

## ⚠️ Known Limitations

### Consumption After Recovery Not Yet Working

**Status**: WAL recovery successfully restores data (10/10 records recovered), but consumption fails.

**Root Cause**: The codebase is mid-refactor. WAL writes CanonicalRecord V2 format, but FetchHandler's `fetch_from_wal()` integration is incomplete.

**Required Fix**: Complete Phase 8 of the layered storage refactor per REFACTOR_IMPLEMENTATION_TRACKER.md.

**Workaround**: None currently. This is blocked on refactor completion.

**Tracking**: See `docs/REFACTOR_IMPLEMENTATION_TRACKER.md` Phase 8.

---

## 🔬 Testing Performed

### WAL V2 Parser Test
```bash
# Produced 10 messages
# Crashed server (killed process)
# Restarted server
# Result: "Successfully recovered 10 batches" ✅
```

### End-to-End Test Status
```
Phase 1: PRODUCE - ✅ 10 messages sent (offsets 0-9)
Phase 2: CRASH - ✅ Server killed
Phase 3: RECOVER - ✅ 10 batches recovered from WAL
Phase 4: CONSUME - ❌ 0 messages (requires refactor completion)
```

---

## 📊 Technical Details

### WAL V2 Record Format
```
magic(2) + version(1) + flags(1) + length(4) + crc32(4) +
topic_len(2) + topic + partition(4) +
canonical_data_len(4) + canonical_data
```

### Parser Implementation
- Manual parsing matching serialization in `record.rs::to_bytes()`
- Cursor tracking via `std::io::Cursor::position()`
- V2 records don't store offset in WAL envelope (it's in CanonicalRecord)

### Segment Registration Logic
```rust
// OLD: if active.record_count > 0
// NEW: if active.record_count > 0 || active.has_raw_kafka
```

---

## 📝 Files Changed

1. `crates/chronik-wal/src/manager.rs`
   - Fixed cursor advancement in WAL V2 parser
   - Fixed flush to use append mode
   - Added buffer clear after flush
   - Added fallback to read from file when buffer empty

2. `crates/chronik-storage/src/segment_writer.rs`
   - Updated segment registration condition for raw_kafka data

3. `crates/chronik-server/src/produce_handler.rs`
   - Simplified recovered batch handling (skip indexing, preserve raw_bytes)

4. `CLAUDE.md`
   - Added ownership and E2E verification rules

---

## 🚀 Next Steps

1. **Complete Layered Storage Refactor** (Phase 8: Integration Testing)
   - Update FetchHandler to use `fetch_from_wal()` properly
   - Test with Java client for CRC validation
   - KSQL integration testing
   - Performance benchmarks

2. **Verify E2E Recovery**
   - Produce → Crash → Recover → **Consume** (currently blocked)

3. **Clean Up**
   - Update documentation
   - Migration guide
   - Consolidate release notes

---

## 🔗 References

- REFACTOR_IMPLEMENTATION_TRACKER.md - Layered storage refactor progress
- REFACTOR_PLAN_LAYERED_STORAGE.md - Architecture plan
- CLAUDE.md - Updated work ethic standards

---

## ⚡ Upgrade Notes

**Breaking Changes**: None

**Compatibility**: Fully compatible with v1.3.27-v1.3.37 WAL files.

**Recommended Action**: Upgrade immediately to fix critical WAL parsing bugs.

---

**Contributors**: Claude (AI Assistant)
**Review Status**: Awaiting user review and refactor completion
