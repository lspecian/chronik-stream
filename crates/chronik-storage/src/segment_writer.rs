//! Segment writer for creating and managing segments.

use crate::{RecordBatch, Segment, SegmentBuilder, ObjectStoreTrait, ObjectStoreFactory, ObjectStoreConfig, ObjectStoreBackend};
use chronik_common::{Result, types::{SegmentId, TopicPartition}};
use chronik_common::metadata::traits::SegmentMetadata;
use chrono::{Utc, DateTime};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
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
    /// Enable dual storage (raw Kafka + indexed records)
    /// If false, only stores raw Kafka batches for protocol compatibility
    pub enable_dual_storage: bool,
    /// Max segment age before rotation (in seconds)
    pub max_segment_age_secs: u64,
    /// Retention period for segments (in seconds)
    pub retention_period_secs: u64,
    /// Enable automatic cleanup of old segments
    pub enable_cleanup: bool,
}

impl Default for SegmentWriterConfig {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from("/tmp/chronik-segments"),
            compression_codec: "none".to_string(),
            max_segment_size: 1024 * 1024 * 1024, // 1GB
            enable_dual_storage: true,
            max_segment_age_secs: 3600, // 1 hour
            retention_period_secs: 7 * 24 * 3600, // 7 days
            enable_cleanup: true,
        }
    }
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
    /// Stores raw Kafka batches for v2 format
    has_raw_kafka: bool,
    /// Time when the segment was created
    created_at: Instant,
}

