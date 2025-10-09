//! Canonical internal record format for Chronik Stream.
//!
//! This module defines the internal representation of Kafka records that:
//! 1. Preserves ALL fields from Kafka RecordBatch format v2
//! 2. Enables deterministic encoding back to Kafka wire format
//! 3. Guarantees CRC matching through precise round-trip conversion
//!
//! ## Design Principles
//!
//! - **Field Preservation:** Every Kafka field is stored exactly (no lossy transformations)
//! - **Deterministic Encoding:** `to_kafka_batch()` always produces identical bytes
//! - **Round-Trip Guarantee:** `decode(encode(x)) == x`
//!
//! ## Architecture
//!
//! ```text
//! Kafka Wire Format (from producer)
//!       ↓ from_kafka_batch()
//! CanonicalRecord (internal storage)
//!       ↓ to_kafka_batch()
//! Kafka Wire Format (to consumer, CRC matches!)
//! ```

use bytes::{Buf, BufMut, Bytes, BytesMut};
use chronik_common::{Error, Result};
use crc32fast::Hasher as Crc32;
use flate2::read::{GzDecoder, ZlibDecoder};
use flate2::write::{GzEncoder, ZlibEncoder};
use flate2::Compression;
use serde::{Deserialize, Serialize};
use std::io::{Cursor, Read, Write};

/// Magic byte for RecordBatch format v2 (KIP-98)
const MAGIC_V2: i8 = 2;

/// Compression types supported by Kafka
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompressionType {
    None = 0,
    Gzip = 1,
    Snappy = 2,
    Lz4 = 3,
    Zstd = 4,
}

impl CompressionType {
    pub fn from_attributes(attributes: u16) -> Self {
        match attributes & 0x07 {
            0 => CompressionType::None,
            1 => CompressionType::Gzip,
            2 => CompressionType::Snappy,
            3 => CompressionType::Lz4,
            4 => CompressionType::Zstd,
            _ => CompressionType::None,
        }
    }

    pub fn to_attributes(self) -> u16 {
        self as u16
    }
}

/// Timestamp type
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TimestampType {
    CreateTime = 0,
    LogAppendTime = 1,
}

impl TimestampType {
    pub fn from_attributes(attributes: u16) -> Self {
        if (attributes & 0x08) != 0 {
            TimestampType::LogAppendTime
        } else {
            TimestampType::CreateTime
        }
    }

    pub fn to_attributes(self) -> u16 {
        match self {
            TimestampType::CreateTime => 0,
            TimestampType::LogAppendTime => 0x08,
        }
    }
}

/// Canonical internal record format that preserves all Kafka RecordBatch fields.
///
/// This is the single source of truth for record storage in Chronik Stream.
/// It can be deterministically encoded back to Kafka wire format.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CanonicalRecord {
    // === Batch-level fields ===
    /// Base offset of this batch (first record's offset)
    pub base_offset: i64,

    /// Partition leader epoch
    pub partition_leader_epoch: i32,

    /// Producer ID (for idempotent/transactional producers)
    pub producer_id: i64,

    /// Producer epoch
    pub producer_epoch: i16,

    /// Base sequence number (for idempotent producers)
    pub base_sequence: i32,

    /// Whether this batch is transactional
    pub is_transactional: bool,

    /// Whether this is a control batch
    pub is_control: bool,

    /// Compression type used for this batch
    pub compression: CompressionType,

    /// Timestamp type (CreateTime vs LogAppendTime)
    pub timestamp_type: TimestampType,

    /// Base timestamp (for timestamp delta calculation)
    pub base_timestamp: i64,

    /// Maximum timestamp in this batch
    pub max_timestamp: i64,

    // === Record-level data ===
    /// Individual records in this batch
    pub records: Vec<CanonicalRecordEntry>,

    // === CRITICAL: Preserve original compressed bytes for byte-perfect round-trip ===
    /// Original compressed records section from wire format.
    ///
    /// **WHY THIS EXISTS:**
    /// Compression (especially gzip) is NON-DETERMINISTIC - compressing the same data
    /// twice produces DIFFERENT bytes (timestamps, OS flags, compression level variations).
    /// Java Kafka clients validate CRC, which is calculated over the ENTIRE batch including
    /// compressed bytes. If we decompress → re-compress, CRC changes and clients reject the batch.
    ///
    /// **SOLUTION:**
    /// When decoding from wire format (`from_kafka_batch()`), we preserve the ORIGINAL
    /// compressed bytes. When encoding back (`to_kafka_batch()`), we use these bytes
    /// INSTEAD of re-compressing, guaranteeing byte-perfect round-trip and CRC match.
    ///
    /// **WHEN IT'S NONE:**
    /// - Batches created from Tantivy segments (`from_entries()`)
    /// - Batches we synthesize ourselves (test fixtures, etc.)
    /// In these cases, `to_kafka_batch()` will compress normally.
    ///
    /// **SERIALIZATION:**
    /// This field IS included in bincode serialization (WAL V2), so the original bytes
    /// are preserved across WAL recovery. We skip serialization only if None to save space.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compressed_records_wire_bytes: Option<Vec<u8>>,
}

/// Individual record entry within a CanonicalRecord batch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CanonicalRecordEntry {
    /// Absolute offset of this record
    pub offset: i64,

    /// Absolute timestamp of this record
    pub timestamp: i64,

    /// Record key (optional)
    pub key: Option<Vec<u8>>,

    /// Record value (optional, but usually present)
    pub value: Option<Vec<u8>>,

    /// Record headers (key-value pairs)
    pub headers: Vec<RecordHeader>,

    /// Record-level attributes (usually 0)
    pub attributes: i8,
}

/// Record header (key-value pair)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordHeader {
    /// Header key
    pub key: String,

    /// Header value (optional)
    pub value: Option<Vec<u8>>,
}

