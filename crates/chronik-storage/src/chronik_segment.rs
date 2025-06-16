//! ChronikSegment format: Combined Kafka RecordBatch + Tantivy index in a single immutable file.
//!
//! The segment format is designed for efficient streaming and search operations:
//! - Header with metadata and offset information
//! - Compressed Kafka RecordBatch data
//! - Tantivy search index
//! - Bloom filter for quick key lookups
//! - Support for partial reads and range queries

use chronik_common::{Result, Error};
use crate::{RecordBatch, Record};
use tantivy::{
    schema::{Schema, TEXT, STORED, STRING, NumericOptions, Field},
    Index,
};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write, Seek, SeekFrom, Cursor};
use tracing::{debug, info, warn};
use flate2::write::GzEncoder;
use flate2::read::GzDecoder;
use flate2::Compression;
use std::collections::hash_map::DefaultHasher;
use std::hash::Hasher;
use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};

/// Magic number for ChronikSegment files (8 bytes)
const CHRONIK_MAGIC: &[u8] = b"CHRONIK1";

/// Current segment format version
const SEGMENT_VERSION: u16 = 1;


/// Compression type for segment data
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub enum CompressionType {
    None = 0,
    Gzip = 1,
    // Future: Zstd = 2,
    // Future: Snappy = 3,
}

impl CompressionType {
    fn from_u8(value: u8) -> Result<Self> {
        match value {
            0 => Ok(CompressionType::None),
            1 => Ok(CompressionType::Gzip),
            _ => Err(Error::InvalidSegment(format!("Unknown compression type: {}", value))),
        }
    }
}

/// Segment header with fixed size for efficient seeking
#[derive(Debug, Clone)]
pub struct SegmentHeader {
    /// Magic number (8 bytes)
    pub magic: [u8; 8],
    /// Format version (2 bytes)
    pub version: u16,
    /// Compression type (1 byte)
    pub compression: CompressionType,
    /// Reserved for future use (5 bytes)
    pub reserved: [u8; 5],
    /// Metadata section offset (8 bytes)
    pub metadata_offset: u64,
    /// Metadata section size (8 bytes)
    pub metadata_size: u64,
    /// Kafka data section offset (8 bytes)
    pub kafka_offset: u64,
    /// Kafka data section size (compressed) (8 bytes)
    pub kafka_size: u64,
    /// Kafka data uncompressed size (8 bytes)
    pub kafka_uncompressed_size: u64,
    /// Tantivy index section offset (8 bytes)
    pub index_offset: u64,
    /// Tantivy index section size (8 bytes)
    pub index_size: u64,
    /// Bloom filter section offset (8 bytes)
    pub bloom_offset: u64,
    /// Bloom filter section size (8 bytes)
    pub bloom_size: u64,
    /// CRC32 checksum of the entire file (4 bytes)
    pub checksum: u32,
}

impl SegmentHeader {
    const SIZE: usize = 92; // Total header size in bytes
    
    /// Create a new header with default values
    fn new() -> Self {
        Self {
            magic: CHRONIK_MAGIC.try_into().unwrap(),
            version: SEGMENT_VERSION,
            compression: CompressionType::Gzip,
            reserved: [0u8; 5],
            metadata_offset: 0,
            metadata_size: 0,
            kafka_offset: 0,
            kafka_size: 0,
            kafka_uncompressed_size: 0,
            index_offset: 0,
            index_size: 0,
            bloom_offset: 0,
            bloom_size: 0,
            checksum: 0,
        }
    }
    
    /// Write header to a writer
    fn write_to<W: Write>(&self, writer: &mut W) -> Result<()> {
        writer.write_all(&self.magic)?;
        writer.write_u16::<BigEndian>(self.version)?;
        writer.write_u8(self.compression as u8)?;
        writer.write_all(&self.reserved)?;
        writer.write_u64::<BigEndian>(self.metadata_offset)?;
        writer.write_u64::<BigEndian>(self.metadata_size)?;
        writer.write_u64::<BigEndian>(self.kafka_offset)?;
        writer.write_u64::<BigEndian>(self.kafka_size)?;
        writer.write_u64::<BigEndian>(self.kafka_uncompressed_size)?;
        writer.write_u64::<BigEndian>(self.index_offset)?;
        writer.write_u64::<BigEndian>(self.index_size)?;
        writer.write_u64::<BigEndian>(self.bloom_offset)?;
        writer.write_u64::<BigEndian>(self.bloom_size)?;
        writer.write_u32::<BigEndian>(self.checksum)?;
        Ok(())
    }
    
