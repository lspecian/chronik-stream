//! Kafka wire protocol parser.

use bytes::{Buf, BufMut, Bytes, BytesMut};
use chronik_common::{Result, Error};
use std::collections::HashMap;

/// Kafka API keys
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(i16)]
pub enum ApiKey {
    Produce = 0,
    Fetch = 1,
    ListOffsets = 2,
    Metadata = 3,
    LeaderAndIsr = 4,
    StopReplica = 5,
    UpdateMetadata = 6,
    ControlledShutdown = 7,
    OffsetCommit = 8,
    OffsetFetch = 9,
    FindCoordinator = 10,
    JoinGroup = 11,
    Heartbeat = 12,
    LeaveGroup = 13,
    SyncGroup = 14,
    DescribeGroups = 15,
    ListGroups = 16,
    SaslHandshake = 17,
    ApiVersions = 18,
    CreateTopics = 19,
    DeleteTopics = 20,
    DeleteRecords = 21,
    InitProducerId = 22,
    OffsetForLeaderEpoch = 23,
    AddPartitionsToTxn = 24,
    AddOffsetsToTxn = 25,
    EndTxn = 26,
    WriteTxnMarkers = 27,
    TxnOffsetCommit = 28,
    DescribeAcls = 29,
    CreateAcls = 30,
    DeleteAcls = 31,
    DescribeConfigs = 32,
    AlterConfigs = 33,
}

impl ApiKey {
    /// Try to create an ApiKey from an i16
    pub fn from_i16(value: i16) -> Option<Self> {
        match value {
            0 => Some(ApiKey::Produce),
            1 => Some(ApiKey::Fetch),
            2 => Some(ApiKey::ListOffsets),
            3 => Some(ApiKey::Metadata),
            8 => Some(ApiKey::OffsetCommit),
            9 => Some(ApiKey::OffsetFetch),
            10 => Some(ApiKey::FindCoordinator),
            11 => Some(ApiKey::JoinGroup),
            12 => Some(ApiKey::Heartbeat),
            13 => Some(ApiKey::LeaveGroup),
            14 => Some(ApiKey::SyncGroup),
            15 => Some(ApiKey::DescribeGroups),
            16 => Some(ApiKey::ListGroups),
            17 => Some(ApiKey::SaslHandshake),
            18 => Some(ApiKey::ApiVersions),
            19 => Some(ApiKey::CreateTopics),
            20 => Some(ApiKey::DeleteTopics),
            _ => None,
        }
    }
}

/// Kafka request header
#[derive(Debug, Clone)]
pub struct RequestHeader {
    pub api_key: ApiKey,
    pub api_version: i16,
    pub correlation_id: i32,
    pub client_id: Option<String>,
}

/// Kafka response header
#[derive(Debug, Clone)]
pub struct ResponseHeader {
    pub correlation_id: i32,
}

/// Version range for an API
#[derive(Debug, Clone)]
pub struct VersionRange {
    pub min: i16,
    pub max: i16,
}

/// Protocol decoder for reading Kafka protocol primitives
pub struct Decoder<'a> {
    buf: &'a mut dyn Buf,
}

impl<'a> Decoder<'a> {
    /// Create a new decoder
    pub fn new(buf: &'a mut dyn Buf) -> Self {
        Self { buf }
    }
    
    /// Read a boolean
    pub fn read_bool(&mut self) -> Result<bool> {
        if self.buf.remaining() < 1 {
            return Err(Error::Protocol("Not enough bytes for bool".into()));
        }
        Ok(self.buf.get_u8() != 0)
    }
    
    /// Read an i8
    pub fn read_i8(&mut self) -> Result<i8> {
        if self.buf.remaining() < 1 {
            return Err(Error::Protocol("Not enough bytes for i8".into()));
        }
        Ok(self.buf.get_i8())
    }
    
    /// Read an i16
    pub fn read_i16(&mut self) -> Result<i16> {
        if self.buf.remaining() < 2 {
            return Err(Error::Protocol("Not enough bytes for i16".into()));
        }
        Ok(self.buf.get_i16())
    }
    
    /// Read an i32
    pub fn read_i32(&mut self) -> Result<i32> {
        if self.buf.remaining() < 4 {
            return Err(Error::Protocol("Not enough bytes for i32".into()));
        }
        Ok(self.buf.get_i32())
    }
    