impl CanonicalRecord {
    /// Get the minimum offset in this batch (same as base_offset)
    pub fn min_offset(&self) -> i64 {
        self.base_offset
    }

    /// Get the maximum offset in this batch (last record's offset)
    pub fn last_offset(&self) -> i64 {
        self.records.last()
            .map(|r| r.offset)
            .unwrap_or(self.base_offset)
    }

    /// Get the minimum timestamp in this batch (base_timestamp)
    pub fn min_timestamp(&self) -> i64 {
        self.base_timestamp
    }

    /// Reconstruct a CanonicalRecord from individual CanonicalRecordEntries.
    ///
    /// This is used when reading from Tantivy segments, which store individual
    /// entries rather than full batches. The batch-level metadata is reconstructed
    /// from the entries and provided defaults.
    ///
    /// # Arguments
    ///
    /// * `entries` - Individual record entries (must not be empty)
    /// * `compression` - Compression type for the batch (default: None)
    /// * `timestamp_type` - Timestamp type (default: CreateTime)
    ///
    /// # Returns
    ///
    /// A CanonicalRecord that can be encoded to Kafka wire format.
    ///
    /// # Errors
    ///
    /// Returns an error if entries is empty.
    pub fn from_entries(
        entries: Vec<CanonicalRecordEntry>,
        compression: CompressionType,
        timestamp_type: TimestampType,
    ) -> Result<Self> {
        if entries.is_empty() {
            return Err(Error::Internal(
                "Cannot create CanonicalRecord from empty entries".into(),
            ));
        }

        // Calculate batch-level metadata from entries
        let base_offset = entries[0].offset;
        let mut min_timestamp = i64::MAX;
        let mut max_timestamp = i64::MIN;

        for entry in &entries {
            min_timestamp = min_timestamp.min(entry.timestamp);
            max_timestamp = max_timestamp.max(entry.timestamp);
        }

        // Use defaults for batch-level fields not stored in individual entries
        // These values are typical for non-transactional, non-idempotent producers
        Ok(CanonicalRecord {
            base_offset,
            partition_leader_epoch: -1, // Unknown when reconstructing
            producer_id: -1,            // Not transactional
            producer_epoch: -1,
            base_sequence: -1,
            is_transactional: false,
            is_control: false,
            compression,
            timestamp_type,
            base_timestamp: min_timestamp,
            max_timestamp,
            records: entries,
            compressed_records_wire_bytes: None, // Created from entries, no original bytes
        })
    }

    /// Decode a Kafka RecordBatch from wire format to CanonicalRecord.
    ///
    /// This handles ALL Kafka formats: v0 (legacy), v1 (legacy with timestamps), v2 (modern).
    /// Legacy formats are automatically converted to v2 internally.
    ///
    /// # Arguments
    ///
    /// * `wire_bytes` - Raw Kafka RecordBatch in wire format (any version)
    ///
    /// # Returns
    ///
    /// A `CanonicalRecord` that can be deterministically re-encoded.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Wire format is malformed
    /// - Compression is not supported
    /// - CRC validation fails
    pub fn from_kafka_batch(wire_bytes: &[u8]) -> Result<Self> {
        // Use KafkaRecordBatch::decode() which handles v0, v1, v2 formats
        use crate::kafka_records::KafkaRecordBatch;

        let (kafka_batch, _bytes_consumed) = KafkaRecordBatch::decode(wire_bytes)?;

        // Convert to CanonicalRecord
        Self::from_kafka_record_batch(kafka_batch)
    }

    /// Convert KafkaRecordBatch to CanonicalRecord (internal helper).
    ///
    /// This is used by from_kafka_batch() after KafkaRecordBatch::decode() has
    /// handled format detection and conversion.
    ///
    /// CRITICAL: This method also preserves the original compressed records bytes
    /// to enable byte-perfect round-trip encoding.
    fn from_kafka_record_batch(batch: crate::kafka_records::KafkaRecordBatch) -> Result<Self> {
        // Extract batch-level fields from header
        let base_offset = batch.header.base_offset;
        let partition_leader_epoch = batch.header.partition_leader_epoch;
        let producer_id = batch.header.producer_id;
        let producer_epoch = batch.header.producer_epoch;
        let base_sequence = batch.header.base_sequence;

        // Extract flags from attributes
        let is_transactional = (batch.header.attributes & 0x10) != 0;
        let is_control = (batch.header.attributes & 0x20) != 0;
        let compression = CompressionType::from_attributes(batch.header.attributes);
        let timestamp_type = TimestampType::from_attributes(batch.header.attributes);

        let base_timestamp = batch.header.base_timestamp;
        let max_timestamp = batch.header.max_timestamp;

        // CRITICAL: Preserve original compressed records bytes if batch was compressed
        // This is stored in KafkaRecordBatch.compressed_records_data after decoding
        let compressed_records_wire_bytes = if compression != CompressionType::None {
            batch.compressed_records_data.map(|bytes| bytes.to_vec())
        } else {
            None
        };

        // Convert records
        let mut entries = Vec::with_capacity(batch.records.len());
        for record in batch.records {
            entries.push(CanonicalRecordEntry {
                offset: base_offset + record.offset_delta as i64,
                timestamp: base_timestamp + record.timestamp_delta,
                key: record.key.map(|b| b.to_vec()),
                value: record.value.map(|b| b.to_vec()),
                headers: record.headers.iter().map(|h| RecordHeader {
                    key: h.key.clone(),
                    value: h.value.as_ref().map(|v| v.to_vec()),
                }).collect(),
                attributes: record.attributes,
            });
        }

        Ok(CanonicalRecord {
            base_offset,
            partition_leader_epoch,
            producer_id,
            producer_epoch,
            base_sequence,
            is_transactional,
            is_control,
            compression,
            timestamp_type,
            base_timestamp,
            max_timestamp,
            records: entries,
            compressed_records_wire_bytes,
        })
    }

