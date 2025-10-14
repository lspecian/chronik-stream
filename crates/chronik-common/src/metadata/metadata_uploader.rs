//! Metadata Uploader - Background task to upload metadata WAL and snapshots to object store
//!
//! This module implements disaster recovery for metadata by:
//! 1. Uploading sealed metadata WAL segments to S3/GCS/Azure
//! 2. Uploading metadata snapshots to object store
//! 3. Enabling cold-start recovery from S3 when local disk is lost
//!
//! This ensures that losing a node doesn't result in losing topics, partitions,
//! consumer offsets, or high watermarks - all metadata can be reconstructed from S3.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::time::{interval, Duration};
use tracing::{debug, error, info, instrument, warn};

use super::{MetadataError, Result};

/// Configuration for metadata uploader
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetadataUploaderConfig {
    /// Interval between upload runs (seconds)
    pub upload_interval_secs: u64,

    /// Base path in object store for metadata WAL segments
    pub wal_base_path: String,

    /// Base path in object store for metadata snapshots
    pub snapshot_base_path: String,

    /// Whether to delete local WAL segments after successful upload
    pub delete_after_upload: bool,

    /// Whether to delete local snapshots after successful upload (keep latest locally)
    pub delete_old_snapshots: bool,

    /// How many local snapshots to keep (even if uploaded)
    pub keep_local_snapshot_count: usize,

    /// Enable automatic uploading
    pub enabled: bool,
}

impl Default for MetadataUploaderConfig {
    fn default() -> Self {
        Self {
            upload_interval_secs: 60, // Upload every minute
            wal_base_path: "metadata-wal".to_string(),
            snapshot_base_path: "metadata-snapshots".to_string(),
            delete_after_upload: true,
            delete_old_snapshots: true,
            keep_local_snapshot_count: 2, // Keep 2 latest snapshots locally
            enabled: true,
        }
    }
}

/// Object store interface for metadata uploads
#[async_trait]
pub trait ObjectStoreInterface: Send + Sync {
    /// Upload data to object store
    async fn put(&self, key: &str, data: bytes::Bytes) -> Result<()>;

    /// Download data from object store
    async fn get(&self, key: &str) -> Result<bytes::Bytes>;

    /// List objects with prefix
    async fn list(&self, prefix: &str) -> Result<Vec<String>>;

    /// Check if object exists
    async fn exists(&self, key: &str) -> Result<bool>;

    /// Delete object
    async fn delete(&self, key: &str) -> Result<()>;
}

/// Statistics from an upload run
#[derive(Debug, Default, Clone)]
pub struct UploadStats {
    /// Number of WAL segments uploaded
    pub wal_segments_uploaded: usize,

    /// Number of snapshots uploaded
    pub snapshots_uploaded: usize,

    /// Total bytes uploaded
    pub bytes_uploaded: u64,

    /// Number of errors encountered
    pub errors: usize,

    /// Duration of upload run (milliseconds)
    pub duration_ms: u64,
}

/// Metadata uploader for disaster recovery
pub struct MetadataUploader {
    /// Configuration
    config: MetadataUploaderConfig,

    /// Object store for uploading
    object_store: Arc<dyn ObjectStoreInterface>,

    /// Local data directory
    data_dir: PathBuf,

    /// Statistics from last upload run
    last_stats: Arc<tokio::sync::RwLock<UploadStats>>,

    /// Whether the uploader is running
    running: Arc<tokio::sync::RwLock<bool>>,
}

impl MetadataUploader {
    /// Create a new metadata uploader
    pub fn new(
        config: MetadataUploaderConfig,
        object_store: Arc<dyn ObjectStoreInterface>,
        data_dir: impl AsRef<Path>,
    ) -> Self {
        Self {
            config,
            object_store,
            data_dir: data_dir.as_ref().to_path_buf(),
            last_stats: Arc::new(tokio::sync::RwLock::new(UploadStats::default())),
            running: Arc::new(tokio::sync::RwLock::new(false)),
        }
    }

