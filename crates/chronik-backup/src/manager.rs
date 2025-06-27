//! Backup manager implementation

use crate::{
    BackupError, Result, BackupMetadata, MetadataBackup, CompressionType,
    EncryptionConfig, compression, encryption,
};
use chronik_storage::object_store::ObjectStore;
use std::sync::Arc;
use chrono::Utc;
use tokio::sync::Semaphore;
use tracing::{info, debug};
use sha2::{Sha256, Digest};

/// Backup configuration
#[derive(Debug, Clone)]
pub struct BackupConfig {
    /// Number of days to retain backups
    pub retention_days: u32,
    /// Compression type to use
    pub compression: CompressionType,
    /// Optional encryption configuration
    pub encryption: Option<EncryptionConfig>,
    /// Number of parallel workers for backup operations
    pub parallel_workers: usize,
    /// Maximum segment size for incremental backups
    pub max_segment_size: u64,
    /// Whether to verify checksums during backup
    pub verify_checksums: bool,
}

impl Default for BackupConfig {
    fn default() -> Self {
        Self {
            retention_days: 30,
            compression: CompressionType::Zstd,
            encryption: None,
            parallel_workers: 4,
            max_segment_size: 1024 * 1024 * 1024, // 1GB
            verify_checksums: true,
        }
    }
}

/// Backup manager for coordinating backup operations
pub struct BackupManager {
    /// Primary object store (source data)
    object_store: Arc<dyn ObjectStore>,
    /// Backup object store (destination)
    backup_store: Arc<dyn ObjectStore>,
    /// Backup configuration
    config: BackupConfig,
    /// Semaphore for limiting parallel operations
    semaphore: Arc<Semaphore>,
}

impl BackupManager {
    /// Create a new backup manager
    pub fn new(
        object_store: Arc<dyn ObjectStore>,
        backup_store: Arc<dyn ObjectStore>,
        config: BackupConfig,
    ) -> Self {
        let semaphore = Arc::new(Semaphore::new(config.parallel_workers));
        
        Self {
            object_store,
            backup_store,
            config,
            semaphore,
        }
    }
    
    /// Create a full backup
    pub async fn create_full_backup(&self, backup_id: &str) -> Result<BackupMetadata> {
        info!("Starting full backup: {}", backup_id);
        
        let start_time = Utc::now();
        
        // Create backup metadata
        let mut metadata = BackupMetadata {
            backup_id: backup_id.to_string(),
            timestamp: start_time,
            backup_type: crate::metadata::BackupType::Full,
            version: env!("CARGO_PKG_VERSION").to_string(),
            compression: self.config.compression,
            encryption: self.config.encryption.is_some(),
            topics: Vec::new(),
            total_size: 0,
            total_segments: 0,
            checksum: String::new(),
            duration: Default::default(),
            parent_backup_id: None,
        };
        
        // Backup metadata from metastore
        let metastore_backup = self.backup_metastore(&metadata).await?;
        
        // Backup all topics and their segments
        let topic_backups = self.backup_all_topics(&metadata).await?;
        metadata.topics = topic_backups;
        
        // Calculate total size and segments
        metadata.total_size = metadata.topics.iter()
            .map(|t| t.total_size)
            .sum();
        metadata.total_segments = metadata.topics.iter()
            .map(|t| t.segment_count)
            .sum();
        
        // Calculate duration
        metadata.duration = Utc::now() - start_time;
        
        // Calculate final checksum
        metadata.checksum = self.calculate_backup_checksum(&metadata)?;
        
        // Save backup metadata
        self.save_backup_metadata(&metadata).await?;
        
        info!("Full backup completed: {} topics, {} segments, {} bytes in {:?}",
              metadata.topics.len(),
              metadata.total_segments,
              metadata.total_size,
              metadata.duration);
        
        Ok(metadata)
    }
    