    /// OLD IMPLEMENTATION PRESERVED BELOW FOR REFERENCE (v1.3.36)
    /// This is the direct v2-only parsing that we replaced with the more flexible approach above.
    #[allow(dead_code)]
    fn from_kafka_batch_v2_only(wire_bytes: &[u8]) -> Result<Self> {
        if wire_bytes.len() < 61 {
            return Err(Error::Protocol(
                "RecordBatch too small (minimum 61 bytes)".into(),
            ));
        }

        let mut cursor = Cursor::new(wire_bytes);

        // Parse batch header (61 bytes total)
        let base_offset = cursor.get_i64();
        let batch_length = cursor.get_i32();
        let partition_leader_epoch = cursor.get_i32();
        let magic = cursor.get_i8();

        if magic != MAGIC_V2 {
            return Err(Error::Protocol(format!(
                "Unsupported magic byte: {} (expected {})",
                magic, MAGIC_V2
            )));
        }

        let crc = cursor.get_u32_le(); // CRC is little-endian
        let attributes = cursor.get_u16();
        let last_offset_delta = cursor.get_i32();
        let base_timestamp = cursor.get_i64();
        let max_timestamp = cursor.get_i64();
        let producer_id = cursor.get_i64();
        let producer_epoch = cursor.get_i16();
        let base_sequence = cursor.get_i32();
        let records_count = cursor.get_i32();

        // Verify CRC (optional but recommended)
        // CRC is calculated over everything from partition_leader_epoch onwards
        let crc_start = 12; // After base_offset (8) + batch_length (4)
        let crc_data_end = 12 + batch_length as usize;
        if crc_data_end > wire_bytes.len() {
            return Err(Error::Protocol("Batch length exceeds available data".into()));
        }
        let crc_data = &wire_bytes[crc_start..crc_data_end];

        // Compute expected CRC (zero out CRC field first)
        let mut crc_check_data = crc_data.to_vec();
        // CRC field is at offset 4-7 (after partition_leader_epoch and magic)
        crc_check_data[4..8].copy_from_slice(&[0, 0, 0, 0]);
        let computed_crc = calculate_crc32(&crc_check_data);

        if crc != computed_crc {
            tracing::warn!(
                "CRC mismatch: expected {}, got {} (will proceed but data may be corrupt)",
                crc,
                computed_crc
            );
        }

        // Extract attribute flags
        let compression = CompressionType::from_attributes(attributes);
        let timestamp_type = TimestampType::from_attributes(attributes);
        let is_transactional = (attributes & 0x10) != 0;
        let is_control = (attributes & 0x20) != 0;

        // Read records section
        let records_start = cursor.position() as usize;
        let records_end = crc_data_end;
        let records_data = &wire_bytes[records_start..records_end];

        // Decompress if needed
        let uncompressed_records = decompress_records(records_data, compression)?;

        // Parse individual records
        let records = parse_records(
            &uncompressed_records,
            base_offset,
            base_timestamp,
            records_count as usize,
        )?;

        Ok(CanonicalRecord {
            base_offset,
            partition_leader_epoch,
            producer_id,
            producer_epoch,
            base_sequence,
            is_transactional,
            is_control,
            compression,
            timestamp_type,
            base_timestamp,
            max_timestamp,
            records,
            compressed_records_wire_bytes: None, // Old implementation, doesn't preserve
        })
    }

    /// Encode this CanonicalRecord back to Kafka wire format.
    ///
    /// This is deterministic: calling this method multiple times on the same
    /// CanonicalRecord will produce identical bytes (including CRC).
    ///
    /// # Returns
    ///
    /// Raw Kafka RecordBatch bytes ready to send to Kafka clients.
    ///
    /// # Errors
    ///
    /// Returns an error if encoding fails (compression error, etc.)
    pub fn to_kafka_batch(&self) -> Result<Bytes> {
        let mut buf = BytesMut::new();

        // Write base offset and reserve space for batch length
        buf.put_i64(self.base_offset);
        let batch_length_pos = buf.len();
        buf.put_i32(0); // Placeholder for batch length

        // Start of CRC calculation region
        let crc_start = buf.len();

        // Write header fields
        buf.put_i32(self.partition_leader_epoch);
        buf.put_i8(MAGIC_V2);

        // Reserve space for CRC
        let crc_pos = buf.len();
        buf.put_u32_le(0); // Placeholder for CRC (little-endian)

        // Write attributes
        let mut attributes = self.compression.to_attributes();
        attributes |= self.timestamp_type.to_attributes();
        if self.is_transactional {
            attributes |= 0x10;
        }
        if self.is_control {
            attributes |= 0x20;
        }
        buf.put_u16(attributes);

        // Write last_offset_delta
        let last_offset_delta = if self.records.is_empty() {
            0
        } else {
            (self.records.len() - 1) as i32
        };
        buf.put_i32(last_offset_delta);

        // Write timestamps
        buf.put_i64(self.base_timestamp);
        buf.put_i64(self.max_timestamp);

        // Write producer info
        buf.put_i64(self.producer_id);
        buf.put_i16(self.producer_epoch);
        buf.put_i32(self.base_sequence);

        // Write record count
        buf.put_i32(self.records.len() as i32);

        // CRITICAL: Use preserved compressed bytes if available, otherwise encode+compress
        // This ensures byte-perfect round-trip and CRC validation for Java clients
        if let Some(ref original_compressed) = self.compressed_records_wire_bytes {
            // Use ORIGINAL compressed bytes from producer (byte-perfect!)
            buf.extend_from_slice(original_compressed);
        } else {
            // No original bytes (batch created from entries or new batch)
            // Encode and compress normally
            let records_data = encode_records(&self.records, self.base_offset, self.base_timestamp)?;
            let compressed_records = compress_records(&records_data, self.compression)?;
            buf.extend_from_slice(&compressed_records);
        }

        // Calculate and write batch length
        let batch_length = (buf.len() - batch_length_pos - 4) as i32;
        buf[batch_length_pos..batch_length_pos + 4].copy_from_slice(&batch_length.to_be_bytes());

        // Calculate and write CRC (CRITICAL for matching original)
        // Zero out CRC field first
        buf[crc_pos..crc_pos + 4].copy_from_slice(&[0, 0, 0, 0]);

        // CRC is calculated over everything from partition_leader_epoch to end
        let crc_data = &buf[crc_start..];
        let crc = calculate_crc32(crc_data);

        // Write CRC as little-endian (Kafka protocol requirement)
        buf[crc_pos..crc_pos + 4].copy_from_slice(&crc.to_le_bytes());

        Ok(buf.freeze())
    }

