//! Kafka record batch format handling.
//! 
//! Supports both legacy message format (v0-v1) and new record batch format (v2+).

use bytes::{Buf, BufMut, Bytes, BytesMut};
use crc32fast::Hasher;
use std::time::{SystemTime, UNIX_EPOCH};

use chronik_common::{Result, Error};
use crate::compression::{CompressionType, CompressionHandler};

/// Magic byte for record batch format v2
const MAGIC_V2: i8 = 2;

/// Record attributes
#[derive(Debug, Clone, Copy)]
pub struct RecordAttributes {
    pub compression: CompressionType,
    pub timestamp_type: TimestampType,
    pub is_transactional: bool,
    pub is_control_batch: bool,
}

impl RecordAttributes {
    /// Create from attributes byte
    pub fn from_byte(attrs: i8) -> Self {
        Self {
            compression: CompressionType::from_id(attrs & 0x07).unwrap_or(CompressionType::None),
            timestamp_type: if (attrs & 0x08) != 0 {
                TimestampType::LogAppendTime
            } else {
                TimestampType::CreateTime
            },
            is_transactional: (attrs & 0x10) != 0,
            is_control_batch: (attrs & 0x20) != 0,
        }
    }

    /// Convert to attributes byte
    pub fn to_byte(&self) -> i8 {
        let mut attrs = self.compression.id();
        if matches!(self.timestamp_type, TimestampType::LogAppendTime) {
            attrs |= 0x08;
        }
        if self.is_transactional {
            attrs |= 0x10;
        }
        if self.is_control_batch {
            attrs |= 0x20;
        }
        attrs
    }
}

/// Timestamp type for records
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimestampType {
    CreateTime,
    LogAppendTime,
}

/// A single Kafka record
#[derive(Debug, Clone)]
pub struct Record {
    pub offset: i64,
    pub timestamp: i64,
    pub key: Option<Bytes>,
    pub value: Option<Bytes>,
    pub headers: Vec<RecordHeader>,
}

/// Record header (key-value pair)
#[derive(Debug, Clone)]
pub struct RecordHeader {
    pub key: String,
    pub value: Option<Bytes>,
}

/// Record batch (Kafka format v2)
#[derive(Debug)]
pub struct RecordBatch {
    pub base_offset: i64,
    pub batch_length: i32,
    pub partition_leader_epoch: i32,
    pub magic: i8,
    pub crc: u32,
    pub attributes: RecordAttributes,
    pub last_offset_delta: i32,
    pub first_timestamp: i64,
    pub max_timestamp: i64,
    pub producer_id: i64,
    pub producer_epoch: i16,
    pub base_sequence: i32,
    pub records: Vec<Record>,
}

impl RecordBatch {
    /// Create a new record batch
    pub fn new(base_offset: i64, producer_id: i64, producer_epoch: i16) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;

