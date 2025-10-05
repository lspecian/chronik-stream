//! Segment format implementation.

use std::io::Read;
use bytes::{Bytes, BytesMut, BufMut};
use chronik_common::types::SegmentMetadata;
use chronik_common::{Result, Error};
use serde::{Deserialize, Serialize};

/// Magic bytes for segment file format
const SEGMENT_MAGIC: &[u8] = b"CHRN";
/// Current segment format version
/// v1: Original format with kafka_data and index
/// v2: Dual storage (raw_kafka_batches + indexed_records)
/// v3: v2 + length prefixes for indexed_records batches (fixes multi-batch bug)
const SEGMENT_VERSION: u16 = 3;

/// Header for segment files
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SegmentHeader {
    /// Magic bytes (CHRN)
    pub magic: [u8; 4],
    /// Format version (v2 supports dual format)
    pub version: u16,
    /// Size of metadata section
    pub metadata_size: u32,
    /// Size of raw Kafka batch data (original wire format)
    pub raw_kafka_size: u64,
    /// Size of indexed records data (for search)
    pub indexed_data_size: u64,
    /// Size of search index section
    pub index_size: u64,
    /// CRC32 checksum of the entire file
    pub checksum: u32,
}

/// A Chronik Stream segment combining Kafka data with search index.
pub struct Segment {
    pub header: SegmentHeader,
    pub metadata: SegmentMetadata,
    /// Raw Kafka batch data in wire format (for Kafka client compatibility)
    pub raw_kafka_batches: Bytes,
    /// Indexed record data (for search functionality)
    pub indexed_records: Bytes,
    /// Search index data
    pub index_data: Bytes,
}

/// Builder for creating segments.
#[derive(Debug, Clone)]
pub struct SegmentBuilder {
    metadata: Option<SegmentMetadata>,
    raw_kafka_batches: BytesMut,
    indexed_records: BytesMut,
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
        buf.put_u64(self.raw_kafka_batches.len() as u64);
        buf.put_u64(self.indexed_records.len() as u64);
        buf.put_u64(self.index_data.len() as u64);
        buf.put_u32(self.header.checksum);
        
        // Write metadata
        buf.put_slice(&metadata_bytes);
        
        // Write raw Kafka batches
        buf.put_slice(&self.raw_kafka_batches);
        
        // Write indexed records
        buf.put_slice(&self.indexed_records);
        
        // Write index data
        buf.put_slice(&self.index_data);
        
