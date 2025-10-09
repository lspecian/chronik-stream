/// Integration test for CanonicalRecord CRC validation
///
/// This test verifies that the CanonicalRecord encode/decode implementation
/// correctly preserves CRC checksums, fixing the v1.3.29-v1.3.33 CRC corruption bugs.

use chronik_storage::canonical_record::CanonicalRecord;
use bytes::Bytes;

#[test]
fn test_canonical_record_with_java_kafka_batch() {
    // This is a real Kafka RecordBatch v2 created by Java KafkaProducer
    // Base offset: 0
    // Records: 1 message with key="test-key" value="test-value"
    // This will be replaced with actual Java producer output once we capture it

    // For now, create a test batch using our test helper
    let test_batch = create_simple_kafka_batch();

    // Step 1: Decode to canonical format
    let canonical = CanonicalRecord::from_kafka_batch(&test_batch)
        .expect("Failed to decode Kafka batch");

    println!("Decoded canonical record:");
    println!("  base_offset: {}", canonical.base_offset);
    println!("  records count: {}", canonical.records.len());
    println!("  compression: {:?}", canonical.compression);

    // Step 2: Encode back to Kafka format
    let re_encoded = canonical.to_kafka_batch()
        .expect("Failed to encode canonical record");

    // Step 3: Extract CRCs for comparison
    let original_crc = extract_crc(&test_batch);
    let re_encoded_crc = extract_crc(&re_encoded);

    println!("CRC comparison:");
    println!("  Original:    0x{:08x}", original_crc);
    println!("  Re-encoded:  0x{:08x}", re_encoded_crc);

    // CRITICAL ASSERTION: CRCs must match exactly
    assert_eq!(
        original_crc, re_encoded_crc,
        "CRC mismatch! Original: 0x{:08x}, Re-encoded: 0x{:08x}",
        original_crc, re_encoded_crc
    );

    // Step 4: Verify full round-trip
    let canonical2 = CanonicalRecord::from_kafka_batch(&re_encoded)
        .expect("Failed to decode re-encoded batch");

    assert_eq!(canonical.base_offset, canonical2.base_offset);
    assert_eq!(canonical.records.len(), canonical2.records.len());
    assert_eq!(canonical.records[0].key, canonical2.records[0].key);
    assert_eq!(canonical.records[0].value, canonical2.records[0].value);

    println!("✓ CRC validation test passed!");
}

/// Create a simple Kafka RecordBatch v2 for testing
fn create_simple_kafka_batch() -> Bytes {
    use bytes::{BufMut, BytesMut};
    use std::io::Cursor;
    use crc32fast::Hasher as Crc32;

    let mut buf = BytesMut::new();

    // Header
    buf.put_i64(0); // base_offset
    buf.put_i32(0); // batch_length (placeholder)
    buf.put_i32(0); // partition_leader_epoch
    buf.put_i8(2); // magic (v2)
    let crc_pos = buf.len();
    buf.put_u32_le(0); // CRC placeholder

    // Attributes (no compression, no transactional, no control)
    buf.put_i16(0);
    buf.put_i32(0); // last_offset_delta
    buf.put_i64(1234567890); // base_timestamp
    buf.put_i64(1234567890); // max_timestamp
    buf.put_i64(-1); // producer_id
    buf.put_i16(-1); // producer_epoch
    buf.put_i32(-1); // base_sequence
    buf.put_i32(1); // records_count

    // Single record (key="test-key", value="test-value")
    let mut record = Vec::new();
    record.push(0); // attributes
    record.push(0); // timestamp_delta (varint 0)
    record.push(0); // offset_delta (varint 0)

    // Key: "test-key" (8 bytes)
    record.push(16); // key length (varint 8)
    record.extend_from_slice(b"test-key");

    // Value: "test-value" (10 bytes)
    record.push(20); // value length (varint 10)
    record.extend_from_slice(b"test-value");

    // Headers
    record.push(0); // headers count (varint 0)

    // Write record with length prefix
    buf.put_u8(record.len() as u8); // length (varint, assuming < 128)
    buf.extend_from_slice(&record);

    // Calculate batch_length
    let batch_length = (buf.len() - 12) as i32;
    buf[8..12].copy_from_slice(&batch_length.to_be_bytes());

    // Calculate CRC (from attributes onward)
    let crc_start = crc_pos + 4;
    let crc_data = &buf[crc_start..];
    let mut hasher = Crc32::new();
    hasher.update(crc_data);
    let crc = hasher.finalize();
    buf[crc_pos..crc_pos + 4].copy_from_slice(&crc.to_le_bytes());

    buf.freeze()
}