    /// Start the background upload task
    #[instrument(skip(self))]
    pub async fn start(&self) -> Result<()> {
        if !self.config.enabled {
            info!("Metadata uploader disabled by configuration");
            return Ok(());
        }

        let mut running = self.running.write().await;
        if *running {
            warn!("Metadata uploader already running");
            return Ok(());
        }
        *running = true;
        drop(running);

        info!(
            interval_secs = self.config.upload_interval_secs,
            "Starting metadata uploader background task"
        );

        let config = self.config.clone();
        let object_store = Arc::clone(&self.object_store);
        let data_dir = self.data_dir.clone();
        let last_stats = Arc::clone(&self.last_stats);
        let running = Arc::clone(&self.running);

        tokio::spawn(async move {
            let mut interval_timer = interval(Duration::from_secs(config.upload_interval_secs));

            loop {
                interval_timer.tick().await;

                // Check if still running
                let is_running = *running.read().await;
                if !is_running {
                    info!("Metadata uploader stopped");
                    break;
                }

                // Run upload
                debug!("Metadata uploader tick - checking for segments to upload");

                match Self::upload_metadata_internal(
                    &config,
                    &object_store,
                    &data_dir,
                )
                .await
                {
                    Ok(stats) => {
                        if stats.wal_segments_uploaded > 0 || stats.snapshots_uploaded > 0 {
                            info!(
                                wal_segments = stats.wal_segments_uploaded,
                                snapshots = stats.snapshots_uploaded,
                                bytes = stats.bytes_uploaded,
                                errors = stats.errors,
                                duration_ms = stats.duration_ms,
                                "Metadata upload run complete"
                            );
                        }
                        *last_stats.write().await = stats;
                    }
                    Err(e) => {
                        error!(error = %e, "Metadata upload run failed");
                    }
                }
            }
        });

        Ok(())
    }

    /// Stop the background upload task
    pub async fn stop(&self) {
        info!("Stopping metadata uploader");
        *self.running.write().await = false;
    }

    /// Get statistics from last upload run
    pub async fn get_stats(&self) -> UploadStats {
        self.last_stats.read().await.clone()
    }

    /// Check if uploader is running
    pub async fn is_running(&self) -> bool {
        *self.running.read().await
    }

    /// Upload metadata segments and snapshots (internal implementation)
    #[instrument(skip(config, object_store, data_dir))]
    async fn upload_metadata_internal(
        config: &MetadataUploaderConfig,
        object_store: &Arc<dyn ObjectStoreInterface>,
        data_dir: &Path,
    ) -> Result<UploadStats> {
        let start_time = std::time::Instant::now();
        let mut stats = UploadStats::default();

        // Upload WAL segments
        match Self::upload_wal_segments(config, object_store, data_dir).await {
            Ok((count, bytes)) => {
                stats.wal_segments_uploaded = count;
                stats.bytes_uploaded += bytes;
            }
            Err(e) => {
                error!(error = %e, "Failed to upload WAL segments");
                stats.errors += 1;
            }
        }

        // Upload snapshots
        match Self::upload_snapshots(config, object_store, data_dir).await {
            Ok((count, bytes)) => {
                stats.snapshots_uploaded = count;
                stats.bytes_uploaded += bytes;
            }
            Err(e) => {
                error!(error = %e, "Failed to upload snapshots");
                stats.errors += 1;
            }
        }

        stats.duration_ms = start_time.elapsed().as_millis() as u64;
        Ok(stats)
    }

    /// Upload metadata WAL segments to object store
    #[instrument(skip(config, object_store, data_dir))]
    async fn upload_wal_segments(
        config: &MetadataUploaderConfig,
        object_store: &Arc<dyn ObjectStoreInterface>,
        data_dir: &Path,
    ) -> Result<(usize, u64)> {
        // Metadata WAL location: {data_dir}/wal/__meta/0/wal_0_0.log
        let wal_dir = data_dir.join("wal").join("__meta").join("0");

        if !wal_dir.exists() {
            debug!("No metadata WAL directory found");
            return Ok((0, 0));
        }

        let mut count = 0;
        let mut total_bytes = 0u64;

        // Read WAL directory for segments
        let mut entries = tokio::fs::read_dir(&wal_dir)
            .await
            .map_err(|e| MetadataError::StorageError(format!("Failed to read WAL dir: {}", e)))?;

        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|e| MetadataError::StorageError(format!("Failed to read entry: {}", e)))?
        {
            let path = entry.path();
            let file_name = match path.file_name().and_then(|n| n.to_str()) {
                Some(name) => name,
                None => continue,
            };

            // Only upload .log files (skip .sealed or other files)
            if !file_name.ends_with(".log") {
                continue;
            }

            // Read WAL file
            let data = tokio::fs::read(&path).await.map_err(|e| {
                MetadataError::StorageError(format!("Failed to read WAL file: {}", e))
            })?;

            let file_size = data.len() as u64;

            // Generate object key: metadata-wal/__meta/0/{timestamp}-{filename}
            let timestamp = chrono::Utc::now().timestamp();
            let object_key = format!(
                "{}/{}",
                config.wal_base_path,
                format!("__meta/0/{}-{}", timestamp, file_name)
            );

            // Upload to object store
            let data_bytes = bytes::Bytes::from(data);
            object_store
                .put(&object_key, data_bytes)
                .await
                .map_err(|e| {
                    MetadataError::StorageError(format!("Failed to upload WAL segment: {}", e))
                })?;

            info!(
                file = %file_name,
                object_key = %object_key,
                bytes = file_size,
                "Uploaded metadata WAL segment"
            );

            count += 1;
            total_bytes += file_size;

            // Delete local WAL segment if configured
            if config.delete_after_upload {
                if let Err(e) = tokio::fs::remove_file(&path).await {
                    warn!(file = %file_name, error = %e, "Failed to delete local WAL segment");
                }
            }
        }

