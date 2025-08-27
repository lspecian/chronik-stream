//! Storage service for the ingest node.

use chronik_common::Result;
use chronik_storage::{
    ObjectStoreTrait as ObjectStore, ObjectStoreConfig, ObjectStoreFactory,
    SegmentWriter, SegmentWriterConfig, SegmentReader, SegmentReaderConfig,
};
use std::sync::Arc;
use tracing::info;

/// Storage configuration
#[derive(Debug, Clone)]
pub struct StorageConfig {
    /// Object store configuration
    pub object_store_config: ObjectStoreConfig,
    /// Segment writer configuration
    pub segment_writer_config: SegmentWriterConfig,
    /// Segment reader configuration
    pub segment_reader_config: SegmentReaderConfig,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            object_store_config: ObjectStoreConfig::default(),
            segment_writer_config: SegmentWriterConfig {
                data_dir: std::path::PathBuf::from("/tmp/chronik/segments"),
                compression_codec: "none".to_string(),
                max_segment_size: 1024 * 1024 * 128, // 128MB
                enable_dual_storage: false, // Default to raw-only for better performance
                max_segment_age_secs: 3600, // 1 hour
                retention_period_secs: 7 * 24 * 3600, // 7 days
                enable_cleanup: true,
            },
            segment_reader_config: SegmentReaderConfig::default(),
        }
    }
}

/// Storage service providing access to object storage
pub struct StorageService {
    object_store: Arc<dyn ObjectStore>,
}

impl StorageService {
    /// Create a new storage service
    pub async fn new(config: StorageConfig) -> Result<Self> {
        let object_store = ObjectStoreFactory::create(config.object_store_config).await?;
        
        info!("Storage service initialized");
        
        Ok(Self {
            object_store: Arc::from(object_store),
        })
    }
    
    /// Get the object store
    pub fn object_store(&self) -> Arc<dyn ObjectStore> {
        Arc::clone(&self.object_store)
    }
    
    /// Create a new segment writer
    pub async fn create_segment_writer(
        &self,
        _topic: String,
        _partition: i32,
        config: SegmentWriterConfig,
    ) -> Result<SegmentWriter> {
        SegmentWriter::new(config).await
    }
    
    /// Create a new segment reader
    pub fn create_segment_reader(
        &self,
        config: SegmentReaderConfig,
    ) -> Result<SegmentReader> {
        Ok(SegmentReader::new(
            config,
            self.object_store(),
        ))
    }
}