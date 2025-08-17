//! Offset cleanup implementation with enhanced retention logic
//!
//! This module provides enhanced cleanup functionality for expired consumer offsets,
//! implementing proper retention policies without modifying the MetadataStore trait.

use chronik_common::{Result, Error};
use chronik_common::metadata::ConsumerOffset;
use std::sync::Arc;
use std::time::Duration;
use std::collections::HashSet;
use chrono::{DateTime, Utc};
use tracing::{debug, info, warn};
use tikv_client::{RawClient, Key};

/// Enhanced offset cleanup manager
pub struct OffsetCleanupManager {
    tikv_client: Option<Arc<RawClient>>,
    retention_duration: Duration,
}

impl OffsetCleanupManager {
    /// Create a new cleanup manager
    pub fn new(retention_duration: Duration) -> Self {
        Self {
            tikv_client: None,
            retention_duration,
        }
    }
    
    /// Initialize with TiKV client for direct access
    pub async fn init_tikv(&mut self, pd_endpoints: Vec<String>) -> Result<()> {
        match RawClient::new(pd_endpoints).await {
            Ok(client) => {
                self.tikv_client = Some(Arc::new(client));
                Ok(())
            }
            Err(e) => {
                warn!("Failed to connect to TiKV for cleanup: {}. Cleanup will be disabled.", e);
                Ok(())
            }
        }
    }
    
    /// Perform cleanup of expired offsets
    pub async fn cleanup_expired_offsets(&self) -> Result<usize> {
        let tikv_client = match &self.tikv_client {
            Some(client) => client,
            None => {
                debug!("TiKV client not initialized, skipping cleanup");
                return Ok(0);
            }
        };
        
        let retention_cutoff = Utc::now() - chrono::Duration::from_std(self.retention_duration)
            .map_err(|e| Error::Internal(format!("Invalid duration: {}", e)))?;
        
        debug!("Running offset cleanup, removing offsets older than {}", retention_cutoff);
        
        // Scan all offset keys
        let prefix = b"offset:";
        let mut cleaned_count = 0;
        let mut batch_keys = Vec::new();
        
        // Scan in batches
        let batch_size = 1000;
        let mut start_key = prefix.to_vec();
        
        loop {
            // Scan next batch
            let end_key = {
                let mut k = start_key.clone();
                k.push(0xFF); // Scan to the end of the prefix
                k
            };
            
            let pairs = tikv_client.scan(start_key.clone()..end_key, batch_size as u32).await
                .map_err(|e| Error::Storage(format!("Failed to scan offsets: {}", e)))?;
            
            if pairs.is_empty() {
                break;
            }
            
            for pair in &pairs {
                // Check if this is an offset key
                // Convert TiKV Key to bytes for comparison
                let key_bytes: Vec<u8> = pair.0.clone().into();
                if !key_bytes.starts_with(prefix) {
                    // We've passed all offset keys
                    break;
                }
                
                // Try to parse the offset data
                if let Ok(offset) = serde_json::from_slice::<ConsumerOffset>(&pair.1) {
                    if offset.commit_timestamp < retention_cutoff {
                        batch_keys.push(pair.0.clone());
                        cleaned_count += 1;
                        
                        debug!(
                            "Marking offset for deletion: group={}, topic={}, partition={}, timestamp={}",
                            offset.group_id, offset.topic, offset.partition, offset.commit_timestamp
                        );
                    }
                }
            }
            
            // Delete batch if we have enough keys
            if batch_keys.len() >= 100 {
                self.delete_batch(&tikv_client, &mut batch_keys).await?;
            }
            
            // Update start key for next scan
            if let Some(last_pair) = pairs.last() {
                start_key = last_pair.0.clone().into();
                // Add a byte to ensure we don't re-scan the same key
                start_key.push(0);
            } else {
                break;
            }
            
            // Exit if we've scanned past offset keys
            if !start_key.starts_with(prefix) {
                break;
            }
        }
        
        // Delete any remaining keys
        if !batch_keys.is_empty() {
            self.delete_batch(&tikv_client, &mut batch_keys).await?;
        }
        
        if cleaned_count > 0 {
            info!("Cleaned up {} expired consumer offsets", cleaned_count);
        }
        
        Ok(cleaned_count)
    }
    
