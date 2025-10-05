//! Kafka RecordBatch storage and serialization.

use bytes::Bytes;
use chronik_common::Result;
use serde::{Deserialize, Serialize};

/// Kafka RecordBatch representation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordBatch {
    pub records: Vec<Record>,
}

/// Individual record within a batch
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub offset: i64,
    pub timestamp: i64,
    pub key: Option<Vec<u8>>,
    pub value: Vec<u8>,
    pub headers: std::collections::HashMap<String, Vec<u8>>,
}

/// Record header
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordHeader {
    pub key: String,
    pub value: Option<Bytes>,
}

/// Stats about a collection of record batches
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordBatchStats {
    pub count: usize,
    pub total_bytes: usize,
    pub base_offset: i64,
    pub last_offset: i64,
    pub base_timestamp: i64,
    pub max_timestamp: i64,
}

impl Record {
    /// Get the size of this record in bytes
    pub fn size_bytes(&self) -> usize {
        let key_size = self.key.as_ref().map(|k| k.len()).unwrap_or(0);
        let value_size = self.value.len();
        let headers_size: usize = self.headers.values().map(|v| v.len()).sum();
        key_size + value_size + headers_size + 24 // 24 bytes for offset/timestamp
    }
}

impl RecordBatch {
    /// Get the size of this batch in bytes
    pub fn size_bytes(&self) -> usize {
        self.records.iter().map(|r| {
            let key_size = r.key.as_ref().map(|k| k.len()).unwrap_or(0);
            let value_size = r.value.len();
            let headers_size: usize = r.headers.values().map(|v| v.len()).sum();
            key_size + value_size + headers_size + 24 // 24 bytes for offset/timestamp
        }).sum()
    }
    
    /// Encode the batch to bytes (simplified format)
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        
        // Write record count
        buf.extend_from_slice(&(self.records.len() as u32).to_be_bytes());
        
        // Write records
        for record in &self.records {
            // Offset
            buf.extend_from_slice(&record.offset.to_be_bytes());
            // Timestamp
            buf.extend_from_slice(&record.timestamp.to_be_bytes());
            
            // Key
            if let Some(key) = &record.key {
                buf.extend_from_slice(&(key.len() as u32).to_be_bytes());
                buf.extend_from_slice(key);
            } else {
                buf.extend_from_slice(&0u32.to_be_bytes());
            }
            
            // Value
            buf.extend_from_slice(&(record.value.len() as u32).to_be_bytes());
            buf.extend_from_slice(&record.value);
            
            // Headers count
            buf.extend_from_slice(&(record.headers.len() as u32).to_be_bytes());
            
            // Headers
            for (k, v) in &record.headers {
                buf.extend_from_slice(&(k.len() as u32).to_be_bytes());
                buf.extend_from_slice(k.as_bytes());
                buf.extend_from_slice(&(v.len() as u32).to_be_bytes());
                buf.extend_from_slice(v);
            }
        }
        
        Ok(buf)
    }
    
    /// Decode a batch from bytes, returning the batch and number of bytes consumed
    pub fn decode(data: &[u8]) -> Result<(Self, usize)> {
        use std::io::Cursor;
        use byteorder::{BigEndian, ReadBytesExt};

        let mut cursor = Cursor::new(data);
        let mut records = Vec::new();

        // Read record count
        let count = cursor.read_u32::<BigEndian>()
            .map_err(|e| chronik_common::Error::Internal(format!("Failed to read count: {}", e)))? as usize;

        // Read records
        for _ in 0..count {
            // Offset
            let offset = cursor.read_i64::<BigEndian>()
                .map_err(|e| chronik_common::Error::Internal(format!("Failed to read offset: {}", e)))?;

            // Timestamp
            let timestamp = cursor.read_i64::<BigEndian>()
                .map_err(|e| chronik_common::Error::Internal(format!("Failed to read timestamp: {}", e)))?;

            // Key
            let key_len = cursor.read_u32::<BigEndian>()
                .map_err(|e| chronik_common::Error::Internal(format!("Failed to read key len: {}", e)))? as usize;
            let key = if key_len > 0 {
                let mut key_buf = vec![0u8; key_len];
                std::io::Read::read_exact(&mut cursor, &mut key_buf)
                    .map_err(|e| chronik_common::Error::Internal(format!("Failed to read key: {}", e)))?;
                Some(key_buf)
            } else {
                None
            };

            // Value
            let value_len = cursor.read_u32::<BigEndian>()
                .map_err(|e| chronik_common::Error::Internal(format!("Failed to read value len: {}", e)))? as usize;
            let mut value = vec![0u8; value_len];
            std::io::Read::read_exact(&mut cursor, &mut value)
                .map_err(|e| chronik_common::Error::Internal(format!("Failed to read value: {}", e)))?;

            // Headers
            let header_count = cursor.read_u32::<BigEndian>()
                .map_err(|e| chronik_common::Error::Internal(format!("Failed to read header count: {}", e)))? as usize;

            let mut headers = std::collections::HashMap::new();
            for _ in 0..header_count {
                // Key
                let key_len = cursor.read_u32::<BigEndian>()
                    .map_err(|e| chronik_common::Error::Internal(format!("Failed to read header key len: {}", e)))? as usize;
                let mut key_buf = vec![0u8; key_len];
                std::io::Read::read_exact(&mut cursor, &mut key_buf)
                    .map_err(|e| chronik_common::Error::Internal(format!("Failed to read header key: {}", e)))?;
                let key = String::from_utf8(key_buf)
                    .map_err(|e| chronik_common::Error::Internal(format!("Invalid header key: {}", e)))?;

                // Value
                let value_len = cursor.read_u32::<BigEndian>()
                    .map_err(|e| chronik_common::Error::Internal(format!("Failed to read header value len: {}", e)))? as usize;
                let mut value_buf = vec![0u8; value_len];
                std::io::Read::read_exact(&mut cursor, &mut value_buf)
                    .map_err(|e| chronik_common::Error::Internal(format!("Failed to read header value: {}", e)))?;

                headers.insert(key, value_buf);
            }

            records.push(Record {
                offset,
                timestamp,
                key,
                value,
                headers,
            });
        }

        let bytes_consumed = cursor.position() as usize;
        Ok((Self { records }, bytes_consumed))
    }
}