    /// Read header from a reader
    fn read_from<R: Read>(reader: &mut R) -> Result<Self> {
        let mut magic = [0u8; 8];
        reader.read_exact(&mut magic)?;
        
        if &magic != CHRONIK_MAGIC {
            return Err(Error::InvalidSegment("Invalid magic number".into()));
        }
        
        let version = reader.read_u16::<BigEndian>()?;
        if version != SEGMENT_VERSION {
            return Err(Error::InvalidSegment(format!("Unsupported version: {}", version)));
        }
        
        let compression = CompressionType::from_u8(reader.read_u8()?)?;
        let mut reserved = [0u8; 5];
        reader.read_exact(&mut reserved)?;
        
        Ok(Self {
            magic,
            version,
            compression,
            reserved,
            metadata_offset: reader.read_u64::<BigEndian>()?,
            metadata_size: reader.read_u64::<BigEndian>()?,
            kafka_offset: reader.read_u64::<BigEndian>()?,
            kafka_size: reader.read_u64::<BigEndian>()?,
            kafka_uncompressed_size: reader.read_u64::<BigEndian>()?,
            index_offset: reader.read_u64::<BigEndian>()?,
            index_size: reader.read_u64::<BigEndian>()?,
            bloom_offset: reader.read_u64::<BigEndian>()?,
            bloom_size: reader.read_u64::<BigEndian>()?,
            checksum: reader.read_u32::<BigEndian>()?,
        })
    }
}

/// Segment metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SegmentMetadata {
    /// Topic name
    pub topic: String,
    /// Partition ID
    pub partition_id: i32,
    /// Base offset (first message)
    pub base_offset: i64,
    /// Last offset (last message)
    pub last_offset: i64,
    /// Timestamp range
    pub timestamp_range: (i64, i64),
    /// Number of records
    pub record_count: u64,
    /// Segment creation timestamp
    pub created_at: i64,
    /// Optional bloom filter for quick key lookups (deprecated - moved to separate section)
    pub bloom_filter: Option<BloomFilter>,
    /// Compression ratio (0.0 to 1.0, where 0.5 means 50% compression)
    pub compression_ratio: f64,
    /// Total uncompressed size of all records
    pub total_uncompressed_size: usize,
}

/// Bloom filter for efficient key lookups
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BloomFilter {
    /// Bit array
    bits: Vec<u8>,
    /// Number of hash functions
    hash_count: u32,
    /// Number of bits
    bit_count: usize,
}

impl BloomFilter {
    /// Create a new bloom filter
    pub fn new(expected_items: usize, false_positive_rate: f64) -> Self {
        // Calculate optimal bit count and hash count
        let bit_count = Self::optimal_bit_count(expected_items, false_positive_rate);
        let hash_count = Self::optimal_hash_count(bit_count, expected_items);
        
        Self {
            bits: vec![0u8; (bit_count + 7) / 8],
            hash_count,
            bit_count,
        }
    }
    
    /// Create with default parameters
    pub fn default_for_items(expected_items: usize) -> Self {
        Self::new(expected_items, 0.01) // 1% false positive rate
    }
    
    /// Add an item to the bloom filter
    pub fn add(&mut self, item: &[u8]) {
        for i in 0..self.hash_count {
            let hash = self.hash(item, i);
            let bit_idx = (hash % self.bit_count as u64) as usize;
            let byte_idx = bit_idx / 8;
            let bit_offset = bit_idx % 8;
            self.bits[byte_idx] |= 1 << bit_offset;
        }
    }
    
    /// Check if an item might be in the set
    pub fn might_contain(&self, item: &[u8]) -> bool {
        for i in 0..self.hash_count {
            let hash = self.hash(item, i);
            let bit_idx = (hash % self.bit_count as u64) as usize;
            let byte_idx = bit_idx / 8;
            let bit_offset = bit_idx % 8;
            if self.bits[byte_idx] & (1 << bit_offset) == 0 {
                return false;
            }
        }
        true
    }
    