        Ok((count, total_bytes))
    }

    /// Upload metadata snapshots to object store
    #[instrument(skip(config, object_store, data_dir))]
    async fn upload_snapshots(
        config: &MetadataUploaderConfig,
        object_store: &Arc<dyn ObjectStoreInterface>,
        data_dir: &Path,
    ) -> Result<(usize, u64)> {
        // Snapshot location: {data_dir}/metadata_snapshots/latest.snapshot
        let snapshot_dir = data_dir.join("metadata_snapshots");

        if !snapshot_dir.exists() {
            debug!("No metadata snapshots directory found");
            return Ok((0, 0));
        }

        let snapshot_file = snapshot_dir.join("latest.snapshot");
        if !snapshot_file.exists() {
            debug!("No latest snapshot found");
            return Ok((0, 0));
        }

        // Read snapshot file
        let data = tokio::fs::read(&snapshot_file).await.map_err(|e| {
            MetadataError::StorageError(format!("Failed to read snapshot: {}", e))
        })?;

        let file_size = data.len() as u64;

        // Generate object key with timestamp: metadata-snapshots/{timestamp}.snapshot
        let timestamp = chrono::Utc::now().timestamp();
        let object_key = format!("{}/{}.snapshot", config.snapshot_base_path, timestamp);

        // Check if we've already uploaded this snapshot (compare with latest in S3)
        // For simplicity, we'll just upload with timestamp-based key (duplicates are OK for DR)

        // Upload to object store
        let data_bytes = bytes::Bytes::from(data);
        object_store
            .put(&object_key, data_bytes)
            .await
            .map_err(|e| {
                MetadataError::StorageError(format!("Failed to upload snapshot: {}", e))
            })?;

        info!(
            object_key = %object_key,
            bytes = file_size,
            "Uploaded metadata snapshot"
        );

        Ok((1, file_size))
    }

    /// Recover metadata from object store on cold start
    ///
    /// This is called when local WAL is empty/missing and we need to reconstruct
    /// metadata state from S3.
    #[instrument(skip(config, object_store, data_dir))]
    pub async fn recover_from_object_store(
        config: &MetadataUploaderConfig,
        object_store: &Arc<dyn ObjectStoreInterface>,
        data_dir: &Path,
    ) -> Result<()> {
        info!("Starting metadata recovery from object store");

        // Create directories
        let wal_dir = data_dir.join("wal").join("__meta").join("0");
        let snapshot_dir = data_dir.join("metadata_snapshots");
        tokio::fs::create_dir_all(&wal_dir).await.map_err(|e| {
            MetadataError::StorageError(format!("Failed to create WAL dir: {}", e))
        })?;
        tokio::fs::create_dir_all(&snapshot_dir).await.map_err(|e| {
            MetadataError::StorageError(format!("Failed to create snapshot dir: {}", e))
        })?;

        // Step 1: Download latest snapshot from S3
        match Self::download_latest_snapshot(config, object_store, &snapshot_dir).await {
            Ok(Some(offset)) => {
                info!(offset = offset, "Downloaded latest snapshot from S3");
            }
            Ok(None) => {
                info!("No snapshots found in object store, will replay full WAL");
            }
            Err(e) => {
                warn!(error = %e, "Failed to download snapshot, will replay full WAL");
            }
        }

        // Step 2: Download all WAL segments from S3
        let wal_count = Self::download_wal_segments(config, object_store, &wal_dir).await?;
        info!(count = wal_count, "Downloaded WAL segments from S3");

        info!("Metadata recovery from object store complete");
        Ok(())
    }

    /// Download latest snapshot from object store
    async fn download_latest_snapshot(
        config: &MetadataUploaderConfig,
        object_store: &Arc<dyn ObjectStoreInterface>,
        snapshot_dir: &Path,
    ) -> Result<Option<u64>> {
        // List all snapshots in S3
        let snapshots = object_store
            .list(&config.snapshot_base_path)
            .await
            .map_err(|e| {
                MetadataError::StorageError(format!("Failed to list snapshots: {}", e))
            })?;

        if snapshots.is_empty() {
            return Ok(None);
        }

        // Find latest snapshot (by timestamp in filename)
        let latest = snapshots
            .iter()
            .filter(|s| s.ends_with(".snapshot"))
            .max()
            .ok_or_else(|| MetadataError::NotFound("No snapshots found".to_string()))?;

        // Download snapshot
        let data = object_store.get(latest).await.map_err(|e| {
            MetadataError::StorageError(format!("Failed to download snapshot: {}", e))
        })?;

        // Write to local disk
        let snapshot_file = snapshot_dir.join("latest.snapshot");
        tokio::fs::write(&snapshot_file, data.as_ref())
            .await
            .map_err(|e| {
                MetadataError::StorageError(format!("Failed to write snapshot: {}", e))
            })?;

        // Parse offset from snapshot (requires deserializing)
        // For now, return 0 (full WAL replay)
        Ok(Some(0))
    }

    /// Download all WAL segments from object store
    async fn download_wal_segments(
        config: &MetadataUploaderConfig,
        object_store: &Arc<dyn ObjectStoreInterface>,
        wal_dir: &Path,
    ) -> Result<usize> {
        // List all WAL segments in S3
        let wal_prefix = format!("{}/__meta/0/", config.wal_base_path);
        let segments = object_store.list(&wal_prefix).await.map_err(|e| {
            MetadataError::StorageError(format!("Failed to list WAL segments: {}", e))
        })?;

        let mut count = 0;

        for segment_key in segments {
            if !segment_key.ends_with(".log") {
                continue;
            }

            // Download segment
            let data = object_store.get(&segment_key).await.map_err(|e| {
                MetadataError::StorageError(format!("Failed to download WAL segment: {}", e))
            })?;

            // Extract filename from key
            let filename = segment_key
                .rsplit('/')
                .next()
                .and_then(|s| s.split('-').last())
                .unwrap_or("wal_0_0.log");

            // Write to local disk
            let local_path = wal_dir.join(filename);
            tokio::fs::write(&local_path, data.as_ref())
                .await
                .map_err(|e| {
                    MetadataError::StorageError(format!("Failed to write WAL segment: {}", e))
                })?;

            info!(
                segment = %segment_key,
                local_path = %local_path.display(),
                "Downloaded WAL segment"
            );

            count += 1;
        }

        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tokio::sync::Mutex;

    // Mock object store for testing
    struct MockObjectStore {
        storage: Arc<Mutex<HashMap<String, bytes::Bytes>>>,
    }

    impl MockObjectStore {
        fn new() -> Self {
            Self {
                storage: Arc::new(Mutex::new(HashMap::new())),
            }
        }
    }

    #[async_trait]
    impl ObjectStoreInterface for MockObjectStore {
        async fn put(&self, key: &str, data: bytes::Bytes) -> Result<()> {
            self.storage.lock().await.insert(key.to_string(), data);
            Ok(())
        }

        async fn get(&self, key: &str) -> Result<bytes::Bytes> {
            self.storage
                .lock()
                .await
                .get(key)
                .cloned()
                .ok_or_else(|| MetadataError::NotFound(format!("Key not found: {}", key)))
        }

        async fn list(&self, prefix: &str) -> Result<Vec<String>> {
            let storage = self.storage.lock().await;
            Ok(storage
                .keys()
                .filter(|k| k.starts_with(prefix))
                .cloned()
                .collect())
        }

        async fn exists(&self, key: &str) -> Result<bool> {
            Ok(self.storage.lock().await.contains_key(key))
        }

        async fn delete(&self, key: &str) -> Result<()> {
            self.storage.lock().await.remove(key);
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_metadata_uploader_config() {
        let config = MetadataUploaderConfig::default();
        assert_eq!(config.upload_interval_secs, 60);
        assert!(config.enabled);
        assert!(config.delete_after_upload);
    }

    #[tokio::test]
    async fn test_mock_object_store() {
        let store = Arc::new(MockObjectStore::new());

        // Put data
        let data = bytes::Bytes::from("test data");
        store.put("test-key", data.clone()).await.unwrap();

        // Get data
        let retrieved = store.get("test-key").await.unwrap();
        assert_eq!(retrieved, data);

        // List
        let keys = store.list("test").await.unwrap();
        assert_eq!(keys.len(), 1);

        // Exists
        assert!(store.exists("test-key").await.unwrap());
        assert!(!store.exists("missing-key").await.unwrap());
    }
}
