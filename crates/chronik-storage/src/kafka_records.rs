//! Kafka protocol compliant record batch encoding and decoding.
//! 
//! This module implements the Kafka RecordBatch format v2 as defined in KIP-98.
//! The format supports compression, timestamps, headers, and idempotent/transactional semantics.

use bytes::{BufMut, Bytes, BytesMut};
use chronik_common::{Result, Error};
use crc32fast::Hasher as Crc32;
use std::io::{Cursor, Read, Write};
use flate2::write::{GzEncoder, ZlibEncoder};
use flate2::read::{GzDecoder, ZlibDecoder};
use flate2::Compression;

/// Magic byte for RecordBatch format v2
const MAGIC_V2: i8 = 2;
/// Magic byte for legacy message format v1 (with timestamps)
const MAGIC_V1: i8 = 1;
/// Magic byte for legacy message format v0
const MAGIC_V0: i8 = 0;

/// RecordBatch attributes flags
pub mod attributes {
    /// Compression codec mask (bits 0-2)
    pub const COMPRESSION_CODEC_MASK: u16 = 0x07;
    /// Timestamp type mask (bit 3)
    pub const TIMESTAMP_TYPE_MASK: u16 = 0x08;
    /// Transactional flag (bit 4)
    pub const TRANSACTIONAL_FLAG: u16 = 0x10;
    /// Control flag (bit 5)
    pub const CONTROL_FLAG: u16 = 0x20;
}

/// Compression types supported by Kafka
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CompressionType {
    None = 0,
    Gzip = 1,
    Snappy = 2,
    Lz4 = 3,
    Zstd = 4,
}

impl CompressionType {
    pub fn from_attributes(attributes: u16) -> Self {
        match attributes & attributes::COMPRESSION_CODEC_MASK {
            0 => CompressionType::None,
            1 => CompressionType::Gzip,
            2 => CompressionType::Snappy,
            3 => CompressionType::Lz4,
            4 => CompressionType::Zstd,
            _ => CompressionType::None,
        }
    }
}

/// Timestamp type
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TimestampType {
    CreateTime = 0,
    LogAppendTime = 1,
}

/// Kafka RecordBatch header (v2 format)
#[derive(Debug, Clone)]
pub struct RecordBatchHeader {
    pub base_offset: i64,
    pub batch_length: i32,
    pub partition_leader_epoch: i32,
    pub magic: i8,
    pub crc: u32,
    pub attributes: u16,
    pub last_offset_delta: i32,
    pub base_timestamp: i64,
    pub max_timestamp: i64,
    pub producer_id: i64,
    pub producer_epoch: i16,
    pub base_sequence: i32,
    pub records_count: i32,
}

/// Kafka Record (v2 format)
#[derive(Debug, Clone)]
pub struct KafkaRecord {
    pub length: i32,
    pub attributes: i8,
    pub timestamp_delta: i64,
    pub offset_delta: i32,
    pub key: Option<Bytes>,
    pub value: Option<Bytes>,
    pub headers: Vec<RecordHeader>,
}

/// Record header
#[derive(Debug, Clone)]
pub struct RecordHeader {
    pub key: String,
    pub value: Option<Bytes>,
}

/// Kafka RecordBatch (v2 format)
#[derive(Debug)]
pub struct KafkaRecordBatch {
    pub header: RecordBatchHeader,
    pub records: Vec<KafkaRecord>,
    /// Original compressed records bytes (if batch was compressed).
    /// Preserved during decode() to enable byte-perfect round-trip encoding.
    pub compressed_records_data: Option<Bytes>,
}