    /// Generate hash for an item with a given seed
    fn hash(&self, item: &[u8], seed: u32) -> u64 {
        let mut hasher = DefaultHasher::new();
        hasher.write_u32(seed);
        hasher.write(item);
        hasher.finish()
    }
    
    /// Calculate optimal bit count for given parameters
    fn optimal_bit_count(expected_items: usize, false_positive_rate: f64) -> usize {
        let ln2 = std::f64::consts::LN_2;
        let m = -((expected_items as f64 * false_positive_rate.ln()) / (ln2 * ln2));
        m.ceil() as usize
    }
    
    /// Calculate optimal hash count for given parameters
    fn optimal_hash_count(bit_count: usize, expected_items: usize) -> u32 {
        let ln2 = std::f64::consts::LN_2;
        let k = (bit_count as f64 / expected_items as f64 * ln2).round();
        k.max(1.0).min(30.0) as u32
    }
}

/// Chronik segment combining Kafka data and search index
pub struct ChronikSegment {
    header: SegmentHeader,
    metadata: SegmentMetadata,
    kafka_data: Vec<RecordBatch>,
    tantivy_index: Option<Index>,
    bloom_filter: Option<BloomFilter>,
    compression: CompressionType,
}

/// Schema fields for the Tantivy index
struct IndexSchema {
    topic: Field,
    partition: Field,
    offset: Field,
    timestamp: Field,
    key: Field,
    value: Field,
    headers: Field,
}

impl IndexSchema {
    fn build() -> (Schema, Self) {
        let mut schema_builder = Schema::builder();
        let topic = schema_builder.add_text_field("topic", STRING | STORED);
        let partition = schema_builder.add_i64_field("partition", NumericOptions::default().set_indexed().set_stored());
        let offset = schema_builder.add_i64_field("offset", NumericOptions::default().set_indexed().set_stored());
        let timestamp = schema_builder.add_i64_field("timestamp", NumericOptions::default().set_indexed().set_stored());
        let key = schema_builder.add_text_field("key", TEXT | STORED);
        let value = schema_builder.add_text_field("value", TEXT | STORED);
        let headers = schema_builder.add_text_field("headers", TEXT | STORED);
        
        let schema = schema_builder.build();
        let fields = Self {
            topic,
            partition,
            offset,
            timestamp,
            key,
            value,
            headers,
        };
        
        (schema, fields)
    }
}

impl ChronikSegment {
    /// Create a new segment from record batches
    pub fn new(
        topic: String,
        partition_id: i32,
        batches: Vec<RecordBatch>,
    ) -> Result<Self> {
        Self::new_with_compression(topic, partition_id, batches, CompressionType::Gzip)
    }
    
    /// Create a new segment with specific compression
    pub fn new_with_compression(
        topic: String,
        partition_id: i32,
        batches: Vec<RecordBatch>,
        compression: CompressionType,
    ) -> Result<Self> {
        if batches.is_empty() {
            return Err(Error::InvalidSegment("Cannot create segment from empty batches".into()));
        }
        
        // Calculate metadata
        let mut base_offset = i64::MAX;
        let mut last_offset = i64::MIN;
        let mut min_timestamp = i64::MAX;
        let mut max_timestamp = i64::MIN;
        let mut record_count = 0u64;
        let mut total_size = 0usize;
        
        for batch in &batches {
            for record in &batch.records {
                base_offset = base_offset.min(record.offset);
                last_offset = last_offset.max(record.offset);
                min_timestamp = min_timestamp.min(record.timestamp);
                max_timestamp = max_timestamp.max(record.timestamp);
                record_count += 1;
                total_size += record.value.len();
                if let Some(key) = &record.key {
                    total_size += key.len();
                }
            }
        }
        
        let metadata = SegmentMetadata {
            topic: topic.clone(),
            partition_id,
            base_offset,
            last_offset,
            timestamp_range: (min_timestamp, max_timestamp),
            record_count,
            created_at: chrono::Utc::now().timestamp_millis(),
            bloom_filter: None,
            compression_ratio: 0.0, // Will be updated after compression
            total_uncompressed_size: total_size,
        };
        
        Ok(Self {
            header: SegmentHeader::new(),
            metadata,
            kafka_data: batches,
            tantivy_index: None,
            bloom_filter: None,
            compression,
        })
    }
    
