//! Log compaction for topic cleanup.

use chronik_common::{Result, Error, Uuid};
use chronik_common::metadata::{MetadataStore, SegmentMetadata};
use chronik_storage::object_store::ObjectStore;
use chronik_storage::{Record, RecordBatch};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::interval;

/// Compaction policy
#[derive(Debug, Clone)]
pub struct CompactionPolicy {
    /// Minimum segment age before compaction
    pub min_segment_age: Duration,
    /// Minimum number of segments per partition
    pub min_segments_per_partition: usize,
    /// Target segment size after compaction
    pub target_segment_size: u64,
    /// Compaction interval
    pub compaction_interval: Duration,
    /// Topics to compact (None means all)
    pub topics: Option<Vec<String>>,
}

impl Default for CompactionPolicy {
    fn default() -> Self {
        Self {
            min_segment_age: Duration::from_secs(60 * 60), // 1 hour
            min_segments_per_partition: 2,
            target_segment_size: 100 * 1024 * 1024, // 100 MB
            compaction_interval: Duration::from_secs(6 * 60 * 60), // 6 hours
            topics: None,
        }
    }
}

/// Log compactor
pub struct LogCompactor {
    storage: Arc<dyn ObjectStore>,
    metadata_store: Arc<dyn MetadataStore>,
    policy: CompactionPolicy,
}

impl LogCompactor {
    /// Create a new log compactor
    pub fn new(storage: Arc<dyn ObjectStore>, metadata_store: Arc<dyn MetadataStore>, policy: CompactionPolicy) -> Self {
        Self {
            storage,
            metadata_store,
            policy,
        }
    }
    
    /// Start the compactor
    pub async fn start(self: Arc<Self>) -> Result<()> {
        let compactor = self.clone();
        tokio::spawn(async move {
            if let Err(e) = compactor.run_compaction_loop().await {
                tracing::error!("Compaction loop failed: {}", e);
            }
        });
        
        Ok(())
    }
    
    /// Run the compaction loop
    async fn run_compaction_loop(&self) -> Result<()> {
        let mut interval = interval(self.policy.compaction_interval);
        
        loop {
            interval.tick().await;
            
            if let Err(e) = self.run_compaction().await {
                tracing::error!("Compaction failed: {}", e);
            }
        }
    }
    
    /// Run compaction
    pub async fn run_compaction(&self) -> Result<()> {
        let age_cutoff = chrono::Utc::now() - chrono::Duration::from_std(self.policy.min_segment_age)
            .map_err(|e| Error::Internal(format!("Invalid duration: {}", e)))?;
        
        // Get topics to compact
        let topics = match &self.policy.topics {
            Some(topics) => topics.clone(),
            None => {
                // Get all topics
                self.metadata_store.list_topics().await?
                    .into_iter()
                    .map(|t| t.name)
                    .collect()
            }
        };
        
        for topic in topics {
            if let Err(e) = self.compact_topic(&topic, age_cutoff).await {
                tracing::error!("Failed to compact topic {}: {}", topic, e);
            }
        }
        
        Ok(())
    }
    
    /// Compact a single topic
    async fn compact_topic(&self, topic: &str, age_cutoff: chrono::DateTime<chrono::Utc>) -> Result<()> {
        // Get topic metadata
        let topic_metadata = match self.metadata_store.get_topic(topic).await? {
            Some(metadata) => metadata,
            None => {
                tracing::warn!("Topic {} not found", topic);
                return Ok(());
            }
        };
        
        // Process each partition
        for partition in 0..topic_metadata.config.partition_count {
            // Get segments for partition
            let segments = self.metadata_store.list_segments(topic, Some(partition)).await?;
            let mut partition_segments: Vec<SegmentMetadata> = segments
                .into_iter()
                .filter(|s| s.partition == partition && s.created_at < age_cutoff)
                .collect();
            
            // Sort by start offset
            partition_segments.sort_by_key(|s| s.start_offset);
            
            if partition_segments.len() < self.policy.min_segments_per_partition {
                continue;
            }
            
            // Group segments for compaction
            let mut groups = Vec::new();
            let mut current_group = Vec::new();
            let mut current_size = 0u64;
            
            for segment in partition_segments {
                current_size += segment.size as u64;
                current_group.push(segment);
                
                if current_size >= self.policy.target_segment_size {
                    groups.push(current_group);
                    current_group = Vec::new();
                    current_size = 0;
                }
            }
            
            if current_group.len() > 1 {
                groups.push(current_group);
            }
            
            // Compact each group
            for group in groups {
                if group.len() < 2 {
                    continue;
                }
                
                if let Err(e) = self.compact_segment_group(topic, partition as i32, group).await {
                    tracing::error!("Failed to compact segment group: {}", e);
                }
            }
        }
        
        Ok(())
    }
    
    /// Compact a group of segments
    async fn compact_segment_group(
        &self,
        topic: &str,
        partition: i32,
        segments: Vec<SegmentMetadata>,
    ) -> Result<()> {
        tracing::info!("Compacting {} segments for topic {} partition {}", 
            segments.len(), topic, partition);
        
        // Read all records and deduplicate by key
        let mut records_by_key: HashMap<Option<Vec<u8>>, Record> = HashMap::new();
        let mut min_offset = i64::MAX;
        let mut max_offset = i64::MIN;
        
        for segment in &segments {
            let path = format!("topics/{}/segments/{}", topic, segment.segment_id);
            min_offset = min_offset.min(segment.start_offset);
            max_offset = max_offset.max(segment.end_offset);
            
            // Read segment data
            let data = self.storage.get(&path).await?;
            let data_bytes = data.to_vec();
            
            // Decode records
            if let Ok(batch) = RecordBatch::decode(&data_bytes) {
                for record in batch.records {
                    // Keep only the latest record for each key
                    records_by_key.insert(record.key.clone(), record);
                }
            }
        }
        
        // Create new compacted segment
        let compacted_records: Vec<Record> = records_by_key.into_values().collect();
        
        if compacted_records.is_empty() {
            // All records were duplicates, delete segments
            for segment in segments {
                let path = format!("topics/{}/segments/{}", topic, segment.segment_id);
                self.storage.delete(&path).await?;
                self.metadata_store.delete_segment(topic, &segment.segment_id).await?;
            }
            return Ok(());
        }
        
        // Encode compacted records
        let batch = RecordBatch {
            records: compacted_records,
        };
        let data = batch.encode()?;
        
        // Generate new segment ID
        let new_segment_id = Uuid::new_v4();
        let path = format!("topics/{}/segments/{}", topic, new_segment_id);
        
        // Write compacted segment
        self.storage.put(&path, data.clone().into()).await?;
        
        // Create new segment metadata
        let new_segment = SegmentMetadata {
            segment_id: new_segment_id.to_string(),
            topic: topic.to_string(),
            partition: partition as u32,
            start_offset: min_offset,
            end_offset: max_offset,
            size: data.len() as i64,
            record_count: batch.records.len() as i64,
            path: format!("topics/{}/segments/{}", topic, new_segment_id),
            created_at: chrono::Utc::now(),
        };
        
        // Save new segment metadata
        self.metadata_store.persist_segment_metadata(new_segment).await?;
        
        // Delete old segments
        for segment in segments {
            let path = format!("topics/{}/segments/{}", topic, segment.segment_id);
            self.storage.delete(&path).await?;
            self.metadata_store.delete_segment(topic, &segment.segment_id).await?;
        }
        
        tracing::info!("Compacted segments into {} with {} records", new_segment_id, batch.records.len());
        
        Ok(())
    }
}