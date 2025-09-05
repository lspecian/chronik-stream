# librdkafka v2.11.1 Compatibility Investigation

## Current Issue
librdkafka v2.11.1 reports "Read underflow" when connecting to Chronik Stream, despite the ApiVersions v3 response being correctly formatted according to Kafka spec.

## What We Know Works
1. Python client can successfully parse our ApiVersions v3 response
2. Response is 431 bytes total (correct size)
3. Response structure matches Kafka specification

## Attempts and Results

### Attempt 1: Fix Header Tagged Fields
- **Issue**: Extra 0x00 byte between correlation ID and response body
- **Fix**: Skip header tagged fields for ApiVersions
- **Result**: ✅ Fixed malloc crash, but now getting "Read underflow"

### Attempt 2: Fix Response Header Flexibility
- **Issue**: ApiVersions response header should NOT be flexible even though body is
- **Fix**: Set `is_flexible = false` for ApiVersions response headers
- **Result**: ✅ Correct, but still "Read underflow"

### Attempt 3: Remove Extra Tagged Field at End
- **Issue**: Extra tagged field at end of v3 response (424 bytes instead of 423)
- **Fix**: Only write end tagged field for v4+
- **Result**: ✅ Correct size, but still "Read underflow"

### Attempt 4: Add throttle_time_ms
- **Issue**: librdkafka expects throttle_time for v1+ (found in source code analysis)
- **Fix**: Added throttle_time at END of response for v3
- **Result**: ❌ Still "Read underflow"

### Attempt 5: INT16 vs INT8 for min/max versions
- **Issue**: Kafka spec says v3+ uses INT8, but we were using INT16
- **Tried**: Both INT8 and INT16
- **Result**: ❌ Both fail with "Read underflow"

### Attempt 6: Add throttle_time_ms after API array for v3
- **Issue**: librdkafka source shows it expects throttle_time for v1+
- **Fix**: Added throttle_time_ms AFTER api_versions array for v3
- **Result**: ❌ Still "Read underflow" (response now 431 bytes)

### Attempt 7: Add throttle_time AND tagged fields for v3
- **Issue**: v3 needs both throttle_time_ms and tagged fields at the end
- **Fix**: Added throttle_time_ms followed by tagged fields (0x00) for v3
- **Result**: ✅ SUCCESS! ApiVersions now works (428 body + 4 header = 432 total)
- **New Issue**: Now Metadata v12 fails with underflow

## Metadata v12 Investigation

### Finding 1: Request Parsing Issue
- **Issue**: Server returned only 8 bytes (just header, no body)
- **Cause**: Our parsing was failing silently when request was malformed
- **Fix**: Added debug logging to identify parsing failure points

### Finding 2: Test Script Bug
- **Issue**: Test script was missing include_cluster_authorized_operations field
- **Fix**: Added missing field to test script
- **Result**: Python client now receives full 1148-byte response ✅

### Finding 3: librdkafka Still Fails
- **Issue**: librdkafka receives only 4 bytes for Metadata v12
- **Observation**: Same server responds correctly to Python (1148 bytes) but not librdkafka (4 bytes)
- **Hypothesis**: librdkafka is sending a different/malformed request that our parser fails on

## Key Observations

1. **librdkafka source code analysis** (rdkafka_request.c:3178-3179):
   - Expects throttle_time_ms for ALL v1+ versions
   - This contradicts Kafka spec which says v3 has no throttle_time

2. **Response size discrepancy**:
   - Our buffer: 427 bytes (with throttle_time)
   - Python receives: 431 bytes (correct)
   - librdkafka: Fails before parsing

3. **Field encoding for v3+**:
   - Kafka spec: INT8 for min/max versions in v3+
   - librdkafka: Might expect INT16 (needs testing)

## Next Steps to Try

1. **Create conditional encoding based on version**:
   - v0-2: Use INT16 for min/max
   - v3+: Use INT8 for min/max (per spec)
   - Add proper conditionals for throttle_time placement

2. **Test with different librdkafka versions**:
   - Try older versions (v2.0.0, v1.9.0)
   - See if issue is specific to v2.11.1

3. **Capture actual librdkafka request**:
   - Use tcpdump/Wireshark to see exact bytes
   - Compare with what we're sending

4. **Try different ApiVersions versions**:
   - Force v0 response for librdkafka
   - See if older protocol works

5. **Debug librdkafka parsing**:
   - Build librdkafka with debug symbols
   - Step through parsing code

## Current Code Structure

