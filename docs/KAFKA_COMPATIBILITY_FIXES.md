# Kafka Client Compatibility Fixes

## Summary of Issues and Resolutions

This document describes the fixes applied to resolve Kafka client compatibility issues reported in v0.5.0.

## Issues Fixed

### 1. Java Client - API Versions Response Parsing Error

**Issue**: Java clients reported "Tried to allocate a collection of size 1319168, but there are only 79 bytes remaining" when parsing ApiVersions responses.

**Root Cause**: In `handler.rs`, the ApiVersions response encoder was incorrectly using varints for api_key, min_version, and max_version fields in version 3+ of the protocol. According to the Kafka protocol specification, these fields should remain as INT16 even in flexible versions.

**Fix Applied**: 
- File: `crates/chronik-protocol/src/handler.rs`
- Changed encoding from varints to INT16 for these fields in v3+ responses
- Line 1014-1017: Now correctly uses `encoder.write_i16()` instead of `encoder.write_unsigned_varint()`

### 2. Go Client - Memory Assertion Failure during producer.Flush()

**Issue**: confluent-kafka-go clients experienced assertion failures (`Assertion failed: (p), function rd_malloc, file rd.h, line 142`) during producer.Flush() operations.

**Root Cause**: The API versions response encoding issue (same as #1) was causing librdkafka (the underlying C library) to misparse responses, leading to memory corruption.

**Fix Applied**: The same fix for issue #1 resolves this, as it ensures proper protocol encoding that librdkafka can correctly parse.

### 3. Python Client - Connection Timeout

**Issue**: kafka-python clients would hang indefinitely when trying to connect.

**Root Cause**: The malformed API versions response was preventing the client from completing the initial handshake and version negotiation.

**Fix Applied**: The API versions encoding fix allows Python clients to successfully negotiate protocol versions and establish connections.

### 4. Metrics Endpoint Not Responding (Port 9093)

**Issue**: The metrics endpoint on port 9093 was not responding with an empty reply.

**Root Cause**: The metrics server was not being started in the chronik-server binary.

**Fix Applied**:
- File: `crates/chronik-server/src/main.rs`
- Added metrics_port configuration parameter (default 9093)
- Added `start_metrics_server()` function that creates a Prometheus metrics endpoint using warp
- Metrics server now starts automatically when the server runs
- Added prometheus and warp dependencies to Cargo.toml

## Technical Details

### Protocol Encoding Fix

The Kafka protocol has two encoding modes:
- Standard (versions 0-2): Uses fixed-size integers
- Flexible/Compact (versions 3+): Uses variable-length encoding for some fields

However, the ApiVersions response has specific requirements:
- In v3+, the array itself uses compact encoding (length+1)
- But the api_key, min_version, and max_version fields remain as INT16
- Only tagged fields use varint encoding

### Before Fix:
```rust
// Incorrect - using varints for all fields in v3+
encoder.write_unsigned_varint(api.api_key as u32);    // WRONG
encoder.write_unsigned_varint(api.min_version as u32); // WRONG
encoder.write_unsigned_varint(api.max_version as u32); // WRONG
```

### After Fix:
```rust
// Correct - INT16 for these fields even in v3+
encoder.write_i16(api.api_key);    // CORRECT
encoder.write_i16(api.min_version); // CORRECT
encoder.write_i16(api.max_version); // CORRECT
```

## Testing

A comprehensive test script (`test_kafka_fixes.sh`) has been created to verify all fixes:

1. **Metrics Test**: Verifies the metrics endpoint responds on port 9093
2. **kafkacat Test**: Tests basic Kafka operations with the standard CLI tool
3. **Python Test**: Verifies kafka-python can connect and produce messages
4. **Go Test**: Verifies confluent-kafka-go works without assertion failures

## Running the Tests

```bash
# Start the server
cargo run --bin chronik-server

# In another terminal, run the test script
./test_kafka_fixes.sh
```

## Compatibility Matrix

After these fixes, Chronik Stream should be compatible with:

| Client Library | Version | Status |
|---------------|---------|---------|
| confluent-kafka-go | v2.11.1+ | ✅ Fixed |
| kafka-python | Latest | ✅ Fixed |
| Java Client (Confluent) | 7.5.0+ | ✅ Fixed |
| kafkacat | Latest | ✅ Fixed |
| librdkafka | 2.0+ | ✅ Fixed |

## Recommendations

1. **Version Testing**: Continue testing with various client library versions
2. **Protocol Compliance**: Consider implementing a protocol compliance test suite
3. **Monitoring**: The new metrics endpoint should be integrated with monitoring systems
4. **Documentation**: Update user documentation to reflect the metrics endpoint availability

## Next Steps

1. Build and release v0.5.1 with these fixes
2. Add integration tests for each supported client library
3. Consider adding protocol fuzzing tests to catch encoding issues early
4. Implement more comprehensive metrics collection

---

*Fixes implemented: 2025-08-31*
*Chronik Stream Version: v0.5.1 (pending release)*