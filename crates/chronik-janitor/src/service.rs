//! Janitor service implementation.

use crate::cleaner::{SegmentCleaner, CleanupPolicy};
use crate::compactor::{LogCompactor, CompactionPolicy};
use chronik_common::{Result, Error};
use chronik_common::metadata::{MetadataStore, TiKVMetadataStore};
use chronik_storage::object_store::{ObjectStore, ObjectStoreConfig, create_object_store};
use std::sync::Arc;

/// Janitor service configuration
#[derive(Debug, Clone)]
pub struct JanitorConfig {
    /// Metadata store path
    pub metadata_path: String,
    /// Storage configuration
    pub storage_config: ObjectStoreConfig,
    /// Cleanup policy
    pub cleanup_policy: CleanupPolicy,
    /// Compaction policy
    pub compaction_policy: CompactionPolicy,
    /// Enable cleanup
    pub enable_cleanup: bool,
    /// Enable compaction
    pub enable_compaction: bool,
}

impl Default for JanitorConfig {
    fn default() -> Self {
        Self {
            metadata_path: "/var/chronik/metadata".to_string(),
            storage_config: ObjectStoreConfig::default(),
            cleanup_policy: CleanupPolicy::default(),
            compaction_policy: CompactionPolicy::default(),
            enable_cleanup: true,
            enable_compaction: true,
        }
    }
}

/// Service for cleaning up expired segments and performing compaction
pub struct JanitorService {
    config: JanitorConfig,
    storage: Arc<dyn ObjectStore>,
    metadata_store: Arc<dyn MetadataStore>,
    cleaner: Option<Arc<SegmentCleaner>>,
    compactor: Option<Arc<LogCompactor>>,
}

impl JanitorService {
    /// Create a new janitor service
    pub async fn new(config: JanitorConfig) -> Result<Self> {
        // Create storage
        let storage: Arc<dyn ObjectStore> = Arc::from(create_object_store(config.storage_config.clone()).await?);
        
        // Create metadata store using TiKV
        let pd_endpoints = std::env::var("TIKV_PD_ENDPOINTS")
            .unwrap_or_else(|_| "localhost:2379".to_string());
        let endpoints = pd_endpoints
            .split(',')
            .map(|s| s.to_string())
            .collect::<Vec<String>>();
        let metadata_store = Arc::new(TiKVMetadataStore::new(endpoints).await?) as Arc<dyn MetadataStore>;
        
        // Initialize metadata store
        metadata_store.init_system_state().await?;
        
        // Create cleaner if enabled
        let cleaner = if config.enable_cleanup {
            Some(Arc::new(SegmentCleaner::new(
                storage.clone(),
                metadata_store.clone(),
                config.cleanup_policy.clone(),
            )))
        } else {
            None
        };
        
        // Create compactor if enabled
        let compactor = if config.enable_compaction {
            Some(Arc::new(LogCompactor::new(
                storage.clone(),
                metadata_store.clone(),
                config.compaction_policy.clone(),
            )))
        } else {
            None
        };
        
        Ok(Self {
            config,
            storage,
            metadata_store,
            cleaner,
            compactor,
        })
    }
    
    /// Start the janitor service
    pub async fn start(&self) -> Result<()> {
        tracing::info!("Starting janitor service");
        
        // Start cleaner
        if let Some(cleaner) = &self.cleaner {
            cleaner.clone().start().await?;
            tracing::info!("Started segment cleaner");
        }
        
        // Start compactor
        if let Some(compactor) = &self.compactor {
            compactor.clone().start().await?;
            tracing::info!("Started log compactor");
        }
        
        Ok(())
    }
    
    /// Run cleanup manually
    pub async fn run_cleanup(&self) -> Result<()> {
        if let Some(cleaner) = &self.cleaner {
            cleaner.cleanup_old_segments().await?;
            cleaner.cleanup_empty_segments().await?;
        } else {
            return Err(Error::InvalidInput("Cleanup not enabled".into()));
        }
        Ok(())
    }
    
    /// Run compaction manually
    pub async fn run_compaction(&self) -> Result<()> {
        if let Some(compactor) = &self.compactor {
            compactor.run_compaction().await?;
        } else {
            return Err(Error::InvalidInput("Compaction not enabled".into()));
        }
        Ok(())
    }
    
    /// Get storage statistics
    pub async fn get_stats(&self) -> Result<StorageStats> {
        // Get all topics and calculate segment statistics
        let topics = self.metadata_store.list_topics().await?;
        let mut total_segments = 0u64;
        let mut total_size = 0u64;
        let mut oldest_segment: Option<chrono::DateTime<chrono::Utc>> = None;
        let mut newest_segment: Option<chrono::DateTime<chrono::Utc>> = None;
        
        for topic in topics {
            let segments = self.metadata_store.list_segments(&topic.name, None).await?;
            total_segments += segments.len() as u64;
            
            for segment in segments {
                total_size += segment.size as u64;
                
                // Update oldest segment
                if oldest_segment.is_none() || segment.created_at < oldest_segment.unwrap() {
                    oldest_segment = Some(segment.created_at);
                }
                
                // Update newest segment
                if newest_segment.is_none() || segment.created_at > newest_segment.unwrap() {
                    newest_segment = Some(segment.created_at);
                }
            }
        }
        
        Ok(StorageStats {
            total_segments,
            total_size,
            oldest_segment,
            newest_segment,
        })
    }
}

/// Storage statistics
#[derive(Debug, Clone)]
pub struct StorageStats {
    /// Total number of segments
    pub total_segments: u64,
    /// Total storage size in bytes
    pub total_size: u64,
    /// Oldest segment timestamp
    pub oldest_segment: Option<chrono::DateTime<chrono::Utc>>,
    /// Newest segment timestamp
    pub newest_segment: Option<chrono::DateTime<chrono::Utc>>,
}