```rust
// ApiVersions v3 encoding (current):
- Error code (INT16)
- Array length (COMPACT_VARINT) 
- For each API:
  - api_key (INT16)
  - min_version (INT8 or INT16?) <- INCONSISTENT
  - max_version (INT8 or INT16?) <- INCONSISTENT  
  - tagged_fields (VARINT)
- throttle_time_ms (INT32) at end <- ADDED FOR COMPATIBILITY
```

## Test Results Matrix

| Client | Version | INT8 | INT16 | With throttle | Without throttle | Result |
|--------|---------|------|-------|---------------|------------------|--------|
| Python | N/A | ✅ | ✅ | ✅ | ✅ | Works |
| librdkafka | v2.11.1 | ❌ | Testing... | ❌ | ❌ | Read underflow |
| Go (sarama) | N/A | ? | ? | ? | ? | Not tested |

## Current Testing Configuration
- Using INT16 for min/max versions (despite spec saying INT8)
- throttle_time at END of response for v3
- Response size: 431 bytes with INT16

## CRITICAL DISCOVERY: librdkafka Sends Metadata v9, Not v12!

### Finding 4: Version Mismatch
- **Discovery**: Despite negotiating v12 support, librdkafka sends Metadata v9 requests
- **Evidence**: Captured actual requests from librdkafka:
  - Request #1: API v9, CorrID 2, topics=NULL (all topics)
  - Request #2: API v9, CorrID 3, topics=empty array
- **Issue**: Our parser expects compact string for client ID in v9 (flexible version)
- **Reality**: librdkafka sends normal string even for v9 (INT16 length prefix)

### Metadata v9 Request Format (What librdkafka sends):
```
- API key: 3 (Metadata)
- API version: 9
- Correlation ID: 4 bytes
- Client ID: NORMAL STRING (INT16 length + data) <- NOT COMPACT!
- Header tagged fields: 0x00
- Topics: Compact array (0x00 = NULL, 0x01 = empty)
- Allow auto topic creation: BOOL
- Include cluster authorized ops: BOOL (missing in first request!)
- Include topic authorized ops: BOOL (missing in first request!)
- Body tagged fields: 0x00
```

### Root Cause:
librdkafka uses a hybrid format for ALL flexible Metadata versions (v9, v12, v13) - normal string for client ID despite these being flexible versions. This causes our parser to fail when expecting compact string encoding.

### Fix Applied:
Added special handling in parser.rs to use normal string for client ID when:
- API is Metadata (api_key=3)
- Version is flexible (v9+)
- Client is librdkafka (detected by this pattern)

## Current Status After Fix

### What Works:
1. ✅ ApiVersions v3 parsing and response
2. ✅ Client ID parsing for all Metadata versions
3. ✅ Metadata request parsing (topics, flags)
4. ✅ Metadata response generation (1148 bytes for full, 49 for empty)

## Summary of All Fixes Applied

1. **ApiVersions v3 Response Fix**:
   - Added throttle_time_ms at end of response (after API array)
   - Added tagged fields after throttle_time
   - Response header is NOT flexible (no tagged fields)
   - Total size: 432 bytes (4 header + 428 body)

2. **Metadata Request Parsing Fix**:
   - Special handling for librdkafka's client ID encoding
   - Uses normal string (INT16 length) even in flexible versions (v9+)
   - Applies to all Metadata versions >= 9

3. **Metadata v12 Response Field Ordering Fix** (from METADATA_V12_INVESTIGATION.md):
   - Fixed cluster_id and controller_id ordering (cluster_id must come FIRST)
   - Limited cluster_authorized_operations to v8-10 only (not v11+)
   - Added topic_id (UUID) field for v10+

4. **Metadata v12 Request Missing Tagged Fields Fix**:
   - librdkafka v2.11.1 sends malformed first request missing body tagged fields
   - Added check for remaining buffer bytes before reading tagged fields
   - If buffer is empty, assume tagged field count = 0 (librdkafka compatibility)

## Current Status

librdkafka v2.11.1 is FULLY COMPATIBLE with Chronik Stream after all fixes:
- ✅ ApiVersions v3 works correctly
- ✅ Metadata v12 works correctly (both requests and responses)
- ✅ First connection attempt succeeds without errors
- ✅ Can retrieve metadata and list topics/brokers
- ✅ Connection is stable from the start
- ✅ No more "Incomplete varint" or "Protocol parse failure" errors

## Known librdkafka v2.11.1 Quirks

1. **Client ID encoding**: Uses normal strings in flexible Metadata versions
2. **Missing tagged fields**: First Metadata request may omit body tagged fields
3. **ApiVersions v3**: Expects throttle_time_ms despite ambiguous spec
4. **Field ordering**: Strictly expects cluster_id before controller_id