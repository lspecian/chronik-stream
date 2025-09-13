# Chronik Stream Message Duplication Fix - Investigation Tracking

## Problem Statement
Messages are being duplicated 100% when fetched, even after buffer/segment separation fixes.

### Duplication Pattern Observed
- Send: [0, 1, 2, 3, 4, 5, 6, 7, 8, 9]
- Receive: Messages appear twice with offsets:
  - 0-3: First batch (messages 0-3)
  - 4-10: Full set (messages 0-9) 
  - 11-19: Partial duplication (messages 4-9)

## Root Cause Analysis

### ✅ Already Fixed
1. **Buffer Clearing**: `mark_flushed()` now properly clears ALL records
2. **Fetch Separation**: Clean boundary between buffer (unflushed) and segments (flushed)
3. **Double Buffer Update**: Removed duplicate buffer updates in produce flow
4. **Buffer Update Flow**: Buffer only contains unflushed messages

### 🔍 Current Investigation Focus
The duplication is happening at the **segment storage layer**, not the buffer.

## Investigation Steps

### Step 1: Audit SegmentWriter
**File**: `crates/chronik-storage/src/segment_writer.rs`
- [ ] Check how segments track their offset ranges
- [ ] Verify new segments start at correct offset
- [ ] Ensure no overlapping offset ranges between segments
- [ ] Check if messages are written multiple times

### Step 2: Segment Metadata Tracking
- [ ] Add explicit start_offset/end_offset tracking
- [ ] Prevent overlapping ranges
- [ ] Add validation for contiguous segments

### Step 3: Fetch from Segments
**File**: `crates/chronik-server/src/fetch_handler.rs`
- [ ] Verify segment reader doesn't return duplicates
- [ ] Check segment iteration logic
- [ ] Ensure proper offset boundary handling

### Step 4: Produce-to-Flush Flow
**File**: `crates/chronik-server/src/produce_handler.rs`
- [ ] Track which messages have been flushed
- [ ] Prevent re-flushing of already persisted messages
- [ ] Verify flush_partition only writes new messages

## Key Files to Investigate

1. **segment_writer.rs** - Core segment writing logic
2. **segment.rs** - Segment structure and metadata
3. **produce_handler.rs** - flush_partition() method
4. **fetch_handler.rs** - fetch_records() segment reading

## Findings

### Segment Writer Analysis
✅ **Offset Tracking**: SegmentWriter properly tracks `last_offsets` to ensure continuity
✅ **Base Offset Logic**: New segments correctly use `last_offset + 1` as base_offset
✅ **Metadata Tracking**: Segments track start_offset/end_offset correctly

### Flush Logic Analysis
✅ **Buffer Update**: Fixed - buffer only updated once in handle_partition_produce
✅ **Mark Flushed**: Properly clears ALL records from buffer after flush
⚠️ **Potential Issue**: Messages might be written to multiple segments

### Fetch Logic Analysis
✅ **Phase Separation**: Clean separation between segment fetch (Phase 1) and buffer fetch (Phase 2)
✅ **Offset Boundaries**: Buffer only returns records with offset > max_segment_offset
🔴 **CRITICAL ISSUE**: Segments might contain overlapping offset ranges

## Solution Implementation

### Changes Made

#### 1. Buffer Deduplication Fix
**File**: `crates/chronik-server/src/fetch_handler.rs`
- Modified `update_buffer` method to prevent duplicate records
- Now checks if records already exist in buffer before adding
- Only appends truly new records instead of replacing entire buffer
- Added extensive logging to track buffer operations

#### 2. Diagnostic Logging
**Files**: 
- `crates/chronik-server/src/fetch_handler.rs`
- `crates/chronik-server/src/produce_handler.rs`
- Added warning-level logs to track:
  - Records being sent to buffer with offset ranges
  - Existing buffer contents before updates
  - Number of records added vs skipped as duplicates

### Root Cause
The issue was that `update_buffer` was being called multiple times with cumulative data for each batch. When the client sent 10 messages, they were split into multiple batches, and each subsequent batch included ALL previous messages plus new ones, causing duplication.

### Tests Added
- Created `compat-tests/test_single_partition_no_dup.py` to verify fix

## Verification Results
*Testing in progress - server startup issues preventing full validation*