    /// Read an i64
    pub fn read_i64(&mut self) -> Result<i64> {
        if self.buf.remaining() < 8 {
            return Err(Error::Protocol("Not enough bytes for i64".into()));
        }
        Ok(self.buf.get_i64())
    }
    
    /// Read a string (null = -1 length)
    pub fn read_string(&mut self) -> Result<Option<String>> {
        let len = self.read_i16()?;
        if len < 0 {
            return Ok(None);
        }
        
        let len = len as usize;
        if self.buf.remaining() < len {
            return Err(Error::Protocol(format!("Not enough bytes for string of length {}", len)));
        }
        
        let mut bytes = vec![0u8; len];
        self.buf.copy_to_slice(&mut bytes);
        
        String::from_utf8(bytes)
            .map(Some)
            .map_err(|e| Error::Protocol(format!("Invalid UTF-8 in string: {}", e)))
    }
    
    /// Read a byte array (null = -1 length)
    pub fn read_bytes(&mut self) -> Result<Option<Bytes>> {
        let len = self.read_i32()?;
        if len < 0 {
            return Ok(None);
        }
        
        let len = len as usize;
        if self.buf.remaining() < len {
            return Err(Error::Protocol(format!("Not enough bytes for byte array of length {}", len)));
        }
        
        Ok(Some(self.buf.copy_to_bytes(len)))
    }
    
    /// Read a compact string (uses varint length)
    pub fn read_compact_string(&mut self) -> Result<Option<String>> {
        let len = self.read_unsigned_varint()? as i32 - 1;
        if len < 0 {
            return Ok(None);
        }
        
        let len = len as usize;
        if self.buf.remaining() < len {
            return Err(Error::Protocol(format!("Not enough bytes for compact string of length {}", len)));
        }
        
        let mut bytes = vec![0u8; len];
        self.buf.copy_to_slice(&mut bytes);
        
        String::from_utf8(bytes)
            .map(Some)
            .map_err(|e| Error::Protocol(format!("Invalid UTF-8 in string: {}", e)))
    }
    
    /// Read an unsigned varint
    pub fn read_unsigned_varint(&mut self) -> Result<u32> {
        let mut value = 0u32;
        let mut i = 0;
        
        loop {
            if !self.buf.has_remaining() {
                return Err(Error::Protocol("Incomplete varint".into()));
            }
            
            let byte = self.buf.get_u8();
            value |= ((byte & 0x7F) as u32) << (i * 7);
            
            if byte & 0x80 == 0 {
                return Ok(value);
            }
            
            i += 1;
            if i >= 5 {
                return Err(Error::Protocol("Varint too long".into()));
            }
        }
    }
}

/// Protocol encoder for writing Kafka protocol primitives
pub struct Encoder<'a> {
    buf: &'a mut BytesMut,
}

impl<'a> Encoder<'a> {
    /// Create a new encoder
    pub fn new(buf: &'a mut BytesMut) -> Self {
        Self { buf }
    }
    
    /// Write a boolean
    pub fn write_bool(&mut self, value: bool) {
        self.buf.put_u8(if value { 1 } else { 0 });
    }
    
    /// Write an i8
    pub fn write_i8(&mut self, value: i8) {
        self.buf.put_i8(value);
    }
    
    /// Write an i16
    pub fn write_i16(&mut self, value: i16) {
        self.buf.put_i16(value);
    }
    
    /// Write an i32
    pub fn write_i32(&mut self, value: i32) {
        self.buf.put_i32(value);
    }
    
    /// Write an i64
    pub fn write_i64(&mut self, value: i64) {
        self.buf.put_i64(value);
    }
    
    /// Write a string (null = None)
    pub fn write_string(&mut self, value: Option<&str>) {
        match value {
            Some(s) => {
                self.write_i16(s.len() as i16);
                self.buf.put_slice(s.as_bytes());
            }
            None => {
                self.write_i16(-1);
            }
        }
    }
    
    /// Write a byte array (null = None)
    pub fn write_bytes(&mut self, value: Option<&[u8]>) {
        match value {
            Some(bytes) => {
                self.write_i32(bytes.len() as i32);
                self.buf.put_slice(bytes);
            }
            None => {
                self.write_i32(-1);
            }
        }
    }
    
    /// Write a compact string (uses varint length)
    pub fn write_compact_string(&mut self, value: Option<&str>) {
        match value {
            Some(s) => {
                self.write_unsigned_varint((s.len() + 1) as u32);
                self.buf.put_slice(s.as_bytes());
            }
            None => {
                self.write_unsigned_varint(0);
            }
        }
    }
    