/// Segment writer for managing segment creation
pub struct SegmentWriter {
    config: SegmentWriterConfig,
    object_store: Box<dyn ObjectStoreTrait>,
    active_segments: Arc<RwLock<HashMap<(String, i32), ActiveSegment>>>,
    /// Track the last offset written for each partition to ensure continuity
    last_offsets: Arc<RwLock<HashMap<(String, i32), i64>>>,
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
            last_offsets: Arc::new(RwLock::new(HashMap::new())),
        })
    }
    
    /// Write a batch of records in dual format (raw Kafka + indexed)
    /// Returns the segment metadata if a segment was uploaded
    pub async fn write_dual_format(
        &self,
        topic: &str,
        partition: i32,
        raw_kafka_batch: &[u8],
        indexed_batch: RecordBatch,
    ) -> Result<Option<SegmentMetadata>> {
        let key = (topic.to_string(), partition);
        let mut segments = self.active_segments.write().await;
        
        // Get or create active segment
        let is_new_segment = !segments.contains_key(&key);
        
        // If we need a new segment, determine the correct base_offset
        let new_segment_base_offset = if is_new_segment {
            // Check if we have a previous offset for this partition
            let last_offsets = self.last_offsets.read().await;
            if let Some(&last_offset) = last_offsets.get(&key) {
                // Continue from where we left off
                last_offset + 1
            } else if let Some(first_record) = indexed_batch.records.first() {
                // First segment for this partition, use the first record's offset
                first_record.offset
            } else {
                0
            }
        } else {
            0 // Won't be used since we're not creating a new segment
        };
        
        let active = segments.entry(key.clone()).or_insert_with(|| {
            let segment_id = SegmentId(Uuid::new_v4());
            let builder = SegmentBuilder::new();
            
            tracing::info!(
                "Creating new segment for {}-{} with base_offset={} (continuity from previous segment)",
                topic, partition, new_segment_base_offset
            );
            
            ActiveSegment {
                builder,
                id: segment_id,
                topic: topic.to_string(),
                partition,
                base_offset: new_segment_base_offset,
                last_offset: new_segment_base_offset,
                timestamp_min: indexed_batch.records.first().map(|r| r.timestamp).unwrap_or(0),
                timestamp_max: indexed_batch.records.first().map(|r| r.timestamp).unwrap_or(0),
                record_count: 0,
                current_size: 0,
                has_raw_kafka: true, // v2 format with dual storage
                created_at: Instant::now(),
            }
        });
        
        // If this is an existing segment and we're adding new records,
        // make sure the base_offset doesn't change
        if !is_new_segment && !indexed_batch.records.is_empty() {
            tracing::debug!(
                "Appending to existing segment {}-{}: base_offset={}, new_records_start={}",
                topic, partition, active.base_offset, indexed_batch.records.first().unwrap().offset
            );
        }
        
        // Update segment metadata
        for record in &indexed_batch.records {
            active.last_offset = record.offset;
            active.timestamp_min = active.timestamp_min.min(record.timestamp);
            active.timestamp_max = active.timestamp_max.max(record.timestamp);
            active.record_count += 1;
        }
        
        // Update the last offset tracker for this partition
        if !indexed_batch.records.is_empty() {
            let mut last_offsets = self.last_offsets.write().await;
            let last_record_offset = indexed_batch.records.last().unwrap().offset;
            last_offsets.insert(key.clone(), last_record_offset);
        }
        
        tracing::info!(
            "Segment {}-{} now contains {} records (offsets {}-{})",
            topic, partition, active.record_count, active.base_offset, active.last_offset
        );
        
        // Add raw Kafka batch data
        active.builder.add_raw_kafka_batch(raw_kafka_batch);
        
        // Conditionally add indexed data based on configuration
        let indexed_size = if self.config.enable_dual_storage {
            // Serialize indexed batch and add to segment
            let batch_bytes = indexed_batch.encode()?;
            active.builder.add_indexed_record(&batch_bytes);
            batch_bytes.len() as u64
        } else {
            0
        };
        
        // Update size
        active.current_size += raw_kafka_batch.len() as u64 + indexed_size;
        
        // Check if we should rotate the segment (size-based or time-based)
        let should_rotate = active.current_size >= self.config.max_segment_size ||
            active.created_at.elapsed().as_secs() >= self.config.max_segment_age_secs;
        
        if should_rotate {
            let segment_data = segments.remove(&key).unwrap();
            
            // Create metadata for segment builder (using types::SegmentMetadata)
            let segment_metadata = chronik_common::types::SegmentMetadata {
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
                object_key: format!("{}/{}/{}.segment",
                    segment_data.topic,
                    segment_data.partition,
                    segment_data.id.0
                ),
                created_at: Utc::now(),
            };
            
            // Build and upload segment
            let built_segment = segment_data.builder
                .with_metadata(segment_metadata)
                .build()?;
            self.upload_segment(built_segment).await?;
            
            // Create metadata for metadata store (using traits::SegmentMetadata)
            let metadata = SegmentMetadata {
                segment_id: segment_data.id.0.to_string(),
                topic: segment_data.topic.clone(),
                partition: segment_data.partition as u32,
                start_offset: segment_data.base_offset,
                end_offset: segment_data.last_offset,
                size: segment_data.current_size as i64,
                record_count: segment_data.record_count as i64,
                path: format!("{}/{}/{}.segment",
                    segment_data.topic,
                    segment_data.partition,
                    segment_data.id.0
                ),
                created_at: Utc::now(),
            };
            
            return Ok(Some(metadata));
        }
        
        Ok(None)
    }
    
    /// Write a batch of records (legacy single format - for backwards compatibility)
    /// Returns the segment metadata if a segment was uploaded
    pub async fn write_batch(
        &self,
        topic: &str,
        partition: i32,
        batch: RecordBatch,
    ) -> Result<Option<SegmentMetadata>> {
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
                has_raw_kafka: false, // v1 format - indexed only
                created_at: Instant::now(),
            }
        });
        
        // Update segment metadata
        for record in &batch.records {
            active.last_offset = record.offset;
            active.timestamp_min = active.timestamp_min.min(record.timestamp);
            active.timestamp_max = active.timestamp_max.max(record.timestamp);
            active.record_count += 1;
        }
        
        // Serialize batch and add to segment (for backwards compatibility, store as indexed only)
        let batch_bytes = batch.encode()?;
        active.builder.add_indexed_record(&batch_bytes);
        active.current_size += batch_bytes.len() as u64;
        
        // Check if we should rotate the segment (size-based or time-based)
        let should_rotate = active.current_size >= self.config.max_segment_size ||
            active.created_at.elapsed().as_secs() >= self.config.max_segment_age_secs;
        
        if should_rotate {
            let segment_data = segments.remove(&key).unwrap();
            
            // Create metadata for segment builder (using types::SegmentMetadata)
            let segment_metadata = chronik_common::types::SegmentMetadata {
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
                object_key: format!("{}/{}/{}.segment",
                    segment_data.topic,
                    segment_data.partition,
                    segment_data.id.0
                ),
                created_at: Utc::now(),
            };
            
            // Build and upload segment
            let built_segment = segment_data.builder
                .with_metadata(segment_metadata)
                .build()?;
            self.upload_segment(built_segment).await?;
            
            // Create metadata for metadata store (using traits::SegmentMetadata)
            let metadata = SegmentMetadata {
                segment_id: segment_data.id.0.to_string(),
                topic: segment_data.topic.clone(),
                partition: segment_data.partition as u32,
                start_offset: segment_data.base_offset,
                end_offset: segment_data.last_offset,
                size: segment_data.current_size as i64,
                record_count: segment_data.record_count as i64,
                path: format!("{}/{}/{}.segment",
                    segment_data.topic,
                    segment_data.partition,
                    segment_data.id.0
                ),
                created_at: Utc::now(),
            };
            
            return Ok(Some(metadata));
        }
        
        Ok(None)
    }
    
    /// Flush all active segments
    /// Returns metadata for all flushed segments
    pub async fn flush_all(&self) -> Result<Vec<SegmentMetadata>> {
        let mut segments = self.active_segments.write().await;
        let mut flushed_metadata = Vec::new();
        
        for (_, segment_data) in segments.drain() {
            // Create metadata for segment builder (using types::SegmentMetadata)
            let segment_metadata = chronik_common::types::SegmentMetadata {
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
                object_key: format!("{}/{}/{}.segment",
                    segment_data.topic,
                    segment_data.partition,
                    segment_data.id.0
                ),
                created_at: Utc::now(),
            };
            
            // Build and upload segment
            let built_segment = segment_data.builder
                .with_metadata(segment_metadata)
                .build()?;
            self.upload_segment(built_segment).await?;
            
            // Create metadata for metadata store (using traits::SegmentMetadata)
            let metadata = SegmentMetadata {
                segment_id: segment_data.id.0.to_string(),
                topic: segment_data.topic.clone(),
                partition: segment_data.partition as u32,
                start_offset: segment_data.base_offset,
                end_offset: segment_data.last_offset,
                size: segment_data.current_size as i64,
                record_count: segment_data.record_count as i64,
                path: format!("{}/{}/{}.segment",
                    segment_data.topic,
                    segment_data.partition,
                    segment_data.id.0
                ),
                created_at: Utc::now(),
            };
            
            flushed_metadata.push(metadata);
        }
        
        Ok(flushed_metadata)
    }
    
    /// Upload a segment to object storage
    async fn upload_segment(&self, segment: Segment) -> Result<()> {
        let key = format!(
            "{}/{}/{}.segment",
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
    
    /// Clean up old segments based on retention policy
    pub async fn cleanup_old_segments(&self, topic: &str, partition: i32) -> Result<u32> {
        let mut deleted_count = 0;
        
        // List all segments for this topic-partition
        let prefix = format!("{}/{}/", topic, partition);
        let segment_metadata_list = self.object_store.list(&prefix).await?;
        
        let retention_cutoff = Utc::now() - chrono::Duration::seconds(self.config.retention_period_secs as i64);
        
        for segment_metadata in segment_metadata_list {
            // Check if segment is older than retention period
            // Use last_modified time from ObjectMetadata (u64 timestamp)
            let segment_age = Utc::now().timestamp() as u64 - segment_metadata.last_modified;
            
            if segment_age > self.config.retention_period_secs {
                // Delete old segment
                if self.config.enable_cleanup {
                    self.object_store.delete(&segment_metadata.key).await?;
                    deleted_count += 1;
                    tracing::info!("Deleted old segment: {}", segment_metadata.key);
                }
            }
        }
        
        Ok(deleted_count)
    }
    
    /// Start a background task to periodically rotate and cleanup segments
    pub fn start_background_tasks(self: Arc<Self>) {
        // Start rotation checker task
        let writer_clone = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(30)); // Check every 30 seconds
            
            loop {
                interval.tick().await;
                
                // Check all active segments for time-based rotation
                let segments = writer_clone.active_segments.read().await;
                let mut segments_to_rotate = Vec::new();
                
                for (key, segment) in segments.iter() {
                    if segment.created_at.elapsed().as_secs() >= writer_clone.config.max_segment_age_secs {
                        segments_to_rotate.push(key.clone());
                    }
                }
                drop(segments); // Release read lock
                
                // Rotate aged segments
                for key in segments_to_rotate {
                    tracing::info!("Time-based rotation for topic {} partition {}", key.0, key.1);
                    // Force rotation by flushing
                    // In production, you'd want a more elegant rotation mechanism
                    let mut segments = writer_clone.active_segments.write().await;
                    if let Some(segment_data) = segments.remove(&key) {
                        // Upload the segment
                        let segment_metadata = chronik_common::types::SegmentMetadata {
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
                            object_key: format!("{}/{}/{}.segment",
                                segment_data.topic,
                                segment_data.partition,
                                segment_data.id.0
                            ),
                            created_at: Utc::now(),
                        };
                        
                        let built_segment = segment_data.builder
                            .with_metadata(segment_metadata)
                            .build()
                            .unwrap();
                        writer_clone.upload_segment(built_segment).await.unwrap();
                    }
                }
            }
        });
        
        // Start cleanup task if enabled
        if self.config.enable_cleanup {
            let writer_clone = self.clone();
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_secs(3600)); // Run cleanup every hour
                
                loop {
                    interval.tick().await;
                    
                    tracing::info!("Running segment cleanup task");
                    
                    // In production, you'd want to get list of all topics and partitions
                    // For now, this is a simplified example
                    // You would need to integrate with metadata store to get all topics
                    
                    // Example cleanup for a known topic (would need to be dynamic in production)
                    match writer_clone.cleanup_old_segments("example-topic", 0).await {
                        Ok(count) => {
                            if count > 0 {
                                tracing::info!("Cleaned up {} old segments", count);
                            }
                        }
                        Err(e) => {
                            tracing::error!("Segment cleanup failed: {:?}", e);
                        }
                    }
                }
            });
        }
    }
}