    /// Create an incremental backup based on a parent backup
    pub async fn create_incremental_backup(
        &self,
        backup_id: &str,
        parent_backup_id: &str,
    ) -> Result<BackupMetadata> {
        info!("Starting incremental backup: {} (parent: {})", backup_id, parent_backup_id);
        
        // Load parent backup metadata
        let parent_metadata = self.load_backup_metadata(parent_backup_id).await?;
        
        let start_time = Utc::now();
        
        // Create backup metadata
        let mut metadata = BackupMetadata {
            backup_id: backup_id.to_string(),
            timestamp: start_time,
            backup_type: crate::metadata::BackupType::Incremental,
            version: env!("CARGO_PKG_VERSION").to_string(),
            compression: self.config.compression,
            encryption: self.config.encryption.is_some(),
            topics: Vec::new(),
            total_size: 0,
            total_segments: 0,
            checksum: String::new(),
            duration: Default::default(),
            parent_backup_id: Some(parent_backup_id.to_string()),
        };
        
        // Backup only changed data since parent backup
        let topic_backups = self.backup_changed_topics(&metadata, &parent_metadata).await?;
        metadata.topics = topic_backups;
        
        // Calculate total size and segments
        metadata.total_size = metadata.topics.iter()
            .map(|t| t.total_size)
            .sum();
        metadata.total_segments = metadata.topics.iter()
            .map(|t| t.segment_count)
            .sum();
        
        // Calculate duration
        metadata.duration = Utc::now() - start_time;
        
        // Calculate final checksum
        metadata.checksum = self.calculate_backup_checksum(&metadata)?;
        
        // Save backup metadata
        self.save_backup_metadata(&metadata).await?;
        
        info!("Incremental backup completed: {} changed topics, {} segments, {} bytes in {:?}",
              metadata.topics.len(),
              metadata.total_segments,
              metadata.total_size,
              metadata.duration);
        
        Ok(metadata)
    }
    
    /// Backup metastore data
    async fn backup_metastore(&self, backup_metadata: &BackupMetadata) -> Result<MetadataBackup> {
        debug!("Backing up metastore data");
        
        // In a real implementation, this would connect to the metastore
        // For now, create a placeholder
        let metastore_backup = MetadataBackup {
            version: backup_metadata.version.clone(),
            timestamp: backup_metadata.timestamp,
            topics: Vec::new(), // Would be populated from metastore
            consumer_groups: Vec::new(),
            acls: Vec::new(),
            configs: std::collections::HashMap::new(),
        };
        
        // Serialize and compress
        let data = self.serialize_and_compress(&metastore_backup)?;
        
        // Optionally encrypt
        let data = if let Some(encryption) = &self.config.encryption {
            encryption::encrypt(&data, encryption)?
        } else {
            data
        };
        
        // Store in backup location
        let key = format!("backups/{}/metadata.bin", backup_metadata.backup_id);
        self.backup_store.put(&key, data.into()).await
            .map_err(|e| BackupError::Storage(e.to_string()))?;
        
        Ok(metastore_backup)
    }
    
    /// Backup all topics
    async fn backup_all_topics(
        &self,
        backup_metadata: &BackupMetadata,
    ) -> Result<Vec<crate::metadata::TopicBackupInfo>> {
        debug!("Backing up all topics");
        
        // In a real implementation, this would list all topics from metastore
        // and backup their segments in parallel
        let topics = vec!["events", "logs", "metrics"]; // Placeholder
        
        let mut topic_backups = Vec::new();
        
        for topic in topics {
            let topic_backup = self.backup_topic(backup_metadata, topic).await?;
            topic_backups.push(topic_backup);
        }
        
        Ok(topic_backups)
    }
    
    /// Backup changed topics since parent backup
    async fn backup_changed_topics(
        &self,
        backup_metadata: &BackupMetadata,
        parent_metadata: &BackupMetadata,
    ) -> Result<Vec<crate::metadata::TopicBackupInfo>> {
        debug!("Backing up changed topics since {}", parent_metadata.timestamp);
        
        // In a real implementation, this would compare segment lists
        // and only backup new or modified segments
        
        Ok(Vec::new()) // Placeholder
    }
    