        Ok(buf.freeze())
    }
    
    /// Deserialize a segment from bytes
    pub fn deserialize(data: Bytes) -> Result<Self> {
        if data.len() < 38 { // Minimum header size for v2
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
        
        // Handle version differences
        let (raw_kafka_size, indexed_data_size, index_size) = if version == 1 {
            // Version 1: kafka_data_size, index_size
            cursor.read_exact(&mut buf8)?;
            let kafka_data_size = u64::from_be_bytes(buf8);

            cursor.read_exact(&mut buf8)?;
            let index_size = u64::from_be_bytes(buf8);

            // In v1, kafka_data is our indexed records, no raw batches
            (0u64, kafka_data_size, index_size)
        } else {
            // Version 2 & 3: raw_kafka_size, indexed_data_size, index_size
            // v2: indexed_records without length prefixes (multi-batch bug)
            // v3: indexed_records with u32 length prefixes (fixed)
            cursor.read_exact(&mut buf8)?;
            let raw_kafka_size = u64::from_be_bytes(buf8);

            cursor.read_exact(&mut buf8)?;
            let indexed_data_size = u64::from_be_bytes(buf8);

            cursor.read_exact(&mut buf8)?;
            let index_size = u64::from_be_bytes(buf8);

            (raw_kafka_size, indexed_data_size, index_size)
        };
        
        cursor.read_exact(&mut buf4)?;
        let checksum = u32::from_be_bytes(buf4);
        
        let header = SegmentHeader {
            magic,
            version,
            metadata_size,
            raw_kafka_size,
            indexed_data_size,
            index_size,
            checksum,
        };
        
        // Calculate header size based on version
        let header_size = if version == 1 { 30 } else { 38 };
        
        // Read metadata
        let metadata_start = header_size;
        let metadata_end = metadata_start + metadata_size as usize;
        
        // Ensure we have enough data
        if data.len() < metadata_end {
            return Err(Error::InvalidSegment(format!(
                "Segment too small: expected at least {} bytes, got {}",
                metadata_end, data.len()
            )));
        }
        
        let metadata: SegmentMetadata = serde_json::from_slice(&data[metadata_start..metadata_end])?;
        
        // Read data sections based on version
        let (raw_kafka_batches, indexed_records, index_data) = if version == 1 {
            // Version 1: only has kafka_data (indexed) and index_data
            let kafka_start = metadata_end;
            let kafka_end = kafka_start + indexed_data_size as usize;
            let indexed_records = data.slice(kafka_start..kafka_end);
            
            let index_start = kafka_end;
            let index_end = index_start + index_size as usize;
            let index_data = data.slice(index_start..index_end);
            
            // No raw kafka batches in v1
            (Bytes::new(), indexed_records, index_data)
        } else {
            // Version 2: has raw_kafka, indexed, and index
            let raw_start = metadata_end;
            let raw_end = raw_start + raw_kafka_size as usize;
            let raw_kafka_batches = data.slice(raw_start..raw_end);
            
            let indexed_start = raw_end;
            let indexed_end = indexed_start + indexed_data_size as usize;
            let indexed_records = data.slice(indexed_start..indexed_end);
            
            let index_start = indexed_end;
            let index_end = index_start + index_size as usize;
            let index_data = data.slice(index_start..index_end);
            
            (raw_kafka_batches, indexed_records, index_data)
        };
        
        Ok(Segment {
            header,
            metadata,
            raw_kafka_batches,
            indexed_records,
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
            raw_kafka_batches: BytesMut::new(),
            indexed_records: BytesMut::new(),
            index_data: BytesMut::new(),
        }
    }
    
    /// Set segment metadata
    pub fn with_metadata(mut self, metadata: SegmentMetadata) -> Self {
        self.metadata = Some(metadata);
        self
    }
    
    /// Add raw Kafka batch data (wire format)
    ///
    /// In v3 format, each batch is prefixed with a u32 length to allow
    /// proper multi-batch deserialization. This fixes the bug where only
    /// the first batch in a segment could be read.
    pub fn add_raw_kafka_batch(&mut self, data: &[u8]) {
        // v3 format: Add 4-byte length prefix before each batch
        // This allows the reader to know exactly where each batch ends
        // and the next batch begins, fixing the multi-batch bug
        self.raw_kafka_batches.put_u32(data.len() as u32);
        self.raw_kafka_batches.put_slice(data);
    }
    
    /// Add indexed record data (for search)
    ///
    /// In v3 format, each batch is prefixed with a u32 length to allow
    /// proper multi-batch deserialization. This fixes the bug where only
    /// the first batch in a segment could be read.
    pub fn add_indexed_record(&mut self, data: &[u8]) {
        // v3 format: Add 4-byte length prefix before each batch
        // This allows the reader to know exactly where each batch ends
        // and the next batch begins, fixing the multi-batch bug
        self.indexed_records.put_u32(data.len() as u32);
        self.indexed_records.put_slice(data);
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
            raw_kafka_size: self.raw_kafka_batches.len() as u64,
            indexed_data_size: self.indexed_records.len() as u64,
            index_size: self.index_data.len() as u64,
            checksum: 0, // TODO: Calculate CRC32
        };
        
        Ok(Segment {
            header,
            metadata,
            raw_kafka_batches: self.raw_kafka_batches.freeze(),
            indexed_records: self.indexed_records.freeze(),
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
            version: 2,
            metadata_size: 100,
            raw_kafka_size: 1000,
            indexed_data_size: 800,
            index_size: 500,
            checksum: 0,
        };
        
        let mut buf = BytesMut::new();
        buf.put_slice(&header.magic);
        buf.put_u16(header.version);
        buf.put_u32(header.metadata_size);
        buf.put_u64(header.raw_kafka_size);
        buf.put_u64(header.indexed_data_size);
        buf.put_u64(header.index_size);
        buf.put_u32(header.checksum);
        
        let data = buf.freeze();
        assert_eq!(data.len(), 38); // 4 + 2 + 4 + 8 + 8 + 8 + 4
        assert_eq!(&data[0..4], b"CHRN");
    }
}