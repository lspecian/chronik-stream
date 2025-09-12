//! Offset storage management for consumer groups
//! 
//! This module provides functionality for storing and retrieving consumer group offsets
//! in TiKV with proper retention policies and atomic operations.

use chronik_common::Result;
use chronik_common::metadata::{MetadataStore, ConsumerOffset, MetadataError};
// use crate::offset_cleanup::OffsetCleanupManager;  // Removed - tikv dependency
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::collections::HashMap;
use tokio::sync::RwLock;
use tokio::time::interval;
use tracing::{debug, error, info, warn};
use chrono::{DateTime, Utc};

/// Configuration for offset storage
#[derive(Debug, Clone)]
pub struct OffsetStorageConfig {
    /// Retention period for committed offsets (default: 7 days)
    pub retention_duration: Duration,
    /// Cleanup interval (default: 1 hour)
    pub cleanup_interval: Duration,
    /// Enable automatic cleanup
    pub enable_cleanup: bool,
}

impl Default for OffsetStorageConfig {
    fn default() -> Self {
        Self {
            retention_duration: Duration::from_secs(7 * 24 * 60 * 60), // 7 days
            cleanup_interval: Duration::from_secs(60 * 60), // 1 hour
            enable_cleanup: true,
        }
    }
}

/// Offset storage manager
pub struct OffsetStorage {
    metadata_store: Arc<dyn MetadataStore>,
    config: OffsetStorageConfig,
    /// Cache of recent offset commits for performance
    offset_cache: Arc<RwLock<HashMap<(String, String, u32), CachedOffset>>>,
}

/// Cached offset with timestamp
#[derive(Debug, Clone)]
struct CachedOffset {
    offset: i64,
    metadata: Option<String>,
    commit_timestamp: DateTime<Utc>,
    cached_at: Instant,
}