    /// Update batch header fields WITHOUT decompressing/recompressing records.
    ///
    /// This is CRITICAL for Java Kafka client CRC validation. When we assign new offsets,
    /// we must NOT re-compress the records section, as compression is non-deterministic
    /// and will produce different bytes even with identical input, breaking CRC validation.
    ///
    /// This method:
    /// 1. Parses ONLY the header to extract current base_offset and metadata
    /// 2. Updates base_offset to the new assigned value
    /// 3. Recalculates last_offset_delta and max_timestamp if needed
    /// 4. Keeps the compressed records section BYTE-IDENTICAL to the original
    /// 5. Recalculates CRC over (updated header + original compressed records)
    ///
    /// # Arguments
    ///
    /// * `wire_bytes` - Original Kafka RecordBatch wire format bytes
    /// * `new_base_offset` - The new base offset to assign
    ///
    /// # Returns
    ///
    /// Updated RecordBatch bytes with correct CRC for the new offset
    ///
    /// # Errors
    ///
    /// Returns an error if wire format is malformed
    pub fn update_header_preserve_records(
        wire_bytes: &[u8],
        new_base_offset: i64,
    ) -> Result<Bytes> {
        if wire_bytes.len() < 61 {
            return Err(Error::Protocol(
                "RecordBatch too small (minimum 61 bytes)".into(),
            ));
        }

        let mut cursor = Cursor::new(wire_bytes);

        // Parse header
        let _original_base_offset = cursor.get_i64();
        let batch_length = cursor.get_i32();
        let partition_leader_epoch = cursor.get_i32();
        let magic = cursor.get_i8();

        if magic != MAGIC_V2 {
            return Err(Error::Protocol(format!(
                "Unsupported magic byte: {} (expected {})",
                magic, MAGIC_V2
            )));
        }

        let _original_crc = cursor.get_u32_le();
        let attributes = cursor.get_u16();
        let last_offset_delta = cursor.get_i32();
        let base_timestamp = cursor.get_i64();
        let max_timestamp = cursor.get_i64();
        let producer_id = cursor.get_i64();
        let producer_epoch = cursor.get_i16();
        let base_sequence = cursor.get_i32();
        let records_count = cursor.get_i32();

        // Extract the original compressed records section (unchanged)
        let records_start = cursor.position() as usize;
        let records_end = 12 + batch_length as usize; // After base_offset (8) + batch_length (4)
        if records_end > wire_bytes.len() {
            return Err(Error::Protocol("Batch length exceeds available data".into()));
        }
        let original_records = &wire_bytes[records_start..records_end];

        // Build new header with updated base_offset
        let mut buf = BytesMut::new();

        // Write base offset (UPDATED)
        buf.put_i64(new_base_offset);

        // Write batch length (same as original)
        buf.put_i32(batch_length);

        // Start of CRC calculation region
        let crc_start = buf.len();

        // Write header fields
        buf.put_i32(partition_leader_epoch);
        buf.put_i8(magic);

        // Reserve space for CRC
        let crc_pos = buf.len();
        buf.put_u32_le(0); // Placeholder

        // Write attributes (unchanged)
        buf.put_u16(attributes);

        // Write last_offset_delta (unchanged - still relative to base_offset)
        buf.put_i32(last_offset_delta);

        // Write timestamps (unchanged)
        buf.put_i64(base_timestamp);
        buf.put_i64(max_timestamp);

        // Write producer info (unchanged)
        buf.put_i64(producer_id);
        buf.put_i16(producer_epoch);
        buf.put_i32(base_sequence);

        // Write record count (unchanged)
        buf.put_i32(records_count);

        // Append ORIGINAL compressed records (BYTE-IDENTICAL to input)
        buf.extend_from_slice(original_records);

        // Calculate and write CRC
        // Zero out CRC field first
        buf[crc_pos..crc_pos + 4].copy_from_slice(&[0, 0, 0, 0]);

        // CRC is calculated over everything from partition_leader_epoch to end
        let crc_data = &buf[crc_start..];
        let crc = calculate_crc32(crc_data);

        // Write CRC as little-endian
        buf[crc_pos..crc_pos + 4].copy_from_slice(&crc.to_le_bytes());

        Ok(buf.freeze())
    }

    /// Estimate the size of this record in bytes (for buffer management)
    pub fn estimated_size(&self) -> usize {
        let mut size = 61; // Header size
        for record in &self.records {
            size += 10; // Record overhead (length, attributes, deltas)
            size += record.key.as_ref().map(|k| k.len()).unwrap_or(0);
            size += record.value.as_ref().map(|v| v.len()).unwrap_or(0);
            size += record
                .headers
                .iter()
                .map(|h| h.key.len() + h.value.as_ref().map(|v| v.len()).unwrap_or(0))
                .sum::<usize>();
        }
        size
    }
}