        Self {
            base_offset,
            batch_length: 0,
            partition_leader_epoch: -1,
            magic: MAGIC_V2,
            crc: 0,
            attributes: RecordAttributes {
                compression: CompressionType::None,
                timestamp_type: TimestampType::CreateTime,
                is_transactional: false,
                is_control_batch: false,
            },
            last_offset_delta: 0,
            first_timestamp: now,
            max_timestamp: now,
            producer_id,
            producer_epoch,
            base_sequence: 0,
            records: Vec::new(),
        }
    }

    /// Add a record to the batch
    pub fn add_record(&mut self, key: Option<Bytes>, value: Option<Bytes>, headers: Vec<RecordHeader>) {
        let offset_delta = self.records.len() as i32;
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;

        // Update timestamps
        if self.records.is_empty() {
            self.first_timestamp = timestamp;
        }
        self.max_timestamp = timestamp;
        self.last_offset_delta = offset_delta;

        self.records.push(Record {
            offset: self.base_offset + offset_delta as i64,
            timestamp,
            key,
            value,
            headers,
        });
    }

    /// Encode the record batch to bytes
    pub fn encode(&self) -> Result<Bytes> {
        let mut buf = BytesMut::new();
        
        // Skip batch length field for now (will write later)
        buf.put_i32(0);
        
        // Write batch header
        buf.put_i64(self.base_offset);
        buf.put_i32(self.batch_length);
        buf.put_i32(self.partition_leader_epoch);
        buf.put_i8(self.magic);
        buf.put_u32(0); // CRC placeholder
        buf.put_i16(self.attributes.to_byte() as i16);
        buf.put_i32(self.last_offset_delta);
        buf.put_i64(self.first_timestamp);
        buf.put_i64(self.max_timestamp);
        buf.put_i64(self.producer_id);
        buf.put_i16(self.producer_epoch);
        buf.put_i32(self.base_sequence);
        buf.put_i32(self.records.len() as i32);

        // Encode records
        let mut records_buf = BytesMut::new();
        for (i, record) in self.records.iter().enumerate() {
            Self::encode_record(&mut records_buf, record, i as i32, self.first_timestamp)?;
        }

        // Apply compression if needed
        let records_data = if self.attributes.compression.is_compressed() {
            CompressionHandler::compress(&records_buf, self.attributes.compression)?
        } else {
            records_buf.freeze()
        };

        buf.extend_from_slice(&records_data);

        // Calculate batch length (excluding first 12 bytes: 4 for padding + 8 for offset)
        let batch_length = (buf.len() - 12) as i32;

        // Write batch length at position 12-16 (after padding and base_offset)
        buf[12..16].copy_from_slice(&batch_length.to_be_bytes());

        // Calculate CRC32C checksum per Kafka protocol spec
        // CRC covers: attributes + last_offset_delta + timestamps + producer info + records
        // CRC field itself (4 bytes after magic) must NOT be included in calculation
        //
        // Buffer layout: [4 padding][8 base_offset][4 batch_length][4 partition_leader_epoch]
        //                [1 magic][4 CRC][2 attributes]...
        // Position 20 = magic byte
        // Position 21-24 = CRC field (to be filled)
        // Position 25 = attributes (start of CRC calculation)
        let crc_start = 25; // Start AFTER CRC field
        let crc_data = &buf[crc_start..];
        let mut hasher = Hasher::new();
        hasher.update(crc_data);
        let crc = hasher.finalize();

        tracing::debug!(
            "RecordBatch CRC: crc={}, data_len={}, producer_id={}, records={}, is_txn={}",
            crc, crc_data.len(), self.producer_id, self.records.len(), self.attributes.is_transactional
        );

        // Write CRC at position 21-24 (4 bytes after magic byte)
        buf[21..25].copy_from_slice(&crc.to_be_bytes());

        Ok(buf.freeze())
    }

    /// Encode a single record
    fn encode_record(buf: &mut BytesMut, record: &Record, offset_delta: i32, base_timestamp: i64) -> Result<()> {
        let start_pos = buf.len();
        
        // Skip length field for now
        buf.put_i8(0); // Length placeholder (varint)
        
        // Attributes (unused in v2)
        buf.put_i8(0);
        
        // Timestamp delta
        let timestamp_delta = record.timestamp - base_timestamp;
        Self::write_varint(buf, timestamp_delta);
        
        // Offset delta
        Self::write_varint(buf, offset_delta as i64);
        
        // Key
        if let Some(ref key) = record.key {
            Self::write_varint(buf, key.len() as i64);
            buf.put_slice(key);
        } else {
            Self::write_varint(buf, -1);
        }
        
        // Value
        if let Some(ref value) = record.value {
            Self::write_varint(buf, value.len() as i64);
            buf.put_slice(value);
        } else {
            Self::write_varint(buf, -1);
        }
        
        // Headers
        Self::write_varint(buf, record.headers.len() as i64);
        for header in &record.headers {
            Self::write_varint(buf, header.key.len() as i64);
            buf.put_slice(header.key.as_bytes());
            
            if let Some(ref value) = header.value {
                Self::write_varint(buf, value.len() as i64);
                buf.put_slice(value);
            } else {
                Self::write_varint(buf, -1);
            }
        }
        
        // Calculate and write record length
        let record_length = buf.len() - start_pos - 1;
        buf[start_pos] = record_length as u8; // Simplified for small records
        
        Ok(())
    }

    /// Write a varint to the buffer
    fn write_varint(buf: &mut BytesMut, value: i64) -> usize {
        let mut count = 0;
        
        // ZigZag encode for signed values
        let encoded = if value < 0 {
            ((!value) << 1) | 1
        } else {
            value << 1
        } as u64;
        
        let mut val = encoded;
        while (val & !0x7F) != 0 {
            buf.put_u8((val & 0x7F) as u8 | 0x80);
            val >>= 7;
            count += 1;
        }
        buf.put_u8(val as u8);
        count + 1
    }

    /// Decode a record batch from bytes
    pub fn decode(mut data: Bytes) -> Result<Self> {
        if data.len() < 61 {  // Minimum batch header size
            return Err(Error::Protocol("Record batch too small".into()));
        }

        let base_offset = data.get_i64();
        let batch_length = data.get_i32();
        let partition_leader_epoch = data.get_i32();
        let magic = data.get_i8();
        
        if magic != MAGIC_V2 {
            return Err(Error::Protocol(format!("Unsupported magic byte: {}", magic)));
        }

        let stored_crc = data.get_u32();

        // Save position to calculate CRC over remaining data
        let crc_verify_start = data.remaining();

        let attributes = RecordAttributes::from_byte(data.get_i16() as i8);
        let last_offset_delta = data.get_i32();
        let first_timestamp = data.get_i64();
        let max_timestamp = data.get_i64();
        let producer_id = data.get_i64();
        let producer_epoch = data.get_i16();
        let base_sequence = data.get_i32();
        let record_count = data.get_i32();

        // Clone data for CRC verification before decompression
        let data_for_crc = data.clone();

        // Decompress records if needed
        let records_data = if attributes.compression.is_compressed() {
            CompressionHandler::decompress(&data, attributes.compression)?
        } else {
            data
        };

        // Verify CRC after parsing header but before processing records
        // CRC covers: attributes + last_offset_delta + timestamps + producer info + records
        let crc_data = &data_for_crc[data_for_crc.len() - crc_verify_start..];
        let calculated_crc = crc32fast::hash(crc_data);

        if calculated_crc != stored_crc {
            tracing::warn!(
                "CRC mismatch: stored={}, calculated={}, producer_id={}, is_txn={}",
                stored_crc, calculated_crc, producer_id, attributes.is_transactional
            );
            return Err(Error::Protocol(format!(
                "Record is corrupt (stored crc = {}, computed crc = {})",
                stored_crc, calculated_crc
            )));
        }

        tracing::debug!(
            "CRC verified: crc={}, producer_id={}, records={}, is_txn={}",
            calculated_crc, producer_id, record_count, attributes.is_transactional
        );

        // Decode records
        let mut records = Vec::with_capacity(record_count as usize);
        let mut records_buf = records_data;
        
        for i in 0..record_count {
            let record = Self::decode_record(&mut records_buf, base_offset, i, first_timestamp)?;
            records.push(record);
        }

        Ok(RecordBatch {
            base_offset,
            batch_length,
            partition_leader_epoch,
            magic,
            crc: stored_crc,
            attributes,
            last_offset_delta,
            first_timestamp,
            max_timestamp,
            producer_id,
            producer_epoch,
            base_sequence,
            records,
        })
    }

    /// Decode a single record
    fn decode_record(data: &mut Bytes, base_offset: i64, offset_delta: i32, base_timestamp: i64) -> Result<Record> {
        // Length (varint)
        let _length = Self::read_varint(data)?;
        
        // Attributes (unused)
        let _attrs = data.get_u8();
        
        // Timestamp delta
        let timestamp_delta = Self::read_varint(data)?;
        let timestamp = base_timestamp + timestamp_delta;
        
        // Offset delta (should match expected)
        let _offset_delta = Self::read_varint(data)?;
        
        // Key
        let key_len = Self::read_varint(data)?;
        let key = if key_len >= 0 {
            Some(data.copy_to_bytes(key_len as usize))
        } else {
            None
        };
        
        // Value
        let value_len = Self::read_varint(data)?;
        let value = if value_len >= 0 {
            Some(data.copy_to_bytes(value_len as usize))
        } else {
            None
        };
        
        // Headers
        let header_count = Self::read_varint(data)?;
        let mut headers = Vec::with_capacity(header_count as usize);
        
        for _ in 0..header_count {
            let key_len = Self::read_varint(data)?;
            let key_bytes = data.copy_to_bytes(key_len as usize);
            let key = String::from_utf8(key_bytes.to_vec())
                .map_err(|e| Error::Protocol(format!("Invalid header key: {}", e)))?;
            
            let value_len = Self::read_varint(data)?;
            let value = if value_len >= 0 {
                Some(data.copy_to_bytes(value_len as usize))
            } else {
                None
            };
            
            headers.push(RecordHeader { key, value });
        }
        
        Ok(Record {
            offset: base_offset + offset_delta as i64,
            timestamp,
            key,
            value,
            headers,
        })
    }

    /// Read a varint from the buffer
    fn read_varint(data: &mut Bytes) -> Result<i64> {
        let mut result = 0u64;
        let mut shift = 0;
        
        loop {
            if !data.has_remaining() {
                return Err(Error::Protocol("Incomplete varint".into()));
            }
            
            let byte = data.get_u8();
            result |= ((byte & 0x7F) as u64) << shift;
            
            if byte & 0x80 == 0 {
                break;
            }
            
            shift += 7;
            if shift >= 64 {
                return Err(Error::Protocol("Varint too long".into()));
            }
        }
        
        // ZigZag decode
        let decoded = if result & 1 == 0 {
            (result >> 1) as i64
        } else {
            -((result >> 1) as i64) - 1
        };
        
        Ok(decoded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_record_attributes() {
        let attrs = RecordAttributes {
            compression: CompressionType::Gzip,
            timestamp_type: TimestampType::LogAppendTime,
            is_transactional: true,
            is_control_batch: false,
        };
        
        let byte = attrs.to_byte();
        assert_eq!(byte, 0x19); // 0x01 (gzip) | 0x08 (log append) | 0x10 (transactional)
        
        let decoded = RecordAttributes::from_byte(byte);
        assert_eq!(decoded.compression.id(), CompressionType::Gzip.id());
        assert_eq!(decoded.timestamp_type, TimestampType::LogAppendTime);
        assert!(decoded.is_transactional);
        assert!(!decoded.is_control_batch);
    }

    #[test]
    fn test_record_batch() {
        let mut batch = RecordBatch::new(100, 12345, 1);
        
        // Add some records
        batch.add_record(
            Some(Bytes::from("key1")),
            Some(Bytes::from("value1")),
            vec![RecordHeader {
                key: "header1".to_string(),
                value: Some(Bytes::from("hvalue1")),
            }],
        );
        
        batch.add_record(
            Some(Bytes::from("key2")),
            Some(Bytes::from("value2")),
            vec![],
        );
        
        assert_eq!(batch.records.len(), 2);
        assert_eq!(batch.last_offset_delta, 1);
        
        // Encode and check it doesn't panic
        let encoded = batch.encode().unwrap();
        assert!(!encoded.is_empty());
    }

    #[test]
    fn test_varint_encoding() {
        let mut buf = BytesMut::new();
        
        // Test various values
        RecordBatch::write_varint(&mut buf, 0);
        RecordBatch::write_varint(&mut buf, 127);
        RecordBatch::write_varint(&mut buf, 128);
        RecordBatch::write_varint(&mut buf, -1);
        RecordBatch::write_varint(&mut buf, -128);
        
        let mut data = buf.freeze();
        
        assert_eq!(RecordBatch::read_varint(&mut data).unwrap(), 0);
        assert_eq!(RecordBatch::read_varint(&mut data).unwrap(), 127);
        assert_eq!(RecordBatch::read_varint(&mut data).unwrap(), 128);
        assert_eq!(RecordBatch::read_varint(&mut data).unwrap(), -1);
        assert_eq!(RecordBatch::read_varint(&mut data).unwrap(), -128);
    }
}