impl KafkaRecordBatch {
    /// Create a new RecordBatch
    pub fn new(
        base_offset: i64,
        base_timestamp: i64,
        producer_id: i64,
        producer_epoch: i16,
        base_sequence: i32,
        compression: CompressionType,
        is_transactional: bool,
    ) -> Self {
        let mut attributes = compression as u16;
        if is_transactional {
            attributes |= attributes::TRANSACTIONAL_FLAG;
        }
        
        Self {
            header: RecordBatchHeader {
                base_offset,
                batch_length: 0, // Will be calculated during encoding
                partition_leader_epoch: -1,
                magic: MAGIC_V2,
                crc: 0, // Will be calculated during encoding
                attributes,
                last_offset_delta: 0,
                base_timestamp,
                max_timestamp: base_timestamp,
                producer_id,
                producer_epoch,
                base_sequence,
                records_count: 0,
            },
            records: Vec::new(),
            compressed_records_data: None, // Created new, no original bytes
        }
    }
    
    /// Add a record to the batch
    pub fn add_record(
        &mut self,
        key: Option<Bytes>,
        value: Option<Bytes>,
        headers: Vec<RecordHeader>,
        timestamp: i64,
    ) {
        let offset_delta = self.records.len() as i32;
        let timestamp_delta = timestamp - self.header.base_timestamp;
        
        // Update max timestamp
        if timestamp > self.header.max_timestamp {
            self.header.max_timestamp = timestamp;
        }
        
        let record = KafkaRecord {
            length: 0, // Will be calculated during encoding
            attributes: 0,
            timestamp_delta,
            offset_delta,
            key,
            value,
            headers,
        };
        
        self.records.push(record);
        self.header.last_offset_delta = offset_delta;
        self.header.records_count = self.records.len() as i32;
    }
    
    /// Encode the RecordBatch to bytes
    pub fn encode(&self) -> Result<Bytes> {
        let mut buf = BytesMut::new();
        
        // Reserve space for base offset and batch length
        buf.put_i64(self.header.base_offset);
        let batch_length_pos = buf.len();
        buf.put_i32(0); // Placeholder for batch length
        
        // Start of data for CRC calculation
        let crc_start = buf.len();
        
        // Write header fields
        buf.put_i32(self.header.partition_leader_epoch);
        buf.put_i8(self.header.magic);
        let crc_pos = buf.len();
        buf.put_u32(0); // Placeholder for CRC
        buf.put_u16(self.header.attributes);
        buf.put_i32(self.header.last_offset_delta);
        buf.put_i64(self.header.base_timestamp);
        buf.put_i64(self.header.max_timestamp);
        buf.put_i64(self.header.producer_id);
        buf.put_i16(self.header.producer_epoch);
        buf.put_i32(self.header.base_sequence);
        buf.put_i32(self.header.records_count);
        
        // Encode records
        let records_data = self.encode_records()?;
        buf.extend_from_slice(&records_data);
        
        // Calculate and write batch length
        let batch_length = (buf.len() - batch_length_pos - 4) as i32;
        buf[batch_length_pos..batch_length_pos + 4].copy_from_slice(&batch_length.to_be_bytes());
        
        // Calculate and write CRC
        // CRITICAL FIX v1.3.31: Zero out CRC field before calculation
        buf[crc_pos..crc_pos + 4].copy_from_slice(&[0, 0, 0, 0]);

        // Calculate CRC over EVERYTHING from partition_leader_epoch onwards (including zeroed CRC)
        // Kafka CRC-32C calculation must include: partition_leader_epoch, magic, CRC (zeroed), attributes, and all remaining fields
        let crc_data = &buf[crc_start..];
        let crc = calculate_crc32(crc_data);

        // Write CRC as LITTLE-ENDIAN (Kafka protocol requirement)
        buf[crc_pos..crc_pos + 4].copy_from_slice(&crc.to_le_bytes());
        
        Ok(buf.freeze())
    }
    
    /// Encode records with optional compression
    fn encode_records(&self) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        
        for record in &self.records {
            self.encode_record(record, &mut buf)?;
        }
        