    /// Write an unsigned varint
    pub fn write_unsigned_varint(&mut self, mut value: u32) {
        while (value & !0x7F) != 0 {
            self.buf.put_u8((value & 0x7F) as u8 | 0x80);
            value >>= 7;
        }
        self.buf.put_u8(value as u8);
    }
}

/// Parse a request header from bytes
pub fn parse_request_header(buf: &mut dyn Buf) -> Result<RequestHeader> {
    let mut decoder = Decoder::new(buf);
    
    let api_key_raw = decoder.read_i16()?;
    let api_key = ApiKey::from_i16(api_key_raw)
        .ok_or_else(|| Error::Protocol(format!("Unknown API key: {}", api_key_raw)))?;
    
    let api_version = decoder.read_i16()?;
    let correlation_id = decoder.read_i32()?;
    let client_id = decoder.read_string()?;
    
    Ok(RequestHeader {
        api_key,
        api_version,
        correlation_id,
        client_id,
    })
}

/// Write a response header to bytes
pub fn write_response_header(buf: &mut BytesMut, header: &ResponseHeader) {
    let mut encoder = Encoder::new(buf);
    encoder.write_i32(header.correlation_id);
}

/// Get supported API versions
pub fn supported_api_versions() -> HashMap<ApiKey, VersionRange> {
    let mut versions = HashMap::new();
    
    // Core APIs
    versions.insert(ApiKey::Produce, VersionRange { min: 0, max: 9 });
    versions.insert(ApiKey::Fetch, VersionRange { min: 0, max: 13 });
    versions.insert(ApiKey::ListOffsets, VersionRange { min: 0, max: 7 });
    versions.insert(ApiKey::Metadata, VersionRange { min: 0, max: 12 });
    versions.insert(ApiKey::OffsetCommit, VersionRange { min: 0, max: 8 });
    versions.insert(ApiKey::OffsetFetch, VersionRange { min: 0, max: 8 });
    versions.insert(ApiKey::FindCoordinator, VersionRange { min: 0, max: 4 });
    versions.insert(ApiKey::JoinGroup, VersionRange { min: 0, max: 9 });
    versions.insert(ApiKey::Heartbeat, VersionRange { min: 0, max: 4 });
    versions.insert(ApiKey::LeaveGroup, VersionRange { min: 0, max: 5 });
    versions.insert(ApiKey::SyncGroup, VersionRange { min: 0, max: 5 });
    versions.insert(ApiKey::DescribeGroups, VersionRange { min: 0, max: 5 });
    versions.insert(ApiKey::ListGroups, VersionRange { min: 0, max: 4 });
    versions.insert(ApiKey::SaslHandshake, VersionRange { min: 0, max: 1 });
    versions.insert(ApiKey::ApiVersions, VersionRange { min: 0, max: 3 });
    versions.insert(ApiKey::CreateTopics, VersionRange { min: 0, max: 7 });
    versions.insert(ApiKey::DeleteTopics, VersionRange { min: 0, max: 6 });
    
    versions
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_varint_encoding() {
        let mut buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut buf);
        
        encoder.write_unsigned_varint(0);
        encoder.write_unsigned_varint(127);
        encoder.write_unsigned_varint(128);
        encoder.write_unsigned_varint(16383);
        encoder.write_unsigned_varint(16384);
        
        let mut frozen_buf = buf.freeze();
        let mut decoder = Decoder::new(&mut frozen_buf);
        assert_eq!(decoder.read_unsigned_varint().unwrap(), 0);
        assert_eq!(decoder.read_unsigned_varint().unwrap(), 127);
        assert_eq!(decoder.read_unsigned_varint().unwrap(), 128);
        assert_eq!(decoder.read_unsigned_varint().unwrap(), 16383);
        assert_eq!(decoder.read_unsigned_varint().unwrap(), 16384);
    }
    
    #[test]
    fn test_string_encoding() {
        let mut buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut buf);
        
        encoder.write_string(Some("hello"));
        encoder.write_string(None);
        encoder.write_string(Some(""));
        
        let mut frozen_buf = buf.freeze();
        let mut decoder = Decoder::new(&mut frozen_buf);
        assert_eq!(decoder.read_string().unwrap(), Some("hello".to_string()));
        assert_eq!(decoder.read_string().unwrap(), None);
        assert_eq!(decoder.read_string().unwrap(), Some("".to_string()));
    }
}