/// Extract CRC from Kafka batch at position 21-24 (little-endian)
fn extract_crc(batch: &Bytes) -> u32 {
    use bytes::Buf;
    use std::io::Cursor;

    let mut cursor = Cursor::new(&batch[..]);
    cursor.set_position(21); // CRC position in header
    cursor.get_u32_le()
}

#[test]
fn test_deterministic_encoding_multiple_times() {
    let batch = create_simple_kafka_batch();
    let canonical = CanonicalRecord::from_kafka_batch(&batch)
        .expect("Failed to decode");

    // Encode 10 times
    let mut encodings = Vec::new();
    for _ in 0..10 {
        encodings.push(canonical.to_kafka_batch().expect("Failed to encode"));
    }

    // All must be identical
    let first = &encodings[0];
    for (i, encoding) in encodings.iter().enumerate().skip(1) {
        assert_eq!(
            first, encoding,
            "Encoding {} differs from first encoding",
            i
        );
    }

    println!("✓ Deterministic encoding test passed (10 iterations)!");
}

#[test]
fn test_canonical_with_compression() {
    // TODO: Test with gzip/snappy/lz4/zstd compression
    // This requires implementing compression in to_kafka_batch()
    println!("⚠ Compression test not yet implemented");
}

#[test]
fn test_canonical_with_headers() {
    // Create batch with multiple headers
    use bytes::{BufMut, BytesMut};
    use crc32fast::Hasher as Crc32;

    let mut buf = BytesMut::new();

    // Header (same as before)
    buf.put_i64(100);
    buf.put_i32(0); // placeholder
    buf.put_i32(0);
    buf.put_i8(2);
    let crc_pos = buf.len();
    buf.put_u32_le(0);

    buf.put_i16(0); // attributes
    buf.put_i32(0); // last_offset_delta
    buf.put_i64(1000000);
    buf.put_i64(1000000);
    buf.put_i64(-1);
    buf.put_i16(-1);
    buf.put_i32(-1);
    buf.put_i32(1); // records_count

    // Record with headers
    let mut record = Vec::new();
    record.push(0); // attributes
    record.push(0); // timestamp_delta
    record.push(0); // offset_delta
    record.push(4); // key length (varint 2)
    record.extend_from_slice(b"k1");
    record.push(4); // value length (varint 2)
    record.extend_from_slice(b"v1");

    // Headers: 2 headers
    record.push(2); // headers count (varint 2)

    // Header 1: "user-id" = "12345"
    record.push(14); // key length (varint 7)
    record.extend_from_slice(b"user-id");
    record.push(10); // value length (varint 5)
    record.extend_from_slice(b"12345");

    // Header 2: "trace-id" = "abc-def"
    record.push(16); // key length (varint 8)
    record.extend_from_slice(b"trace-id");
    record.push(14); // value length (varint 7)
    record.extend_from_slice(b"abc-def");

    buf.put_u8(record.len() as u8);
    buf.extend_from_slice(&record);

    // Fix batch_length and CRC
    let batch_length = (buf.len() - 12) as i32;
    buf[8..12].copy_from_slice(&batch_length.to_be_bytes());
    let crc_data = &buf[crc_pos + 4..];
    let mut hasher = Crc32::new();
    hasher.update(crc_data);
    let crc = hasher.finalize();
    buf[crc_pos..crc_pos + 4].copy_from_slice(&crc.to_le_bytes());

    let batch = buf.freeze();

    // Decode and verify headers
    let canonical = CanonicalRecord::from_kafka_batch(&batch)
        .expect("Failed to decode batch with headers");

    assert_eq!(canonical.records.len(), 1);
    assert_eq!(canonical.records[0].headers.len(), 2);
    assert_eq!(canonical.records[0].headers[0].key, "user-id");
    assert_eq!(
        canonical.records[0].headers[0].value,
        Some(b"12345".to_vec())
    );

    // Re-encode and verify CRC
    let re_encoded = canonical.to_kafka_batch()
        .expect("Failed to encode");

    let original_crc = extract_crc(&batch);
    let re_encoded_crc = extract_crc(&re_encoded);

    assert_eq!(original_crc, re_encoded_crc, "CRC mismatch with headers");

    println!("✓ Headers test passed!");
}