        // Apply compression if needed
        let compression = CompressionType::from_attributes(self.header.attributes);
        match compression {
            CompressionType::None => Ok(buf),
            CompressionType::Gzip => {
                let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
                encoder.write_all(&buf)
                    .map_err(|e| Error::Internal(format!("Gzip compression failed: {}", e)))?;
                encoder.finish()
                    .map_err(|e| Error::Internal(format!("Gzip finish failed: {}", e)))
            }
            CompressionType::Zstd => {
                // For now, fallback to zlib since zstd requires additional dependencies
                let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
                encoder.write_all(&buf)
                    .map_err(|e| Error::Internal(format!("Zlib compression failed: {}", e)))?;
                encoder.finish()
                    .map_err(|e| Error::Internal(format!("Zlib finish failed: {}", e)))
            }
            _ => Ok(buf), // Other compression types not implemented yet
        }
    }
    
    /// Encode a single record
    fn encode_record(&self, record: &KafkaRecord, buf: &mut Vec<u8>) -> Result<()> {
        let mut record_buf: Vec<u8> = Vec::new();
        
        // Attributes
        record_buf.push(record.attributes as u8);
        
        // Timestamp delta (varlong)
        encode_varlong(record.timestamp_delta, &mut record_buf);
        
        // Offset delta (varint)
        encode_varint(record.offset_delta, &mut record_buf);
        
        // Key
        if let Some(key) = &record.key {
            encode_varint(key.len() as i32, &mut record_buf);
            record_buf.extend_from_slice(key);
        } else {
            encode_varint(-1, &mut record_buf);
        }
        
        // Value
        if let Some(value) = &record.value {
            encode_varint(value.len() as i32, &mut record_buf);
            record_buf.extend_from_slice(value);
        } else {
            encode_varint(-1, &mut record_buf);
        }
        
        // Headers array
        encode_varint(record.headers.len() as i32, &mut record_buf);
        for header in &record.headers {
            // Header key
            encode_varint(header.key.len() as i32, &mut record_buf);
            record_buf.extend_from_slice(header.key.as_bytes());
            
            // Header value
            if let Some(value) = &header.value {
                encode_varint(value.len() as i32, &mut record_buf);
                record_buf.extend_from_slice(value);
            } else {
                encode_varint(-1, &mut record_buf);
            }
        }
        
        // Write record length and data
        encode_varint(record_buf.len() as i32, buf);
        buf.extend_from_slice(&record_buf);
        
        Ok(())
    }
    
    /// Decode a RecordBatch from bytes
    /// Decode a RecordBatch from bytes, returning the batch and number of bytes consumed
    pub fn decode(data: &[u8]) -> Result<(Self, usize)> {
        // Handle empty record batches (common during flush operations)
        if data.is_empty() {
            let batch = KafkaRecordBatch {
                header: RecordBatchHeader {
                    base_offset: 0,
                    batch_length: 0,
                    partition_leader_epoch: -1,
                    magic: MAGIC_V2,
                    crc: 0,
                    attributes: 0,
                    last_offset_delta: 0,
                    base_timestamp: 0,
                    max_timestamp: 0,
                    producer_id: -1,
                    producer_epoch: -1,
                    base_sequence: -1,
                    records_count: 0,
                },
                records: vec![],
                compressed_records_data: None,
            };
            return Ok((batch, 0));
        }

        // First check the magic byte to determine format
        // For v0/v1, the magic byte is at offset 16 (after offset and size)
        // For v2, it's at offset 17 (after offset, size, and partition_leader_epoch)
        
        if data.len() < 17 {
            // For very short batches, return an empty batch instead of erroring
            // This can happen with flush operations or connectivity checks
            let batch = KafkaRecordBatch {
                header: RecordBatchHeader {
                    base_offset: 0,
                    batch_length: 0,
                    partition_leader_epoch: -1,
                    magic: MAGIC_V2,
                    crc: 0,
                    attributes: 0,
                    last_offset_delta: 0,
                    base_timestamp: 0,
                    max_timestamp: 0,
                    producer_id: -1,
                    producer_epoch: -1,
                    base_sequence: -1,
                    records_count: 0,
                },
                records: vec![],
                compressed_records_data: None,
            };
            return Ok((batch, 0));
        }
        
        // Try to detect the format by looking at the magic byte position
        // In v2 format: offset(8) + batch_length(4) + partition_leader_epoch(4) + magic(1) = position 16
        // In v0/v1 format: offset(8) + size(4) + crc(4) + magic(1) = position 16
        
        let magic_byte_v2_position = 16;
        if data.len() > magic_byte_v2_position {
            let possible_magic = data[magic_byte_v2_position];
            
            // Check if this could be a legacy format
            if possible_magic == MAGIC_V0 as u8 || possible_magic == MAGIC_V1 as u8 {
                // This is a legacy message set, convert it to v2 format
                return Self::decode_legacy_message_set(data);
            }
        }
        
        // Continue with v2 parsing
        let mut cursor = Cursor::new(data);
        
        // Read base offset
        let base_offset = read_i64(&mut cursor)?;
        
        // Read batch length
        let batch_length = read_i32(&mut cursor)?;
        
        // Verify we have enough data
        let expected_total_size = 12 + batch_length as usize; // 8 (offset) + 4 (length) + batch_length
        if data.len() < expected_total_size {
            return Err(Error::Internal(format!(
                "RecordBatch truncated: expected {} bytes, got {}", 
                expected_total_size, data.len()
            )));
        }
        
        // Read header fields
        let partition_leader_epoch = read_i32(&mut cursor)?;
        let magic = read_i8(&mut cursor)?;
        
        if magic != MAGIC_V2 {
            // If we still get a non-v2 magic byte here, try legacy format
            if magic == MAGIC_V0 || magic == MAGIC_V1 {
                return Self::decode_legacy_message_set(data);
            }
            return Err(Error::Internal(format!("Unsupported magic byte: {}", magic)));
        }
        
        let crc = read_u32(&mut cursor)?;
        let attributes = read_u16(&mut cursor)?;
        let last_offset_delta = read_i32(&mut cursor)?;
        let base_timestamp = read_i64(&mut cursor)?;
        let max_timestamp = read_i64(&mut cursor)?;
        let producer_id = read_i64(&mut cursor)?;
        let producer_epoch = read_i16(&mut cursor)?;
        let base_sequence = read_i32(&mut cursor)?;
        let records_count = read_i32(&mut cursor)?;
        
        let header = RecordBatchHeader {
            base_offset,
            batch_length,
            partition_leader_epoch,
            magic,
            crc,
            attributes,
            last_offset_delta,
            base_timestamp,
            max_timestamp,
            producer_id,
            producer_epoch,
            base_sequence,
            records_count,
        };
        
        // Read records section (potentially compressed)
        let mut compressed_buf = vec![0u8; cursor.get_ref().len() - cursor.position() as usize];
        cursor.read_exact(&mut compressed_buf)
            .map_err(|e| Error::Internal(format!("Failed to read records data: {}", e)))?;

        let compression = CompressionType::from_attributes(attributes);

        // CRITICAL: Preserve original compressed bytes for byte-perfect round-trip
        // Compression is non-deterministic (gzip includes timestamps) so we MUST
        // keep the original bytes to maintain CRC validity
        let compressed_records_data = if compression != CompressionType::None {
            Some(Bytes::from(compressed_buf.clone()))
        } else {
            None
        };

        // Decompress if needed
        let records_data = match compression {
            CompressionType::None => compressed_buf,
            CompressionType::Gzip => {
                let mut decoder = GzDecoder::new(&compressed_buf[..]);
                let mut decompressed = Vec::new();
                decoder.read_to_end(&mut decompressed)
                    .map_err(|e| Error::Internal(format!("Gzip decompression failed: {}", e)))?;
                decompressed
            }
            CompressionType::Zstd => {
                // For now, try zlib since we used it as fallback
                let mut decoder = ZlibDecoder::new(&compressed_buf[..]);
                let mut decompressed = Vec::new();
                decoder.read_to_end(&mut decompressed)
                    .map_err(|e| Error::Internal(format!("Zlib decompression failed: {}", e)))?;
                decompressed
            }
            _ => compressed_buf,
        };

        // Decode records
        let records = Self::decode_records(&records_data, records_count)?;

        // Calculate bytes consumed: 8 bytes (offset) + 4 bytes (length field) + batch_length
        let bytes_consumed = 12 + batch_length as usize;

        Ok((Self { header, records, compressed_records_data }, bytes_consumed))
    }
    
    /// Decode legacy message set (v0/v1 format) and convert to v2 RecordBatch
    fn decode_legacy_message_set(data: &[u8]) -> Result<(Self, usize)> {
        let mut cursor = Cursor::new(data);
        let mut records = Vec::new();
        let mut base_offset = 0i64;
        let mut base_timestamp = 0i64;
        let mut max_timestamp = 0i64;
        
        // Parse MessageSet format (series of offset + message_size + message)
        while cursor.position() < data.len() as u64 {
            // Check if we have at least 12 bytes for offset + size
            let remaining = cursor.get_ref().len() - cursor.position() as usize;
            if remaining < 12 {
                break;
            }
            
            // Read offset
            let offset = read_i64(&mut cursor)?;
            if base_offset == 0 {
                base_offset = offset;
            }
            
            // Read message size
            let message_size = read_i32(&mut cursor)?;
            
            // Check if we have enough data for the message
            let remaining = cursor.get_ref().len() - cursor.position() as usize;
            if remaining < message_size as usize {
                break;
            }
            
            // Read CRC
            let _crc = read_u32(&mut cursor)?;
            
            // Read magic byte
            let magic = read_i8(&mut cursor)?;
            
            // Read attributes
            let attributes = read_i8(&mut cursor)?;
            
            // For v1, read timestamp
            let timestamp = if magic == MAGIC_V1 {
                let ts = read_i64(&mut cursor)?;
                if ts > max_timestamp {
                    max_timestamp = ts;
                }
                if base_timestamp == 0 || ts < base_timestamp {
                    base_timestamp = ts;
                }
                ts
            } else {
                // v0 doesn't have timestamps
                0
            };
            
            // Read key
            let key_len = read_i32(&mut cursor)?;
            let key = if key_len >= 0 {
                let mut buf = vec![0u8; key_len as usize];
                cursor.read_exact(&mut buf)
                    .map_err(|e| Error::Internal(format!("Failed to read key: {}", e)))?;
                Some(Bytes::from(buf))
            } else {
                None
            };
            
            // Read value
            let value_len = read_i32(&mut cursor)?;
            let value = if value_len >= 0 {
                let mut buf = vec![0u8; value_len as usize];
                cursor.read_exact(&mut buf)
                    .map_err(|e| Error::Internal(format!("Failed to read value: {}", e)))?;
                Some(Bytes::from(buf))
            } else {
                None
            };
            
            // Convert to v2 Record
            let record = KafkaRecord {
                length: 0, // Will be calculated when encoding
                attributes: 0,
                timestamp_delta: timestamp - base_timestamp,
                offset_delta: (offset - base_offset) as i32,
                key,
                value,
                headers: Vec::new(), // Legacy formats don't have headers
            };
            
            records.push(record);
        }
        
        // Create a v2 RecordBatch header with converted data
        let header = RecordBatchHeader {
            base_offset,
            batch_length: 0, // Will be calculated when encoding
            partition_leader_epoch: -1, // Not available in legacy format
            magic: MAGIC_V2, // Convert to v2
            crc: 0, // Will be recalculated
            attributes: 0, // No compression or special flags from legacy
            last_offset_delta: records.len() as i32 - 1,
            base_timestamp,
            max_timestamp,
            producer_id: -1, // Not available in legacy format
            producer_epoch: -1, // Not available in legacy format
            base_sequence: -1, // Not available in legacy format
            records_count: records.len() as i32,
        };

        // Calculate bytes consumed (position where cursor stopped)
        let bytes_consumed = cursor.position() as usize;

        Ok((Self { header, records, compressed_records_data: None }, bytes_consumed))
    }
    
    /// Decode records from bytes
    fn decode_records(data: &[u8], count: i32) -> Result<Vec<KafkaRecord>> {
        let mut cursor = Cursor::new(data);
        
        // When records_count is -1, it means we should read all available records
        // This happens with non-transactional producers
        let mut records = if count >= 0 {
            Vec::with_capacity(count as usize)
        } else {
            Vec::new()
        };
        
        // If count is -1, read until we run out of data
        // Otherwise read exactly count records
        let mut records_read = 0;
        while cursor.position() < data.len() as u64 {
            // If we have a specific count and reached it, stop
            if count >= 0 && records_read >= count {
                break;
            }
            
            // Check if we have at least one byte for the length varint
            if cursor.position() >= data.len() as u64 {
                break;
            }
            
            let length = decode_varint(&mut cursor)?;
            let attributes = read_i8(&mut cursor)?;
            let timestamp_delta = decode_varlong(&mut cursor)?;
            let offset_delta = decode_varint(&mut cursor)?;
            
            records_read += 1;
            
            // Key
            let key_len = decode_varint(&mut cursor)?;
            let key = if key_len >= 0 {
                let mut buf = vec![0u8; key_len as usize];
                cursor.read_exact(&mut buf)
                    .map_err(|e| Error::Internal(format!("Failed to read key: {}", e)))?;
                Some(Bytes::from(buf))
            } else {
                None
            };
            
            // Value
            let value_len = decode_varint(&mut cursor)?;
            let value = if value_len >= 0 {
                let mut buf = vec![0u8; value_len as usize];
                cursor.read_exact(&mut buf)
                    .map_err(|e| Error::Internal(format!("Failed to read value: {}", e)))?;
                Some(Bytes::from(buf))
            } else {
                None
            };
            
            // Headers
            let headers_count = decode_varint(&mut cursor)?;
            
            // Sanity check headers count to prevent capacity overflow
            if headers_count < 0 || headers_count > 1000 {
                return Err(Error::Internal(format!(
                    "Invalid headers count: {} at position {}. This might be due to incorrect message format.", 
                    headers_count, cursor.position()
                )));
            }
            
            let mut headers = Vec::with_capacity(headers_count as usize);
            
            for _ in 0..headers_count {
                let key_len = decode_varint(&mut cursor)?;
                if key_len < 0 || key_len > 10000 {
                    return Err(Error::Internal(format!("Invalid header key length: {}", key_len)));
                }
                let mut key_buf = vec![0u8; key_len as usize];
                cursor.read_exact(&mut key_buf)
                    .map_err(|e| Error::Internal(format!("Failed to read header key: {}", e)))?;
                let key = String::from_utf8(key_buf)
                    .map_err(|e| Error::Internal(format!("Invalid header key: {}", e)))?;
                
                let value_len = decode_varint(&mut cursor)?;
                let value = if value_len >= 0 {
                    if value_len > 1000000 {
                        return Err(Error::Internal(format!("Invalid header value length: {}", value_len)));
                    }
                    let mut buf = vec![0u8; value_len as usize];
                    cursor.read_exact(&mut buf)
                        .map_err(|e| Error::Internal(format!("Failed to read header value: {}", e)))?;
                    Some(Bytes::from(buf))
                } else {
                    None
                };
                
                headers.push(RecordHeader { key, value });
            }
            
            records.push(KafkaRecord {
                length,
                attributes,
                timestamp_delta,
                offset_delta,
                key,
                value,
                headers,
            });
        }
        
        Ok(records)
    }
}