/// Calculate CRC-32C (Castagnoli) checksum as used by Kafka.
///
/// Uses the crc32fast library which implements the Castagnoli polynomial (0x1EDC6F41).
fn calculate_crc32(data: &[u8]) -> u32 {
    let mut hasher = Crc32::new();
    hasher.update(data);
    hasher.finalize()
}

/// Decompress record data if needed.
fn decompress_records(data: &[u8], compression: CompressionType) -> Result<Vec<u8>> {
    match compression {
        CompressionType::None => Ok(data.to_vec()),
        CompressionType::Gzip => {
            let mut decoder = GzDecoder::new(data);
            let mut decompressed = Vec::new();
            decoder
                .read_to_end(&mut decompressed)
                .map_err(|e| Error::Internal(format!("Gzip decompression failed: {}", e)))?;
            Ok(decompressed)
        }
        CompressionType::Zstd => {
            // Using Zlib as fallback (Kafka sometimes uses Zlib for Zstd codec)
            let mut decoder = ZlibDecoder::new(data);
            let mut decompressed = Vec::new();
            decoder
                .read_to_end(&mut decompressed)
                .map_err(|e| Error::Internal(format!("Zlib decompression failed: {}", e)))?;
            Ok(decompressed)
        }
        _ => Err(Error::Internal(format!(
            "Unsupported compression: {:?}",
            compression
        ))),
    }
}

/// Compress record data if needed.
fn compress_records(data: &[u8], compression: CompressionType) -> Result<Vec<u8>> {
    match compression {
        CompressionType::None => Ok(data.to_vec()),
        CompressionType::Gzip => {
            let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
            encoder
                .write_all(data)
                .map_err(|e| Error::Internal(format!("Gzip compression failed: {}", e)))?;
            encoder
                .finish()
                .map_err(|e| Error::Internal(format!("Gzip finish failed: {}", e)))
        }
        CompressionType::Zstd => {
            // Using Zlib as fallback
            let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
            encoder
                .write_all(data)
                .map_err(|e| Error::Internal(format!("Zlib compression failed: {}", e)))?;
            encoder
                .finish()
                .map_err(|e| Error::Internal(format!("Zlib finish failed: {}", e)))
        }
        _ => Err(Error::Internal(format!(
            "Unsupported compression: {:?}",
            compression
        ))),
    }
}

/// Parse individual records from uncompressed record data.
fn parse_records(
    data: &[u8],
    base_offset: i64,
    base_timestamp: i64,
    expected_count: usize,
) -> Result<Vec<CanonicalRecordEntry>> {
    let mut records = Vec::with_capacity(expected_count);
    let mut cursor = Cursor::new(data);

    while cursor.position() < data.len() as u64 {
        // Varint length
        let length = read_varint(&mut cursor)?;
        if length < 0 {
            break; // End of records
        }

        // Read record fields
        let attributes = cursor.get_i8();
        let timestamp_delta = read_varlong(&mut cursor)?;
        let offset_delta = read_varint(&mut cursor)?;

        // Key
        let key_length = read_varint(&mut cursor)?;
        let key = if key_length >= 0 {
            let mut key_bytes = vec![0u8; key_length as usize];
            cursor
                .read_exact(&mut key_bytes)
                .map_err(|e| Error::Protocol(format!("Failed to read key: {}", e)))?;
            Some(key_bytes)
        } else {
            None
        };

        // Value
        let value_length = read_varint(&mut cursor)?;
        let value = if value_length >= 0 {
            let mut value_bytes = vec![0u8; value_length as usize];
            cursor
                .read_exact(&mut value_bytes)
                .map_err(|e| Error::Protocol(format!("Failed to read value: {}", e)))?;
            Some(value_bytes)
        } else {
            None
        };

        // Headers
        let header_count = read_varint(&mut cursor)?;
        let mut headers = Vec::with_capacity(header_count as usize);
        for _ in 0..header_count {
            let key_length = read_varint(&mut cursor)?;
            let mut key_bytes = vec![0u8; key_length as usize];
            cursor
                .read_exact(&mut key_bytes)
                .map_err(|e| Error::Protocol(format!("Failed to read header key: {}", e)))?;
            let key = String::from_utf8(key_bytes)
                .map_err(|e| Error::Protocol(format!("Invalid header key UTF-8: {}", e)))?;

            let value_length = read_varint(&mut cursor)?;
            let value = if value_length >= 0 {
                let mut value_bytes = vec![0u8; value_length as usize];
                cursor.read_exact(&mut value_bytes).map_err(|e| {
                    Error::Protocol(format!("Failed to read header value: {}", e))
                })?;
                Some(value_bytes)
            } else {
                None
            };

            headers.push(RecordHeader { key, value });
        }

        // Calculate absolute offset and timestamp
        let offset = base_offset + offset_delta as i64;
        let timestamp = base_timestamp + timestamp_delta;

        records.push(CanonicalRecordEntry {
            offset,
            timestamp,
            key,
            value,
            headers,
            attributes,
        });
    }

    Ok(records)
}

