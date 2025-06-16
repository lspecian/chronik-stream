//! Segment format implementation.

use std::io::Read;
use bytes::{Bytes, BytesMut, BufMut};
use chronik_common::types::SegmentMetadata;
use chronik_common::{Result, Error};
use serde::{Deserialize, Serialize};

/// Magic bytes for segment file format
const SEGMENT_MAGIC: &[u8] = b"CHRN";
/// Current segment format version
const SEGMENT_VERSION: u16 = 1;

/// Header for segment files
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SegmentHeader {
    /// Magic bytes (CHRN)
    pub magic: [u8; 4],
    /// Format version
    pub version: u16,
    /// Size of metadata section
    pub metadata_size: u32,
    /// Size of Kafka data section
    pub kafka_data_size: u64,
    /// Size of search index section
    pub index_size: u64,
    /// CRC32 checksum of the entire file
    pub checksum: u32,
}

/// A Chronik Stream segment combining Kafka data with search index.
pub struct Segment {
    pub header: SegmentHeader,
    pub metadata: SegmentMetadata,
    pub kafka_data: Bytes,
    pub index_data: Bytes,
}

/// Builder for creating segments.
#[derive(Debug)]
pub struct SegmentBuilder {
    metadata: Option<SegmentMetadata>,
    kafka_data: BytesMut,
    index_data: BytesMut,
}

impl Segment {
    /// Serialize the segment to bytes for storage
    pub fn serialize(&self) -> Result<Bytes> {
        let mut buf = BytesMut::new();
        
        // Serialize metadata first to get its size
        let metadata_bytes = serde_json::to_vec(&self.metadata)?;
        
        // Write header
        buf.put_slice(&self.header.magic);
        buf.put_u16(self.header.version);
        buf.put_u32(metadata_bytes.len() as u32);
        buf.put_u64(self.kafka_data.len() as u64);
        buf.put_u64(self.index_data.len() as u64);
        buf.put_u32(self.header.checksum);
        
        // Write metadata
        buf.put_slice(&metadata_bytes);
        
        // Write Kafka data
        buf.put_slice(&self.kafka_data);
        
        // Write index data
        buf.put_slice(&self.index_data);
        
        Ok(buf.freeze())
    }
    
    /// Deserialize a segment from bytes
    pub fn deserialize(data: Bytes) -> Result<Self> {
        if data.len() < 30 { // Minimum header size
            return Err(Error::InvalidSegment("Data too small for segment header".into()));
        }
        
        let mut cursor = std::io::Cursor::new(&data[..]);
        
        // Read header
        let mut magic = [0u8; 4];
        cursor.read_exact(&mut magic)?;
        if &magic != SEGMENT_MAGIC {
            return Err(Error::InvalidSegment("Invalid magic bytes".into()));
        }
        
        let mut buf2 = [0u8; 2];
        cursor.read_exact(&mut buf2)?;
        let version = u16::from_be_bytes(buf2);
        
        let mut buf4 = [0u8; 4];
        cursor.read_exact(&mut buf4)?;
        let metadata_size = u32::from_be_bytes(buf4);
        
        let mut buf8 = [0u8; 8];
        cursor.read_exact(&mut buf8)?;
        let kafka_data_size = u64::from_be_bytes(buf8);
        
        cursor.read_exact(&mut buf8)?;
        let index_size = u64::from_be_bytes(buf8);
        
        cursor.read_exact(&mut buf4)?;
        let checksum = u32::from_be_bytes(buf4);
        
        let header = SegmentHeader {
            magic,
            version,
            metadata_size,
            kafka_data_size,
            index_size,
            checksum,
        };
        
        // Read metadata
        let metadata_start = 30;
        let metadata_end = metadata_start + metadata_size as usize;
        
        // Ensure we have enough data
        if data.len() < metadata_end {
            return Err(Error::InvalidSegment(format!(
                "Segment too small: expected at least {} bytes, got {}",
                metadata_end, data.len()
            )));
        }
        
        let metadata: SegmentMetadata = serde_json::from_slice(&data[metadata_start..metadata_end])?;
        
        // Read Kafka data
        let kafka_start = metadata_end;
        let kafka_end = kafka_start + kafka_data_size as usize;
        let kafka_data = data.slice(kafka_start..kafka_end);
        
        // Read index data
        let index_start = kafka_end;
        let index_end = index_start + index_size as usize;
        let index_data = data.slice(index_start..index_end);
        
        Ok(Segment {
            header,
            metadata,
            kafka_data,
            index_data,
        })
    }
    
    /// Get the storage key for this segment
    pub fn storage_key(&self) -> String {
        format!(
            "segments/{}/partition-{:05}/segment-{:016}-{:016}.chrn",
            self.metadata.topic_partition.topic,
            self.metadata.topic_partition.partition,
            self.metadata.base_offset,
            self.metadata.last_offset
        )
    }
}

impl SegmentBuilder {
    /// Create a new segment builder.
    pub fn new() -> Self {
        Self {
            metadata: None,
            kafka_data: BytesMut::new(),
            index_data: BytesMut::new(),
        }
    }
    
    /// Set segment metadata
    pub fn with_metadata(mut self, metadata: SegmentMetadata) -> Self {
        self.metadata = Some(metadata);
        self
    }
    
    /// Add Kafka data
    pub fn add_kafka_data(&mut self, data: &[u8]) {
        self.kafka_data.put_slice(data);
    }
    
    /// Add index data
    pub fn add_index_data(&mut self, data: &[u8]) {
        self.index_data.put_slice(data);
    }
    
    /// Build the segment
    pub fn build(self) -> Result<Segment> {
        let metadata = self.metadata
            .ok_or_else(|| Error::InvalidSegment("Metadata not set".into()))?;
        
        let header = SegmentHeader {
            magic: *b"CHRN",
            version: SEGMENT_VERSION,
            metadata_size: 0, // Will be calculated during serialization
            kafka_data_size: self.kafka_data.len() as u64,
            index_size: self.index_data.len() as u64,
            checksum: 0, // TODO: Calculate CRC32
        };
        
        Ok(Segment {
            header,
            metadata,
            kafka_data: self.kafka_data.freeze(),
            index_data: self.index_data.freeze(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chronik_common::types::{SegmentId, TopicPartition};
    
    #[test]
    fn test_header_serialization() {
        let header = SegmentHeader {
            magic: *b"CHRN",
            version: 1,
            metadata_size: 100,
            kafka_data_size: 1000,
            index_size: 500,
            checksum: 0,
        };
        
        let mut buf = BytesMut::new();
        buf.put_slice(&header.magic);
        buf.put_u16(header.version);
        buf.put_u32(header.metadata_size);
        buf.put_u64(header.kafka_data_size);
        buf.put_u64(header.index_size);
        buf.put_u32(header.checksum);
        
        let data = buf.freeze();
        assert_eq!(data.len(), 30); // 4 + 2 + 4 + 8 + 8 + 4
        assert_eq!(&data[0..4], b"CHRN");
    }
}