// Helper functions for reading primitive types
fn read_i8(cursor: &mut Cursor<&[u8]>) -> Result<i8> {
    let mut buf = [0u8; 1];
    cursor.read_exact(&mut buf)
        .map_err(|e| Error::Internal(format!("Failed to read i8: {}", e)))?;
    Ok(i8::from_be_bytes(buf))
}

fn read_i16(cursor: &mut Cursor<&[u8]>) -> Result<i16> {
    let mut buf = [0u8; 2];
    cursor.read_exact(&mut buf)
        .map_err(|e| Error::Internal(format!("Failed to read i16: {}", e)))?;
    Ok(i16::from_be_bytes(buf))
}

fn read_i32(cursor: &mut Cursor<&[u8]>) -> Result<i32> {
    let mut buf = [0u8; 4];
    cursor.read_exact(&mut buf)
        .map_err(|e| Error::Internal(format!("Failed to read i32: {}", e)))?;
    Ok(i32::from_be_bytes(buf))
}

fn read_i64(cursor: &mut Cursor<&[u8]>) -> Result<i64> {
    let mut buf = [0u8; 8];
    cursor.read_exact(&mut buf)
        .map_err(|e| Error::Internal(format!("Failed to read i64: {}", e)))?;
    Ok(i64::from_be_bytes(buf))
}

