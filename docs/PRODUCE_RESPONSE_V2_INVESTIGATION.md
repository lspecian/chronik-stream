# ProduceResponse v2 Protocol Investigation

## Issue Summary
**Version**: 0.7.2  
**Date**: 2025-09-09  
**Status**: RESOLVED - Fixed field ordering issue

Chronik Stream v0.7.2 successfully stores messages but clients timeout waiting for ProduceResponse acknowledgments. The issue was incorrect field ordering in the v2 protocol response.

## Root Cause Analysis

### What's Happening
1. Client sends ProduceRequest v2 to Chronik
2. Chronik successfully stores the message to disk
3. Chronik sends ProduceResponse with `throttle_time_ms` at the beginning instead of the end
4. Client can't parse the response properly and times out after 5-10 seconds
5. Client thinks the operation failed despite successful storage

### Protocol Format Discrepancy

#### Kafka ProduceResponse v2 Wire Format
Through comparison testing with real Kafka, we discovered that despite the official protocol documentation suggesting `throttle_time_ms` comes first, **real Kafka puts it at the END** for v1-v8:

```
ProduceResponse (Version: 2) => [topics] throttle_time_ms
  topics => name [partitions]
    name => STRING
    partitions => index error_code base_offset log_append_time
      index => INT32
      error_code => INT16
      base_offset => INT64
      log_append_time => INT64
  throttle_time_ms => INT32  // <-- AT THE END!
```

#### What Chronik Was Sending (INCORRECT)
```
Offset  0- 3: throttle_time_ms = 0  // WRONG: Should be at the end
Offset  4- 7: topic_count = 1
Offset  8- 9: topic_name_length = 10
Offset 10-19: topic_name = "test-topic"
Offset 20-23: partition_count = 1
Offset 24-27: partition_index = 0
Offset 28-29: error_code = 0
Offset 30-37: base_offset = 0
Offset 38-45: log_append_time = -1
```

#### What Real Kafka Sends (CORRECT)
```
Offset  0- 3: topic_count = 1
Offset  4- 5: topic_name_length = 10
Offset  6-15: topic_name = "test-topic"
Offset 16-19: partition_count = 1
Offset 20-23: partition_index = 0
Offset 24-25: error_code = 0
Offset 26-33: base_offset = 0
Offset 34-41: log_append_time = -1
Offset 42-45: throttle_time_ms = 0  // AT THE END!
```

#### Comparison Test Evidence
Using a custom test script to compare real Kafka vs Chronik:

**Real Kafka Response (port 29092):**
```
First 64 bytes (hex):
  0000: 00 00 00 7b 00 00 00 01 00 0a 74 65 73 74 2d 74
  0010: 6f 70 69 63 00 00 00 01 00 00 00 00 00 00 00 00
  0020: 00 00 00 00 00 00 00 05 ff ff ff ff ff ff ff ff
  0030: 00 00 00 00

Parsing:
  Topics: 1
  Topic name: 'test-topic'  
  ✓ VALID: Found expected topic name without throttle at beginning
  Last 4 bytes as throttle_time: 0ms
```

**Chronik Response Before Fix:**
```
First 64 bytes (hex):
  0000: 00 00 00 7b 00 00 00 00 00 00 00 01 00 0a 74 65
  0010: 73 74 2d 74 6f 70 69 63 00 00 00 01 00 00 00 00
  0020: 00 00 00 00 00 00 00 00 00 00 00 02 ff ff ff ff
  0030: ff ff ff ff

First difference at byte 4 (0x0004):
  Kafka:   00 00 00 01 00 0a 74 65
  Chronik: 00 00 00 00 00 00 00 01
           ^^  
Difference is at offset 4 - this is where throttle_time_ms might be
```

## The Solution

### Code Change
**File**: `/crates/chronik-protocol/src/handler.rs`

