//! Extended Tansu segment format with vector search support.
//!
//! This module extends the base segment format to support future vector search capabilities
//! while maintaining backward compatibility with existing segments.

use crate::{
    RecordBatch, Record, SegmentMetadata as BaseSegmentMetadata,
    VectorIndexData, VectorIndex, VectorIndexFactory, DistanceMetric,
    chronik_segment::BloomFilter,
};
use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use bytes::{BytesMut, BufMut};
use std::collections::{BTreeMap, HashMap};
use tracing::{info, instrument};

/// Version of the extended segment format
const EXTENDED_SEGMENT_VERSION: u32 = 2;

/// Magic number for extended segments
const EXTENDED_MAGIC: &[u8] = b"CHREXT02";

/// Extended segment metadata with vector search information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtendedSegmentMetadata {
    /// Base metadata (compatible with v1)
    #[serde(flatten)]
    pub base: BaseSegmentMetadata,
    /// Vector field configurations
    pub vector_fields: Vec<VectorFieldConfig>,
    /// Hybrid search statistics
    pub hybrid_stats: Option<HybridStats>,
}

/// Configuration for a vector field
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorFieldConfig {
    /// Field name in the record
    pub field_name: String,
    /// Number of dimensions
    pub dimensions: usize,
    /// Distance metric
    pub metric: DistanceMetric,
    /// Index type (e.g., "hnsw")
    pub index_type: String,
    /// Index-specific parameters
    pub index_params: serde_json::Value,
}

/// Statistics for hybrid search
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HybridStats {
    /// Number of documents with vectors
    pub vector_doc_count: u64,
    /// Average vector dimensions across fields
    pub avg_dimensions: f32,
    /// Fields with both text and vector data
    pub hybrid_fields: Vec<String>,
}

/// Extended Tansu segment with vector search support
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtendedTansuSegment {
    /// Format version
    pub version: u32,
    /// Kafka record batches
    pub kafka_data: Vec<RecordBatch>,
    /// Tantivy text index data
    pub tantivy_index: Option<Vec<u8>>,
    /// Vector indices by field name
    pub vector_indices: BTreeMap<String, VectorIndexData>,
    /// Extended metadata
    pub metadata: ExtendedSegmentMetadata,
    /// Bloom filter for key lookups
    pub bloom_filter: Option<BloomFilter>,
}

/// Builder for extended segments
pub struct ExtendedSegmentBuilder {
    version: u32,
    records: Vec<Record>,
    vector_configs: Vec<VectorFieldConfig>,
    vector_indices: BTreeMap<String, Box<dyn VectorIndex>>,
    build_text_index: bool,
    build_bloom_filter: bool,
}

impl ExtendedSegmentBuilder {
    /// Create a new builder
    pub fn new() -> Self {
        Self {
            version: EXTENDED_SEGMENT_VERSION,
            records: Vec::new(),
            vector_configs: Vec::new(),
            vector_indices: BTreeMap::new(),
            build_text_index: true,
            build_bloom_filter: true,
        }
    }
    
    /// Add records to the segment
    pub fn add_records(mut self, records: Vec<Record>) -> Self {
        self.records.extend(records);
        self
    }
    
    /// Add a vector field configuration
    pub fn add_vector_field(mut self, config: VectorFieldConfig) -> Result<Self> {
        // Create the vector index
        let index = VectorIndexFactory::create(
            &config.index_type,
            config.dimensions,
            config.metric,
            config.index_params.clone(),
        )?;
        
        self.vector_indices.insert(config.field_name.clone(), index);
        self.vector_configs.push(config);
        Ok(self)
    }
    
    /// Enable/disable text index building
    pub fn with_text_index(mut self, build: bool) -> Self {
        self.build_text_index = build;
        self
    }
    
    /// Enable/disable bloom filter building
    pub fn with_bloom_filter(mut self, build: bool) -> Self {
        self.build_bloom_filter = build;
        self
    }
    
