# Fix Plan for v1.3.20 - Segment Reader Truncation Bug

## Problem Statement

**Chronik v1.3.19 has a critical bug where only ~2 records are returned from segments containing 20 records, resulting in 85-90% data loss.**

### Evidence
- **Test 1 (30 records)**: SUCCESS - But test completed BEFORE first rotation
- **Test 2 (20 MTG decks)**: FAILURE - Only 3/20 records returned (offsets 0, 1, 19)
  - 2 records from segment (offsets 0-1)
  - 1 record from buffer (offset 19)
  - **17 records missing** (offsets 2-18)

### Root Cause Analysis

The bug is in **`fetch_from_segment()`** method in `crates/chronik-server/src/fetch_handler.rs` at lines 802-828.

**Current code** (BUGGY):
```rust
let record_batch = if !segment.indexed_records.is_empty() {
    // Decode the RecordBatch from indexed_records
    RecordBatch::decode(&segment.indexed_records)?  // ❌ ONLY DECODES FIRST BATCH!
} else if !segment.raw_kafka_batches.is_empty() {
    // Decode from raw Kafka batches
    let kafka_batch = KafkaRecordBatch::decode(&segment.raw_kafka_batches)?;  // ❌ ONLY DECODES FIRST BATCH!

    // Convert to RecordBatch...
    RecordBatch { records }
} else {
    return Ok(vec![]);
};
```

**The Problem**:
1. A segment can contain MULTIPLE Kafka record batches
2. When 20 messages are written, they may be split into 10 batches of 2 messages each
3. `RecordBatch::decode()` and `KafkaRecordBatch::decode()` only decode the FIRST batch
4. The remaining 9 batches (18 records) are never decoded

**Why Test 1 passed**:
- Test completed before rotation, so data was still in buffer
- Segment reading code path not fully exercised

**Why Test 2 failed**:
- After rotation, buffer was cleared
- Segment reader only returned first 2 records from first batch
- Last record (offset 19) was still in buffer because it was written after last flush

## Fix Strategy

### Option 1: Fix RecordBatch::decode to decode ALL batches (PREFERRED)
Modify `RecordBatch::decode()` to loop through all batches in the byte array

**Pros**:
- Fixes the root cause
- Works for all callers of RecordBatch::decode
- Clean separation of concerns

**Cons**:
- Need to modify low-level decode logic
- May affect other code paths

### Option 2: Fix fetch_from_segment to loop through batches
Keep RecordBatch::decode as-is, but call it multiple times in fetch_from_segment

**Pros**:
- Minimal changes to existing decode logic
- Easy to add logging for debugging

**Cons**:
- Need to manually track position in byte array
- Duplicates batch-loop logic if needed elsewhere

## Recommended Approach: Option 2 (Safer)

Modify `fetch_from_segment()` to loop through ALL batches in the segment data. This is safer because it doesn't risk breaking other code that uses RecordBatch::decode().

### Implementation Steps

1. **Investigate how multiple batches are stored in segment**
   - Check if indexed_records or raw_kafka_batches contains multiple batches
   - Understand the format (likely concatenated batches)

2. **Fix fetch_from_segment() to loop through all batches**
   ```rust
   // Instead of single decode:
   let record_batch = RecordBatch::decode(&segment.indexed_records)?;

   // Do multiple decodes:
   let mut all_records = Vec::new();
   let mut offset = 0;
   while offset < segment.indexed_records.len() {
       let batch = RecordBatch::decode(&segment.indexed_records[offset..])?;
       all_records.extend(batch.records);
       offset += batch.size_in_bytes(); // Need to track batch size
   }
   ```

3. **Add detailed logging**
   ```rust
   tracing::warn!(
       "SEGMENT→DECODE: Processing segment {}, total_bytes={}, batch_count={}",
       segment_info.segment_id,
       segment.indexed_records.len(),
       batch_count
   );
   tracing::warn!(
       "SEGMENT→RECORDS: Decoded {} total records from {} batches",
       all_records.len(),
       batch_count
   );
   ```

4. **Test with 20-record dataset**
   - Write 20 messages
   - Wait for flush/rotation
   - Query all 20 messages
   - Verify 20/20 returned

## Files to Modify

1. **`crates/chronik-server/src/fetch_handler.rs`** (lines 800-828)
   - Modify `fetch_from_segment()` to loop through all batches
   - Add detailed logging

## Success Criteria

- ✅ Test 1 (30 records): Still passes
- ✅ Test 2 (20 MTG decks): Returns 20/20 records
- ✅ All records from segment files are fetched
- ✅ No data loss after rotation
- ✅ Logging shows correct batch/record counts

## Version

- Current: v1.3.19 (broken)
- Target: v1.3.20 (fixed)