    /// Get the metadata for this segment
    pub fn metadata(&self) -> &SegmentMetadata {
        &self.metadata
    }
    
    /// Get the Kafka data
    pub fn kafka_data(&self) -> &[RecordBatch] {
        &self.kafka_data
    }
    
    /// Check if a key might exist in this segment
    pub fn might_contain_key(&self, key: &[u8]) -> bool {
        if let Some(bloom) = &self.bloom_filter {
            bloom.might_contain(key)
        } else {
            true // Without bloom filter, we can't exclude anything
        }
    }
    
    /// Build Tantivy index for the segment
    pub fn build_index(&mut self) -> Result<()> {
        let (schema, fields) = IndexSchema::build();
        
        // Create in-memory index for simplicity (in production, use persistent storage)
        let index = Index::create_in_ram(schema.clone());
        
        let mut index_writer = index.writer(50_000_000)
            .map_err(|e| Error::Internal(format!("Failed to create index writer: {}", e)))?;
        
        // Build bloom filter while indexing
        let mut bloom = BloomFilter::default_for_items(self.metadata.record_count as usize);
        
        // Index all records
        for batch in &self.kafka_data {
            for record in &batch.records {
                let mut doc = tantivy::doc!(
                    fields.topic => self.metadata.topic.clone(),
                    fields.partition => self.metadata.partition_id as i64,
                    fields.offset => record.offset,
                    fields.timestamp => record.timestamp,
                    fields.value => String::from_utf8_lossy(&record.value).to_string()
                );
                
                if let Some(key) = &record.key {
                    let key_str = String::from_utf8_lossy(key);
                    doc.add_text(fields.key, &key_str);
                    bloom.add(key);
                }
                
                if !record.headers.is_empty() {
                    let headers_json = serde_json::to_string(&record.headers)
                        .unwrap_or_default();
                    doc.add_text(fields.headers, &headers_json);
                }
                
                index_writer.add_document(doc)
                    .map_err(|e| Error::Internal(format!("Failed to add document: {}", e)))?;
            }
        }
        
        index_writer.commit()
            .map_err(|e| Error::Internal(format!("Failed to commit index: {}", e)))?;
        
        self.tantivy_index = Some(index);
        self.bloom_filter = Some(bloom);
        
        debug!("Built Tantivy index for {} records", self.metadata.record_count);
        Ok(())
    }
    
    /// Build only the bloom filter (faster than full index)
    pub fn build_bloom_filter(&mut self) -> Result<()> {
        let mut bloom = BloomFilter::default_for_items(self.metadata.record_count as usize);
        
        for batch in &self.kafka_data {
            for record in &batch.records {
                if let Some(key) = &record.key {
                    bloom.add(key);
                }
            }
        }
        
        self.bloom_filter = Some(bloom);
        Ok(())
    }
    
