# WAL Write & Recovery Investigation

## Status: CRITICAL BUG FOUND - WAL RECOVERY BROKEN
**Date Started**: 2025-10-10
**Current Focus**: WAL recovery not restoring messages at scale

---

## CRITICAL BUG: WAL Recovery Broken

### Test Results

**10 messages (fresh-test):**
- ✅ WAL files created (1.1KB total)
- ✅ Server recovered 3 partitions from WAL
- ✅ All 10/10 messages consumed after restart

**100 messages (volume-test-100):**
- ✅ WAL files created (18KB total across 3 partitions)
- ✅ Server says "WAL manager recovered with 6 partitions"
- ❌ **0/100 messages can be consumed after restart**
- ❌ **WAL recovery is NOT restoring data to consumable state**

### Breaking Point Analysis

The bug appears between 10-100 messages:
1. ✅ WAL write succeeds (logs show "WAL✓" for all batches)
2. ✅ WAL files exist on disk with correct data (hexdump shows messages in files)
3. ✅ WAL recovery says it succeeded ("recovered with 6 partitions")
4. ❌ **Messages are not accessible to consumers after recovery**

### Root Cause Hypothesis

WAL recovery (`WalManager::recover()`) is reading the files but NOT:
1. Restoring in-memory topic/partition metadata, OR
2. Writing recovered data to segment storage, OR
3. Setting high watermarks correctly, OR
4. Registering topics/partitions with metadata store

**Key Question**: What does `WalManager::recover()` actually do with the recovered records?

---

## FINDINGS LOG

### Finding #1: WAL Writes ARE Working (RESOLVED)
**Status**: RESOLVED
**Evidence**:
- ✅ All 10 messages written to WAL and recovered
- ✅ All 100 messages written to WAL (but not recovered)
- ✅ WAL✓ logs appear for all batches

**Root Cause**: Multiple old server binaries were running in background, interfering with tests

**Resolution**: Killed all background servers, started fresh binary

---

### Finding #2: WAL Recovery Broken at Scale (FIXED v1.3.48)
**Status**: FIXED
**Evidence**:
- Works: 10 messages
- Fails: 100 messages
- WAL files contain data but consumers get 0 messages after restart

**Root Cause**: `WalManager::recover()` only loaded segments but never read records or restored high watermarks

**Fix**: Added WAL replay logic in integrated_server.rs (lines 314-373) that:
- Reads all WAL records after recovery
- Finds max offset per partition
- Calls `produce_handler.restore_partition_state()` with correct high watermark

**Verification**: 100 and 500 messages now fully recovered ✅

---

### Finding #3: Messages Stuck in Buffer (CRITICAL - IN PROGRESS)
**Status**: INVESTIGATING ROOT CAUSE FOUND
**Evidence**:
- 5000 messages: Only 4164/5000 consumable (836 missing)
- 10000 messages: Only 9008/10000 consumable (992 missing)
- Data loss happens even WITHOUT restart
- Pattern: Gaps in offsets, not just missing from end

**Root Cause Analysis**:
1. ✅ Buffer trimming confirmed - FetchHandler keeps only 100 batches
2. ✅ WAL contains all data (verified with file size)
3. ✅ Added min_offset_in_buffer tracking
4. ✅ Improved WAL fallback detection for trimmed regions
5. ❌ Fixed `WalManager::read_from()` condition (removed `offset >= active_base_offset`)
6. **NEW DISCOVERY**: WAL read doesn't skip batches before requested offset!

**THE ACTUAL BUG** (v1.3.49):
`WalManager::read_from(offset, max_records)` has critical flaw:
- Reads from file cursor=0 every time
- Returns first `max_records` batches (e.g., 8 batches)
- Those 8 batches might only cover offsets 0-247
- Consumer requests offset 1485
- WAL returns same 8 batches (offsets 0-247)
- FetchHandler filters and gets 0 records
- Consumer retries, gets same batches again → infinite loop

**The Fix Needed**:
WAL must deserialize CanonicalRecord to extract offset ranges and SKIP batches that don't contain requested offset range. Need to read sequentially until we find batches covering the requested offset.

**Fix Implemented (v1.3.49)**:
- Added `extract_offset_range()` helper function in manager.rs (lines 1302-1342)
- Deserializes minimal CanonicalRecord struct to get base_offset and last_offset
- Filters batches: include if `last_offset >= requested_offset`
- Logs skipped batches for debugging

**Test Results (v1.3.49)**:
- ✅ Single partition (5000 messages): **5000/5000 consumed** - PERFECT!
- ⚠️  3 partitions (5000 messages): **4500/5000 consumed** - 500 missing (exactly 1500/partition)

**Analysis**:
The WAL offset filtering works correctly! Single-partition test proves this.
The 500 missing messages in 3-partition test suggests a different issue - possibly:
1. kafka-python's partitioner sent fewer messages to some partitions
2. Some batches got lost during production (need to verify WAL contains all data)
3. High watermark issue with one partition

**Next Steps**:
1. Check WAL file sizes for all 3 partitions of final-test-5k
2. Verify each partition's high watermark
3. Test with explicit partition assignment to isolate which partition has issues

---

## INVESTIGATION STRATEGY

### Phase 1: Understand WAL Recovery (CURRENT)
**Goal**: Find why recovery doesn't restore messages to consumable state

**Approach**:
1. ⏳ Read `WalManager::recover()` code
2. ⏳ Understand what it does with recovered records
3. ⏳ Find where state restoration should happen
4. ⏳ Add debug logging to see what's actually restored
5. ⏳ Compare 10-message vs 100-message recovery behavior

**Files to Check**:
- `crates/chronik-wal/src/manager.rs` - recover() method
- `crates/chronik-server/src/integrated_server.rs` - how recovery is called
- `crates/chronik-common/src/metadata/` - metadata restoration
- `crates/chronik-storage/src/` - segment write during recovery

---

### Phase 2: Fix WAL Recovery
**Goal**: Ensure recovered messages are consumable

**Requirements**:
- Restored messages must be consumable by clients
- High watermarks must be set correctly
- Topic/partition metadata must be registered
- Segment storage must contain recovered data

---

### Phase 3: Volume Testing
**Goal**: Verify fix works at scale

**Test Plan**:
- 100 messages → restart → consume all
- 500 messages → restart → consume all
- 1,000 messages → restart → consume all
- 5,000 messages (~5MB) → restart → consume all
- 50,000 messages (~50MB) → restart → consume all
- 500,000 messages (~500MB) → restart → consume all

---

## CODE CHANGES MADE

### Change #1: Added Entry Point Logging
**File**: `crates/chronik-server/src/produce_handler.rs`
**Line**: 842
**Change**: Added `info!("→ produce_to_partition()")`
**Impact**: Can verify produce path is called

### Change #2: Added WAL Success Logging
**File**: `crates/chronik-server/src/produce_handler.rs`
**Line**: 1007
**Change**: Added `warn!("WAL✓ {}-{}: {} bytes...")`
**Impact**: Confirms WAL writes are happening

### Change #3: Added Batch Summary Logging
**File**: `crates/chronik-server/src/produce_handler.rs`
**Line**: 975
**Change**: Added `info!("Batch: topic={} partition={} ...")`
**Impact**: Shows batch-level metrics

---

## IMMEDIATE NEXT ACTIONS

1. **Read WalManager::recover() code** - Understand current implementation
2. **Trace recovery flow** - See what happens to recovered records
3. **Add debug logging** - Make recovery process visible
4. **Find the bug** - Where does recovery fail to restore state?
5. **Fix it properly** - No shortcuts, production-ready solution

---

*Last Updated: 2025-10-10 20:36*
