//! Segment writer for creating and managing segments.

use crate::{RecordBatch, Segment, SegmentBuilder, ObjectStoreTrait, ObjectStoreFactory, ObjectStoreConfig, ObjectStoreBackend};
use chronik_common::{Result, types::{SegmentId, SegmentMetadata, TopicPartition}};
use chrono::Utc;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use uuid::Uuid;

/// Segment writer configuration
#[derive(Debug, Clone)]
pub struct SegmentWriterConfig {
    /// Local data directory
    pub data_dir: PathBuf,
    /// Compression codec
    pub compression_codec: String,
    /// Max segment size in bytes
    pub max_segment_size: u64,
}

/// Active segment info
struct ActiveSegment {
    builder: SegmentBuilder,
    id: SegmentId,
    topic: String,
    partition: i32,
    base_offset: i64,
    last_offset: i64,
    timestamp_min: i64,
    timestamp_max: i64,
    record_count: u64,
    current_size: u64,
}

/// Segment writer for managing segment creation
pub struct SegmentWriter {
    config: SegmentWriterConfig,
    object_store: Box<dyn ObjectStoreTrait>,
    active_segments: Arc<RwLock<HashMap<(String, i32), ActiveSegment>>>,
}

impl SegmentWriter {
    /// Create a new segment writer
    pub async fn new(config: SegmentWriterConfig) -> Result<Self> {
        // Create object store for local storage
        let object_store_config = ObjectStoreConfig {
            backend: ObjectStoreBackend::Local { 
                path: config.data_dir.to_string_lossy().to_string() 
            },
            bucket: "chronik".to_string(),
            prefix: None,
            ..Default::default()
        };
        
        let object_store = ObjectStoreFactory::create(object_store_config).await?;
        
        Ok(Self {
            config,
            object_store,
            active_segments: Arc::new(RwLock::new(HashMap::new())),
        })
    }
    
    /// Write a batch of records
    pub async fn write_batch(
        &self,
        topic: &str,
        partition: i32,
        batch: RecordBatch,
    ) -> Result<()> {
        let key = (topic.to_string(), partition);
        let mut segments = self.active_segments.write().await;
        
        // Get or create active segment
        let active = segments.entry(key.clone()).or_insert_with(|| {
            let segment_id = SegmentId(Uuid::new_v4());
            let builder = SegmentBuilder::new();
            let first_record = batch.records.first().unwrap();
            
            ActiveSegment {
                builder,
                id: segment_id,
                topic: topic.to_string(),
                partition,
                base_offset: first_record.offset,
                last_offset: first_record.offset,
                timestamp_min: first_record.timestamp,
                timestamp_max: first_record.timestamp,
                record_count: 0,
                current_size: 0,
            }
        });
        
        // Update segment metadata
        for record in &batch.records {
            active.last_offset = record.offset;
            active.timestamp_min = active.timestamp_min.min(record.timestamp);
            active.timestamp_max = active.timestamp_max.max(record.timestamp);
            active.record_count += 1;
        }
        
        // Serialize batch and add to segment
        let batch_bytes = batch.encode()?;
        active.builder.add_kafka_data(&batch_bytes);
        active.current_size += batch_bytes.len() as u64;
        
        // Check if we should rotate the segment
        if active.current_size >= self.config.max_segment_size {
            let segment_data = segments.remove(&key).unwrap();
            
            // Create metadata
            let metadata = SegmentMetadata {
                id: segment_data.id,
                topic_partition: TopicPartition {
                    topic: segment_data.topic.clone(),
                    partition: segment_data.partition,
                },
                base_offset: segment_data.base_offset,
                last_offset: segment_data.last_offset,
                timestamp_min: segment_data.timestamp_min,
                timestamp_max: segment_data.timestamp_max,
                size_bytes: segment_data.current_size,
                record_count: segment_data.record_count,
                object_key: "".to_string(), // Will be set during upload
                created_at: Utc::now(),
            };
            
            // Build and upload segment
            let built_segment = segment_data.builder
                .with_metadata(metadata)
                .build()?;
            self.upload_segment(built_segment).await?;
        }
        
        Ok(())
    }
    
    /// Flush all active segments
    pub async fn flush_all(&self) -> Result<()> {
        let mut segments = self.active_segments.write().await;
        
        for (_, segment_data) in segments.drain() {
            // Create metadata
            let metadata = SegmentMetadata {
                id: segment_data.id,
                topic_partition: TopicPartition {
                    topic: segment_data.topic.clone(),
                    partition: segment_data.partition,
                },
                base_offset: segment_data.base_offset,
                last_offset: segment_data.last_offset,
                timestamp_min: segment_data.timestamp_min,
                timestamp_max: segment_data.timestamp_max,
                size_bytes: segment_data.current_size,
                record_count: segment_data.record_count,
                object_key: "".to_string(), // Will be set during upload
                created_at: Utc::now(),
            };
            
            // Build and upload segment
            let built_segment = segment_data.builder
                .with_metadata(metadata)
                .build()?;
            self.upload_segment(built_segment).await?;
        }
        
        Ok(())
    }
    
    /// Upload a segment to object storage
    async fn upload_segment(&self, segment: Segment) -> Result<()> {
        let key = format!(
            "segments/{}/{}/{}.segment",
            segment.metadata.topic_partition.topic,
            segment.metadata.topic_partition.partition,
            segment.metadata.id.0
        );
        
        // Serialize segment
        let data = segment.serialize()?;
        
        // Upload to object store
        self.object_store.put(&key, data).await?;
        
        tracing::info!(
            "Uploaded segment {} for {}:{} with {} records ({} bytes)",
            segment.metadata.id.0,
            segment.metadata.topic_partition.topic,
            segment.metadata.topic_partition.partition,
            segment.metadata.record_count,
            segment.metadata.size_bytes
        );
        
        Ok(())
    }
}

