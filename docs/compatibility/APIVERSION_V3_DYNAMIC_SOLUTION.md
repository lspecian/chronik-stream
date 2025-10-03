# Dynamic ApiVersions V3 Encoding Detection Solution

## Overview

Instead of hardcoding ApiVersions v3 to always use non-flexible encoding, we'll implement dynamic detection that automatically determines which encoding style the client is using.

## Detection Strategies

### Strategy 1: Parse Attempt with Fallback (Recommended)

Try parsing with one encoding, and if it fails in a specific way, try the other encoding.

```rust
pub fn parse_request_header_with_correlation(buf: &mut Bytes) -> Result<(RequestHeader, Option<PartialRequestHeader>)> {
    let mut decoder = Decoder::new(buf);

    // Read the fixed part that's always the same
    let api_key_raw = decoder.read_i16()?;
    let api_version = decoder.read_i16()?;
    let correlation_id = decoder.read_i32()?;

    // Special handling for ApiVersions v3+
    if api_key_raw == 18 && api_version >= 3 {
        // Save position for potential retry
        let saved_position = decoder.buf.clone();

        // Try flexible encoding first (spec-compliant)
        match try_parse_as_flexible(&mut decoder.clone()) {
            Ok((client_id, remaining_decoder)) => {
                // Successfully parsed as flexible
                decoder = remaining_decoder;
                return finish_parsing_flexible(api_key_raw, api_version, correlation_id, client_id, &mut decoder);
            }
            Err(_) => {
                // Failed with flexible, try non-flexible (Confluent style)
                decoder.buf = saved_position;
                match try_parse_as_non_flexible(&mut decoder) {
                    Ok((client_id, remaining_decoder)) => {
                        decoder = remaining_decoder;
                        return finish_parsing_non_flexible(api_key_raw, api_version, correlation_id, client_id, &mut decoder);
                    }
                    Err(e) => return Err(e),
                }
            }
        }
    }

    // Normal processing for other APIs
    // ... existing code ...
}
```

### Strategy 2: Smart Detection Based on Byte Pattern

Peek at the next bytes to determine encoding type:

```rust
fn detect_string_encoding(decoder: &Decoder) -> StringEncoding {
    // Peek at the next few bytes without consuming
    let bytes = decoder.buf.chunk();

    if bytes.len() < 2 {
        return StringEncoding::Unknown;
    }

    // For compact strings (flexible), first byte is varint length
    // For normal strings, first two bytes are i16 length

    // If first byte has high bit set, it's definitely varint (compact)
    if bytes[0] & 0x80 != 0 {
        return StringEncoding::Compact;
    }

    // If first byte is 0x00 and second byte is small, likely normal string
    if bytes[0] == 0x00 && bytes[1] < 0x7F {
        // This looks like a normal string with length < 127
        return StringEncoding::Normal;
    }

    // For small values, we need more heuristics
    // Compact string with length 13 would be: 0x1A (26 in varint encoding)
    // Normal string with length 13 would be: 0x00 0x0D

    // Try parsing both ways and see which makes more sense
    StringEncoding::Ambiguous
}
```

### Strategy 3: Client ID Pattern Detection

Detect known Confluent client patterns:

```rust
fn is_likely_confluent_client(client_id: &Option<String>) -> bool {
    if let Some(id) = client_id {
        // Known Confluent client patterns
        id.starts_with("adminclient-") ||
        id.starts_with("ksql-") ||
        id.contains("confluent") ||
        id.contains("schema-registry") ||
        id.contains("connect-")
    } else {
        false
    }
}
```

## Recommended Implementation

Combine Strategy 1 with intelligent error recovery:

```rust
// In parser.rs

pub fn parse_request_header_with_correlation(buf: &mut Bytes) -> Result<(RequestHeader, Option<PartialRequestHeader>)> {
    let mut decoder = Decoder::new(buf);

    let api_key_raw = decoder.read_i16()?;
    let api_version = decoder.read_i16()?;
    let correlation_id = decoder.read_i32()?;

    // Determine expected flexibility
    let expected_flexible = if let Some(api_key) = ApiKey::from_i16(api_key_raw) {
        is_flexible_version(api_key, api_version)
    } else {
        false
    };

    // Special handling for ApiVersions v3+ which may use either encoding
    if api_key_raw == 18 && api_version >= 3 {
        return parse_api_versions_v3_header(api_key_raw, api_version, correlation_id, &mut decoder);
    }

    // Standard parsing for everything else
    parse_header_with_encoding(api_key_raw, api_version, correlation_id, &mut decoder, expected_flexible)
}

fn parse_api_versions_v3_header(
    api_key_raw: i16,
    api_version: i16,
    correlation_id: i32,
    decoder: &mut Decoder,
) -> Result<(RequestHeader, Option<PartialRequestHeader>)> {
    // Clone the decoder state for retry
    let saved_buf = decoder.buf.clone();

    // Try flexible encoding first (spec-compliant)
    let flexible_result = parse_client_id_flexible(decoder);

    match flexible_result {
        Ok(client_id) => {
            tracing::debug!("ApiVersions v3: Successfully parsed as flexible encoding, client_id: {:?}", client_id);

            // Continue with flexible parsing for tagged fields
            let tagged_field_count = decoder.read_unsigned_varint()?;
            for i in 0..tagged_field_count {
                let tag_id = decoder.read_unsigned_varint()?;
                let tag_size = decoder.read_unsigned_varint()? as usize;
                decoder.advance(tag_size)?;
            }

            return build_response(api_key_raw, api_version, correlation_id, client_id);
        }
        Err(e) => {
            // Check if error suggests wrong encoding
            if is_encoding_error(&e) {
                tracing::debug!("ApiVersions v3: Flexible parsing failed, trying non-flexible encoding");

                // Reset decoder to saved position
                decoder.buf = saved_buf;

                // Try non-flexible encoding (Confluent style)
                match parse_client_id_non_flexible(decoder) {
                    Ok(client_id) => {
                        tracing::debug!("ApiVersions v3: Successfully parsed as non-flexible encoding (Confluent-style), client_id: {:?}", client_id);

                        // No tagged fields in non-flexible header
                        return build_response(api_key_raw, api_version, correlation_id, client_id);
                    }
                    Err(e2) => {
                        tracing::error!("ApiVersions v3: Failed with both encodings. Flexible: {:?}, Non-flexible: {:?}", e, e2);
                        return Err(e); // Return original error
                    }
                }
            } else {
                return Err(e);
            }
        }
    }
}

fn parse_client_id_flexible(decoder: &mut Decoder) -> Result<Option<String>> {
    decoder.read_compact_string()
}

fn parse_client_id_non_flexible(decoder: &mut Decoder) -> Result<Option<String>> {
    decoder.read_string()
}

fn is_encoding_error(error: &Error) -> bool {
    match error {
        Error::Protocol(msg) => {
            msg.contains("Cannot advance") ||
            msg.contains("varint") ||
            msg.contains("Invalid string length") ||
            msg.contains("bytes remaining")
        }
        _ => false
    }
}
```

## Benefits of Dynamic Detection

1. **No Configuration Required**: Works automatically with any client
2. **Future Proof**: Handles both spec-compliant and Confluent-style clients
3. **Transparent**: Logs which encoding was detected for debugging
4. **Robust**: Falls back gracefully if first attempt fails
5. **Performance**: Only pays the retry cost for ApiVersions v3, which is infrequent

## Testing Plan

```python
# Test both encoding styles
def test_api_versions_v3_both_encodings():
    # Test 1: Flexible encoding (spec-compliant)
    sock = connect_to_chronik()
    send_api_versions_v3_flexible(sock)
    response = receive_response(sock)
    assert response.is_valid()

    # Test 2: Non-flexible encoding (Confluent-style)
    sock2 = connect_to_chronik()
    send_api_versions_v3_non_flexible(sock2)
    response2 = receive_response(sock2)
    assert response2.is_valid()

    # Test 3: KSQLDB actual client
    ksql_client = create_ksqldb_client()
    assert ksql_client.can_connect()
```

## Logging for Debugging

Add detailed logging to help diagnose issues:

```rust
tracing::debug!(
    "ApiVersions v3 detection: client_id={:?}, encoding={:?}, remaining_bytes={}",
    client_id,
    if flexible_worked { "flexible" } else { "non-flexible" },
    decoder.buf.remaining()
);
```

## Metrics (Optional)

Track which encoding style is being used:

```rust
static FLEXIBLE_COUNT: AtomicU64 = AtomicU64::new(0);
static NON_FLEXIBLE_COUNT: AtomicU64 = AtomicU64::new(0);

// In parsing code
if flexible_worked {
    FLEXIBLE_COUNT.fetch_add(1, Ordering::Relaxed);
} else {
    NON_FLEXIBLE_COUNT.fetch_add(1, Ordering::Relaxed);
}
```

## Edge Cases Handled

1. **Corrupted data**: Will fail with appropriate error
2. **Unknown client**: Will try both encodings
3. **Future versions**: ApiVersions v4+ will use same logic
4. **Empty client_id**: Both encodings handle None properly
5. **Very long client_id**: Length validation applies to both

## Implementation Priority

1. Implement parse_api_versions_v3_header function
2. Add retry logic with buffer reset
3. Add comprehensive logging
4. Test with real KSQLDB client
5. Test with standard Kafka clients
6. Add unit tests for both paths

This dynamic solution provides the best of both worlds: compatibility with all clients while maintaining clean, understandable code.