    /// Backup a single topic
    async fn backup_topic(
        &self,
        backup_metadata: &BackupMetadata,
        topic: &str,
    ) -> Result<crate::metadata::TopicBackupInfo> {
        debug!("Backing up topic: {}", topic);
        
        // Placeholder implementation
        Ok(crate::metadata::TopicBackupInfo {
            name: topic.to_string(),
            partition_count: 3,
            replication_factor: 1,
            segment_count: 10,
            total_size: 1024 * 1024 * 100, // 100MB
            segments: Vec::new(),
        })
    }
    
    /// Serialize and compress data
    fn serialize_and_compress<T: serde::Serialize>(&self, data: &T) -> Result<Vec<u8>> {
        // Serialize
        let serialized = bincode::serialize(data)?;
        
        // Compress
        let compressed = compression::compress(&serialized, self.config.compression)?;
        
        Ok(compressed)
    }
    
    /// Calculate backup checksum
    fn calculate_backup_checksum(&self, metadata: &BackupMetadata) -> Result<String> {
        let data = serde_json::to_vec(metadata)
            .map_err(|e| BackupError::Serialization(e.to_string()))?;
        
        let mut hasher = Sha256::new();
        hasher.update(&data);
        let result = hasher.finalize();
        
        Ok(hex::encode(result))
    }
    
    /// Save backup metadata
    async fn save_backup_metadata(&self, metadata: &BackupMetadata) -> Result<()> {
        let data = serde_json::to_vec_pretty(metadata)?;
        
        let key = format!("backups/{}/backup.json", metadata.backup_id);
        self.backup_store.put(&key, data.into()).await
            .map_err(|e| BackupError::Storage(e.to_string()))?;
        
        Ok(())
    }
    
    /// Load backup metadata
    async fn load_backup_metadata(&self, backup_id: &str) -> Result<BackupMetadata> {
        let key = format!("backups/{}/backup.json", backup_id);
        
        let data = self.backup_store.get(&key).await
            .map_err(|e| BackupError::NotFound(format!("Backup {} not found: {}", backup_id, e)))?;
        
        let metadata: BackupMetadata = serde_json::from_slice(&data)?;
        
        Ok(metadata)
    }
    
    /// List available backups
    pub async fn list_backups(&self) -> Result<Vec<BackupMetadata>> {
        let prefix = "backups/";
        let mut backups = Vec::new();
        
        let objects = self.backup_store.list(prefix).await
            .map_err(|e| BackupError::Storage(e.to_string()))?;
        
        for object in objects {
            if object.key.ends_with("/backup.json") {
                if let Ok(data) = self.backup_store.get(&object.key).await {
                    if let Ok(metadata) = serde_json::from_slice::<BackupMetadata>(&data) {
                        backups.push(metadata);
                    }
                }
            }
        }
        
        // Sort by timestamp (newest first)
        backups.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        
        Ok(backups)
    }
    
    /// Delete old backups based on retention policy
    pub async fn cleanup_old_backups(&self) -> Result<u32> {
        let cutoff_date = Utc::now() - chrono::Duration::days(self.config.retention_days as i64);
        let mut deleted_count = 0;
        
        let backups = self.list_backups().await?;
        
        for backup in backups {
            if backup.timestamp < cutoff_date {
                info!("Deleting old backup: {} (created: {})", 
                      backup.backup_id, backup.timestamp);
                
                // Delete all files for this backup
                let prefix = format!("backups/{}/", backup.backup_id);
                let objects = self.backup_store.list(&prefix).await
                    .map_err(|e| BackupError::Storage(e.to_string()))?;
                
                for object in objects {
                    self.backup_store.delete(&object.key).await
                        .map_err(|e| BackupError::Storage(e.to_string()))?;
                }
                
                deleted_count += 1;
            }
        }
        
        if deleted_count > 0 {
            info!("Deleted {} old backups", deleted_count);
        }
        
        Ok(deleted_count)
    }
}

/// Hex encoding utility
mod hex {
    pub fn encode(data: impl AsRef<[u8]>) -> String {
        data.as_ref()
            .iter()
            .map(|byte| format!("{:02x}", byte))
            .collect()
    }
}