# Release Notes - v1.3.47

**Release Date**: 2025-10-10

## What's Fixed

### AdminClient API Compatibility
- **DescribeAcls v0-v3** - Fixed protocol version support and flexible encoding
- **Kafka UI** should now work (no more protocol parse errors)
- **Apache Flink KafkaSource** should work (AdminClient initialization fixed)
- **Consumer group auto-assignment** working

### Compression Support
- **Snappy, LZ4, Zstd** compression now work (were broken before)
- Go/Java/Python clients with compression compatible

---

## Technical Details

### AdminClient Bug
**Problem**: DescribeAcls only supported v0, modern clients need v2+ with flexible encoding

**Root cause**:
1. Version range was `min: 0, max: 0` instead of `min: 0, max: 3`
2. Flexible encoding (compact strings/arrays, tagged fields) not implemented
3. Critical bug: created new `Encoder` instances for each field instead of single instance

**Fix**: [`describe_acls_types.rs:157-266`](../../crates/chronik-protocol/src/describe_acls_types.rs#L157)
- Updated version support to v0-v3
- Implemented flexible encoding with single Encoder pattern
- Added proper compact encoding and tagged fields

### Compression Bug
**Problem**: Snappy/LZ4/Zstd batches stored compressed but never decompressed on fetch

**Fix**: [`kafka_records.rs`](../../crates/chronik-storage/src/kafka_records.rs)
- Added decompression for Snappy, LZ4, Zstd in `decode()`
- Added dependencies: `snap`, `lz4_flex`

## Known Issues

Minor: DescribeAcls v2 has extra 0x00 byte in response header (functionally works, will fix in v1.3.48)
