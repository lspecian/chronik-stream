# Chronik Stream v1.3.6 - Metadata Response Structure Fix

## Release Date: September 24, 2025

## 🔍 Purpose

This release fixes a critical structural issue in the MetadataResponse that was preventing proper protocol encoding by removing an incorrectly included correlation_id field from the response body.

## The Root Cause

After v1.3.5's partial fix, deeper investigation revealed:

- **Bug**: The MetadataResponse struct in types.rs incorrectly included a `correlation_id` field
- **Impact**: This field should only exist in the response header, not the response body
- **Effect**: Response encoding was malformed, causing parsing failures

## What Changed

### 1. Removed correlation_id from MetadataResponse struct

In `crates/chronik-protocol/src/types.rs`:

```rust
// Before (INCORRECT):
pub struct MetadataResponse {
    pub correlation_id: i32,  // WRONG: Should not be here
    pub throttle_time_ms: i32,
    pub brokers: Vec<MetadataBroker>,
    pub cluster_id: Option<String>,
    pub controller_id: i32,
    pub topics: Vec<MetadataTopic>,
    pub cluster_authorized_operations: Option<i32>,
}

// After (CORRECT):
pub struct MetadataResponse {
    pub throttle_time_ms: i32,
    pub brokers: Vec<MetadataBroker>,
    pub cluster_id: Option<String>,
    pub controller_id: i32,
    pub topics: Vec<MetadataTopic>,
    pub cluster_authorized_operations: Option<i32>,
}
```

### 2. Updated all MetadataResponse instantiations

Removed correlation_id assignment from:
- `crates/chronik-protocol/src/handler.rs`
- `crates/chronik-server/src/handler.rs`

### 3. Fixed related encoding functions

Updated metadata_fix.rs and types.rs to remove correlation_id handling from response body encoding.

## Testing Verification

The fix ensures:
- Correlation ID is only in the response header (where it belongs)
- Response body contains only metadata-specific fields
- Proper field ordering for all Metadata protocol versions

## Expected Behavior

With this fix:
1. MetadataResponse structure matches Kafka protocol specification
2. Response headers and bodies are properly separated
3. All Metadata versions (v0-v12) encode correctly
4. Clients can parse responses without field misalignment

## Protocol Compliance

This fix ensures proper separation of concerns:
- **Response Header**: Contains correlation_id (handled by framework)
- **Response Body**: Contains only API-specific fields (brokers, topics, etc.)

## Known Issues

While the structural issue is fixed, testing shows that Metadata v1 responses still need field ordering verification:
- The response correctly excludes throttle_time for v1
- Controller_id is properly included for v1+
- Further testing with kafka-python client is ongoing

## Next Steps

1. Continue testing with various Kafka clients
2. Verify field ordering matches official Kafka implementation
3. Monitor for any remaining protocol compatibility issues

---

**Full Changelog**: https://github.com/chronik-stream/chronik-stream/compare/v1.3.5...v1.3.6