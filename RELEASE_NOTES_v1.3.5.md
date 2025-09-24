# Chronik Stream v1.3.5 - Metadata Response Header Fix

## Release Date: September 24, 2025

## 🔍 Purpose

This release fixes the critical issue preventing kafka-python and other Kafka clients from connecting by correcting the Metadata response header format for protocol versions 0-8.

## The Root Cause

The issue was identified through systematic testing without Kafka UI interference. The core problem was in the Metadata response header encoding:

- **Bug**: Metadata v1-v8 responses incorrectly included tagged fields byte in the header
- **Impact**: Clients couldn't parse responses, interpreting the tagged fields byte as part of the throttle_time field
- **Effect**: Connection failures with "Unable to decode buffer" errors

## What Changed

### 1. Fixed Metadata Response Header Format

In `crates/chronik-protocol/src/handler.rs`:

```rust
// Before (INCORRECT):
let header_has_tagged_fields = if api_key == ApiKey::ApiVersions {
    false
} else if api_key == ApiKey::DescribeCluster && header.api_version == 0 {
    false
} else {
    true  // WRONG: This included v1-v8 metadata responses
};

// After (CORRECT):
let header_has_tagged_fields = if api_key == ApiKey::ApiVersions {
    false
} else if api_key == ApiKey::DescribeCluster && header.api_version == 0 {
    false
} else if api_key == ApiKey::Metadata && header.api_version < 9 {
    false  // FIX: Metadata v0-v8 use NON-flexible headers
} else {
    true
};
```

### 2. Error Recovery for Invalid Requests (Already in place)

The error recovery mechanism from v1.3.4 remains to handle non-Kafka clients like Kafka UI's "5.0\0" probe.

## Testing Verification

Created comprehensive test script (`test_metadata_v1.py`) that:
- Sends Metadata v1 request
- Analyzes response byte structure
- Verifies proper header format (no tagged fields)
- Tests with kafka-python decoder

## Expected Behavior

With this fix:
1. kafka-python and other Kafka clients can connect successfully
2. Metadata v0-v8 responses have correct non-flexible headers
3. Metadata v9+ responses continue using flexible headers correctly
4. Error recovery handles non-Kafka client probes gracefully

## Protocol Compliance

This fix ensures compliance with the Kafka wire protocol specification:
- Metadata v0-v8: headerVersion=0 (non-flexible, no tagged fields)
- Metadata v9+: headerVersion=1 (flexible, has tagged fields)

## Next Steps

1. Complete release build and deploy
2. Verify with real kafka-python clients
3. Test with Kafka UI to ensure error recovery still works
4. Monitor for any remaining protocol compatibility issues

---

**Full Changelog**: https://github.com/chronik-stream/chronik-stream/compare/v1.3.4...v1.3.5