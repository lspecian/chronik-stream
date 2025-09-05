# Remaining Issues (Not Related to librdkafka Connection/Metadata)

## Overview
After successfully fixing all librdkafka v2.11.1 connection and metadata compatibility issues, there are some remaining issues related to other Kafka APIs that still need implementation or fixes.

## 1. IPv6 Connection Attempts (Harmless)

### Issue
```
%3|1756870813.207|FAIL|test-librdkafka#producer-1| [thrd:localhost:9092/bootstrap]: localhost:9092/bootstrap: Connect to ipv6#[::1]:9092 failed: Connection refused
```

### Status
**Not a real issue** - This is expected behavior

### Details
- librdkafka tries IPv6 first when connecting to `localhost`
- When IPv6 fails, it automatically falls back to IPv4
- Connection succeeds on IPv4 (127.0.0.1:9092)
- This is normal behavior and doesn't affect functionality

### Resolution
No action needed. This could be suppressed by:
- Binding to `127.0.0.1` instead of `0.0.0.0`
- Or explicitly configuring librdkafka to use IPv4 only

## 2. Produce API v9 UTF-8 Parsing Errors

### Issue
```
ERROR Internal server error in request from 127.0.0.1:51566: Protocol error: Invalid UTF-8 in string: invalid utf-8 sequence of 1 bytes from index 21
```

### Status
**Needs Fix** - Produce API implementation issue

### Details
- Occurs when librdkafka sends Produce requests (API key 0, version 9)
- The error suggests we're trying to read a string where there isn't one
- Likely a mismatch in how we're parsing the Produce v9 request format
- This prevents messages from being successfully produced to topics

### Root Cause
- Produce v9 uses flexible/compact encoding
- We may be incorrectly parsing some fields as strings
- Could be related to record batch format or header parsing

### Impact
- Messages cannot be produced to topics
- Delivery reports show "Local: Bad message format" errors

### Next Steps
1. Analyze Produce v9 request format specification
2. Compare our parsing logic with Kafka protocol spec
3. Fix string parsing in Produce request handler
4. Test with various message formats and sizes

## 3. Consumer Group APIs (Partial Implementation)

### Issue
Consumer functionality works but some group coordination features may be incomplete

### Status
**Partially Implemented**

### Details
Based on the test output, basic consumer creation and subscription work, but:
- Group coordination may not be fully implemented
- Offset management might be incomplete
- Consumer group rebalancing needs verification

### APIs Potentially Affected
- JoinGroup (API 11)
- SyncGroup (API 14)
- Heartbeat (API 12)
- LeaveGroup (API 13)
- OffsetCommit (API 8)
- OffsetFetch (API 9)

### Next Steps
1. Test consumer group functionality thoroughly
2. Implement missing group coordination features
3. Verify offset commit/fetch operations

## 4. Message Delivery Confirmation

### Issue
```
%4|1756870830.441|TERMINATE|test-librdkafka#producer-1| [thrd:app]: Producer terminating with 1 message (30 bytes) still in queue or transit
```

### Status
**Related to Issue #2** - Consequence of Produce API errors

### Details
- Messages remain in librdkafka's queue because Produce requests fail
- librdkafka reports messages as undelivered on shutdown
- This is a symptom of the Produce API parsing issue

### Resolution
Will be resolved when Produce API v9 is fixed

## 5. Record Batch Format Support

### Issue
The test shows delivery failures with "Bad message format"

### Status
**Needs Investigation**

### Details
- May be related to how we handle Kafka record batch format v2
- Could involve compression, CRC validation, or record headers
- Affects both produce and fetch operations

### Next Steps
1. Verify record batch v2 format implementation
2. Check compression codec support (gzip, snappy, lz4, zstd)
3. Validate CRC32C calculation for record batches

## Priority Order for Fixes

1. **High Priority**: Produce API v9 UTF-8 parsing (#2)
   - Blocks message production
   - Most critical for basic functionality

2. **Medium Priority**: Record Batch Format (#5)
   - Affects message integrity
   - Important for data correctness

3. **Low Priority**: Consumer Group APIs (#3)
   - Basic consumer works
   - Advanced features can be added incrementally

4. **No Action**: IPv6 attempts (#1)
   - Harmless, doesn't affect functionality

## Testing Recommendations

1. Create dedicated test files for each API:
   - `test_produce_v9.go` - Test various message formats
   - `test_consumer_groups.go` - Test group coordination
   - `test_record_batches.go` - Test different batch formats

2. Use Wireshark or tcpdump to capture actual Kafka protocol messages

3. Compare with real Kafka broker responses for validation

## Success Metrics

Once all issues are resolved:
- ✅ Produce API accepts all valid message formats
- ✅ No UTF-8 parsing errors in server logs
- ✅ Messages successfully written to topics
- ✅ Consumer groups can coordinate and rebalance
- ✅ All record batch formats are supported
- ✅ Full compatibility with librdkafka v2.11.1

## Notes

- The core connection and metadata exchange is now rock-solid
- These remaining issues are isolated to specific APIs
- Each can be fixed independently without affecting the others
- The architecture is sound; these are implementation details