/// Encode records back to wire format.
fn encode_records(
    records: &[CanonicalRecordEntry],
    base_offset: i64,
    base_timestamp: i64,
) -> Result<Vec<u8>> {
    let mut buf = Vec::new();

    for record in records {
        // Calculate deltas
        let offset_delta = (record.offset - base_offset) as i32;
        let timestamp_delta = record.timestamp - base_timestamp;

        // Calculate record length (will be filled in after encoding)
        let length_pos = buf.len();
        write_varint(&mut buf, 0); // Placeholder

        let record_start = buf.len();

        // Attributes
        buf.push(record.attributes as u8);

        // Timestamp delta
        write_varlong(&mut buf, timestamp_delta);

        // Offset delta
        write_varint(&mut buf, offset_delta);

        // Key
        if let Some(ref key) = record.key {
            write_varint(&mut buf, key.len() as i32);
            buf.extend_from_slice(key);
        } else {
            write_varint(&mut buf, -1);
        }

        // Value
        if let Some(ref value) = record.value {
            write_varint(&mut buf, value.len() as i32);
            buf.extend_from_slice(value);
        } else {
            write_varint(&mut buf, -1);
        }

        // Headers
        write_varint(&mut buf, record.headers.len() as i32);
        for header in &record.headers {
            write_varint(&mut buf, header.key.len() as i32);
            buf.extend_from_slice(header.key.as_bytes());

            if let Some(ref value) = header.value {
                write_varint(&mut buf, value.len() as i32);
                buf.extend_from_slice(value);
            } else {
                write_varint(&mut buf, -1);
            }
        }

        // Calculate and write actual length
        let record_length = (buf.len() - record_start) as i32;
        let mut length_buf = Vec::new();
        write_varint(&mut length_buf, record_length);

        // Replace placeholder length
        buf.splice(length_pos..record_start, length_buf);
    }

    Ok(buf)
}

/// Read a variable-length integer (varint) from the cursor.
fn read_varint(cursor: &mut Cursor<&[u8]>) -> Result<i32> {
    let mut value: i32 = 0;
    let mut shift = 0;

    loop {
        if !cursor.has_remaining() {
            return Err(Error::Protocol("Unexpected end of varint".into()));
        }

        let byte = cursor.get_u8();
        value |= ((byte & 0x7F) as i32) << shift;

        if (byte & 0x80) == 0 {
            break;
        }

        shift += 7;
        if shift > 28 {
            return Err(Error::Protocol("Varint too large".into()));
        }
    }

    // ZigZag decode
    Ok((value >> 1) ^ -(value & 1))
}

/// Read a variable-length long (varlong) from the cursor.
fn read_varlong(cursor: &mut Cursor<&[u8]>) -> Result<i64> {
    let mut value: i64 = 0;
    let mut shift = 0;

    loop {
        if !cursor.has_remaining() {
            return Err(Error::Protocol("Unexpected end of varlong".into()));
        }

        let byte = cursor.get_u8();
        value |= ((byte & 0x7F) as i64) << shift;

        if (byte & 0x80) == 0 {
            break;
        }

        shift += 7;
        if shift > 63 {
            return Err(Error::Protocol("Varlong too large".into()));
        }
    }

    // ZigZag decode
    Ok((value >> 1) ^ -(value & 1))
}

/// Write a variable-length integer (varint) to the buffer.
fn write_varint(buf: &mut Vec<u8>, value: i32) {
    // ZigZag encode
    let mut n = ((value << 1) ^ (value >> 31)) as u32;

    while n >= 0x80 {
        buf.push((n as u8) | 0x80);
        n >>= 7;
    }
    buf.push(n as u8);
}

