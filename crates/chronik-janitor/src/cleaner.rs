//! Segment cleaner for removing old data.

use chronik_common::{Result, Error, Uuid};
use chronik_common::metadata::MetadataStore;
use chronik_storage::ObjectStore;
use std::time::Duration;
use std::sync::Arc;
use tokio::time::interval;

/// Cleanup policy
#[derive(Debug, Clone)]
pub struct CleanupPolicy {
    /// Retention period for segments
    pub retention_period: Duration,
    /// Maximum storage size (in bytes)
    pub max_storage_size: Option<u64>,
    /// Delete empty segments
    pub delete_empty_segments: bool,
    /// Cleanup interval
    pub cleanup_interval: Duration,
}

impl Default for CleanupPolicy {
    fn default() -> Self {
        Self {
            retention_period: Duration::from_secs(7 * 24 * 60 * 60), // 7 days
            max_storage_size: None,
            delete_empty_segments: true,
            cleanup_interval: Duration::from_secs(60 * 60), // 1 hour
        }
    }
}

/// Segment cleaner
pub struct SegmentCleaner {
    storage: Arc<dyn ObjectStore>,
    metadata_store: Arc<dyn MetadataStore>,
    policy: CleanupPolicy,
}

impl SegmentCleaner {
    /// Create a new segment cleaner
    pub fn new(storage: Arc<dyn ObjectStore>, metadata_store: Arc<dyn MetadataStore>, policy: CleanupPolicy) -> Self {
        Self {
            storage,
            metadata_store,
            policy,
        }
    }
    
    /// Start the cleaner
    pub async fn start(self: Arc<Self>) -> Result<()> {
        let cleaner = self.clone();
        tokio::spawn(async move {
            if let Err(e) = cleaner.run_cleanup_loop().await {
                tracing::error!("Cleanup loop failed: {}", e);
            }
        });
        
        Ok(())
    }
    
    /// Run the cleanup loop
    async fn run_cleanup_loop(&self) -> Result<()> {
        let mut interval = interval(self.policy.cleanup_interval);
        
        loop {
            interval.tick().await;
            
            if let Err(e) = self.cleanup_old_segments().await {
                tracing::error!("Failed to cleanup segments: {}", e);
            }
            
            if self.policy.max_storage_size.is_some() {
                if let Err(e) = self.cleanup_by_size().await {
                    tracing::error!("Failed to cleanup by size: {}", e);
                }
            }
        }
    }
    
    /// Cleanup old segments based on retention
    pub async fn cleanup_old_segments(&self) -> Result<()> {
        let retention_cutoff = chrono::Utc::now() - chrono::Duration::from_std(self.policy.retention_period)
            .map_err(|e| Error::Internal(format!("Invalid duration: {}", e)))?;
        
        // Get all topics
        let topics = self.metadata_store.list_topics().await?;
        let mut segments_to_delete = Vec::new();
        
        // Find old segments across all topics
        for topic in topics {
            let segments = self.metadata_store.list_segments(&topic.name).await?;
            
            for segment in segments {
                if segment.created_at < retention_cutoff {
                    segments_to_delete.push((topic.name.clone(), segment));
                }
            }
        }
        
        tracing::info!("Found {} segments to cleanup", segments_to_delete.len());
        
        for (topic_name, segment) in segments_to_delete {
            // Delete from storage
            let path = format!("topics/{}/segments/{}", topic_name, segment.segment_id);
            if let Err(e) = self.storage.delete(&path).await {
                tracing::error!("Failed to delete segment {}: {}", path, e);
                continue;
            }
            
            // Delete from metadata store
            if let Err(e) = self.metadata_store.delete_segment(&topic_name, segment.segment_id).await {
                tracing::error!("Failed to delete segment metadata {}: {}", segment.segment_id, e);
                continue;
            }
            
            tracing::info!("Deleted segment {} for topic {} partition {}", 
                segment.segment_id, topic_name, segment.partition);
        }
        
        Ok(())
    }
    
    /// Cleanup segments to stay under size limit
    async fn cleanup_by_size(&self) -> Result<()> {
        let max_size = match self.policy.max_storage_size {
            Some(size) => size,
            None => return Ok(()),
        };
        
        // Calculate total storage size
        let topics = self.metadata_store.list_topics().await?;
        let mut all_segments = Vec::new();
        let mut total_size = 0u64;
        
        for topic in topics {
            let segments = self.metadata_store.list_segments(&topic.name).await?;
            for segment in segments {
                total_size += segment.size;
                all_segments.push((topic.name.clone(), segment));
            }
        }
        
        if total_size <= max_size {
            return Ok(());
        }
        
        let excess_size = total_size - max_size;
        tracing::info!("Storage size {} exceeds limit {}, need to free {} bytes", 
            total_size, max_size, excess_size);
        
        // Sort segments by creation time (oldest first)
        all_segments.sort_by(|a, b| a.1.created_at.cmp(&b.1.created_at));
        
        let mut freed_size = 0u64;
        
        for (topic_name, segment) in all_segments {
            if freed_size >= excess_size {
                break;
            }
            
            // Delete from storage
            let path = format!("topics/{}/segments/{}", topic_name, segment.segment_id);
            if let Err(e) = self.storage.delete(&path).await {
                tracing::error!("Failed to delete segment {}: {}", path, e);
                continue;
            }
            
            // Delete from metadata store
            if let Err(e) = self.metadata_store.delete_segment(&topic_name, segment.segment_id).await {
                tracing::error!("Failed to delete segment metadata {}: {}", segment.segment_id, e);
                continue;
            }
            
            freed_size += segment.size;
            
            tracing::info!("Deleted segment {} to free {} bytes", segment.segment_id, segment.size);
        }
        
        Ok(())
    }
    
    /// Cleanup empty segments
    pub async fn cleanup_empty_segments(&self) -> Result<()> {
        if !self.policy.delete_empty_segments {
            return Ok(());
        }
        
        // Get all topics
        let topics = self.metadata_store.list_topics().await?;
        let mut empty_segments = Vec::new();
        
        // Find empty segments across all topics
        for topic in topics {
            let segments = self.metadata_store.list_segments(&topic.name).await?;
            
            for segment in segments {
                if segment.size == 0 || segment.record_count == 0 {
                    empty_segments.push((topic.name.clone(), segment));
                }
            }
        }
        
        for (topic_name, segment) in empty_segments {
            // Delete from storage
            let path = format!("topics/{}/segments/{}", topic_name, segment.segment_id);
            if let Err(e) = self.storage.delete(&path).await {
                tracing::error!("Failed to delete empty segment {}: {}", path, e);
                continue;
            }
            
            // Delete from metadata store
            if let Err(e) = self.metadata_store.delete_segment(&topic_name, segment.segment_id).await {
                tracing::error!("Failed to delete segment metadata {}: {}", segment.segment_id, e);
                continue;
            }
            
            tracing::info!("Deleted empty segment {}", segment.segment_id);
        }
        
        Ok(())
    }
}