    /// Build the extended segment
    #[instrument(skip(self))]
    pub fn build(mut self) -> Result<ExtendedTansuSegment> {
        if self.records.is_empty() {
            return Err(anyhow!("Cannot build segment from empty records"));
        }
        
        info!("Building extended segment with {} records", self.records.len());
        
        // Sort records by timestamp for better compression
        self.records.sort_by_key(|r| r.timestamp);
        
        // Extract vector data and build indices
        self.build_vector_indices()?;
        
        // Build Kafka batches
        let batches = self.build_kafka_batches();
        
        // Build text index (placeholder)
        let tantivy_index = if self.build_text_index {
            Some(self.build_text_index_data()?)
        } else {
            None
        };
        
        // Build bloom filter
        let bloom_filter = if self.build_bloom_filter {
            Some(self.build_bloom_filter_data())
        } else {
            None
        };
        
        // Create metadata
        let metadata = self.create_metadata();
        
        // Serialize vector indices
        let mut vector_indices_data = BTreeMap::new();
        for (field_name, index) in self.vector_indices {
            let config = self.vector_configs.iter()
                .find(|c| c.field_name == field_name)
                .ok_or_else(|| anyhow!("Missing config for field {}", field_name))?;
            
            let data = VectorIndexData {
                index_type: config.index_type.clone(),
                dimensions: index.dimensions(),
                metric: config.metric,
                data: index.serialize()?,
                metadata: serde_json::json!({
                    "count": index.count(),
                    "params": config.index_params,
                }),
            };
            
            vector_indices_data.insert(field_name, data);
        }
        
        Ok(ExtendedTansuSegment {
            version: self.version,
            kafka_data: batches,
            tantivy_index,
            vector_indices: vector_indices_data,
            metadata,
            bloom_filter,
        })
    }
    
