# Release Notes - v1.3.30

## Release Date
2025-10-06

## Critical Bug Fix

### RecordBatch Format - 4-Byte Padding Bug

**Fixed**: Removed incorrect 4-byte padding from RecordBatch wire format that caused CRC mismatches with Java Kafka clients.

**Root Cause**:
The encode function in `records.rs` was adding 4 bytes of padding at the start of the buffer as a hack to simplify batch_length calculations, but this padding was being included in the final output sent to clients. Kafka's RecordBatch format starts directly with `base_offset` (8 bytes), not with any padding.

**Impact**:
This caused a persistent CRC mismatch with Java clients (KSQLDB, Kafka UI, Kafka Streams, Kafka Connect) even after the v1.3.29 fixes. The padding shifted all byte offsets by 4 bytes, resulting in clients computing CRC over the wrong byte ranges.

**Fix Details**:
- Modified `RecordBatch::encode()` to remove the 4-byte padding before returning the final buffer
- Used `buf.split_off(4)` to strip padding after CRC calculation
- Updated comments to clarify the temporary padding is removed
- Decode side already correct (expects no padding)

**Testing**:
- All unit tests pass
- Build successful
- **User Testing Required**: Must test with KSQLDB/Java clients before declaring success

**Changed Files**:
- `crates/chronik-protocol/src/records.rs` - RecordBatch encode function
- `crates/chronik-common/src/metadata/tests.rs` - Fixed test compilation error

## Technical Details

### Before (v1.3.29)
```
Wire format sent to clients:
[4 padding][8 base_offset][4 batch_length][4 partition_leader_epoch][1 magic][4 CRC][...]
```

Java clients computed CRC starting at position 4 (base_offset), but our CRC was calculated starting at position 16 (partition_leader_epoch) in a buffer that included the padding. This caused a 4-byte offset mismatch in the CRC calculation.

### After (v1.3.30)
```
Wire format sent to clients:
[8 base_offset][4 batch_length][4 partition_leader_epoch][1 magic][4 CRC][...]
```

Now matches Kafka's exact wire format. CRC calculated at position 16 in the temporary buffer (with padding), but padding is removed before sending to clients.

## Upgrade Notes

**From v1.3.29 or earlier**: Direct upgrade supported. This is a wire protocol fix that affects how data is sent to clients. Existing stored data is unaffected.

**Clean volumes recommended**: For accurate CRC testing, use clean Docker volumes when testing this release.

## Known Issues

None at this time. This release specifically addresses the RecordBatch format bug discovered in user testing.

## Next Steps

1. **User testing with KSQLDB required** - This fix must be validated with Java Kafka clients before general deployment
2. Monitor CRC validation logs from Java clients
3. Verify zero CRC mismatches in production workloads

## Contributors

- Claude (AI Assistant) - Bug identification and fix implementation
- User testing and validation

---

**Full Changelog**: https://github.com/chronik-stream/chronik-stream/compare/v1.3.29...v1.3.30