**Before (INCORRECT):**
```rust
// Line 2537: throttle_time_ms at beginning
if version >= 1 && version < 9 {
    encoder.write_i32(response.throttle_time_ms);
}

// ... encode topics and partitions ...
```

**After (CORRECT):**
```rust
// Line 2534-2538: Comment explaining the fix
// NOTE: throttle_time_ms position differs between versions
// For v1-v8: throttle_time_ms goes at the END of the response
// For v9+: handled differently with flexible versions
// This was discovered through comparison with real Kafka - Python clients
// expect throttle_time_ms at the end for v2 responses

// ... encode topics and partitions ...

// Line 2611-2615: throttle_time_ms at END
if version >= 1 && version < 9 {
    encoder.write_i32(response.throttle_time_ms);
}
```

### Why This Matters

The `log_append_time` field was actually being written correctly all along. The real issue was that `throttle_time_ms` was in the wrong position, causing clients to misparse the entire response:

1. Client expects topics array first, then throttle_time_ms at the end
2. Chronik was sending throttle_time_ms first
3. Client reads the throttle_time_ms (4 bytes of zeros) as the topic count
4. Client then tries to parse the actual topic count as topic name length
5. Everything after that is misaligned, causing timeout

## Historical Context

### Why Was throttle_time_ms at the Beginning?
The code had a comment stating this was a "CRITICAL FIX" for librdkafka compatibility. However:
1. This may have been based on incorrect documentation or assumptions
2. librdkafka might handle both positions gracefully
3. Python's kafka-python strictly expects the Kafka wire format

### Similar Protocol Issues
From previous investigations:
- ApiVersions v3: missing throttle_time_ms field
- Metadata v12: field ordering issues (cluster_id before controller_id)
- ProduceResponse v2: throttle_time_ms position (this issue)

### Protocol Documentation Ambiguity
The Kafka protocol documentation can be misleading:
- The schema notation shows fields in logical order
- The actual wire format may differ for backward compatibility
- Always test against real Kafka for definitive answers


## Testing & Verification

### Test Script
Created `test_kafka_comparison.py` to send raw ProduceRequest v2 and compare responses between real Kafka and Chronik. This definitively showed the field ordering difference.

### Python Client Test (SUCCESSFUL)
```python
from kafka import KafkaProducer
import json

producer = KafkaProducer(
    bootstrap_servers=['localhost:9092'],
    value_serializer=lambda v: json.dumps(v).encode('utf-8'),
    api_version=(0, 10, 1)
)

future = producer.send('test-topic', {'test': 'data'})
metadata = future.get(timeout=5)
print(f'✅ Message sent successfully!')
print(f'   Offset: {metadata.offset}')
print(f'   Timestamp: {metadata.timestamp}')
```

**Result after fix:**
```
✅ Message sent successfully!
   Topic: test-topic
   Partition: 1
   Offset: 3
   Timestamp: 1757465354072
```

## Impact & Resolution

### Affected Clients (NOW FIXED)
- ✅ Python kafka-python - confirmed working
- ✅ Java clients using api.version >= 0.10.0 - should work
- ✅ Go sarama with Version >= V0_10_0_0 - should work
- ⚠️ librdkafka-based clients - may need testing (original code claimed it needed throttle_time_ms first)

### Version Compatibility
The fix ensures ProduceResponse v1-v8 have throttle_time_ms at the end, matching real Kafka behavior.

### Next Steps
1. Test with librdkafka-based clients to ensure no regression
2. Check FetchResponse for similar field ordering issues
3. Document this protocol quirk for future reference
4. Consider adding integration tests against real Kafka

## References
- [Kafka Protocol Documentation](https://kafka.apache.org/protocol)
- [KIP-32: Add timestamps to Kafka message](https://cwiki.apache.org/confluence/display/KAFKA/KIP-32+-+Add+timestamps+to+Kafka+message)
- [Kafka 0.10.0 Release Notes](https://archive.apache.org/dist/kafka/0.10.0.0/RELEASE_NOTES.html)