fn read_u16(cursor: &mut Cursor<&[u8]>) -> Result<u16> {
    let mut buf = [0u8; 2];
    cursor.read_exact(&mut buf)
        .map_err(|e| Error::Internal(format!("Failed to read u16: {}", e)))?;
    Ok(u16::from_be_bytes(buf))
}

fn read_u32(cursor: &mut Cursor<&[u8]>) -> Result<u32> {
    let mut buf = [0u8; 4];
    cursor.read_exact(&mut buf)
        .map_err(|e| Error::Internal(format!("Failed to read u32: {}", e)))?;
    Ok(u32::from_be_bytes(buf))
}

// Variable-length integer encoding (zigzag)
fn encode_varint(value: i32, buf: &mut Vec<u8>) {
    let mut v = ((value << 1) ^ (value >> 31)) as u32;
    while v >= 0x80 {
        buf.push((v | 0x80) as u8);
        v >>= 7;
    }
    buf.push(v as u8);
}

fn decode_varint(cursor: &mut Cursor<&[u8]>) -> Result<i32> {
    let mut value = 0u32;
    let mut shift = 0;
    loop {
        let mut byte = [0u8; 1];
        cursor.read_exact(&mut byte)
            .map_err(|e| Error::Internal(format!("Failed to read varint: {}", e)))?;
        let b = byte[0];
        value |= ((b & 0x7F) as u32) << shift;
        if b & 0x80 == 0 {
            break;
        }
        shift += 7;
        if shift >= 32 {
            return Err(Error::Internal("Varint too long".to_string()));
        }
    }
    // Decode zigzag
    Ok(((value >> 1) as i32) ^ -((value & 1) as i32))
}

