//! WAL record format with CRC32 checksums

use bytes::{BufMut, BytesMut};
use crate::buffer_pool::PooledBuffer;
use crc32fast::Hasher;
use serde::{Deserialize, Serialize};
use std::io::Cursor;
use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};

use crate::error::{Result, WalError};

/// Magic number for WAL records
const WAL_MAGIC: u16 = 0xCA7E;

/// Current WAL format version
const WAL_VERSION: u8 = 1;

/// WAL record with versioning and checksums
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalRecord {
    // Header fields
    pub magic: u16,
    pub version: u8,
    pub flags: u8,
    pub length: u32,
    pub offset: i64,
    pub timestamp: i64,
    
    // Checksum
    pub crc32: u32,
    
    // Payload
    pub key: Option<Vec<u8>>,
    pub value: Vec<u8>,
    pub headers: Vec<(String, Vec<u8>)>,
}

impl WalRecord {
    /// Create a new WAL record
    pub fn new(
        offset: i64,
        key: Option<Vec<u8>>,
        value: Vec<u8>,
        timestamp: i64,
    ) -> Self {
        let mut record = Self {
            magic: WAL_MAGIC,
            version: WAL_VERSION,
            flags: 0,
            length: 0,
            offset,
            timestamp,
            crc32: 0,
            key,
            value,
            headers: Vec::new(),
        };
        
        // Calculate length and checksum
        record.length = record.calculate_length();
        record.crc32 = record.calculate_checksum();
        
        record
    }
    
    /// Serialize record to bytes
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let mut buf = Vec::with_capacity(self.length as usize);
        
        // Write header
        buf.write_u16::<LittleEndian>(self.magic)?;
        buf.write_u8(self.version)?;
        buf.write_u8(self.flags)?;
        buf.write_u32::<LittleEndian>(self.length)?;
        buf.write_i64::<LittleEndian>(self.offset)?;
        buf.write_i64::<LittleEndian>(self.timestamp)?;
        buf.write_u32::<LittleEndian>(self.crc32)?;
        
        // Write key length and data
        match &self.key {
            Some(k) => {
                buf.write_i32::<LittleEndian>(k.len() as i32)?;
                buf.extend_from_slice(k);
            }
            None => {
                buf.write_i32::<LittleEndian>(-1)?;
            }
        }
        
        // Write value length and data
        buf.write_i32::<LittleEndian>(self.value.len() as i32)?;
        buf.extend_from_slice(&self.value);
        
        // Write headers count and data
        buf.write_i32::<LittleEndian>(self.headers.len() as i32)?;
        for (name, value) in &self.headers {
            buf.write_u16::<LittleEndian>(name.len() as u16)?;
            buf.extend_from_slice(name.as_bytes());
            buf.write_u32::<LittleEndian>(value.len() as u32)?;
            buf.extend_from_slice(value);
        }
        
