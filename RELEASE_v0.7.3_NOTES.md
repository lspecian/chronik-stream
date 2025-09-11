# Chronik Stream v0.7.3 Release Notes

## Critical Fix: ProduceResponse v2 Protocol Compatibility

### Summary
Fixed a critical protocol compatibility issue where Kafka clients would timeout waiting for ProduceResponse acknowledgments, despite messages being successfully stored. This affected all clients using Kafka protocol v2 or higher (Kafka 0.10.0+).

### The Problem
- Clients (especially Python's kafka-python) would timeout after 5-10 seconds
- Messages were successfully stored but clients thought operations failed
- Root cause: Incorrect field ordering in ProduceResponse v2-v8

### The Solution
Moved `throttle_time_ms` field from the beginning to the end of ProduceResponse for protocol versions 1-8, matching real Kafka's wire format.

### Impact
- **Fixed**: Python kafka-python clients now work correctly
- **Requires Testing**: librdkafka-based clients (confluent-kafka)
- **Expected to Work**: Java and Go clients following standard Kafka protocol

## Changes

### Protocol Handler (`crates/chronik-protocol/src/handler.rs`)
```rust
// Line 2611-2615: Fixed field ordering
// throttle_time_ms now correctly positioned at END of response for v1-v8
if version >= 1 && version < 9 {
    encoder.write_i32(response.throttle_time_ms);
}
```

## Testing Status

### Confirmed Working
- ✅ Python kafka-python (all versions using API v0.10.0+)
- ✅ Message production with correct acknowledgments
- ✅ Offset and timestamp values properly returned

### Pending Verification
- ⚠️ librdkafka-based clients (confluent-kafka-python, confluent-kafka-go)
- ⚠️ Java Apache Kafka clients
- ⚠️ Go Sarama clients

## Known Issues

### Potential librdkafka Incompatibility
Previous code comments indicated librdkafka required `throttle_time_ms` at the beginning. This needs verification before production deployment.

**Workaround if needed**: Users experiencing issues with librdkafka can:
1. Use API version 0.9.0 or lower (ProduceResponse v1)
2. Wait for v0.7.4 with conditional compatibility logic

## Migration Guide

### For Python Users
No changes needed - the fix resolves timeout issues automatically.

### For librdkafka Users
If you experience issues after upgrading:
1. Test in non-production environment first
2. Report issues to help us implement compatibility mode
3. Consider staying on v0.7.2 until librdkafka compatibility is confirmed

## Documentation

### New Documents
- `docs/PRODUCE_RESPONSE_V2_INVESTIGATION.md` - Detailed protocol analysis
- `docs/PROTOCOL_COMPATIBILITY_ANALYSIS.md` - Client compatibility matrix
- `test_kafka_comparison.py` - Protocol comparison tool

### Key Findings
The Kafka protocol documentation doesn't always match actual implementation. Real Kafka puts `throttle_time_ms` at the end of ProduceResponse v2-v8, despite documentation suggesting otherwise.

## Version Information
- **Version**: 0.7.3
- **Release Date**: TBD (pending librdkafka testing)
- **Compatibility**: Kafka protocol 0.10.0 - 3.x

## Acknowledgments
Thanks to the detailed bug report that included reproduction steps with Python's kafka-python client, which was instrumental in identifying and fixing this issue.

## Next Steps

### Before Release
1. Complete librdkafka compatibility testing
2. Test with Java Kafka clients
3. Run regression tests for previous protocol fixes

### Future Improvements (v0.7.4)
- [ ] Client detection for automatic compatibility
- [ ] Configuration flag for librdkafka compatibility mode
- [ ] Comprehensive protocol test suite against real Kafka
- [ ] Integration tests with all major client libraries

## How to Test

### Python
```python
from kafka import KafkaProducer
producer = KafkaProducer(bootstrap_servers=['localhost:9092'])
future = producer.send('test-topic', b'test-message')
result = future.get(timeout=10)  # Should not timeout
print(f"Success! Offset: {result.offset}")
```

### librdkafka (Requires Verification)
```python
from confluent_kafka import Producer
p = Producer({'bootstrap.servers': 'localhost:9092'})
p.produce('test-topic', b'test-message')
p.flush()  # Test if this completes without timeout
```

## Risk Assessment
- **Low Risk**: Python users (fix confirmed working)
- **Medium Risk**: librdkafka users (compatibility uncertain)
- **Low Risk**: Java/Go users (expected to work)

## Rollback Plan
If issues are discovered post-release:
1. Revert to v0.7.2
2. Implement client-specific compatibility logic
3. Release v0.7.4 with comprehensive compatibility

---

**Note**: This is a pre-release document. Final testing with librdkafka is pending.