    /// Delete a batch of keys
    async fn delete_batch(&self, client: &Arc<RawClient>, keys: &mut Vec<Key>) -> Result<()> {
        if keys.is_empty() {
            return Ok(());
        }
        
        let batch_size = keys.len();
        let keys_to_delete = keys.drain(..).collect::<Vec<_>>();
        
        client.batch_delete(keys_to_delete).await
            .map_err(|e| Error::Storage(format!("Failed to delete offset batch: {}", e)))?;
        
        debug!("Deleted batch of {} expired offsets", batch_size);
        Ok(())
    }
    
    /// Get statistics about offset retention
    pub async fn get_retention_stats(&self) -> Result<RetentionStats> {
        let tikv_client = match &self.tikv_client {
            Some(client) => client,
            None => {
                return Ok(RetentionStats::default());
            }
        };
        
        let retention_cutoff = Utc::now() - chrono::Duration::from_std(self.retention_duration)
            .map_err(|e| Error::Internal(format!("Invalid duration: {}", e)))?;
        
        let mut total_offsets = 0;
        let mut expired_offsets = 0;
        let mut groups: HashSet<String> = HashSet::new();
        let mut oldest_offset: Option<DateTime<Utc>> = None;
        let mut newest_offset: Option<DateTime<Utc>> = None;
        
        // Scan all offsets to gather statistics
        let prefix = b"offset:";
        let mut start_key = prefix.to_vec();
        
        loop {
            let end_key = {
                let mut k = start_key.clone();
                k.push(0xFF);
                k
            };
            
            let pairs = tikv_client.scan(start_key.clone()..end_key, 1000).await
                .map_err(|e| Error::Storage(format!("Failed to scan for stats: {}", e)))?;
            
            if pairs.is_empty() {
                break;
            }
            
            for pair in &pairs {
                // Convert TiKV Key to bytes for comparison
                let key_bytes: Vec<u8> = pair.0.clone().into();
                if !key_bytes.starts_with(prefix) {
                    break;
                }
                
                if let Ok(offset) = serde_json::from_slice::<ConsumerOffset>(&pair.1) {
                    total_offsets += 1;
                    groups.insert(offset.group_id.clone());
                    
                    if offset.commit_timestamp < retention_cutoff {
                        expired_offsets += 1;
                    }
                    
                    // Track oldest and newest
                    match oldest_offset {
                        None => oldest_offset = Some(offset.commit_timestamp),
                        Some(old) if offset.commit_timestamp < old => {
                            oldest_offset = Some(offset.commit_timestamp);
                        }
                        _ => {}
                    }
                    
                    match newest_offset {
                        None => newest_offset = Some(offset.commit_timestamp),
                        Some(new) if offset.commit_timestamp > new => {
                            newest_offset = Some(offset.commit_timestamp);
                        }
                        _ => {}
                    }
                }
            }
            
            if let Some(last_pair) = pairs.last() {
                start_key = last_pair.0.clone().into();
                start_key.push(0);
            } else {
                break;
            }
            
            if !start_key.starts_with(prefix) {
                break;
            }
        }
        
        Ok(RetentionStats {
            total_offsets,
            expired_offsets,
            active_groups: groups.len(),
            oldest_offset,
            newest_offset,
            retention_cutoff,
        })
    }
}

/// Statistics about offset retention
#[derive(Debug, Clone)]
pub struct RetentionStats {
    pub total_offsets: usize,
    pub expired_offsets: usize,
    pub active_groups: usize,
    pub oldest_offset: Option<DateTime<Utc>>,
    pub newest_offset: Option<DateTime<Utc>>,
    pub retention_cutoff: DateTime<Utc>,
}

impl Default for RetentionStats {
    fn default() -> Self {
        Self {
            total_offsets: 0,
            expired_offsets: 0,
            active_groups: 0,
            oldest_offset: None,
            newest_offset: None,
            retention_cutoff: Utc::now(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[tokio::test]
    async fn test_cleanup_manager_creation() {
        let manager = OffsetCleanupManager::new(Duration::from_secs(7 * 24 * 60 * 60));
        assert!(manager.tikv_client.is_none());
    }
    
    #[tokio::test]
    async fn test_retention_stats_default() {
        let stats = RetentionStats::default();
        assert_eq!(stats.total_offsets, 0);
        assert_eq!(stats.expired_offsets, 0);
        assert_eq!(stats.active_groups, 0);
        assert!(stats.oldest_offset.is_none());
        assert!(stats.newest_offset.is_none());
    }
}