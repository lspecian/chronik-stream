# ApiVersions V3 Encoding Compatibility Fix Proposal

## Problem Statement

KSQLDB and Confluent Kafka clients send ApiVersions v3 requests with **non-flexible header encoding** (using 2-byte length prefix for client_id) while the request body uses flexible encoding. This contradicts the Kafka protocol specification which states ApiVersions v3 should use flexible encoding throughout.

### Current Behavior
- Chronik expects ApiVersions v3 to use flexible encoding (compact strings with varint length prefix) in both header and body
- KSQLDB/Confluent clients send non-flexible header (2-byte length prefix) with flexible body
- This causes a parsing error: "Cannot advance 100 bytes, only 45 remaining"

### Root Cause Analysis

From packet capture:
```
KSQLDB sends:
00 00 00 39  - Message length
00 12        - API key 18 (ApiVersions)
00 03        - API version 3
00 00 00 00  - Correlation ID
00 0d        - Normal string length (13) ← ISSUE: Should be varint 0x1a for compact string
adminclient-2 - Client ID
```

The issue occurs in `parser.rs:547-551`:
```rust
let flexible = if let Some(api_key) = ApiKey::from_i16(api_key_raw) {
    is_flexible_version(api_key, api_version)  // Returns true for ApiVersions v3
} else {
    false
};
```

## Proposed Solutions

### Solution 1: Special Case for ApiVersions V3 (Recommended)

Treat ApiVersions v3 as a special case that uses non-flexible header encoding for backward compatibility, while maintaining flexible body encoding.

**Advantages:**
- Maintains compatibility with existing Confluent/KSQLDB clients
- Minimal code changes
- Aligns with Confluent's implementation approach
- Clear and explicit handling

**Implementation:**
```rust
// In parse_request_header_with_correlation
let flexible = if let Some(api_key) = ApiKey::from_i16(api_key_raw) {
    // ApiVersions v3+ uses non-flexible header for compatibility
    // but flexible body encoding
    if api_key == ApiKey::ApiVersions && api_version >= 3 {
        false  // Use non-flexible header encoding
    } else {
        is_flexible_version(api_key, api_version)
    }
} else {
    false
};
```

### Solution 2: Try Both Encodings for ApiVersions

Attempt to parse with both flexible and non-flexible encoding when ApiVersions v3 is detected.

**Advantages:**
- Supports both standard and Confluent client behaviors
- More robust

**Disadvantages:**
- More complex implementation
- Performance overhead from double parsing attempts
- Could mask other protocol errors

**Implementation sketch:**
```rust
if api_key == ApiKey::ApiVersions && api_version == 3 {
    // Try non-flexible first (Confluent clients)
    match try_parse_non_flexible_header(&mut decoder_copy) {
        Ok(header) => return Ok(header),
        Err(_) => {
            // Fall back to flexible (standard clients)
            parse_flexible_header(&mut decoder)
        }
    }
}
```

### Solution 3: Configuration Flag

Add a configuration option to control ApiVersions v3 header encoding behavior.

**Advantages:**
- User control over behavior
- Can default to Confluent-compatible mode

**Disadvantages:**
- Requires configuration management
- Additional complexity for users
- Not automatic

## Recommendation

**Solution 1 (Special Case)** is recommended because:

1. **Simplicity**: Minimal code changes required
2. **Compatibility**: Matches Confluent's real-world implementation
3. **Standards**: KIP-511 mentions "Tagged fields are only supported in the body but not in the header"
4. **Precedent**: The Kafka ecosystem already has special cases for ApiVersions (e.g., SASL sends v0)
5. **Clarity**: Explicit handling makes the compatibility choice obvious

## Implementation Plan

### Files to Modify

1. **`crates/chronik-protocol/src/parser.rs`**
   - Modify `parse_request_header_with_correlation` (line ~547)
   - Update `is_flexible_version` documentation to note the special case
   - Add unit tests for both encoding types

2. **`crates/chronik-protocol/src/handler.rs`**
   - No changes needed if fix is in parser
   - Verify ApiVersions v3 handling works correctly

3. **Tests to Add/Update**
   - `tests/test_ksqldb_integration.py` - Verify KSQLDB can connect
   - `tests/test_api_versions.rs` - Add unit tests for both encodings
   - `tests/test_confluent_compatibility.py` - Test with Confluent clients

### Code Changes

#### parser.rs (line ~547)
```rust
// Determine if this API+version uses flexible/compact encoding
let flexible = if let Some(api_key) = ApiKey::from_i16(api_key_raw) {
    // Special case: ApiVersions v3+ uses non-flexible header encoding
    // for backward compatibility with Confluent clients, even though
    // the body uses flexible encoding. This matches Confluent's
    // implementation and KIP-511's note that "Tagged fields are only
    // supported in the body but not in the header."
    if api_key == ApiKey::ApiVersions && api_version >= 3 {
        false
    } else {
        is_flexible_version(api_key, api_version)
    }
} else {
    false
};
```

#### Unit Test Example
```rust
#[test]
fn test_api_versions_v3_non_flexible_header() {
    // Test that ApiVersions v3 with non-flexible header encoding works
    let mut bytes = BytesMut::new();
    let mut encoder = Encoder::new(&mut bytes);

    // Write header with non-flexible encoding
    encoder.write_i16(18);  // ApiVersions
    encoder.write_i16(3);   // Version 3
    encoder.write_i32(123); // Correlation ID
    encoder.write_string(Some("test-client"));  // Non-flexible string

    let mut buf = bytes.freeze();
    let result = parse_request_header(&mut buf);
    assert!(result.is_ok());

    let header = result.unwrap();
    assert_eq!(header.api_key, ApiKey::ApiVersions);
    assert_eq!(header.api_version, 3);
    assert_eq!(header.client_id, Some("test-client".to_string()));
}
```

## Testing Strategy

1. **Unit Tests**: Verify both flexible and non-flexible header parsing for ApiVersions v3
2. **Integration Tests**: Test KSQLDB connectivity with modified parser
3. **Compatibility Tests**: Verify standard Kafka clients still work
4. **Performance Tests**: Ensure no performance regression

## Risk Analysis

- **Low Risk**: Change is isolated to header parsing logic
- **Backward Compatible**: Existing clients continue to work
- **Forward Compatible**: Supports both encoding styles
- **Revertible**: Easy to revert if issues arise

## Timeline

1. Implement parser.rs changes (30 minutes)
2. Add unit tests (30 minutes)
3. Test with KSQLDB (30 minutes)
4. Test with other Kafka clients (30 minutes)
5. Document changes (15 minutes)

Total estimated time: ~2.5 hours

## References

- [KIP-511: Collect and Expose Client's Name and Version](https://cwiki.apache.org/confluence/display/KAFKA/KIP-511)
- [KIP-482: The Kafka Protocol should Support Optional Tagged Fields](https://cwiki.apache.org/confluence/display/KAFKA/KIP-482)
- Confluent Platform Compatibility Documentation
- Packet captures showing KSQLDB behavior