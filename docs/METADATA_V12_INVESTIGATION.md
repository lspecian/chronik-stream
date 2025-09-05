# Metadata v12 Response Investigation

## Problem Statement
librdkafka v2.11.1 fails to parse Metadata v12 responses from Chronik Stream with:
- "Protocol parse failure at 191/1144"
- "109 partitions: tmpabuf memory shortage"
- Byte 191 (0x6D) is being read as partition count but is actually part of topic name string

## Initial Analysis

### Captured Response Data
- Total response size: 1148 bytes (4 header + 1144 body)
- Parse failure at byte 191 of body (position 195 in full response)
- Byte value at position: 0x6D (109 decimal, ASCII 'm')
- Context: Part of string "comp-none-5b30cdc1"

### Response Structure (First 200 bytes)
```
00000000: 00 00 00 03  // Correlation ID: 3
00000004: 00 00 00 00  // Throttle time: 0
00000008: 00 02        // Broker count: 2 (WRONG! Should be compact)
0000000A: 00 00 00 01  // Broker ID: 1
0000000E: 08 30 2e 30 2e 30 2e 30  // Host: "0.0.0.0" (8 bytes with length)
00000016: 00 00 23 84  // Port: 9092
0000001A: 00 00        // Rack: null
0000001C: 0f 63 68 72 6f 6e 69 6b 2d 73 74 72 65 61 6d  // "chronik-stream"
...
000000BF: 6D          // This is byte 191 - the 'm' in "comp-"
```

## Key Issues Identified

1. **Broker Array Encoding**: Using INT16 (00 02) instead of compact varint
2. **String Encoding**: Appears to be using non-compact strings in some places
3. **Structure Misalignment**: librdkafka expects different field ordering/encoding

## Investigation Steps

### Step 1: Analyze Kafka Protocol Spec for Metadata v12
- v12 uses flexible/compact encoding
- All arrays should use compact varint (length + 1)
- All strings should use compact strings (varint length + 1)
- Structure should be:
  ```
  throttle_time_ms: INT32
  brokers: COMPACT_ARRAY of:
    node_id: INT32
    host: COMPACT_STRING
    port: INT32
    rack: COMPACT_STRING (nullable)
    tagged_fields: TAGGED_FIELDS
  controller_id: INT32
  cluster_id: COMPACT_STRING (nullable)
  cluster_authorized_operations: INT32
  topics: COMPACT_ARRAY of:
    error_code: INT16
    name: COMPACT_STRING
    topic_id: UUID
    is_internal: BOOLEAN
    partitions: COMPACT_ARRAY
    topic_authorized_operations: INT32
    tagged_fields: TAGGED_FIELDS
  tagged_fields: TAGGED_FIELDS
  ```

### Step 2: Check Current Implementation
Need to examine:
- How we encode broker array
- How we encode strings (compact vs normal)
- Field ordering in response

## Findings

### Issue Found: Broker Array Not Using Compact Encoding

The captured response shows:
```
00 00 00 03  // Correlation ID (header) - correct
00 00 00 00  // Throttle time (body) - correct
00 02        // Broker count - WRONG! Should be 0x02 (compact varint)
```

For 1 broker, compact encoding should be:
- Value: 1
- Encoded: (1 + 1) = 2 = 0x02 (single byte)

But we're seeing `00 02` which is INT16 encoding.

### Code Analysis

In `handler.rs:encode_metadata_response()`:
- Line 2539: `let flexible = version >= 9;` - Correctly identifies v12 as flexible
- Line 2550-2554: Code SHOULD use `write_compact_array_len` for flexible
- Debug output confirms: "flexible=true"

BUT the actual bytes show INT16 is being written!

### Hypothesis

There might be an issue with:
1. The encoder's `write_compact_array_len` implementation - TESTED: Works correctly
2. OR the response is being double-encoded somewhere
3. OR there's a mismatch between what's being logged and what's actually written

### Additional Finding: Wrong Broker Count

The response shows `00 02` which decodes as INT16 value 2 (meaning 2 brokers).
But logs say "1 broker". This suggests either:
- The broker list is being modified after logging
- There's a phantom broker being added
- The wrong response is being captured

## Fix Plan