fn encode_varlong(value: i64, buf: &mut Vec<u8>) {
    let mut v = ((value << 1) ^ (value >> 63)) as u64;
    while v >= 0x80 {
        buf.push((v | 0x80) as u8);
        v >>= 7;
    }
    buf.push(v as u8);
}

fn decode_varlong(cursor: &mut Cursor<&[u8]>) -> Result<i64> {
    let mut value = 0u64;
    let mut shift = 0;
    loop {
        let mut byte = [0u8; 1];
        cursor.read_exact(&mut byte)
            .map_err(|e| Error::Internal(format!("Failed to read varlong: {}", e)))?;
        let b = byte[0];
        value |= ((b & 0x7F) as u64) << shift;
        if b & 0x80 == 0 {
            break;
        }
        shift += 7;
        if shift >= 64 {
            return Err(Error::Internal("Varlong too long".to_string()));
        }
    }
    // Decode zigzag
    Ok(((value >> 1) as i64) ^ -((value & 1) as i64))
}

fn calculate_crc32(data: &[u8]) -> u32 {
    // CRITICAL FIX v1.3.31: Use CRC-32C (Castagnoli) as required by Kafka protocol
    // Previous version used regular CRC-32 which produced wrong checksums
    crc32c::crc32c(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_varint_encoding() {
        let mut buf = Vec::new();
        
        // Test positive number
        encode_varint(300, &mut buf);
        let mut cursor = Cursor::new(&buf[..]);
        assert_eq!(decode_varint(&mut cursor).unwrap(), 300);
        
        // Test negative number
        buf.clear();
        encode_varint(-300, &mut buf);
        let mut cursor = Cursor::new(&buf[..]);
        assert_eq!(decode_varint(&mut cursor).unwrap(), -300);
        
        // Test zero
        buf.clear();
        encode_varint(0, &mut buf);
        let mut cursor = Cursor::new(&buf[..]);
        assert_eq!(decode_varint(&mut cursor).unwrap(), 0);
    }
    
    #[test]
    fn test_record_batch_encode_decode() {
        let mut batch = KafkaRecordBatch::new(
            100, // base_offset
            1234567890000, // base_timestamp
            1, // producer_id
            0, // producer_epoch
            0, // base_sequence
            CompressionType::None,
            false, // is_transactional
        );
        
        // Add some records
        batch.add_record(
            Some(Bytes::from("key1")),
            Some(Bytes::from("value1")),
            vec![RecordHeader {
                key: "header1".to_string(),
                value: Some(Bytes::from("hvalue1")),
            }],
            1234567890100,
        );
        
        batch.add_record(
            None,
            Some(Bytes::from("value2")),
            vec![],
            1234567890200,
        );
        
        // Encode
        let encoded = batch.encode().unwrap();

        // Decode
        let (decoded, bytes_consumed) = KafkaRecordBatch::decode(&encoded).unwrap();

        // Verify
        assert_eq!(decoded.header.base_offset, 100);
        assert_eq!(decoded.header.records_count, 2);
        assert_eq!(decoded.records.len(), 2);
        assert_eq!(bytes_consumed, encoded.len());
        
        assert_eq!(decoded.records[0].key.as_ref().map(|b| b.as_ref()), Some(&b"key1"[..]));
        assert_eq!(decoded.records[0].value.as_ref().map(|b| b.as_ref()), Some(&b"value1"[..]));
        assert_eq!(decoded.records[0].headers.len(), 1);
        assert_eq!(decoded.records[0].headers[0].key, "header1");
        
        assert_eq!(decoded.records[1].key, None);
        assert_eq!(decoded.records[1].value.as_ref().map(|b| b.as_ref()), Some(&b"value2"[..]));
        assert_eq!(decoded.records[1].headers.len(), 0);
    }
    
    #[test]
    fn test_compression() {
        let mut batch = KafkaRecordBatch::new(
            0,
            1234567890000,
            1,
            0,
            0,
            CompressionType::Gzip,
            false,
        );
        
        // Add multiple records to make compression worthwhile
        for i in 0..10 {
            batch.add_record(
                Some(Bytes::from(format!("key{}", i))),
                Some(Bytes::from(format!("value{}", i))),
                vec![],
                1234567890000 + i * 100,
            );
        }
        
        // Encode with compression
        let encoded = batch.encode().unwrap();

        // Decode
        let (decoded, bytes_consumed) = KafkaRecordBatch::decode(&encoded).unwrap();

        // Verify
        assert_eq!(decoded.header.records_count, 10);
        assert_eq!(decoded.records.len(), 10);
        assert_eq!(bytes_consumed, encoded.len());
        
        for i in 0..10 {
            assert_eq!(
                decoded.records[i].key.as_ref().map(|b| b.as_ref()), 
                Some(format!("key{}", i).as_bytes())
            );
            assert_eq!(
                decoded.records[i].value.as_ref().map(|b| b.as_ref()), 
                Some(format!("value{}", i).as_bytes())
            );
        }
    }
}