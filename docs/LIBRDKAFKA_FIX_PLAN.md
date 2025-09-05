# Librdkafka Compatibility Fix Plan

## Problem Statement
librdkafka v2.11.1 fails with "Read underflow" when connecting to Chronik Stream, despite correct wire protocol encoding.

## Root Cause
The issue is NOT with ApiVersions v0 vs v3 encoding. Analysis shows:
1. librdkafka requests ApiVersions v3 (not v0)
2. Our response is correctly formatted (423 bytes)
3. The "Read underflow" likely occurs AFTER ApiVersions succeeds

## Hypothesis
The real issue is that librdkafka successfully receives ApiVersions, then tries to use a negotiated API version that we don't properly support, causing the underflow on the NEXT request.

## Proposed Solution

### 1. Fix API Version Ranges
Compare our advertised API versions with real Kafka and librdkafka expectations:

```rust
// Current: We advertise Produce v0-9
// Real Kafka 3.5+: Produce v0-11
// librdkafka might try v10+ and fail

// Solution: Update to match Kafka 3.5+ exactly
ApiKey::Produce => VersionRange { min: 0, max: 11 },
ApiKey::Fetch => VersionRange { min: 0, max: 16 },
// ... etc
```

### 2. Add Request Logging
Add detailed logging to see what request comes AFTER ApiVersions:

```rust
// In handle_request
tracing::info!("Request: API key={:?} ({}), version={}, correlation_id={}, client_id={:?}",
    api_key, api_key as i16, header.api_version, header.correlation_id, header.client_id);
```

### 3. Implement Fallback for Unsupported Versions
If a client requests an unsupported version, return proper error:

```rust
fn handle_unsupported_version(&self, header: &RequestHeader, api_key: ApiKey) -> Response {
    // Return UNSUPPORTED_VERSION error (35)
    let error_response = ErrorResponse {
        error_code: 35, // UNSUPPORTED_VERSION
    };
    // Encode based on whether this is flexible version
    ...
}
```

### 4. Fix Metadata Response
librdkafka likely calls Metadata right after ApiVersions. Ensure it works:

```rust
// Metadata must return at least one broker
// Must handle both flexible (v9+) and non-flexible versions
```

### 5. Test Matrix
- librdkafka v2.11.1 (latest)
- librdkafka v2.0.2 (older stable)
- sarama (Go client)
- kafka-python

## Implementation Steps

1. **Enable detailed request logging** to see full request sequence
2. **Update API version ranges** to match Kafka 3.5+
3. **Fix Metadata handler** to ensure it returns valid broker info
4. **Add version validation** to return proper errors for unsupported versions
5. **Test with librdkafka** using debug mode

## Test Commands

```bash
# Test with librdkafka debug output
KAFKA_DEBUG=protocol kafkacat -L -b localhost:9092

# Test with reduced API version negotiation
kafkacat -L -b localhost:9092 -X api.version.request=false

# Test with Go client
go run test_librdkafka.go
```

## Expected Outcome
- librdkafka successfully connects and lists topics
- No "Read underflow" errors
- Proper version negotiation for all APIs