        Ok(buf)
    }
    
    /// Deserialize record from bytes
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        let mut cursor = Cursor::new(data);
        
        // Read header
        let magic = cursor.read_u16::<LittleEndian>()?;
        if magic != WAL_MAGIC {
            return Err(WalError::InvalidFormat(format!(
                "Invalid magic number: 0x{:04X}",
                magic
            )));
        }
        
        let version = cursor.read_u8()?;
        if version != WAL_VERSION {
            return Err(WalError::InvalidFormat(format!(
                "Unsupported version: {}",
                version
            )));
        }
        
        let flags = cursor.read_u8()?;
        let length = cursor.read_u32::<LittleEndian>()?;
        let offset = cursor.read_i64::<LittleEndian>()?;
        let timestamp = cursor.read_i64::<LittleEndian>()?;
        let crc32 = cursor.read_u32::<LittleEndian>()?;
        
        // Read key
        let key_len = cursor.read_i32::<LittleEndian>()?;
        let key = if key_len >= 0 {
            let mut buf = vec![0u8; key_len as usize];
            cursor.read_exact(&mut buf)?;
            Some(buf)
        } else {
            None
        };
        
        // Read value
        let value_len = cursor.read_i32::<LittleEndian>()?;
        let mut value = vec![0u8; value_len as usize];
        cursor.read_exact(&mut value)?;
        
        // Read headers
        let headers_count = cursor.read_i32::<LittleEndian>()?;
        let mut headers = Vec::with_capacity(headers_count as usize);
        for _ in 0..headers_count {
            let name_len = cursor.read_u16::<LittleEndian>()?;
            let mut name_buf = vec![0u8; name_len as usize];
            cursor.read_exact(&mut name_buf)?;
            let name = String::from_utf8(name_buf)
                .map_err(|e| WalError::InvalidFormat(e.to_string()))?;
            
            let value_len = cursor.read_u32::<LittleEndian>()?;
            let mut value_buf = vec![0u8; value_len as usize];
            cursor.read_exact(&mut value_buf)?;
            
            headers.push((name, value_buf));
        }
        
        let record = Self {
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
    
    /// Write record to a pooled buffer (for efficient memory reuse)
    pub fn write_to_pooled(&self) -> Result<PooledBuffer> {
        let mut buf = PooledBuffer::acquire(self.length as usize);
        
        // Write header
        buf.put_u16(self.magic);
        buf.put_u8(self.version);
        buf.put_u8(self.flags);
        buf.put_u32(self.length);
        buf.put_i64(self.offset);
        buf.put_i64(self.timestamp);
        buf.put_u32(self.crc32);
        
        // Write key length and data
        match &self.key {
            Some(k) => {
                buf.put_i32(k.len() as i32);
                buf.put_slice(k);
            }
            None => {
                buf.put_i32(-1);
            }
        }
        
        // Write value length and data
        buf.put_i32(self.value.len() as i32);
        buf.put_slice(&self.value);
        
        // Write headers count and data
        buf.put_i32(self.headers.len() as i32);
        for (name, value) in &self.headers {
            buf.put_u16(name.len() as u16);
            buf.put_slice(name.as_bytes());
            buf.put_u32(value.len() as u32);
            buf.put_slice(value);
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
        if computed != self.crc32 {
            return Err(WalError::ChecksumMismatch {
                offset: self.offset,
                expected: self.crc32,
                actual: computed,
            });
        }
        Ok(())
    }
    
    /// Calculate record length
    pub fn calculate_length(&self) -> u32 {
        let mut len = 28; // Fixed header size
        len += 4; // Key length field
        if let Some(ref k) = self.key {
            len += k.len() as u32;
        }
        len += 4 + self.value.len() as u32; // Value length + data
        len += 4; // Headers count
        for (name, value) in &self.headers {
            len += 2 + name.len() as u32 + 4 + value.len() as u32;
        }
        len
    }
    
    /// Calculate CRC32 checksum
    pub fn calculate_checksum(&self) -> u32 {
        let mut hasher = Hasher::new();
        
        // Hash all fields except the CRC itself
        hasher.update(&self.magic.to_le_bytes());
        hasher.update(&[self.version, self.flags]);
        hasher.update(&self.length.to_le_bytes());
        hasher.update(&self.offset.to_le_bytes());
        hasher.update(&self.timestamp.to_le_bytes());
        
        // Hash key
        match &self.key {
            Some(k) => {
                hasher.update(&(k.len() as i32).to_le_bytes());
                hasher.update(k);
            }
            None => {
                hasher.update(&(-1i32).to_le_bytes());
            }
        }
        
        // Hash value
        hasher.update(&(self.value.len() as i32).to_le_bytes());
        hasher.update(&self.value);
        
        // Hash headers
        hasher.update(&(self.headers.len() as i32).to_le_bytes());
        for (name, value) in &self.headers {
            hasher.update(&(name.len() as u16).to_le_bytes());
            hasher.update(name.as_bytes());
            hasher.update(&(value.len() as u32).to_le_bytes());
            hasher.update(value);
        }
        
        hasher.finalize()
    }
}

use std::io::Read;