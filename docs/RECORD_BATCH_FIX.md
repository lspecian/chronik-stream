# Record Batch Format v2 Fix Progress

## Issue Summary
**Date:** 2025-09-03
**Problem:** "Bad message format" error when librdkafka sends Produce requests
**Root Cause:** librdkafka v2.11.1 sends compact encoding in Produce v9

## Current Status - MAJOR PROGRESS!
- ✅ Fixed UTF-8 parsing errors in Produce API v9 header
- ✅ Fixed librdkafka compatibility issues for client_id
- ✅ Fixed transactional_id parsing - uses compact string in v9+
- ✅ Fixed topic/partition array parsing - uses compact arrays in v9+
- ✅ Fixed tagged field handling for v9+
- ✅ Server now successfully receives and stores messages from librdkafka
- ⚠️ librdkafka still reports "Bad message format" - likely record batch validation issue

## Investigation Log

### Step 1: Analyze Current Error
Looking at the server logs, after fixing the Produce request parsing, we now get to the record batch but fail with "Bad message format".

### Step 2: Understanding Record Batch Format v2
Record Batch v2 (magic byte = 2) structure:
```
baseOffset: int64
batchLength: int32
partitionLeaderEpoch: int32
magic: int8 (must be 2)
crc: int32 (CRC32C of data from attributes to end)
attributes: int16
lastOffsetDelta: int32
baseTimestamp: int64
maxTimestamp: int64
producerId: int64
producerEpoch: int16
baseSequence: int32
records: [Record]
```

### Step 3: The Fix - Compact Encoding in v9+

The critical issue was that librdkafka sends **compact encoding** for Produce v9+ requests, but we were trying to read normal encoding:

**Before (incorrect):**
```rust
// Always used normal string for transactional_id
let id = decoder.read_string()?;
```

**After (correct):**
```rust
// v9+ uses compact string for transactional_id
let id = if flexible {
    decoder.read_compact_string()?
} else {
    decoder.read_string()?
};
```

Similar fixes were needed for:
- Topic/partition arrays (use `read_unsigned_varint()` for counts)
- Topic names (use `read_compact_string()`)
- Tagged fields (must consume them even if not used)

## Remaining Issue
The server now successfully parses and stores messages, but librdkafka reports "Bad message format" when receiving the response. This suggests the record batch validation or response encoding still has issues.