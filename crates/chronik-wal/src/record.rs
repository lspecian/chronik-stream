//! WAL record format with CRC32 checksums
//!
//! Supports two formats:
//! - V1: Individual records (backward compatibility)
//! - V2: CanonicalRecord batches (new layered storage format)

use bytes::{BufMut, BytesMut};
use crate::buffer_pool::PooledBuffer;
use crc32fast::Hasher;
use serde::{Deserialize, Serialize};
use std::io::Cursor;
use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};

use crate::error::{Result, WalError};

/// Magic number for WAL records
const WAL_MAGIC: u16 = 0xCA7E;

/// WAL format versions
const WAL_VERSION_V1: u8 = 1;
const WAL_VERSION_V2: u8 = 2;

/// WAL record with versioning and checksums
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WalRecord {
    /// V1 format: Single record (backward compatibility)
    V1 {
        magic: u16,
        version: u8,
        flags: u8,
        length: u32,
        offset: i64,
        timestamp: i64,
        crc32: u32,
        key: Option<Vec<u8>>,
        value: Vec<u8>,
        headers: Vec<(String, Vec<u8>)>,
    },

    /// V2 format: CanonicalRecord batch (new format for layered storage)
    V2 {
        magic: u16,
        version: u8,
        flags: u8,
        length: u32,
        crc32: u32,
        topic: String,
        partition: i32,
        canonical_data: Vec<u8>,  // Bincode-serialized CanonicalRecord
    },
}

impl WalRecord {
    /// Create a new V1 WAL record (backward compatibility)
    pub fn new(
        offset: i64,
        key: Option<Vec<u8>>,
        value: Vec<u8>,
        timestamp: i64,
    ) -> Self {
        let mut record = WalRecord::V1 {
            magic: WAL_MAGIC,
            version: WAL_VERSION_V1,
            flags: 0,
            length: 0,
            offset,
            timestamp,
            crc32: 0,
            key,
            value,
            headers: Vec::new(),
        };

        record.update_length_and_checksum();
        record
    }

    /// Create a new V2 WAL record from serialized CanonicalRecord
    pub fn new_v2(
        topic: String,
        partition: i32,
        canonical_data: Vec<u8>,
    ) -> Self {
        let mut record = WalRecord::V2 {
            magic: WAL_MAGIC,
            version: WAL_VERSION_V2,
            flags: 0,
            length: 0,
            crc32: 0,
            topic,
            partition,
            canonical_data,
        };

        record.update_length_and_checksum();
        record
    }

    /// Update length and checksum fields
    fn update_length_and_checksum(&mut self) {
        let length = self.calculate_length();
        let crc32 = self.calculate_checksum();

        match self {
            WalRecord::V1 { length: l, crc32: c, .. } => {
                *l = length;
                *c = crc32;
            }
            WalRecord::V2 { length: l, crc32: c, .. } => {
                *l = length;
                *c = crc32;
            }
        }
    }