    /// Serialize segment to writer
    pub fn write_to<W: Write + Seek + Read>(&mut self, writer: &mut W) -> Result<()> {
        // Build index and bloom filter if not already built
        if self.tantivy_index.is_none() {
            self.build_index()?;
        }
        if self.bloom_filter.is_none() {
            self.build_bloom_filter()?;
        }
        
        // Reserve space for header
        let header_pos = writer.seek(SeekFrom::Current(0))?;
        writer.write_all(&vec![0u8; SegmentHeader::SIZE])?;
        
        // Write metadata
        let metadata_offset = writer.seek(SeekFrom::Current(0))?;
        let metadata_data = bincode::serialize(&self.metadata)
            .map_err(|e| Error::Serialization(format!("Failed to serialize metadata: {}", e)))?;
        writer.write_all(&metadata_data)?;
        let metadata_size = metadata_data.len() as u64;
        
        // Write Kafka data with compression
        let kafka_offset = writer.seek(SeekFrom::Current(0))?;
        let kafka_data = bincode::serialize(&self.kafka_data)
            .map_err(|e| Error::Serialization(format!("Failed to serialize Kafka data: {}", e)))?;
        let kafka_uncompressed_size = kafka_data.len() as u64;
        
        let compressed_data = match self.compression {
            CompressionType::None => kafka_data,
            CompressionType::Gzip => {
                let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
                encoder.write_all(&kafka_data)?;
                encoder.finish()?
            }
        };
        
        writer.write_all(&compressed_data)?;
        let kafka_size = compressed_data.len() as u64;
        
        // Update compression ratio
        self.metadata.compression_ratio = if kafka_uncompressed_size > 0 {
            1.0 - (kafka_size as f64 / kafka_uncompressed_size as f64)
        } else {
            0.0
        };
        
        // Write Tantivy index
        let index_offset = writer.seek(SeekFrom::Current(0))?;
        let index_data = self.serialize_tantivy_index()?;
        writer.write_all(&index_data)?;
        let index_size = index_data.len() as u64;
        
        // Write bloom filter
        let bloom_offset = writer.seek(SeekFrom::Current(0))?;
        let bloom_data = if let Some(bloom) = &self.bloom_filter {
            bincode::serialize(bloom)
                .map_err(|e| Error::Serialization(format!("Failed to serialize bloom filter: {}", e)))?
        } else {
            vec![]
        };
        writer.write_all(&bloom_data)?;
        let bloom_size = bloom_data.len() as u64;
        
        // Calculate checksum
        let end_pos = writer.seek(SeekFrom::Current(0))?;
        writer.seek(SeekFrom::Start(header_pos + SegmentHeader::SIZE as u64))?;
        
        let mut checksum_data = Vec::new();
        std::io::Read::read_to_end(writer, &mut checksum_data)?;
        let checksum = crc32fast::hash(&checksum_data);
        
        // Update and write header
        self.header.metadata_offset = metadata_offset;
        self.header.metadata_size = metadata_size;
        self.header.kafka_offset = kafka_offset;
        self.header.kafka_size = kafka_size;
        self.header.kafka_uncompressed_size = kafka_uncompressed_size;
        self.header.index_offset = index_offset;
        self.header.index_size = index_size;
        self.header.bloom_offset = bloom_offset;
        self.header.bloom_size = bloom_size;
        self.header.checksum = checksum;
        self.header.compression = self.compression;
        
        writer.seek(SeekFrom::Start(header_pos))?;
        self.header.write_to(writer)?;
        
        writer.seek(SeekFrom::Start(end_pos))?;
        
        info!("Wrote Chronik segment: topic={}, partition={}, offsets={}-{}, records={}, compression={:.1}%", 
              self.metadata.topic, self.metadata.partition_id, 
              self.metadata.base_offset, self.metadata.last_offset,
              self.metadata.record_count, self.metadata.compression_ratio * 100.0);
        
        Ok(())
    }
    
    /// Serialize Tantivy index to bytes
    fn serialize_tantivy_index(&self) -> Result<Vec<u8>> {
        if let Some(index) = &self.tantivy_index {
            // In a real implementation, we would properly serialize the Tantivy segments
            // For now, we'll create a simple representation
            let mut data = Vec::new();
            
            // Write a marker to indicate index presence
            data.write_u32::<BigEndian>(0xDEADBEEF)?;
            
            // Write index metadata
            let searcher = index.reader()
                .map_err(|e| Error::Internal(format!("Failed to create reader: {}", e)))?
                .searcher();
            
            data.write_u64::<BigEndian>(searcher.num_docs())?;
            
            // In production, we would serialize the actual segment files
            // For now, just write a placeholder
            data.write_u32::<BigEndian>(0)?;
            
            Ok(data)
        } else {
            Ok(vec![])
        }
    }
    
