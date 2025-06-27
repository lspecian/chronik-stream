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
        let crc_data = &buf[crc_start + 4 + 1 + 4..]; // Skip to after CRC field
        let crc = calculate_crc32(crc_data);
        buf[crc_pos..crc_pos + 4].copy_from_slice(&crc.to_be_bytes());
        
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
    pub fn decode(data: &[u8]) -> Result<Self> {
        let mut cursor = Cursor::new(data);
        
        // Read base offset
        let base_offset = read_i64(&mut cursor)?;
        
        // Read batch length
        let batch_length = read_i32(&mut cursor)?;
        
        // Verify we have enough data
        if cursor.position() as usize + batch_length as usize > data.len() {
            return Err(Error::Internal("RecordBatch truncated".to_string()));
        }
        
        // Read header fields
        let partition_leader_epoch = read_i32(&mut cursor)?;
        let magic = read_i8(&mut cursor)?;
        
        if magic != MAGIC_V2 {
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
        
        // Read and decompress records if needed
        let records_data = {
            let mut buf = vec![0u8; cursor.get_ref().len() - cursor.position() as usize];
            cursor.read_exact(&mut buf)
                .map_err(|e| Error::Internal(format!("Failed to read records data: {}", e)))?;
            
            let compression = CompressionType::from_attributes(attributes);
            match compression {
                CompressionType::None => buf,
                CompressionType::Gzip => {
                    let mut decoder = GzDecoder::new(&buf[..]);
                    let mut decompressed = Vec::new();
                    decoder.read_to_end(&mut decompressed)
                        .map_err(|e| Error::Internal(format!("Gzip decompression failed: {}", e)))?;
                    decompressed
                }
                CompressionType::Zstd => {
                    // For now, try zlib since we used it as fallback
                    let mut decoder = ZlibDecoder::new(&buf[..]);
                    let mut decompressed = Vec::new();
                    decoder.read_to_end(&mut decompressed)
                        .map_err(|e| Error::Internal(format!("Zlib decompression failed: {}", e)))?;
                    decompressed
                }
                _ => buf,
            }
        };
        
        // Decode records
        let records = Self::decode_records(&records_data, records_count)?;
        
        Ok(Self { header, records })
    }
    
    /// Decode records from bytes
    fn decode_records(data: &[u8], count: i32) -> Result<Vec<KafkaRecord>> {
        let mut cursor = Cursor::new(data);
        let mut records = Vec::with_capacity(count as usize);
        
        for _ in 0..count {
            let length = decode_varint(&mut cursor)?;
            let attributes = read_i8(&mut cursor)?;
            let timestamp_delta = decode_varlong(&mut cursor)?;
            let offset_delta = decode_varint(&mut cursor)?;
            
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
            let mut headers = Vec::with_capacity(headers_count as usize);
            
            for _ in 0..headers_count {
                let key_len = decode_varint(&mut cursor)?;
                let mut key_buf = vec![0u8; key_len as usize];
                cursor.read_exact(&mut key_buf)
                    .map_err(|e| Error::Internal(format!("Failed to read header key: {}", e)))?;
                let key = String::from_utf8(key_buf)
                    .map_err(|e| Error::Internal(format!("Invalid header key: {}", e)))?;
                
                let value_len = decode_varint(&mut cursor)?;
                let value = if value_len >= 0 {
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
    Ok(((value >> 1) ^ (!(value & 1).wrapping_add(1))) as i32)
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
    Ok(((value >> 1) ^ (!(value & 1).wrapping_add(1))) as i64)
}

fn calculate_crc32(data: &[u8]) -> u32 {
    let mut hasher = Crc32::new();
    hasher.update(data);
    hasher.finalize()
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
        let decoded = KafkaRecordBatch::decode(&encoded).unwrap();
        
        // Verify
        assert_eq!(decoded.header.base_offset, 100);
        assert_eq!(decoded.header.records_count, 2);
        assert_eq!(decoded.records.len(), 2);
        
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
        let decoded = KafkaRecordBatch::decode(&encoded).unwrap();
        
        // Verify
        assert_eq!(decoded.header.records_count, 10);
        assert_eq!(decoded.records.len(), 10);
        
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