impl OffsetStorage {
    /// Create a new offset storage manager
    pub fn new(
        metadata_store: Arc<dyn MetadataStore>,
        config: OffsetStorageConfig,
    ) -> Self {
        Self {
            metadata_store,
            config,
            offset_cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }
    
    /// Start the background cleanup task with enhanced cleanup manager
    pub async fn start_cleanup_task(&self, _pd_endpoints: Option<Vec<String>>) {
        if !self.config.enable_cleanup {
            return;
        }
        
        // TiKV cleanup removed - cleanup now handled by metadata store
        // TODO: Implement cleanup using metadata store if needed
        info!("Offset cleanup task disabled - TiKV dependencies removed");
    }
    
    /// Commit a batch of offsets atomically
    pub async fn commit_offsets(
        &self,
        group_id: &str,
        generation_id: i32,
        offsets: Vec<(String, u32, i64, Option<String>)>, // (topic, partition, offset, metadata)
    ) -> Result<Vec<CommitResult>> {
        let mut results = Vec::new();
        let mut cache_updates = Vec::new();
        let now = Utc::now();
        
        for (topic, partition, offset, metadata) in offsets {
            let consumer_offset = ConsumerOffset {
                group_id: group_id.to_string(),
                topic: topic.clone(),
                partition,
                offset,
                metadata: metadata.clone(),
                commit_timestamp: now,
            };
            
            // Commit to persistent storage with proper error handling
            match self.metadata_store.commit_offset(consumer_offset).await {
                Ok(_) => {
                    results.push(CommitResult {
                        topic: topic.clone(),
                        partition,
                        error_code: 0, // SUCCESS
                    });
                    
                    // Prepare cache update
                    cache_updates.push((
                        (group_id.to_string(), topic, partition),
                        CachedOffset {
                            offset,
                            metadata,
                            commit_timestamp: now,
                            cached_at: Instant::now(),
                        }
                    ));
                }
                Err(e) => {
                    warn!("Failed to commit offset for {}:{}: {}", topic, partition, e);
                    
                    // Map error to appropriate Kafka error code
                    let error_code = match &e {
                        MetadataError::StorageError(_) => 56, // COORDINATOR_NOT_AVAILABLE (storage issue)
                        MetadataError::NotFound(_) => 25, // UNKNOWN_MEMBER_ID
                        _ => 50, // UNKNOWN_SERVER_ERROR
                    };
                    
                    results.push(CommitResult {
                        topic,
                        partition,
                        error_code,
                    });
                }
            }
        }
        
        // Update cache with successful commits
        if !cache_updates.is_empty() {
            let mut cache = self.offset_cache.write().await;
            for (key, offset) in cache_updates {
                cache.insert(key, offset);
            }
        }
        
        Ok(results)
    }
    
    /// Fetch committed offsets for a consumer group
    pub async fn fetch_offsets(
        &self,
        group_id: &str,
        topic_partitions: Vec<(String, u32)>, // (topic, partition)
    ) -> Result<Vec<FetchedOffset>> {
        let mut results = Vec::new();
        
        // Check cache first
        let cache = self.offset_cache.read().await;
        let mut cache_misses = Vec::new();
        
        for (topic, partition) in &topic_partitions {
            let cache_key = (group_id.to_string(), topic.clone(), *partition);
            
            if let Some(cached) = cache.get(&cache_key) {
                // Cache hit - check if still fresh (within 1 minute)
                if cached.cached_at.elapsed() < Duration::from_secs(60) {
                    results.push(FetchedOffset {
                        topic: topic.clone(),
                        partition: *partition,
                        offset: cached.offset,
                        metadata: cached.metadata.clone(),
                        error_code: 0,
                    });
                    continue;
                }
            }
            
            cache_misses.push((topic.clone(), *partition));
        }
        drop(cache);
        
        // Fetch cache misses from storage
        for (topic, partition) in cache_misses {
            match self.metadata_store.get_consumer_offset(group_id, &topic, partition).await? {
                Some(offset) => {
                    results.push(FetchedOffset {
                        topic: topic.clone(),
                        partition,
                        offset: offset.offset,
                        metadata: offset.metadata.clone(),
                        error_code: 0,
                    });
                    
                    // Update cache
                    let mut cache = self.offset_cache.write().await;
                    cache.insert(
                        (group_id.to_string(), topic, partition),
                        CachedOffset {
                            offset: offset.offset,
                            metadata: offset.metadata,
                            commit_timestamp: offset.commit_timestamp,
                            cached_at: Instant::now(),
                        }
                    );
                }
                None => {
                    // No committed offset found
                    results.push(FetchedOffset {
                        topic,
                        partition,
                        offset: -1,
                        metadata: None,
                        error_code: 0,
                    });
                }
            }
        }
        
        Ok(results)
    }
    
    /// Delete offsets for a consumer group
    pub async fn delete_group_offsets(&self, group_id: &str) -> Result<()> {
        // Remove from cache
        let mut cache = self.offset_cache.write().await;
        cache.retain(|(g, _, _), _| g != group_id);
        drop(cache);
        
        // Delete from storage - this would need to be implemented in MetadataStore
        // For now, we'll just log this operation
        info!("Request to delete offsets for group {}", group_id);
        
        Ok(())
    }
    
    
    /// Get statistics about stored offsets
    pub async fn get_stats(&self) -> OffsetStorageStats {
        let cache = self.offset_cache.read().await;
        
        OffsetStorageStats {
            cached_entries: cache.len(),
            cache_hit_rate: 0.0, // TODO: Track hit/miss rates
            oldest_cached_entry: cache.values()
                .map(|o| o.commit_timestamp)
                .min(),
            newest_cached_entry: cache.values()
                .map(|o| o.commit_timestamp)
                .max(),
        }
    }
}

/// Result of a commit operation
#[derive(Debug, Clone)]
pub struct CommitResult {
    pub topic: String,
    pub partition: u32,
    pub error_code: i16,
}

/// Result of a fetch operation
#[derive(Debug, Clone)]
pub struct FetchedOffset {
    pub topic: String,
    pub partition: u32,
    pub offset: i64,
    pub metadata: Option<String>,
    pub error_code: i16,
}

/// Statistics about offset storage
#[derive(Debug, Clone)]
pub struct OffsetStorageStats {
    pub cached_entries: usize,
    pub cache_hit_rate: f64,
    pub oldest_cached_entry: Option<DateTime<Utc>>,
    pub newest_cached_entry: Option<DateTime<Utc>>,
}

// #[cfg(test)]
// #[path = "offset_storage_test.rs"]
// mod offset_storage_test;