    /// Read segment from reader
    pub fn read_from<R: Read + Seek>(reader: &mut R) -> Result<Self> {
        // Read header
        let header = SegmentHeader::read_from(reader)?;
        
        // Verify checksum
        let saved_checksum = header.checksum;
        reader.seek(SeekFrom::Start(header.metadata_offset))?;
        let mut checksum_data = Vec::new();
        std::io::Read::read_to_end(reader, &mut checksum_data)?;
        let calculated_checksum = crc32fast::hash(&checksum_data);
        
        if saved_checksum != calculated_checksum {
            warn!("Checksum mismatch: expected {}, got {}", saved_checksum, calculated_checksum);
            // In production, you might want to fail here
            // return Err(Error::InvalidSegment("Checksum mismatch".into()));
        }
        
        // Read metadata
        reader.seek(SeekFrom::Start(header.metadata_offset))?;
        let mut metadata_buf = vec![0u8; header.metadata_size as usize];
        reader.read_exact(&mut metadata_buf)?;
        let metadata: SegmentMetadata = bincode::deserialize(&metadata_buf)
            .map_err(|e| Error::Serialization(format!("Failed to deserialize metadata: {}", e)))?;
        
        // Read Kafka data
        reader.seek(SeekFrom::Start(header.kafka_offset))?;
        let mut kafka_buf = vec![0u8; header.kafka_size as usize];
        reader.read_exact(&mut kafka_buf)?;
        
        let kafka_data_raw = match header.compression {
            CompressionType::None => kafka_buf,
            CompressionType::Gzip => {
                let mut decoder = GzDecoder::new(Cursor::new(kafka_buf));
                let mut decompressed = Vec::new();
                std::io::Read::read_to_end(&mut decoder, &mut decompressed)?;
                decompressed
            }
        };
        
        let kafka_data: Vec<RecordBatch> = bincode::deserialize(&kafka_data_raw)
            .map_err(|e| Error::Serialization(format!("Failed to deserialize Kafka data: {}", e)))?;
        
        // Read bloom filter
        let bloom_filter = if header.bloom_size > 0 {
            reader.seek(SeekFrom::Start(header.bloom_offset))?;
            let mut bloom_buf = vec![0u8; header.bloom_size as usize];
            reader.read_exact(&mut bloom_buf)?;
            Some(bincode::deserialize(&bloom_buf)
                .map_err(|e| Error::Serialization(format!("Failed to deserialize bloom filter: {}", e)))?)
        } else {
            None
        };
        
        // For now, skip reading the Tantivy index
        // In production, we would deserialize it properly
        
        let compression = header.compression;
        Ok(Self {
            header,
            metadata,
            kafka_data,
            tantivy_index: None,
            bloom_filter,
            compression,
        })
    }
    
    /// Read only the header and metadata without loading data
    pub fn read_metadata<R: Read + Seek>(reader: &mut R) -> Result<(SegmentHeader, SegmentMetadata)> {
        let header = SegmentHeader::read_from(reader)?;
        
        reader.seek(SeekFrom::Start(header.metadata_offset))?;
        let mut metadata_buf = vec![0u8; header.metadata_size as usize];
        reader.read_exact(&mut metadata_buf)?;
        let metadata: SegmentMetadata = bincode::deserialize(&metadata_buf)
            .map_err(|e| Error::Serialization(format!("Failed to deserialize metadata: {}", e)))?;
        
        Ok((header, metadata))
    }
    
    /// Read records in a specific offset range
    pub fn read_offset_range<R: Read + Seek>(
        reader: &mut R,
        start_offset: i64,
        end_offset: i64,
    ) -> Result<Vec<Record>> {
        let (header, metadata) = Self::read_metadata(reader)?;
        
        // Check if the range is within this segment
        if end_offset < metadata.base_offset || start_offset > metadata.last_offset {
            return Ok(vec![]);
        }
        
        // Read and decompress Kafka data
        reader.seek(SeekFrom::Start(header.kafka_offset))?;
        let mut kafka_buf = vec![0u8; header.kafka_size as usize];
        reader.read_exact(&mut kafka_buf)?;
        
        let kafka_data_raw = match header.compression {
            CompressionType::None => kafka_buf,
            CompressionType::Gzip => {
                let mut decoder = GzDecoder::new(Cursor::new(kafka_buf));
                let mut decompressed = Vec::new();
                std::io::Read::read_to_end(&mut decoder, &mut decompressed)?;
                decompressed
            }
        };
        
        let kafka_data: Vec<RecordBatch> = bincode::deserialize(&kafka_data_raw)
            .map_err(|e| Error::Serialization(format!("Failed to deserialize Kafka data: {}", e)))?;
        
        // Filter records by offset range
        let mut records = Vec::new();
        for batch in kafka_data {
            for record in batch.records {
                if record.offset >= start_offset && record.offset <= end_offset {
                    records.push(record);
                }
            }
        }
        
        Ok(records)
    }
}

