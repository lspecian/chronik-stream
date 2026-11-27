//! Storage configuration for the ingest node.

use chronik_storage::{ObjectStoreConfig, SegmentWriterConfig};

/// Storage configuration
#[derive(Debug, Clone)]
pub struct StorageConfig {
    /// Object store configuration
    pub object_store_config: ObjectStoreConfig,
    /// Segment writer configuration
    pub segment_writer_config: SegmentWriterConfig,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            object_store_config: ObjectStoreConfig::default(),
            segment_writer_config: SegmentWriterConfig {
                data_dir: std::path::PathBuf::from("/tmp/chronik/segments"),
                compression_codec: "none".to_string(),
                max_segment_size: 1024 * 1024 * 128, // 128MB
                max_segment_age_secs: 3600, // 1 hour
                retention_period_secs: 7 * 24 * 3600, // 7 days
                enable_cleanup: true,
            },
        }
    }
}