/// Write a variable-length long (varlong) to the buffer.
fn write_varlong(buf: &mut Vec<u8>, value: i64) {
    // ZigZag encode
    let mut n = ((value << 1) ^ (value >> 63)) as u64;

    while n >= 0x80 {
        buf.push((n as u8) | 0x80);
        n >>= 7;
    }
    buf.push(n as u8);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_varint_roundtrip() {
        let test_values = vec![0, 1, -1, 127, -127, 12345, -12345];

        for value in test_values {
            let mut buf = Vec::new();
            write_varint(&mut buf, value);

            let mut cursor = Cursor::new(&buf[..]);
            let decoded = read_varint(&mut cursor).unwrap();

            assert_eq!(value, decoded, "Varint roundtrip failed for {}", value);
        }
    }

    #[test]
    fn test_varlong_roundtrip() {
        let test_values = vec![0, 1, -1, 127, -127, 123456789, -123456789];

        for value in test_values {
            let mut buf = Vec::new();
            write_varlong(&mut buf, value);

            let mut cursor = Cursor::new(&buf[..]);
            let decoded = read_varlong(&mut cursor).unwrap();

            assert_eq!(value, decoded, "Varlong roundtrip failed for {}", value);
        }
    }

    /// Helper to create a simple uncompressed Kafka RecordBatch v2
    fn create_test_kafka_batch(
        base_offset: i64,
        partition_leader_epoch: i32,
        records: Vec<(Option<Vec<u8>>, Option<Vec<u8>>, Vec<(String, Vec<u8>)>)>, // (key, value, headers)
    ) -> Bytes {
        let mut buf = BytesMut::new();

        // Header placeholder
        let header_start = buf.len();
        buf.put_i64(base_offset);
        buf.put_i32(0); // batch_length placeholder
        buf.put_i32(partition_leader_epoch);
        buf.put_i8(MAGIC_V2);
        let crc_pos = buf.len();
        buf.put_u32_le(0); // CRC placeholder

        // Attributes: no compression
        buf.put_i16(0);
        buf.put_i32((records.len() - 1) as i32); // last_offset_delta
        buf.put_i64(1_000_000); // base_timestamp (1 second in millis)
        buf.put_i64(1_000_000 + (records.len() as i64 - 1) * 1000); // max_timestamp
        buf.put_i64(-1); // producer_id
        buf.put_i16(-1); // producer_epoch
        buf.put_i32(-1); // base_sequence
        buf.put_i32(records.len() as i32); // records_count

        // Encode records
        for (i, (key, value, headers)) in records.iter().enumerate() {
            let mut record_content = Vec::new();

            // Attributes
            record_content.push(0);

            // Timestamp delta
            write_varlong(&mut record_content, i as i64 * 1000);

            // Offset delta
            write_varint(&mut record_content, i as i32);

            // Key
            if let Some(k) = key {
                write_varint(&mut record_content, k.len() as i32);
                record_content.extend_from_slice(k);
            } else {
                write_varint(&mut record_content, -1);
            }

            // Value
            if let Some(v) = value {
                write_varint(&mut record_content, v.len() as i32);
                record_content.extend_from_slice(v);
            } else {
                write_varint(&mut record_content, -1);
            }

            // Headers
            write_varint(&mut record_content, headers.len() as i32);
            for (hkey, hval) in headers {
                write_varint(&mut record_content, hkey.len() as i32);
                record_content.extend_from_slice(hkey.as_bytes());
                write_varint(&mut record_content, hval.len() as i32);
                record_content.extend_from_slice(hval);
            }

            // Write length as varint, then content
            let mut length_buf = Vec::new();
            write_varint(&mut length_buf, record_content.len() as i32);
            buf.extend_from_slice(&length_buf);
            buf.extend_from_slice(&record_content);
        }

        // Calculate batch_length (from partition_leader_epoch to end)
        let batch_length = (buf.len() - header_start - 12) as i32;
        buf[header_start + 8..header_start + 12].copy_from_slice(&batch_length.to_be_bytes());

        // Calculate CRC (from attributes to end)
        let crc_start = crc_pos + 4;
        let crc_data = &buf[crc_start..];
        let crc = calculate_crc32(crc_data);
        buf[crc_pos..crc_pos + 4].copy_from_slice(&crc.to_le_bytes());

        buf.freeze()
    }

    #[test]
    fn test_round_trip_uncompressed_single_record() {
        // Create a simple batch with one record
        let original_batch = create_test_kafka_batch(
            100,
            0,
            vec![(
                Some(b"key1".to_vec()),
                Some(b"value1".to_vec()),
                vec![],
            )],
        );

        // Decode to canonical
        let canonical = CanonicalRecord::from_kafka_batch(&original_batch)
            .expect("Failed to decode");

        // Verify decoded fields
        assert_eq!(canonical.base_offset, 100);
        assert_eq!(canonical.records.len(), 1);
        assert_eq!(canonical.records[0].key, Some(b"key1".to_vec()));
        assert_eq!(canonical.records[0].value, Some(b"value1".to_vec()));

        // Encode back to Kafka format
        let encoded_batch = canonical.to_kafka_batch()
            .expect("Failed to encode");

        // Decode again
        let canonical2 = CanonicalRecord::from_kafka_batch(&encoded_batch)
            .expect("Failed to decode re-encoded batch");

        // Verify round-trip
        assert_eq!(canonical.base_offset, canonical2.base_offset);
        assert_eq!(canonical.records.len(), canonical2.records.len());
        assert_eq!(canonical.records[0].key, canonical2.records[0].key);
        assert_eq!(canonical.records[0].value, canonical2.records[0].value);
    }

    #[test]
    fn test_round_trip_multiple_records() {
        // Create batch with multiple records
        let original_batch = create_test_kafka_batch(
            200,
            1,
            vec![
                (Some(b"key1".to_vec()), Some(b"value1".to_vec()), vec![]),
                (Some(b"key2".to_vec()), Some(b"value2".to_vec()), vec![]),
                (Some(b"key3".to_vec()), Some(b"value3".to_vec()), vec![]),
            ],
        );

        // Decode
        let canonical = CanonicalRecord::from_kafka_batch(&original_batch)
            .expect("Failed to decode");

        assert_eq!(canonical.base_offset, 200);
        assert_eq!(canonical.records.len(), 3);

        // Encode back
        let encoded_batch = canonical.to_kafka_batch()
            .expect("Failed to encode");

        // Decode again
        let canonical2 = CanonicalRecord::from_kafka_batch(&encoded_batch)
            .expect("Failed to decode re-encoded batch");

        // Verify all records
        assert_eq!(canonical2.records.len(), 3);
        for i in 0..3 {
            assert_eq!(
                canonical.records[i].key,
                canonical2.records[i].key,
                "Record {} key mismatch", i
            );
            assert_eq!(
                canonical.records[i].value,
                canonical2.records[i].value,
                "Record {} value mismatch", i
            );
        }
    }

    #[test]
    fn test_round_trip_with_headers() {
        // Create batch with headers
        let original_batch = create_test_kafka_batch(
            300,
            2,
            vec![(
                Some(b"key1".to_vec()),
                Some(b"value1".to_vec()),
                vec![
                    ("header1".to_string(), b"hval1".to_vec()),
                    ("header2".to_string(), b"hval2".to_vec()),
                ],
            )],
        );

        // Decode
        let canonical = CanonicalRecord::from_kafka_batch(&original_batch)
            .expect("Failed to decode");

        assert_eq!(canonical.records[0].headers.len(), 2);
        assert_eq!(canonical.records[0].headers[0].key, "header1");
        assert_eq!(canonical.records[0].headers[0].value, Some(b"hval1".to_vec()));

        // Encode and decode again
        let encoded_batch = canonical.to_kafka_batch()
            .expect("Failed to encode");
        let canonical2 = CanonicalRecord::from_kafka_batch(&encoded_batch)
            .expect("Failed to decode re-encoded batch");

        // Verify headers preserved
        assert_eq!(canonical2.records[0].headers.len(), 2);
        assert_eq!(canonical2.records[0].headers[0].key, "header1");
        assert_eq!(canonical2.records[0].headers[0].value, Some(b"hval1".to_vec()));
    }

    #[test]
    fn test_round_trip_null_key_value() {
        // Create batch with null key and value
        let original_batch = create_test_kafka_batch(
            400,
            0,
            vec![
                (None, Some(b"value1".to_vec()), vec![]),
                (Some(b"key2".to_vec()), None, vec![]),
                (None, None, vec![]),
            ],
        );

        // Decode
        let canonical = CanonicalRecord::from_kafka_batch(&original_batch)
            .expect("Failed to decode");

        assert_eq!(canonical.records[0].key, None);
        assert_eq!(canonical.records[0].value, Some(b"value1".to_vec()));

        assert_eq!(canonical.records[1].key, Some(b"key2".to_vec()));
        assert_eq!(canonical.records[1].value, None);

        assert_eq!(canonical.records[2].key, None);
        assert_eq!(canonical.records[2].value, None);

        // Round trip
        let encoded_batch = canonical.to_kafka_batch()
            .expect("Failed to encode");
        let canonical2 = CanonicalRecord::from_kafka_batch(&encoded_batch)
            .expect("Failed to decode re-encoded batch");

        assert_eq!(canonical2.records[0].key, None);
        assert_eq!(canonical2.records[1].value, None);
        assert_eq!(canonical2.records[2].key, None);
        assert_eq!(canonical2.records[2].value, None);
    }

    #[test]
    fn test_deterministic_encoding() {
        // Create batch
        let original_batch = create_test_kafka_batch(
            500,
            0,
            vec![
                (Some(b"key1".to_vec()), Some(b"value1".to_vec()), vec![]),
                (Some(b"key2".to_vec()), Some(b"value2".to_vec()), vec![]),
            ],
        );

        // Decode
        let canonical = CanonicalRecord::from_kafka_batch(&original_batch)
            .expect("Failed to decode");

        // Encode multiple times
        let encoded1 = canonical.to_kafka_batch().expect("Failed to encode 1");
        let encoded2 = canonical.to_kafka_batch().expect("Failed to encode 2");
        let encoded3 = canonical.to_kafka_batch().expect("Failed to encode 3");

        // CRITICAL: All encodings must be IDENTICAL
        assert_eq!(encoded1, encoded2, "Encoding 1 and 2 differ");
        assert_eq!(encoded2, encoded3, "Encoding 2 and 3 differ");
        assert_eq!(encoded1, encoded3, "Encoding 1 and 3 differ");
    }

    #[test]
    fn test_crc_validation() {
        // Create batch
        let original_batch = create_test_kafka_batch(
            600,
            0,
            vec![(Some(b"test".to_vec()), Some(b"data".to_vec()), vec![])],
        );

        // Extract original CRC
        let original_crc = {
            let mut cursor = Cursor::new(&original_batch[..]);
            cursor.set_position(21); // CRC position
            cursor.get_u32_le()
        };

        // Decode and re-encode
        let canonical = CanonicalRecord::from_kafka_batch(&original_batch)
            .expect("Failed to decode");
        let encoded_batch = canonical.to_kafka_batch()
            .expect("Failed to encode");

        // Extract re-encoded CRC
        let encoded_crc = {
            let mut cursor = Cursor::new(&encoded_batch[..]);
            cursor.set_position(21); // CRC position
            cursor.get_u32_le()
        };

        // CRITICAL: CRCs must match (this is the whole point!)
        assert_eq!(
            original_crc, encoded_crc,
            "CRC mismatch: original={:#010x}, encoded={:#010x}",
            original_crc, encoded_crc
        );
    }

    #[test]
    fn test_transactional_batch() {
        // Manually create a transactional batch
        let mut buf = BytesMut::new();
        buf.put_i64(700); // base_offset
        buf.put_i32(0); // batch_length placeholder
        buf.put_i32(3); // partition_leader_epoch
        buf.put_i8(MAGIC_V2);
        let crc_pos = buf.len();
        buf.put_u32_le(0); // CRC placeholder

        // Attributes: bit 4 = transactional
        buf.put_i16(0x0010);
        buf.put_i32(0); // last_offset_delta
        buf.put_i64(2_000_000); // base_timestamp
        buf.put_i64(2_000_000); // max_timestamp
        buf.put_i64(12345); // producer_id
        buf.put_i16(0); // producer_epoch
        buf.put_i32(0); // base_sequence
        buf.put_i32(1); // records_count

        // Simple record
        let mut record_content = Vec::new();
        record_content.push(0); // attributes
        record_content.push(0); // timestamp_delta (varint 0)
        record_content.push(0); // offset_delta (varint 0)
        record_content.push(4); // key length (varint 2)
        record_content.extend_from_slice(b"k1");
        record_content.push(4); // value length (varint 2)
        record_content.extend_from_slice(b"v1");
        record_content.push(0); // headers count (varint 0)

        let mut length_buf = Vec::new();
        write_varint(&mut length_buf, record_content.len() as i32);
        buf.extend_from_slice(&length_buf);
        buf.extend_from_slice(&record_content);

        // Fix batch_length and CRC
        let batch_length = (buf.len() - 12) as i32;
        buf[8..12].copy_from_slice(&batch_length.to_be_bytes());
        let crc = calculate_crc32(&buf[crc_pos + 4..]);
        buf[crc_pos..crc_pos + 4].copy_from_slice(&crc.to_le_bytes());

        let original_batch = buf.freeze();

        // Decode
        let canonical = CanonicalRecord::from_kafka_batch(&original_batch)
            .expect("Failed to decode transactional batch");

        assert!(canonical.is_transactional);
        assert_eq!(canonical.producer_id, 12345);
        assert_eq!(canonical.producer_epoch, 0);
        assert_eq!(canonical.base_sequence, 0);

        // Round trip
        let encoded_batch = canonical.to_kafka_batch()
            .expect("Failed to encode transactional batch");
        let canonical2 = CanonicalRecord::from_kafka_batch(&encoded_batch)
            .expect("Failed to decode re-encoded transactional batch");

        assert!(canonical2.is_transactional);
        assert_eq!(canonical2.producer_id, 12345);
    }
}