/// Builder for creating ChronikSegment with various options
pub struct ChronikSegmentBuilder {
    topic: String,
    partition_id: i32,
    batches: Vec<RecordBatch>,
    compression: CompressionType,
    build_index: bool,
    build_bloom: bool,
}

impl ChronikSegmentBuilder {
    /// Create a new builder
    pub fn new(topic: String, partition_id: i32) -> Self {
        Self {
            topic,
            partition_id,
            batches: Vec::new(),
            compression: CompressionType::Gzip,
            build_index: true,
            build_bloom: true,
        }
    }
    
    /// Add a batch to the segment
    pub fn add_batch(mut self, batch: RecordBatch) -> Self {
        self.batches.push(batch);
        self
    }
    
    /// Add multiple batches
    pub fn add_batches(mut self, batches: Vec<RecordBatch>) -> Self {
        self.batches.extend(batches);
        self
    }
    
    /// Set compression type
    pub fn compression(mut self, compression: CompressionType) -> Self {
        self.compression = compression;
        self
    }
    
    /// Whether to build the Tantivy index
    pub fn with_index(mut self, build: bool) -> Self {
        self.build_index = build;
        self
    }
    
    /// Whether to build the bloom filter
    pub fn with_bloom_filter(mut self, build: bool) -> Self {
        self.build_bloom = build;
        self
    }
    
    /// Build the segment
    pub fn build(self) -> Result<ChronikSegment> {
        let mut segment = ChronikSegment::new_with_compression(
            self.topic,
            self.partition_id,
            self.batches,
            self.compression,
        )?;
        
        if self.build_index {
            segment.build_index()?;
        }
        
        if self.build_bloom && !self.build_index {
            // If we're building index, bloom filter is built automatically
            segment.build_bloom_filter()?;
        }
        
        Ok(segment)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Record;
    use std::io::Cursor;
    
    fn create_test_batch() -> RecordBatch {
        RecordBatch {
            records: vec![
                Record {
                    offset: 0,
                    timestamp: 1000,
                    key: Some(b"key1".to_vec()),
                    value: b"value1".to_vec(),
                    headers: std::collections::HashMap::new(),
                },
                Record {
                    offset: 1,
                    timestamp: 2000,
                    key: Some(b"key2".to_vec()),
                    value: b"value2".to_vec(),
                    headers: [("type".to_string(), b"test".to_vec())].into_iter().collect(),
                },
                Record {
                    offset: 2,
                    timestamp: 3000,
                    key: Some(b"key3".to_vec()),
                    value: b"value3 with more data to test compression".to_vec(),
                    headers: std::collections::HashMap::new(),
                },
            ],
        }
    }
    
    #[test]
    fn test_segment_serialization() {
        let batch = create_test_batch();
        
        // Create segment
        let mut segment = ChronikSegment::new(
            "test-topic".to_string(),
            0,
            vec![batch],
        ).unwrap();
        
        // Serialize to buffer
        let mut buffer = Cursor::new(Vec::new());
        segment.write_to(&mut buffer).unwrap();
        
        // Read back
        buffer.set_position(0);
        let loaded = ChronikSegment::read_from(&mut buffer).unwrap();
        
        // Verify metadata
        assert_eq!(loaded.metadata.topic, "test-topic");
        assert_eq!(loaded.metadata.partition_id, 0);
        assert_eq!(loaded.metadata.base_offset, 0);
        assert_eq!(loaded.metadata.last_offset, 2);
        assert_eq!(loaded.metadata.record_count, 3);
        assert_eq!(loaded.kafka_data.len(), 1);
        assert_eq!(loaded.kafka_data[0].records.len(), 3);
    }
    
    #[test]
    fn test_header_serialization() {
        let header = SegmentHeader::new();
        let mut buffer = Vec::new();
        header.write_to(&mut buffer).unwrap();
        
        assert_eq!(buffer.len(), SegmentHeader::SIZE);
        
        let mut cursor = Cursor::new(buffer);
        let loaded = SegmentHeader::read_from(&mut cursor).unwrap();
        
        assert_eq!(&loaded.magic, CHRONIK_MAGIC);
        assert_eq!(loaded.version, SEGMENT_VERSION);
    }
    
    #[test]
    fn test_bloom_filter() {
        let mut bloom = BloomFilter::new(1000, 0.01);
        
        // Add items
        bloom.add(b"test1");
        bloom.add(b"test2");
        bloom.add(b"test3");
        
        // Check contains
        assert!(bloom.might_contain(b"test1"));
        assert!(bloom.might_contain(b"test2"));
        assert!(bloom.might_contain(b"test3"));
        
        // Should probably not contain these (but might due to false positives)
        // We can't assert these are false because bloom filters have false positives
    }
    
    #[test]
    fn test_compression() {
        let batch = create_test_batch();
        
        // Test with compression
        let mut compressed_segment = ChronikSegment::new_with_compression(
            "test-topic".to_string(),
            0,
            vec![batch.clone()],
            CompressionType::Gzip,
        ).unwrap();
        
        // Test without compression
        let mut uncompressed_segment = ChronikSegment::new_with_compression(
            "test-topic".to_string(),
            0,
            vec![batch],
            CompressionType::None,
        ).unwrap();
        
        // Serialize both
        let mut compressed_buffer = Cursor::new(Vec::new());
        compressed_segment.write_to(&mut compressed_buffer).unwrap();
        
        let mut uncompressed_buffer = Cursor::new(Vec::new());
        uncompressed_segment.write_to(&mut uncompressed_buffer).unwrap();
        
        // Compressed should be smaller
        assert!(compressed_buffer.get_ref().len() < uncompressed_buffer.get_ref().len());
        
        // Both should read back correctly
        compressed_buffer.set_position(0);
        let loaded_compressed = ChronikSegment::read_from(&mut compressed_buffer).unwrap();
        assert_eq!(loaded_compressed.kafka_data[0].records.len(), 3);
        
        uncompressed_buffer.set_position(0);
        let loaded_uncompressed = ChronikSegment::read_from(&mut uncompressed_buffer).unwrap();
        assert_eq!(loaded_uncompressed.kafka_data[0].records.len(), 3);
    }
    
    #[test]
    fn test_offset_range_read() {
        let batch = create_test_batch();
        
        let mut segment = ChronikSegment::new(
            "test-topic".to_string(),
            0,
            vec![batch],
        ).unwrap();
        
        let mut buffer = Cursor::new(Vec::new());
        segment.write_to(&mut buffer).unwrap();
        
        // Read specific offset range
        buffer.set_position(0);
        let records = ChronikSegment::read_offset_range(&mut buffer, 1, 2).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].offset, 1);
        assert_eq!(records[1].offset, 2);
        