    /// Extract vectors from records and build indices
    fn build_vector_indices(&mut self) -> Result<()> {
        for (i, record) in self.records.iter().enumerate() {
            // Parse record value as JSON to extract vector fields
            if let Ok(json_value) = serde_json::from_slice::<serde_json::Value>(&record.value) {
                for (field_name, index) in &mut self.vector_indices {
                    if let Some(vector_value) = json_value.get(field_name) {
                        if let Some(vector) = vector_value.as_array() {
                            let float_vector: Result<Vec<f32>> = vector.iter()
                                .map(|v| v.as_f64()
                                    .map(|f| f as f32)
                                    .ok_or_else(|| anyhow!("Invalid vector element")))
                                .collect();
                            
                            if let Ok(float_vec) = float_vector {
                                index.add_vector(i as u64, &float_vec)?;
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }
    
    /// Build Kafka batches from records
    fn build_kafka_batches(&self) -> Vec<RecordBatch> {
        // Simple implementation: one batch for all records
        vec![RecordBatch {
            records: self.records.clone(),
        }]
    }
    
    /// Build text index data (placeholder)
    fn build_text_index_data(&self) -> Result<Vec<u8>> {
        // In a real implementation, this would build a Tantivy index
        Ok(vec![0u8; 100]) // Placeholder
    }
    
    /// Build bloom filter for key lookups
    fn build_bloom_filter_data(&self) -> BloomFilter {
        let mut bloom = BloomFilter::default_for_items(self.records.len());
        for record in &self.records {
            if let Some(key) = &record.key {
                bloom.add(key);
            }
        }
        bloom
    }
    
    /// Create extended metadata
    fn create_metadata(&self) -> ExtendedSegmentMetadata {
        let base_offset = self.records.first().map(|r| r.offset).unwrap_or(0);
        let last_offset = self.records.last().map(|r| r.offset).unwrap_or(0);
        let first_timestamp = self.records.first().map(|r| r.timestamp).unwrap_or(0);
        let last_timestamp = self.records.last().map(|r| r.timestamp).unwrap_or(0);
        
        // Count documents with vectors
        let mut vector_doc_count = 0;
        let mut total_dimensions = 0;
        let mut hybrid_fields = Vec::new();
        
        for (field_name, index_data) in &self.vector_indices {
            let count = index_data.count();
            if count > 0 {
                vector_doc_count = vector_doc_count.max(count);
                total_dimensions += index_data.dimensions() * count;
                hybrid_fields.push(field_name.clone());
            }
        }
        
        let avg_dimensions = if vector_doc_count > 0 {
            total_dimensions as f32 / vector_doc_count as f32
        } else {
            0.0
        };
        
        ExtendedSegmentMetadata {
            base: BaseSegmentMetadata {
                topic: "unknown".to_string(), // Would be provided in real implementation
                partition_id: 0,
                base_offset,
                last_offset,
                timestamp_range: (first_timestamp, last_timestamp),
                record_count: self.records.len() as u64,
                created_at: chrono::Utc::now().timestamp(),
                bloom_filter: None, // Stored separately in extended format
                compression_ratio: 0.5, // Placeholder
                total_uncompressed_size: self.records.iter()
                    .map(|r| r.key.as_ref().map(|k| k.len()).unwrap_or(0) + r.value.len())
                    .sum(),
            },
            vector_fields: self.vector_configs.clone(),
            hybrid_stats: Some(HybridStats {
                vector_doc_count: vector_doc_count as u64,
                avg_dimensions,
                hybrid_fields,
            }),
        }
    }
}

impl ExtendedTansuSegment {
    /// Check if this is an extended segment by examining the magic number
    pub fn is_extended_format(data: &[u8]) -> bool {
        data.len() >= EXTENDED_MAGIC.len() && &data[..EXTENDED_MAGIC.len()] == EXTENDED_MAGIC
    }
    
    /// Get version without full deserialization
    pub fn peek_version(data: &[u8]) -> Result<u32> {
        if data.len() < EXTENDED_MAGIC.len() + 4 {
            return Err(anyhow!("Data too small for extended segment"));
        }
        
        let version_bytes = &data[EXTENDED_MAGIC.len()..EXTENDED_MAGIC.len() + 4];
        Ok(u32::from_be_bytes(version_bytes.try_into()?))
    }
    
    /// Serialize the segment
    pub fn serialize(&self) -> Result<Vec<u8>> {
        let mut buf = BytesMut::new();
        
        // Write magic and version
        buf.put_slice(EXTENDED_MAGIC);
        buf.put_u32(self.version);
        
        // Create a serializable version of the segment
        #[derive(Serialize)]
        struct SerializableSegment {
            kafka_data: Vec<SerializableRecordBatch>,
            tantivy_index: Option<Vec<u8>>,
            vector_indices: BTreeMap<String, SerializableVectorIndexData>,
            metadata: SerializableExtendedMetadata,
            bloom_filter_bits: Option<Vec<u8>>,
            bloom_filter_params: Option<(usize, usize)>, // num_hashes, num_bits
        }
        
        #[derive(Serialize)]
        struct SerializableExtendedMetadata {
            // Base metadata fields (flattened)
            topic: String,
            partition_id: i32,
            base_offset: i64,
            last_offset: i64,
            timestamp_range: (i64, i64),
            record_count: u64,
            created_at: i64,
            compression_ratio: f64,
            total_uncompressed_size: usize,
            // Extended fields
            vector_fields: Vec<SerializableVectorFieldConfig>,
            hybrid_stats: Option<HybridStats>,
        }
        
        #[derive(Serialize)]
        struct SerializableVectorFieldConfig {
            field_name: String,
            dimensions: usize,
            metric: DistanceMetric,
            index_type: String,
            index_params_json: String, // Store as JSON string
        }
        
        #[derive(Serialize)]
        struct SerializableVectorIndexData {
            index_type: String,
            dimensions: usize,
            metric: DistanceMetric,
            data: Vec<u8>,
            metadata_json: String, // Store as JSON string instead of Value
        }
        
        #[derive(Serialize)]
        struct SerializableRecordBatch {
            records: Vec<SerializableRecord>,
        }
        
        #[derive(Serialize)]
        struct SerializableRecord {
            offset: i64,
            timestamp: i64,
            key: Option<Vec<u8>>,
            value: Vec<u8>,
            headers: HashMap<String, Vec<u8>>,
        }
        
        // Convert to serializable format
        let kafka_data_serializable: Vec<SerializableRecordBatch> = self.kafka_data.iter()
            .map(|batch| {
                SerializableRecordBatch {
                    records: batch.records.iter().map(|record| {
                        SerializableRecord {
                            offset: record.offset,
                            timestamp: record.timestamp,
                            key: record.key.clone(),
                            value: record.value.clone(),
                            headers: record.headers.clone(),
                        }
                    }).collect(),
                }
            })
            .collect();
        
        // Convert vector indices to serializable format
        let mut serializable_vector_indices = BTreeMap::new();
        for (name, index_data) in &self.vector_indices {
            serializable_vector_indices.insert(
                name.clone(),
                SerializableVectorIndexData {
                    index_type: index_data.index_type.clone(),
                    dimensions: index_data.dimensions,
                    metric: index_data.metric,
                    data: index_data.data.clone(),
                    metadata_json: index_data.metadata.to_string(),
                },
            );
        }
        
        // Convert metadata to serializable format
        let serializable_metadata = SerializableExtendedMetadata {
            // Base fields
            topic: self.metadata.base.topic.clone(),
            partition_id: self.metadata.base.partition_id,
            base_offset: self.metadata.base.base_offset,
            last_offset: self.metadata.base.last_offset,
            timestamp_range: self.metadata.base.timestamp_range,
            record_count: self.metadata.base.record_count,
            created_at: self.metadata.base.created_at,
            compression_ratio: self.metadata.base.compression_ratio,
            total_uncompressed_size: self.metadata.base.total_uncompressed_size,
            // Extended fields
            vector_fields: self.metadata.vector_fields.iter()
                .map(|vf| SerializableVectorFieldConfig {
                    field_name: vf.field_name.clone(),
                    dimensions: vf.dimensions,
                    metric: vf.metric,
                    index_type: vf.index_type.clone(),
                    index_params_json: vf.index_params.to_string(),
                })
                .collect(),
            hybrid_stats: self.metadata.hybrid_stats.clone(),
        };
        
        // Since BloomFilter fields are private, we'll serialize it directly
        let bloom_filter_data = self.bloom_filter.as_ref().map(|bf| {
            // Serialize the entire BloomFilter using bincode
            bincode::serialize(bf).unwrap_or_default()
        });
        
        let serializable = SerializableSegment {
            kafka_data: kafka_data_serializable,
            tantivy_index: self.tantivy_index.clone(),
            vector_indices: serializable_vector_indices,
            metadata: serializable_metadata,
            bloom_filter_bits: bloom_filter_data,
            bloom_filter_params: None, // Not needed when serializing entire filter
        };
        
        // Serialize and write the data
        // Use bincode with bounded size for better error messages
        let config = bincode::config();
        let data = config.serialize(&serializable)
            .map_err(|e| anyhow!("Failed to serialize extended segment: {}", e))?;
        buf.put_slice(&data);
        Ok(buf.freeze().to_vec())
    }
    
    /// Deserialize the segment
    pub fn deserialize(data: &[u8]) -> Result<Self> {
        if !Self::is_extended_format(data) {
            return Err(anyhow!("Not an extended segment format"));
        }
        
        let version = Self::peek_version(data)?;
        let data_start = EXTENDED_MAGIC.len() + 4;
        
        // Define the same serializable structures for deserialization
        #[derive(Deserialize)]
        struct SerializableSegment {
            kafka_data: Vec<SerializableRecordBatch>,
            tantivy_index: Option<Vec<u8>>,
            vector_indices: BTreeMap<String, SerializableVectorIndexData>,
            metadata: SerializableExtendedMetadata,
            bloom_filter_bits: Option<Vec<u8>>,
            bloom_filter_params: Option<(usize, usize)>,
        }
        
        #[derive(Deserialize)]
        struct SerializableExtendedMetadata {
            topic: String,
            partition_id: i32,
            base_offset: i64,
            last_offset: i64,
            timestamp_range: (i64, i64),
            record_count: u64,
            created_at: i64,
            compression_ratio: f64,
            total_uncompressed_size: usize,
            vector_fields: Vec<SerializableVectorFieldConfig>,
            hybrid_stats: Option<HybridStats>,
        }
        
        #[derive(Deserialize)]
        struct SerializableVectorFieldConfig {
            field_name: String,
            dimensions: usize,
            metric: DistanceMetric,
            index_type: String,
            index_params_json: String,
        }
        
        #[derive(Deserialize)]
        struct SerializableVectorIndexData {
            index_type: String,
            dimensions: usize,
            metric: DistanceMetric,
            data: Vec<u8>,
            metadata_json: String,
        }
        
        #[derive(Deserialize)]
        struct SerializableRecordBatch {
            records: Vec<SerializableRecord>,
        }
        
        #[derive(Deserialize)]
        struct SerializableRecord {
            offset: i64,
            timestamp: i64,
            key: Option<Vec<u8>>,
            value: Vec<u8>,
            headers: HashMap<String, Vec<u8>>,
        }
        
        let serializable: SerializableSegment = bincode::deserialize(&data[data_start..])?;
        
        // Convert vector indices back to original format
        let mut vector_indices = BTreeMap::new();
        for (name, serializable_data) in serializable.vector_indices {
            let metadata: serde_json::Value = serde_json::from_str(&serializable_data.metadata_json)
                .unwrap_or(serde_json::json!({}));
            
            vector_indices.insert(
                name,
                VectorIndexData {
                    index_type: serializable_data.index_type,
                    dimensions: serializable_data.dimensions,
                    metric: serializable_data.metric,
                    data: serializable_data.data,
                    metadata,
                },
            );
        }
        
        // Convert metadata back to original format
        let metadata = ExtendedSegmentMetadata {
            base: BaseSegmentMetadata {
                topic: serializable.metadata.topic,
                partition_id: serializable.metadata.partition_id,
                base_offset: serializable.metadata.base_offset,
                last_offset: serializable.metadata.last_offset,
                timestamp_range: serializable.metadata.timestamp_range,
                record_count: serializable.metadata.record_count,
                created_at: serializable.metadata.created_at,
                bloom_filter: None, // Stored separately
                compression_ratio: serializable.metadata.compression_ratio,
                total_uncompressed_size: serializable.metadata.total_uncompressed_size,
            },
            vector_fields: serializable.metadata.vector_fields.into_iter()
                .map(|vf| VectorFieldConfig {
                    field_name: vf.field_name,
                    dimensions: vf.dimensions,
                    metric: vf.metric,
                    index_type: vf.index_type,
                    index_params: serde_json::from_str(&vf.index_params_json)
                        .unwrap_or(serde_json::json!({})),
                })
                .collect(),
            hybrid_stats: serializable.metadata.hybrid_stats,
        };
        
        // Convert back to the original format
        let kafka_data: Vec<RecordBatch> = serializable.kafka_data.into_iter()
            .map(|batch| {
                RecordBatch {
                    records: batch.records.into_iter().map(|record| {
                        Record {
                            offset: record.offset,
                            timestamp: record.timestamp,
                            key: record.key,
                            value: record.value,
                            headers: record.headers,
                        }
                    }).collect(),
                }
            })
            .collect();
        
        // Reconstruct bloom filter if present
        let bloom_filter = serializable.bloom_filter_bits
            .and_then(|data| bincode::deserialize::<BloomFilter>(&data).ok());
        
        Ok(Self {
            version,
            kafka_data,
            tantivy_index: serializable.tantivy_index,
            vector_indices,
            metadata,
            bloom_filter,
        })
    }
    
    /// Get a vector index for a field
    pub fn get_vector_index(&self, field_name: &str) -> Result<Box<dyn VectorIndex>> {
        let index_data = self.vector_indices.get(field_name)
            .ok_or_else(|| anyhow!("No vector index for field {}", field_name))?;
        
        VectorIndexFactory::from_data(index_data)
    }
    
    /// Check if segment has vector indices
    pub fn has_vector_indices(&self) -> bool {
        !self.vector_indices.is_empty()
    }
    
    /// Get vector field names
    pub fn vector_fields(&self) -> Vec<&str> {
        self.vector_indices.keys().map(|s| s.as_str()).collect()
    }
}

/// Migration utilities for upgrading segments
pub struct SegmentMigration;

impl SegmentMigration {
    /// Check if a segment needs migration
    pub fn needs_migration(data: &[u8]) -> bool {
        // If it's not extended format, it needs migration
        !ExtendedTansuSegment::is_extended_format(data)
    }
    
    /// Migrate a v1 segment to extended format
    pub fn migrate_v1_to_extended(
        _v1_data: &[u8],
        _vector_configs: Vec<VectorFieldConfig>,
    ) -> Result<ExtendedTansuSegment> {
        // This would deserialize v1 format and convert to extended
        // For now, return an error as placeholder
        Err(anyhow!("V1 to extended migration not yet implemented"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    fn create_test_records() -> Vec<Record> {
        vec![
            Record {
                offset: 0,
                timestamp: 1000,
                key: Some(b"key1".to_vec()),
                value: serde_json::json!({
                    "text": "hello world",
                    "embedding": [0.1, 0.2, 0.3, 0.4],
                }).to_string().into_bytes(),
                headers: Default::default(),
            },
            Record {
                offset: 1,
                timestamp: 2000,
                key: Some(b"key2".to_vec()),
                value: serde_json::json!({
                    "text": "foo bar",
                    "embedding": [0.5, 0.6, 0.7, 0.8],
                }).to_string().into_bytes(),
                headers: Default::default(),
            },
        ]
    }
    
    #[test]
    fn test_extended_segment_builder() {
        let records = create_test_records();
        
        let config = VectorFieldConfig {
            field_name: "embedding".to_string(),
            dimensions: 4,
            metric: DistanceMetric::Cosine,
            index_type: "hnsw".to_string(),
            index_params: serde_json::json!({"m": 16, "ef": 200}),
        };
        
        let segment = ExtendedSegmentBuilder::new()
            .add_records(records)
            .add_vector_field(config).unwrap()
            .build().unwrap();
        
        assert_eq!(segment.version, EXTENDED_SEGMENT_VERSION);
        assert!(segment.vector_indices.contains_key("embedding"));
        assert_eq!(segment.metadata.vector_fields.len(), 1);
    }
    
    #[test]
    fn test_segment_serialization() {
        let records = create_test_records();
        let config = VectorFieldConfig {
            field_name: "embedding".to_string(),
            dimensions: 4,
            metric: DistanceMetric::Euclidean,
            index_type: "hnsw".to_string(),
            index_params: serde_json::json!({}),
        };
        
        let segment = ExtendedSegmentBuilder::new()
            .add_records(records)
            .add_vector_field(config).unwrap()
            .build().unwrap();
        
        let serialized = segment.serialize().unwrap();
        assert!(ExtendedTansuSegment::is_extended_format(&serialized));
        
        let deserialized = ExtendedTansuSegment::deserialize(&serialized).unwrap();
        assert_eq!(deserialized.version, segment.version);
        assert_eq!(deserialized.vector_fields(), vec!["embedding"]);
    }
    
    #[test]
    fn test_vector_index_retrieval() {
        let records = create_test_records();
        let config = VectorFieldConfig {
            field_name: "embedding".to_string(),
            dimensions: 4,
            metric: DistanceMetric::Cosine,
            index_type: "hnsw".to_string(),
            index_params: serde_json::json!({}),
        };
        
        let segment = ExtendedSegmentBuilder::new()
            .add_records(records)
            .add_vector_field(config).unwrap()
            .build().unwrap();
        
        let index = segment.get_vector_index("embedding").unwrap();
        assert_eq!(index.dimensions(), 4);
        assert_eq!(index.count(), 2);
        
        // Test search
        let results = index.search(&[0.1, 0.2, 0.3, 0.4], 1).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, 0); // First record should be closest
    }
}