    /// Serialize record to bytes
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        match self {
            WalRecord::V1 { magic, version, flags, length, offset, timestamp, crc32, key, value, headers } => {
                let mut buf = Vec::with_capacity(*length as usize);

                // Write header
                buf.write_u16::<LittleEndian>(*magic)?;
                buf.write_u8(*version)?;
                buf.write_u8(*flags)?;
                buf.write_u32::<LittleEndian>(*length)?;
                buf.write_i64::<LittleEndian>(*offset)?;
                buf.write_i64::<LittleEndian>(*timestamp)?;
                buf.write_u32::<LittleEndian>(*crc32)?;

                // Write key
                match key {
                    Some(k) => {
                        buf.write_i32::<LittleEndian>(k.len() as i32)?;
                        buf.extend_from_slice(k);
                    }
                    None => {
                        buf.write_i32::<LittleEndian>(-1)?;
                    }
                }

                // Write value
                buf.write_i32::<LittleEndian>(value.len() as i32)?;
                buf.extend_from_slice(value);

                // Write headers
                buf.write_i32::<LittleEndian>(headers.len() as i32)?;
                for (name, value) in headers {
                    buf.write_u16::<LittleEndian>(name.len() as u16)?;
                    buf.extend_from_slice(name.as_bytes());
                    buf.write_u32::<LittleEndian>(value.len() as u32)?;
                    buf.extend_from_slice(value);
                }

                Ok(buf)
            }

            WalRecord::V2 { magic, version, flags, length, crc32, topic, partition, canonical_data } => {
                let mut buf = Vec::with_capacity(*length as usize);

                // Write header
                buf.write_u16::<LittleEndian>(*magic)?;
                buf.write_u8(*version)?;
                buf.write_u8(*flags)?;
                buf.write_u32::<LittleEndian>(*length)?;
                buf.write_u32::<LittleEndian>(*crc32)?;

                // Write topic
                buf.write_u16::<LittleEndian>(topic.len() as u16)?;
                buf.extend_from_slice(topic.as_bytes());

                // Write partition
                buf.write_i32::<LittleEndian>(*partition)?;

                // Write canonical data
                buf.write_u32::<LittleEndian>(canonical_data.len() as u32)?;
                buf.extend_from_slice(canonical_data);

                Ok(buf)
            }
        }
    }

    /// Deserialize record from bytes
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        let mut cursor = Cursor::new(data);

        // Read magic
        let magic = cursor.read_u16::<LittleEndian>()?;
        if magic != WAL_MAGIC {
            return Err(WalError::InvalidFormat(format!(
                "Invalid magic number: 0x{:04X}",
                magic
            )));
        }

        // Read version to determine format
        let version = cursor.read_u8()?;

        match version {
            WAL_VERSION_V1 => {
                let flags = cursor.read_u8()?;
                let length = cursor.read_u32::<LittleEndian>()?;
                let offset = cursor.read_i64::<LittleEndian>()?;
                let timestamp = cursor.read_i64::<LittleEndian>()?;
                let crc32 = cursor.read_u32::<LittleEndian>()?;

                // Read key
                let key_len = cursor.read_i32::<LittleEndian>()?;
                let key = if key_len >= 0 {
                    let mut buf = vec![0u8; key_len as usize];
                    std::io::Read::read_exact(&mut cursor, &mut buf)?;
                    Some(buf)
                } else {
                    None
                };

                // Read value
                let value_len = cursor.read_i32::<LittleEndian>()?;
                let mut value = vec![0u8; value_len as usize];
                std::io::Read::read_exact(&mut cursor, &mut value)?;

                // Read headers
                let headers_count = cursor.read_i32::<LittleEndian>()?;
                let mut headers = Vec::with_capacity(headers_count as usize);
                for _ in 0..headers_count {
                    let name_len = cursor.read_u16::<LittleEndian>()?;
                    let mut name_buf = vec![0u8; name_len as usize];
                    std::io::Read::read_exact(&mut cursor, &mut name_buf)?;
                    let name = String::from_utf8(name_buf)
                        .map_err(|e| WalError::InvalidFormat(e.to_string()))?;

                    let value_len = cursor.read_u32::<LittleEndian>()?;
                    let mut value_buf = vec![0u8; value_len as usize];
                    std::io::Read::read_exact(&mut cursor, &mut value_buf)?;

                    headers.push((name, value_buf));
                }

                let record = WalRecord::V1 {
                    magic,
                    version,
                    flags,
                    length,
                    offset,
                    timestamp,
                    crc32,
                    key,
                    value,
                    headers,
                };

                // Verify checksum
                record.verify_checksum()?;
                Ok(record)
            }

            WAL_VERSION_V2 => {
                let flags = cursor.read_u8()?;
                let length = cursor.read_u32::<LittleEndian>()?;
                let crc32 = cursor.read_u32::<LittleEndian>()?;

                // Read topic
                let topic_len = cursor.read_u16::<LittleEndian>()?;
                let mut topic_buf = vec![0u8; topic_len as usize];
                std::io::Read::read_exact(&mut cursor, &mut topic_buf)?;
                let topic = String::from_utf8(topic_buf)
                    .map_err(|e| WalError::InvalidFormat(e.to_string()))?;

                // Read partition
                let partition = cursor.read_i32::<LittleEndian>()?;

                // Read canonical data
                let data_len = cursor.read_u32::<LittleEndian>()?;
                let mut canonical_data = vec![0u8; data_len as usize];
                std::io::Read::read_exact(&mut cursor, &mut canonical_data)?;

                let record = WalRecord::V2 {
                    magic,
                    version,
                    flags,
                    length,
                    crc32,
                    topic,
                    partition,
                    canonical_data,
                };

                // Verify checksum
                record.verify_checksum()?;
                Ok(record)
            }

            _ => Err(WalError::InvalidFormat(format!(
                "Unsupported version: {}",
                version
            ))),
        }
    }

    /// Write record to a pooled buffer (for efficient memory reuse)
    pub fn write_to_pooled(&self) -> Result<PooledBuffer> {
        let length = self.get_length();
        let mut buf = PooledBuffer::acquire(length as usize);

        match self {
            WalRecord::V1 { magic, version, flags, length, offset, timestamp, crc32, key, value, headers } => {
                buf.put_u16(*magic);
                buf.put_u8(*version);
                buf.put_u8(*flags);
                buf.put_u32(*length);
                buf.put_i64(*offset);
                buf.put_i64(*timestamp);
                buf.put_u32(*crc32);

                // Write key
                match key {
                    Some(k) => {
                        buf.put_i32(k.len() as i32);
                        buf.put_slice(k);
                    }
                    None => {
                        buf.put_i32(-1);
                    }
                }

                // Write value
                buf.put_i32(value.len() as i32);
                buf.put_slice(value);

                // Write headers
                buf.put_i32(headers.len() as i32);
                for (name, value) in headers {
                    buf.put_u16(name.len() as u16);
                    buf.put_slice(name.as_bytes());
                    buf.put_u32(value.len() as u32);
                    buf.put_slice(value);
                }
            }

            WalRecord::V2 { magic, version, flags, length, crc32, topic, partition, canonical_data } => {
                buf.put_u16(*magic);
                buf.put_u8(*version);
                buf.put_u8(*flags);
                buf.put_u32(*length);
                buf.put_u32(*crc32);
                buf.put_u16(topic.len() as u16);
                buf.put_slice(topic.as_bytes());
                buf.put_i32(*partition);
                buf.put_u32(canonical_data.len() as u32);
                buf.put_slice(canonical_data);
            }
        }

        Ok(buf)
    }

    /// Write record to a buffer
    pub fn write_to(&self, buf: &mut BytesMut) -> Result<()> {
        let bytes = self.to_bytes()?;
        buf.put_slice(&bytes);
        Ok(())
    }

    /// Verify record checksum
    pub fn verify_checksum(&self) -> Result<()> {
        let computed = self.calculate_checksum();
        let stored = self.get_crc32();

        if computed != stored {
            let offset = self.get_offset().unwrap_or(-1);
            return Err(WalError::ChecksumMismatch {
                offset,
                expected: stored,
                actual: computed,
            });
        }
        Ok(())
    }

    /// Calculate record length
    pub fn calculate_length(&self) -> u32 {
        match self {
            WalRecord::V1 { key, value, headers, .. } => {
                let mut len = 28; // Fixed header: magic(2) + version(1) + flags(1) + length(4) + offset(8) + timestamp(8) + crc32(4)
                len += 4; // Key length field
                if let Some(ref k) = key {
                    len += k.len() as u32;
                }
                len += 4 + value.len() as u32; // Value length + data
                len += 4; // Headers count
                for (name, value) in headers {
                    len += 2 + name.len() as u32 + 4 + value.len() as u32;
                }
                len
            }

            WalRecord::V2 { topic, canonical_data, .. } => {
                let mut len = 15; // Fixed header: magic(2) + version(1) + flags(1) + length(4) + crc32(4) + topic_len(2) + partition(4) + data_len(4) = 19
                len += topic.len() as u32;
                len += 4; // partition
                len += 4; // canonical_data length field
                len += canonical_data.len() as u32;
                len
            }
        }
    }

    /// Calculate CRC32 checksum
    pub fn calculate_checksum(&self) -> u32 {
        let mut hasher = Hasher::new();

        match self {
            WalRecord::V1 { magic, version, flags, length, offset, timestamp, key, value, headers, .. } => {
                // Hash all fields except the CRC itself
                hasher.update(&magic.to_le_bytes());
                hasher.update(&[*version, *flags]);
                hasher.update(&length.to_le_bytes());
                hasher.update(&offset.to_le_bytes());
                hasher.update(&timestamp.to_le_bytes());

                // Hash key
                match key {
                    Some(k) => {
                        hasher.update(&(k.len() as i32).to_le_bytes());
                        hasher.update(k);
                    }
                    None => {
                        hasher.update(&(-1i32).to_le_bytes());
                    }
                }

                // Hash value
                hasher.update(&(value.len() as i32).to_le_bytes());
                hasher.update(value);

                // Hash headers
                hasher.update(&(headers.len() as i32).to_le_bytes());
                for (name, value) in headers {
                    hasher.update(&(name.len() as u16).to_le_bytes());
                    hasher.update(name.as_bytes());
                    hasher.update(&(value.len() as u32).to_le_bytes());
                    hasher.update(value);
                }
            }

            WalRecord::V2 { magic, version, flags, length, topic, partition, canonical_data, .. } => {
                hasher.update(&magic.to_le_bytes());
                hasher.update(&[*version, *flags]);
                hasher.update(&length.to_le_bytes());
                hasher.update(&(topic.len() as u16).to_le_bytes());
                hasher.update(topic.as_bytes());
                hasher.update(&partition.to_le_bytes());
                hasher.update(&(canonical_data.len() as u32).to_le_bytes());
                hasher.update(canonical_data);
            }
        }

        hasher.finalize()
    }

    /// Get record length
    pub fn get_length(&self) -> u32 {
        match self {
            WalRecord::V1 { length, .. } => *length,
            WalRecord::V2 { length, .. } => *length,
        }
    }

    /// Get CRC32
    pub fn get_crc32(&self) -> u32 {
        match self {
            WalRecord::V1 { crc32, .. } => *crc32,
            WalRecord::V2 { crc32, .. } => *crc32,
        }
    }

    /// Get offset (only available for V1)
    pub fn get_offset(&self) -> Option<i64> {
        match self {
            WalRecord::V1 { offset, .. } => Some(*offset),
            WalRecord::V2 { .. } => None,
        }
    }

    /// Get topic-partition (only available for V2)
    pub fn get_topic_partition(&self) -> Option<(&str, i32)> {
        match self {
            WalRecord::V1 { .. } => None,
            WalRecord::V2 { topic, partition, .. } => Some((topic.as_str(), *partition)),
        }
    }

    /// Get canonical data (only available for V2)
    pub fn get_canonical_data(&self) -> Option<&[u8]> {
        match self {
            WalRecord::V1 { .. } => None,
            WalRecord::V2 { canonical_data, .. } => Some(canonical_data.as_slice()),
        }
    }

    /// Get key (only available for V1)
    pub fn get_key(&self) -> Option<&Vec<u8>> {
        match self {
            WalRecord::V1 { key, .. } => key.as_ref(),
            WalRecord::V2 { .. } => None,
        }
    }

    /// Get value (only available for V1)
    pub fn get_value(&self) -> Option<&[u8]> {
        match self {
            WalRecord::V1 { value, .. } => Some(value.as_slice()),
            WalRecord::V2 { .. } => None,
        }
    }

    /// Get timestamp (only available for V1)
    pub fn get_timestamp(&self) -> Option<i64> {
        match self {
            WalRecord::V1 { timestamp, .. } => Some(*timestamp),
            WalRecord::V2 { .. } => None,
        }
    }

    /// Check if this is a V1 record
    pub fn is_v1(&self) -> bool {
        matches!(self, WalRecord::V1 { .. })
    }

    /// Check if this is a V2 record
    pub fn is_v2(&self) -> bool {
        matches!(self, WalRecord::V2 { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wal_record_v1_roundtrip() {
        let record = WalRecord::new(
            100,
            Some(b"test-key".to_vec()),
            b"test-value".to_vec(),
            1234567890,
        );

        let bytes = record.to_bytes().unwrap();
        let decoded = WalRecord::from_bytes(&bytes).unwrap();

        match (record, decoded) {
            (WalRecord::V1 { offset: o1, key: k1, value: v1, .. },
             WalRecord::V1 { offset: o2, key: k2, value: v2, .. }) => {
                assert_eq!(o1, o2);
                assert_eq!(k1, k2);
                assert_eq!(v1, v2);
            }
            _ => panic!("Expected V1 records"),
        }
    }

    #[test]
    fn test_wal_record_v2_roundtrip() {
        let canonical_data = b"serialized-canonical-record".to_vec();
        let record = WalRecord::new_v2(
            "test-topic".to_string(),
            5,
            canonical_data.clone(),
        );

        let bytes = record.to_bytes().unwrap();
        let decoded = WalRecord::from_bytes(&bytes).unwrap();

        match decoded {
            WalRecord::V2 { topic, partition, canonical_data: data, .. } => {
                assert_eq!(topic, "test-topic");
                assert_eq!(partition, 5);
                assert_eq!(data, canonical_data);
            }
            _ => panic!("Expected V2 record"),
        }
    }

    #[test]
    fn test_wal_version_detection() {
        let v1 = WalRecord::new(0, None, b"test".to_vec(), 0);
        let v2 = WalRecord::new_v2("topic".to_string(), 0, b"data".to_vec());

        let v1_bytes = v1.to_bytes().unwrap();
        let v2_bytes = v2.to_bytes().unwrap();

        let decoded_v1 = WalRecord::from_bytes(&v1_bytes).unwrap();
        let decoded_v2 = WalRecord::from_bytes(&v2_bytes).unwrap();

        assert!(matches!(decoded_v1, WalRecord::V1 { .. }));
        assert!(matches!(decoded_v2, WalRecord::V2 { .. }));
    }

    #[test]
    fn test_checksum_validation() {
        let mut record = WalRecord::new(0, None, b"test".to_vec(), 0);

        // Corrupt the checksum
        if let WalRecord::V1 { ref mut crc32, .. } = record {
            *crc32 = 0xDEADBEEF;
        }

        let bytes = record.to_bytes().unwrap();
        let result = WalRecord::from_bytes(&bytes);

        assert!(result.is_err());
    }
}
