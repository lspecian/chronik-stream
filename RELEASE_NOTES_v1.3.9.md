# Chronik Stream v1.3.9 - MetadataResponse v2+ Field Ordering Fix

## Release Date: September 25, 2025

## Summary

Fixed critical field ordering bug in MetadataResponse for v2+ that prevented kafka-python and other clients from properly decoding metadata responses.

## The Issue

MetadataResponse v2+ was encoding fields in the wrong order:
- **Incorrect**: brokers → controller_id → cluster_id → topics
- **Correct**: brokers → cluster_id → controller_id → topics

This caused kafka-python to fail when parsing v5 responses with:
```
kafka.errors.KafkaProtocolError: Unable to decode response
```

## The Fix

Swapped the order of cluster_id and controller_id fields in `encode_metadata_response()` to match Kafka protocol specification.

### Before (Incorrect)
```rust
// Controller ID comes immediately after brokers for v1+
if version >= 1 {
    encoder.write_i32(response.controller_id);
}

// Cluster ID comes after controller ID for v2+
if version >= 2 {
    encoder.write_string(response.cluster_id.as_deref());
}
```

### After (Correct)
```rust
// Cluster ID comes after brokers for v2+
if version >= 2 {
    encoder.write_string(response.cluster_id.as_deref());
}

// Controller ID comes after cluster_id for v2+, or directly after brokers for v1
if version >= 1 {
    encoder.write_i32(response.controller_id);
}
```

## Protocol Compliance Verification

✅ **MetadataResponse Field Order by Version**:
- v0: brokers → topics
- v1: brokers → controller_id → topics
- v2+: brokers → cluster_id → controller_id → topics
- v3+: throttle_time → brokers → cluster_id → controller_id → topics

## Testing

Verified with kafka-python client:
```python
consumer = KafkaConsumer(
    bootstrap_servers=['localhost:9094'],
    api_version='auto'
)
# SUCCESS: kafka-python can now decode Chronik's metadata responses!
```

## Impact

This fix restores compatibility with kafka-python and other Kafka clients that rely on proper field ordering in MetadataResponse v2+.

---

**Full Changelog**: https://github.com/chronik-stream/chronik-stream/compare/v1.3.8...v1.3.9