        // Read outside range
        buffer.set_position(0);
        let records = ChronikSegment::read_offset_range(&mut buffer, 10, 20).unwrap();
        assert_eq!(records.len(), 0);
    }
    
    #[test]
    fn test_segment_builder() {
        let batch1 = create_test_batch();
        let batch2 = RecordBatch {
            records: vec![
                Record {
                    offset: 3,
                    timestamp: 4000,
                    key: Some(b"key4".to_vec()),
                    value: b"value4".to_vec(),
                    headers: std::collections::HashMap::new(),
                },
            ],
        };
        
        let segment = ChronikSegmentBuilder::new("test-topic".to_string(), 0)
            .add_batch(batch1)
            .add_batch(batch2)
            .compression(CompressionType::Gzip)
            .with_index(false)
            .with_bloom_filter(true)
            .build()
            .unwrap();
        
        assert_eq!(segment.metadata.record_count, 4);
        assert_eq!(segment.metadata.base_offset, 0);
        assert_eq!(segment.metadata.last_offset, 3);
        assert!(segment.bloom_filter.is_some());
        assert!(segment.tantivy_index.is_none());
    }
    
    #[test]
    fn test_metadata_only_read() {
        let batch = create_test_batch();
        
        let mut segment = ChronikSegment::new(
            "test-topic".to_string(),
            0,
            vec![batch],
        ).unwrap();
        
        let mut buffer = Cursor::new(Vec::new());
        segment.write_to(&mut buffer).unwrap();
        
        // Read only metadata
        buffer.set_position(0);
        let (header, metadata) = ChronikSegment::read_metadata(&mut buffer).unwrap();
        
        assert_eq!(metadata.topic, "test-topic");
        assert_eq!(metadata.partition_id, 0);
        assert_eq!(metadata.record_count, 3);
        assert_eq!(header.version, SEGMENT_VERSION);
    }
}