### Immediate Steps
1. Add detailed logging to track exact bytes being written
2. Log broker array contents before and after encoding
3. Check if there's any broker filtering happening
4. Verify the flexible flag is actually being used

### Root Cause Suspects
1. **Double Encoding**: Response might be encoded once with flexible, then re-encoded without
2. **Wrong Function**: A different encoding function might be called
3. **Broker List Mutation**: The broker list might change between logging and encoding
4. **Buffer Corruption**: The buffer might be corrupted or reused

## Attempt 9: Added byte-level logging (BREAKTHROUGH!)

Added detailed logging to track exact bytes being written:
- `write_compact_array_len` is correctly writing `[02]` for 1 broker
- Buffer position tracking shows only 1 byte written for broker array length
- The encoding is CORRECT on the server side!

### New Error Discovery

librdkafka error: "Protocol read buffer underflow for Metadata v12 at 255/1144"
- First response was 1143 bytes (full metadata with 7 topics)
- Error at position 255 trying to read 18446744073709551612 bytes (0xFFFFFFFFFFFFFFFC = -4 as unsigned)
- This looks like a null string being read as a non-null string!

The error is likely in the cluster_id field around byte 255.

## SOLUTION FOUND (Partial)

The issue is NOT with the broker array encoding. The server is correctly writing compact varint `[02]`.

Issues fixed:
1. Field ordering: controller_id must come BEFORE cluster_id (fixed)
2. Missing fields: Added topic_id (UUID) for v10+ and cluster_authorized_operations for v8+
3. Duplicate field: Removed duplicate cluster_authorized_operations

## PROBLEM SOLVED: Empty Response Body

The empty response issue is fixed. The response now includes the full body (1259 bytes).

## REMAINING ISSUE: Parse Error at Byte 30

librdkafka still fails to parse the response with:
- "Protocol parse failure for Metadata v12(flex) at 30/1260"  
- "98 internal topics: tmpabuf memory shortage"

At byte 30, librdkafka is reading 0x62 (98) which it interprets as the topic count.
This suggests there's still a field ordering or encoding issue in the response.

## Summary of Fixes Applied

1. ✅ Fixed broker array encoding - using compact varint correctly
2. ✅ Fixed field ordering - controller_id must come BEFORE cluster_id
3. ✅ Added missing fields - topic_id (UUID) for v10+ and cluster_authorized_operations for v8+
4. ✅ Removed duplicate cluster_authorized_operations field
5. ✅ Fixed response to include body (was only sending header)

## SOLUTION COMPLETE! ✅

### Root Cause Analysis
After analyzing librdkafka v2.11.1 source code, found two critical issues:

1. **Field Ordering Mismatch**: 
   - librdkafka expects: cluster_id → controller_id
   - We were sending: controller_id → cluster_id
   
2. **Extra Field for v11+**:
   - librdkafka only reads cluster_authorized_operations for v8-10
   - We were sending it for all v8+ (including v11 and v12)

### Final Fix Applied

In `handler.rs:encode_metadata_response()`:

```rust
// CORRECT ORDER: Cluster ID comes BEFORE controller ID
if version >= 2 {
    if flexible {
        encoder.write_compact_string(response.cluster_id.as_deref());
    } else {
        encoder.write_string(response.cluster_id.as_deref());
    }
}

if version >= 1 {
    encoder.write_i32(response.controller_id);
}

// Only write cluster_authorized_operations for v8-v10
if version >= 8 && version <= 10 {
    encoder.write_i32(-2147483648); // INT32_MIN indicates null
}
```

### Test Results

librdkafka v2.11.1 now successfully:
- ✅ Connects to Chronik Stream
- ✅ Retrieves metadata (1 broker, 7 topics)
- ✅ Can produce messages
- ✅ No more "Protocol parse failure" errors
- ✅ No more "tmpabuf memory shortage" errors

### Complete List of Fixes

1. **ApiVersions v3**: Added throttle_time_ms and tagged fields at the end of response body
2. **Metadata request parsing**: Handle normal strings for client ID in flexible versions (librdkafka quirk)
3. **Metadata response field ordering**: Fixed cluster_id to come before controller_id
4. **Missing fields**: Added topic_id (UUID) for v10+
5. **Version-specific fields**: Limited cluster_authorized_operations to v8-10 only
6. **Response body inclusion**: Fixed issue where only header was being sent