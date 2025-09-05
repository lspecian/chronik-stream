# librdkafka v2.11.1 Compatibility Documentation

## Overview

This document describes the librdkafka v2.11.1 compatibility fixes implemented in chronik-stream and the test utilities created during the investigation and resolution of compatibility issues.

## The Problem

librdkafka v2.11.1 clients were receiving "Bad message format" errors when connecting to chronik-stream. The investigation revealed two critical issues:

1. **Missing v8+ Fields**: Produce v9 responses were missing the `record_errors` and `error_message` fields introduced in version 8 of the protocol.
2. **Compact String Parsing**: librdkafka sends compact strings for the client_id field in flexible protocol versions (v3+), but the server was expecting normal strings.

## The Solution

### Fix 1: Added Missing v8+ Fields
**File**: `crates/chronik-protocol/src/handler.rs`

Added the missing fields to Produce v9 responses:
- `record_errors`: Empty array (compact encoding)
- `error_message`: Null string (compact encoding)

### Fix 2: Compact String Parsing for Client ID
**File**: `crates/chronik-protocol/src/parser.rs:516`

Changed the client_id parsing logic to use compact strings for flexible protocol versions:
```rust
let client_id = if flexible {
    decoder.read_compact_string()?
} else {
    decoder.read_string()?
};
```

## Test Utilities

During the investigation, several test utilities were created to diagnose and verify the fixes. These utilities are now organized in the `tests/` directory:

### Protocol Analysis Tools (`tests/python/protocol/librdkafka/`)

Tools for analyzing the Kafka wire protocol and debugging librdkafka compatibility:

- **capture_librdkafka.py**: Captures raw requests from librdkafka clients
- **intercept_proxy.py**: TCP proxy for intercepting client-server communication
- **test_produce_capture.py**: Captures and analyzes Produce request/response cycles
- **decode_produce_response.py**: Decodes and validates Produce v9 response structure
- **test_raw_produce.py**: Sends raw Produce requests for testing

### Debug Utilities (`tests/python/debug/librdkafka/`)

Debugging and analysis tools:

- **analyze_response.py**: Analyzes Kafka response structures
- **debug_varint.py**: Debugs compact/varint encoding issues
- **test_exact_response.py**: Tests exact byte-level response matching
- **test_wire_protocol.py**: Wire protocol validation utilities

### Integration Tests (`tests/go/librdkafka/`)

Go-based integration tests using librdkafka:

- **test_librdkafka_produce.go**: Main librdkafka produce test
- **test_librdkafka_debug.go**: Debug version with verbose output
- **test_librdkafka_full.go**: Comprehensive librdkafka test suite
- **test_produce_with_topic.go**: Topic-specific produce tests

## Running the Tests

### Python Protocol Tests
```bash
cd tests/python/protocol/librdkafka
python3 test_produce_capture.py
```

### Go Integration Tests
```bash
cd tests/go/librdkafka
go run test_librdkafka_produce.go
```

### Intercept Proxy (for debugging)
```bash
cd tests/python/protocol/librdkafka
python3 intercept_proxy.py  # Runs on port 9093, forwards to 9092
```

## Verification

The fix has been verified with:
- librdkafka v2.11.1 (Go client via confluent-kafka-go/v2)
- Raw protocol tests sending exact Produce v9 requests
- Intercept proxy confirming correct request/response formats

## Key Findings

1. **librdkafka Quirk**: Despite the Kafka protocol specification, librdkafka actually sends compact strings for client_id in flexible protocol versions. This is a client-specific behavior that the server must accommodate.

2. **Response Size Confusion**: The "52 bytes vs 60 bytes" issue was a red herring. The 52 bytes was the response body size (after the 4-byte size prefix and 4-byte correlation ID), which is normal behavior.

3. **Protocol Evolution**: The Produce protocol has evolved significantly, with v8+ adding fields for better error reporting. Servers must include these fields even if they're empty/null.

## Future Considerations

1. Consider adding more comprehensive protocol version testing
2. Document other potential client-specific quirks
3. Add automated tests for each supported protocol version
4. Consider implementing a protocol compliance test suite

## References

- [Apache Kafka Protocol Guide](https://kafka.apache.org/protocol)
- [librdkafka Documentation](https://docs.confluent.io/kafka-clients/librdkafka/current/overview.html)
- [KIP-482: Flexible Protocol Versions](https://cwiki.apache.org/confluence/display/KAFKA/KIP-482)