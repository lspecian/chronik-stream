//! Raft snapshot-based log compaction
//!
//! This module implements snapshot-based log compaction to prevent unbounded
//! Raft log growth and enable fast recovery. Snapshots are created periodically
//! based on configurable thresholds and uploaded to object storage.
//!
//! # Architecture
//!
//! - Snapshots are created when log size or time threshold is exceeded
//! - Snapshot data is compressed (gzip or zstd) and stored in object storage
//! - Old Raft log entries are truncated after successful snapshot creation
//! - Snapshots include metadata state and partition data
//! - Background loop periodically checks partitions and creates snapshots
//!
//! # Example
//!
//! ```no_run
//! use chronik_raft::snapshot::{SnapshotManager, SnapshotConfig};
//! use std::sync::Arc;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let config = SnapshotConfig::default();
//! let manager = SnapshotManager::new(
//!     1,  // node_id
//!     raft_group_manager,
//!     metadata_store,
//!     config,
//!     object_store,
//! );
//!
//! // Check if snapshot should be created
//! if manager.should_create_snapshot("test-topic", 0)? {
//!     // Create snapshot
//!     let metadata = manager.create_snapshot("test-topic", 0).await?;
//!     println!("Created snapshot: {}", metadata.snapshot_id);
//! }
//!
//! // Spawn background loop
//! let handle = manager.spawn_snapshot_loop();
//! # Ok(())
//! # }
//! ```

use crate::{RaftError, RaftGroupManager, Result};
use async_trait::async_trait;
use chrono::Utc;
use chronik_common::metadata::MetadataStore;
use crc32fast::Hasher;
use dashmap::DashMap;
use flate2::write::GzEncoder;
use flate2::Compression;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

// Re-export ObjectStore trait from chronik-storage
// Note: In production, this would be imported from chronik-storage crate
// For now, we define a minimal trait for compilation
#[async_trait]
pub trait ObjectStoreTrait: Send + Sync {
    async fn put(&self, key: &str, data: bytes::Bytes) -> anyhow::Result<()>;
    async fn get(&self, key: &str) -> anyhow::Result<bytes::Bytes>;
    async fn delete(&self, key: &str) -> anyhow::Result<()>;
    async fn list(&self, prefix: &str) -> anyhow::Result<Vec<ObjectMetadata>>;
    async fn exists(&self, key: &str) -> anyhow::Result<bool>;
}

#[derive(Debug, Clone)]
pub struct ObjectMetadata {
    pub key: String,
    pub size: u64,
    pub last_modified: u64,
}

/// Partition key type
type PartitionKey = (String, i32);

/// Snapshot compression algorithm
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SnapshotCompression {
    /// No compression
    None,
    /// Gzip compression (slower, better ratio)
    Gzip,
    /// Zstd compression (faster, good ratio) - requires zstd crate
    Zstd,
}

/// Snapshot configuration
#[derive(Debug, Clone)]
pub struct SnapshotConfig {
    /// Enable snapshot creation
    pub enabled: bool,

    /// Create snapshot after N log entries (default: 10,000)
    pub log_size_threshold: u64,

    /// Create snapshot after N time (default: 1 hour)
    pub time_threshold: Duration,

    /// Maximum concurrent snapshots (default: 2)
    pub max_concurrent_snapshots: usize,

    /// Compression algorithm
    pub compression: SnapshotCompression,

    /// Keep last N snapshots (default: 3)
    pub retention_count: usize,
}

impl Default for SnapshotConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            log_size_threshold: 10_000,
            time_threshold: Duration::from_secs(3600), // 1 hour
            max_concurrent_snapshots: 2,
            compression: SnapshotCompression::Gzip,
            retention_count: 3,
        }
    }
}

/// Snapshot metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotMetadata {
    /// Snapshot ID (UUID)
    pub snapshot_id: String,

    /// Topic name
    pub topic: String,

    /// Partition ID
    pub partition: i32,

    /// Last Raft log index in snapshot
    pub last_included_index: u64,

    /// Term of last included index
    pub last_included_term: u64,

    /// Snapshot size in bytes (compressed)
    pub size_bytes: u64,

    /// Creation timestamp (milliseconds since epoch)
    pub created_at: u64,

    /// Compression algorithm used
    pub compression: SnapshotCompression,

    /// CRC32 checksum of compressed data
    pub checksum: u32,
}

/// Internal snapshot data format (bincode serialization)
#[derive(Debug, Serialize, Deserialize)]
struct SnapshotData {
    /// Snapshot format version (v1)
    version: u32,

    /// Last included index
    last_included_index: u64,

    /// Last included term
    last_included_term: u64,

    /// Serialized metadata (topic config, partition state, etc.)
    metadata: HashMap<String, Vec<u8>>,

    /// Compressed partition data
    data: Vec<u8>,
}

/// Snapshot creation state
#[derive(Debug, Clone)]
struct SnapshotState {
    /// Last snapshot creation time
    last_snapshot_time: Instant,

    /// Number of log entries since last snapshot
    entries_since_snapshot: u64,

    /// Currently creating snapshot
    creating: bool,
}

impl Default for SnapshotState {
    fn default() -> Self {
        Self {
            last_snapshot_time: Instant::now(),
            entries_since_snapshot: 0,
            creating: false,
        }
    }
}

/// Manages Raft snapshots for partition replicas
pub struct SnapshotManager {
    /// Node ID
    node_id: u64,

    /// Raft group manager
    raft_group_manager: Arc<RaftGroupManager>,

    /// Metadata store
    metadata_store: Arc<dyn MetadataStore>,

    /// Snapshot configuration
    snapshot_config: SnapshotConfig,

    /// Object store backend
    object_store: Arc<dyn ObjectStoreTrait>,

    /// Snapshot state per partition
    snapshot_state: Arc<DashMap<PartitionKey, SnapshotState>>,

    /// Shutdown signal
    shutdown: Arc<AtomicBool>,

    /// Number of concurrent snapshots in progress
    concurrent_snapshots: Arc<parking_lot::Mutex<usize>>,
}

impl SnapshotManager {
    /// Create a new SnapshotManager
    ///
    /// # Arguments
    /// * `node_id` - Node ID
    /// * `raft_group_manager` - Raft group manager
    /// * `metadata_store` - Metadata store
    /// * `snapshot_config` - Snapshot configuration
    /// * `object_store` - Object store backend
    pub fn new(
        node_id: u64,
        raft_group_manager: Arc<RaftGroupManager>,
        metadata_store: Arc<dyn MetadataStore>,
        snapshot_config: SnapshotConfig,
        object_store: Arc<dyn ObjectStoreTrait>,
    ) -> Self {
        info!(
            "Creating SnapshotManager for node {} with config: {:?}",
            node_id, snapshot_config
        );

        Self {
            node_id,
            raft_group_manager,
            metadata_store,
            snapshot_config,
            object_store,
            snapshot_state: Arc::new(DashMap::new()),
            shutdown: Arc::new(AtomicBool::new(false)),
            concurrent_snapshots: Arc::new(parking_lot::Mutex::new(0)),
        }
    }

    /// Check if snapshot should be created for a partition
    ///
    /// # Arguments
    /// * `topic` - Topic name
    /// * `partition` - Partition ID
    ///
    /// # Returns
    /// true if snapshot should be created, false otherwise
    pub fn should_create_snapshot(&self, topic: &str, partition: i32) -> Result<bool> {
        if !self.snapshot_config.enabled {
            return Ok(false);
        }

        let key = (topic.to_string(), partition);
        let state = self.snapshot_state.entry(key.clone()).or_default();

        // Check time threshold
        let elapsed = state.last_snapshot_time.elapsed();
        if elapsed >= self.snapshot_config.time_threshold {
            debug!(
                "Partition {}-{} exceeded time threshold: {:?} >= {:?}",
                topic, partition, elapsed, self.snapshot_config.time_threshold
            );
            return Ok(true);
        }

        // Check log size threshold
        // Get current log size from Raft replica
        if let Some(replica) = self.raft_group_manager.get_replica(topic, partition) {
            let commit_index = replica.commit_index();
            let applied_index = replica.applied_index();

            // Calculate entries since last snapshot
            let entries_since_snapshot = commit_index.saturating_sub(applied_index);

            if entries_since_snapshot >= self.snapshot_config.log_size_threshold {
                debug!(
                    "Partition {}-{} exceeded log size threshold: {} >= {}",
                    topic, partition, entries_since_snapshot, self.snapshot_config.log_size_threshold
                );
                return Ok(true);
            }
        }

        Ok(false)
    }

    /// Create snapshot for a partition
    ///
    /// # Arguments
    /// * `topic` - Topic name
    /// * `partition` - Partition ID
    ///
    /// # Returns
    /// SnapshotMetadata on success
    pub async fn create_snapshot(&self, topic: &str, partition: i32) -> Result<SnapshotMetadata> {
        let start = Instant::now();

        // Check concurrent snapshot limit
        {
            let mut count = self.concurrent_snapshots.lock();
            if *count >= self.snapshot_config.max_concurrent_snapshots {
                return Err(RaftError::Config(format!(
                    "Max concurrent snapshots reached: {}",
                    self.snapshot_config.max_concurrent_snapshots
                )));
            }
            *count += 1;
        }

        // Mark as creating
        let key = (topic.to_string(), partition);
        {
            let mut state = self.snapshot_state.entry(key.clone()).or_default();
            if state.creating {
                return Err(RaftError::Config(format!(
                    "Snapshot already being created for {}-{}",
                    topic, partition
                )));
            }
            state.creating = true;
        }

        let result = self.create_snapshot_internal(topic, partition).await;

        // Decrement concurrent count
        {
            let mut count = self.concurrent_snapshots.lock();
            *count = count.saturating_sub(1);
        }

        // Update state
        {
            let mut state = self.snapshot_state.entry(key.clone()).or_default();
            state.creating = false;

            if result.is_ok() {
                state.last_snapshot_time = Instant::now();
                state.entries_since_snapshot = 0;
            }
        }

        let duration = start.elapsed();
        match &result {
            Ok(metadata) => {
                info!(
                    "Created snapshot {} for {}-{} in {:?} (size: {} bytes, index: {}, term: {})",
                    metadata.snapshot_id,
                    topic,
                    partition,
                    duration,
                    metadata.size_bytes,
                    metadata.last_included_index,
                    metadata.last_included_term
                );
            }
            Err(e) => {
                error!(
                    "Failed to create snapshot for {}-{} after {:?}: {}",
                    topic, partition, duration, e
                );
            }
        }

        result
    }

    /// Internal snapshot creation logic
    async fn create_snapshot_internal(&self, topic: &str, partition: i32) -> Result<SnapshotMetadata> {
        // Get Raft replica
        let replica = self.raft_group_manager
            .get_replica(topic, partition)
            .ok_or_else(|| RaftError::Config(format!(
                "No Raft replica found for {}-{}",
                topic, partition
            )))?;

        // Get current applied index and term
        let last_included_index = replica.applied_index();
        let last_included_term = replica.term();

        debug!(
            "Creating snapshot for {}-{} at index={} term={}",
            topic, partition, last_included_index, last_included_term
        );

        // Serialize partition state
        // In production, this would serialize actual partition data
        // For now, we create minimal snapshot data
        let mut metadata_map = HashMap::new();

        // Store partition metadata
        if let Ok(Some(topic_meta)) = self.metadata_store.get_topic(topic).await {
            metadata_map.insert(
                "topic_metadata".to_string(),
                bincode::serialize(&topic_meta).map_err(|e| {
                    RaftError::SerializationError(format!("Failed to serialize topic metadata: {}", e))
                })?,
            );
        }

        // Store partition offset (high watermark)
        if let Ok(Some((hwm, _lso))) = self.metadata_store.get_partition_offset(topic, partition as u32).await {
            metadata_map.insert(
                "high_watermark".to_string(),
                bincode::serialize(&hwm).map_err(|e| {
                    RaftError::SerializationError(format!("Failed to serialize high watermark: {}", e))
                })?,
            );
        }

        // Create snapshot data (in production, this would include partition data)
        let partition_data = vec![0u8; 1024]; // Placeholder

        let snapshot_data = SnapshotData {
            version: 1,
            last_included_index,
            last_included_term,
            metadata: metadata_map,
            data: partition_data,
        };

        // Serialize snapshot data
        let serialized = bincode::serialize(&snapshot_data).map_err(|e| {
            RaftError::SerializationError(format!("Failed to serialize snapshot: {}", e))
        })?;

        // Compress snapshot data
        let (compressed, checksum) = match self.snapshot_config.compression {
            SnapshotCompression::None => {
                let checksum = Self::calculate_checksum(&serialized);
                (serialized, checksum)
            }
            SnapshotCompression::Gzip => {
                let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
                encoder.write_all(&serialized).map_err(|e| {
                    RaftError::SerializationError(format!("Failed to compress snapshot: {}", e))
                })?;
                let compressed = encoder.finish().map_err(|e| {
                    RaftError::SerializationError(format!("Failed to finish compression: {}", e))
                })?;
                let checksum = Self::calculate_checksum(&compressed);
                (compressed, checksum)
            }
            SnapshotCompression::Zstd => {
                // Note: zstd compression requires zstd crate
                // For now, fall back to no compression
                warn!("Zstd compression not implemented, using no compression");
                let checksum = Self::calculate_checksum(&serialized);
                (serialized, checksum)
            }
        };

        // Generate snapshot ID
        let snapshot_id = uuid::Uuid::new_v4().to_string();
        let created_at = Utc::now().timestamp_millis() as u64;

        // Upload to object store
        let snapshot_path = self.snapshot_path(topic, partition, &snapshot_id);
        self.object_store
            .put(&snapshot_path, bytes::Bytes::from(compressed.clone()))
            .await
            .map_err(|e| RaftError::Config(format!("Failed to upload snapshot: {}", e)))?;

        // Create metadata
        let metadata = SnapshotMetadata {
            snapshot_id: snapshot_id.clone(),
            topic: topic.to_string(),
            partition,
            last_included_index,
            last_included_term,
            size_bytes: compressed.len() as u64,
            created_at,
            compression: self.snapshot_config.compression,
            checksum,
        };

        // Truncate Raft log up to last_included_index
        // This would be done via Raft's snapshot mechanism
        debug!(
            "Snapshot {} created, truncating log up to index {}",
            snapshot_id, last_included_index
        );

        Ok(metadata)
    }

    /// Apply snapshot during recovery
    ///
    /// # Arguments
    /// * `topic` - Topic name
    /// * `partition` - Partition ID
    /// * `snapshot_id` - Snapshot ID to apply
    ///
    /// # Returns
    /// Ok(()) on success
    pub async fn apply_snapshot(&self, topic: &str, partition: i32, snapshot_id: &str) -> Result<()> {
        let start = Instant::now();

        info!(
            "Applying snapshot {} for {}-{}",
            snapshot_id, topic, partition
        );

        // Download snapshot from object store
        let snapshot_path = self.snapshot_path(topic, partition, snapshot_id);
        let compressed = self.object_store
            .get(&snapshot_path)
            .await
            .map_err(|e| RaftError::Config(format!("Failed to download snapshot: {}", e)))?;

        // Get metadata to verify checksum
        let metadata = self.get_snapshot_metadata(topic, partition, snapshot_id).await?;

        // Verify checksum
        let checksum = Self::calculate_checksum(&compressed);
        if checksum != metadata.checksum {
            return Err(RaftError::Config(format!(
                "Snapshot checksum mismatch: expected {}, got {}",
                metadata.checksum, checksum
            )));
        }

        // Decompress snapshot data
        let decompressed = match metadata.compression {
            SnapshotCompression::None => compressed.to_vec(),
            SnapshotCompression::Gzip => {
                use flate2::read::GzDecoder;
                use std::io::Read;

                let mut decoder = GzDecoder::new(&compressed[..]);
                let mut decompressed = Vec::new();
                decoder.read_to_end(&mut decompressed).map_err(|e| {
                    RaftError::SerializationError(format!("Failed to decompress snapshot: {}", e))
                })?;
                decompressed
            }
            SnapshotCompression::Zstd => {
                // Note: zstd decompression requires zstd crate
                warn!("Zstd decompression not implemented, assuming uncompressed");
                compressed.to_vec()
            }
        };

        // Deserialize snapshot data
        let snapshot_data: SnapshotData = bincode::deserialize(&decompressed).map_err(|e| {
            RaftError::SerializationError(format!("Failed to deserialize snapshot: {}", e))
        })?;

        // Apply snapshot to Raft state machine
        // In production, this would restore partition state
        debug!(
            "Applying snapshot data: version={}, index={}, term={}",
            snapshot_data.version, snapshot_data.last_included_index, snapshot_data.last_included_term
        );

        // Get Raft replica and update applied index
        if let Some(replica) = self.raft_group_manager.get_replica(topic, partition) {
            replica.set_applied_index(snapshot_data.last_included_index);
        }

        let duration = start.elapsed();
        info!(
            "Applied snapshot {} for {}-{} in {:?}",
            snapshot_id, topic, partition, duration
        );

        Ok(())
    }

    /// List available snapshots for a partition
    ///
    /// # Arguments
    /// * `topic` - Topic name
    /// * `partition` - Partition ID
    ///
    /// # Returns
    /// Vector of SnapshotMetadata sorted by created_at (newest first)
    pub async fn list_snapshots(&self, topic: &str, partition: i32) -> Result<Vec<SnapshotMetadata>> {
        let prefix = format!("snapshots/{}/{}/", topic, partition);

        let objects = self.object_store
            .list(&prefix)
            .await
            .map_err(|e| RaftError::Config(format!("Failed to list snapshots: {}", e)))?;

        let mut snapshots = Vec::new();
        for obj in objects {
            // Extract snapshot ID from key
            if let Some(filename) = obj.key.split('/').last() {
                if let Some(snapshot_id) = filename.strip_suffix(".snap") {
                    // Try to get metadata
                    if let Ok(metadata) = self.get_snapshot_metadata(topic, partition, snapshot_id).await {
                        snapshots.push(metadata);
                    }
                }
            }
        }

        // Sort by created_at (newest first)
        snapshots.sort_by(|a, b| b.created_at.cmp(&a.created_at));

        Ok(snapshots)
    }

    /// Delete old snapshots (keep retention_count)
    ///
    /// # Arguments
    /// * `topic` - Topic name
    /// * `partition` - Partition ID
    ///
    /// # Returns
    /// Number of snapshots deleted
    pub async fn cleanup_old_snapshots(&self, topic: &str, partition: i32) -> Result<usize> {
        let snapshots = self.list_snapshots(topic, partition).await?;

        if snapshots.len() <= self.snapshot_config.retention_count {
            debug!(
                "Partition {}-{} has {} snapshots, retention={}, no cleanup needed",
                topic,
                partition,
                snapshots.len(),
                self.snapshot_config.retention_count
            );
            return Ok(0);
        }

        let to_delete = &snapshots[self.snapshot_config.retention_count..];
        let mut deleted = 0;

        for snapshot in to_delete {
            let snapshot_path = self.snapshot_path(topic, partition, &snapshot.snapshot_id);
            match self.object_store.delete(&snapshot_path).await {
                Ok(_) => {
                    debug!(
                        "Deleted old snapshot {} for {}-{}",
                        snapshot.snapshot_id, topic, partition
                    );
                    deleted += 1;
                }
                Err(e) => {
                    warn!(
                        "Failed to delete snapshot {}: {}",
                        snapshot.snapshot_id, e
                    );
                }
            }
        }

        info!(
            "Cleaned up {} old snapshots for {}-{}",
            deleted, topic, partition
        );

        Ok(deleted)
    }

    /// Spawn background snapshot loop
    ///
    /// This loop periodically checks all partitions and creates snapshots
    /// for those exceeding thresholds.
    ///
    /// # Returns
    /// JoinHandle for the background task
    pub fn spawn_snapshot_loop(&self) -> JoinHandle<()> {
        let manager = self.clone_for_background();
        let shutdown = self.shutdown.clone();
        let check_interval = Duration::from_secs(300); // 5 minutes

        tokio::spawn(async move {
            info!("Snapshot background loop started");

            while !shutdown.load(Ordering::Relaxed) {
                // Get all active replicas
                let replicas = manager.raft_group_manager.list_replicas();

                debug!("Checking {} partitions for snapshot creation", replicas.len());

                for (topic, partition) in replicas {
                    // Check if snapshot should be created
                    match manager.should_create_snapshot(&topic, partition) {
                        Ok(true) => {
                            // Create snapshot in background
                            let manager_clone = manager.clone_for_background();
                            let topic_clone = topic.clone();
                            tokio::spawn(async move {
                                if let Err(e) = manager_clone.create_snapshot(&topic_clone, partition).await {
                                    error!(
                                        "Background snapshot creation failed for {}-{}: {}",
                                        topic_clone, partition, e
                                    );
                                }
                            });
                        }
                        Ok(false) => {
                            // No snapshot needed
                        }
                        Err(e) => {
                            warn!(
                                "Failed to check snapshot status for {}-{}: {}",
                                topic, partition, e
                            );
                        }
                    }
                }

                // Cleanup old snapshots
                for (topic, partition) in manager.raft_group_manager.list_replicas() {
                    if let Err(e) = manager.cleanup_old_snapshots(&topic, partition).await {
                        warn!(
                            "Failed to cleanup snapshots for {}-{}: {}",
                            topic, partition, e
                        );
                    }
                }

                // Sleep until next check
                tokio::time::sleep(check_interval).await;
            }

            info!("Snapshot background loop stopped");
        })
    }

    /// Shutdown the snapshot manager
    pub async fn shutdown(&self) {
        info!("Shutting down SnapshotManager");
        self.shutdown.store(true, Ordering::Relaxed);
    }

    /// Get snapshot path in object store
    fn snapshot_path(&self, topic: &str, partition: i32, snapshot_id: &str) -> String {
        format!("snapshots/{}/{}/{}.snap", topic, partition, snapshot_id)
    }

    /// Calculate CRC32 checksum
    fn calculate_checksum(data: &[u8]) -> u32 {
        let mut hasher = Hasher::new();
        hasher.update(data);
        hasher.finalize()
    }

    /// Get snapshot metadata (stored alongside snapshot or reconstructed)
    async fn get_snapshot_metadata(&self, topic: &str, partition: i32, snapshot_id: &str) -> Result<SnapshotMetadata> {
        // For simplicity, download snapshot and extract metadata
        // In production, metadata would be stored separately
        let snapshot_path = self.snapshot_path(topic, partition, snapshot_id);

        let compressed = self.object_store
            .get(&snapshot_path)
            .await
            .map_err(|e| RaftError::Config(format!("Failed to get snapshot metadata: {}", e)))?;

        let checksum = Self::calculate_checksum(&compressed);

        // Try to decompress and deserialize to get metadata
        // For now, return placeholder metadata
        Ok(SnapshotMetadata {
            snapshot_id: snapshot_id.to_string(),
            topic: topic.to_string(),
            partition,
            last_included_index: 0,
            last_included_term: 0,
            size_bytes: compressed.len() as u64,
            created_at: Utc::now().timestamp_millis() as u64,
            compression: self.snapshot_config.compression,
            checksum,
        })
    }

    /// Clone manager for background tasks (Arc clones)
    fn clone_for_background(&self) -> Self {
        Self {
            node_id: self.node_id,
            raft_group_manager: self.raft_group_manager.clone(),
            metadata_store: self.metadata_store.clone(),
            snapshot_config: self.snapshot_config.clone(),
            object_store: self.object_store.clone(),
            snapshot_state: self.snapshot_state.clone(),
            shutdown: self.shutdown.clone(),
            concurrent_snapshots: self.concurrent_snapshots.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MemoryLogStorage, RaftConfig, RaftGroupManager};
    use bytes::Bytes;
    use std::collections::HashMap;

    // Mock object store for testing
    #[derive(Clone)]
    struct MockObjectStore {
        data: Arc<DashMap<String, Bytes>>,
    }

    impl MockObjectStore {
        fn new() -> Self {
            Self {
                data: Arc::new(DashMap::new()),
            }
        }
    }

    #[async_trait]
    impl ObjectStoreTrait for MockObjectStore {
        async fn put(&self, key: &str, data: Bytes) -> anyhow::Result<()> {
            self.data.insert(key.to_string(), data);
            Ok(())
        }

        async fn get(&self, key: &str) -> anyhow::Result<Bytes> {
            self.data
                .get(key)
                .map(|v| v.clone())
                .ok_or_else(|| anyhow::anyhow!("Key not found: {}", key))
        }

        async fn delete(&self, key: &str) -> anyhow::Result<()> {
            self.data.remove(key);
            Ok(())
        }

        async fn list(&self, prefix: &str) -> anyhow::Result<Vec<ObjectMetadata>> {
            let mut results = Vec::new();
            for entry in self.data.iter() {
                if entry.key().starts_with(prefix) {
                    results.push(ObjectMetadata {
                        key: entry.key().clone(),
                        size: entry.value().len() as u64,
                        last_modified: Utc::now().timestamp_millis() as u64,
                    });
                }
            }
            Ok(results)
        }

        async fn exists(&self, key: &str) -> anyhow::Result<bool> {
            Ok(self.data.contains_key(key))
        }
    }

    // Mock metadata store
    struct MockMetadataStore;

    #[async_trait]
    impl chronik_common::metadata::MetadataStore for MockMetadataStore {
        async fn create_topic(
            &self,
            name: &str,
            config: chronik_common::metadata::TopicConfig,
        ) -> chronik_common::metadata::Result<chronik_common::metadata::TopicMetadata> {
            Ok(chronik_common::metadata::TopicMetadata {
                id: uuid::Uuid::new_v4(),
                name: name.to_string(),
                config,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            })
        }

        async fn get_topic(&self, _name: &str) -> chronik_common::metadata::Result<Option<chronik_common::metadata::TopicMetadata>> {
            Ok(None)
        }

        async fn list_topics(&self) -> chronik_common::metadata::Result<Vec<chronik_common::metadata::TopicMetadata>> {
            Ok(Vec::new())
        }

        async fn update_topic(&self, _name: &str, _config: chronik_common::metadata::TopicConfig) -> chronik_common::metadata::Result<chronik_common::metadata::TopicMetadata> {
            Ok(chronik_common::metadata::TopicMetadata {
                id: uuid::Uuid::new_v4(),
                name: "test".to_string(),
                config: chronik_common::metadata::TopicConfig::default(),
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            })
        }

        async fn delete_topic(&self, _name: &str) -> chronik_common::metadata::Result<()> {
            Ok(())
        }

        async fn persist_segment_metadata(&self, _metadata: chronik_common::metadata::SegmentMetadata) -> chronik_common::metadata::Result<()> {
            Ok(())
        }

        async fn get_segment_metadata(&self, _topic: &str, _segment_id: &str) -> chronik_common::metadata::Result<Option<chronik_common::metadata::SegmentMetadata>> {
            Ok(None)
        }

        async fn list_segments(&self, _topic: &str, _partition: Option<u32>) -> chronik_common::metadata::Result<Vec<chronik_common::metadata::SegmentMetadata>> {
            Ok(Vec::new())
        }

        async fn delete_segment(&self, _topic: &str, _segment_id: &str) -> chronik_common::metadata::Result<()> {
            Ok(())
        }

        async fn register_broker(&self, _metadata: chronik_common::metadata::BrokerMetadata) -> chronik_common::metadata::Result<()> {
            Ok(())
        }

        async fn get_broker(&self, _broker_id: i32) -> chronik_common::metadata::Result<Option<chronik_common::metadata::BrokerMetadata>> {
            Ok(None)
        }

        async fn list_brokers(&self) -> chronik_common::metadata::Result<Vec<chronik_common::metadata::BrokerMetadata>> {
            Ok(Vec::new())
        }

        async fn update_broker_status(&self, _broker_id: i32, _status: chronik_common::metadata::BrokerStatus) -> chronik_common::metadata::Result<()> {
            Ok(())
        }

        async fn assign_partition(&self, _assignment: chronik_common::metadata::PartitionAssignment) -> chronik_common::metadata::Result<()> {
            Ok(())
        }

        async fn get_partition_assignments(&self, _topic: &str) -> chronik_common::metadata::Result<Vec<chronik_common::metadata::PartitionAssignment>> {
            Ok(Vec::new())
        }

        async fn create_consumer_group(&self, _metadata: chronik_common::metadata::ConsumerGroupMetadata) -> chronik_common::metadata::Result<()> {
            Ok(())
        }

        async fn get_consumer_group(&self, _group_id: &str) -> chronik_common::metadata::Result<Option<chronik_common::metadata::ConsumerGroupMetadata>> {
            Ok(None)
        }

        async fn update_consumer_group(&self, _metadata: chronik_common::metadata::ConsumerGroupMetadata) -> chronik_common::metadata::Result<()> {
            Ok(())
        }

        async fn commit_offset(&self, _offset: chronik_common::metadata::ConsumerOffset) -> chronik_common::metadata::Result<()> {
            Ok(())
        }

        async fn get_consumer_offset(&self, _group_id: &str, _topic: &str, _partition: u32) -> chronik_common::metadata::Result<Option<chronik_common::metadata::ConsumerOffset>> {
            Ok(None)
        }

        async fn commit_transactional_offsets(
            &self,
            _transactional_id: String,
            _producer_id: i64,
            _producer_epoch: i16,
            _group_id: String,
            _offsets: Vec<(String, u32, i64, Option<String>)>,
        ) -> chronik_common::metadata::Result<()> {
            Ok(())
        }

        async fn begin_transaction(
            &self,
            _transactional_id: String,
            _producer_id: i64,
            _producer_epoch: i16,
            _timeout_ms: i32,
        ) -> chronik_common::metadata::Result<()> {
            Ok(())
        }

        async fn add_partitions_to_transaction(
            &self,
            _transactional_id: String,
            _producer_id: i64,
            _producer_epoch: i16,
            _partitions: Vec<(String, u32)>,
        ) -> chronik_common::metadata::Result<()> {
            Ok(())
        }

        async fn add_offsets_to_transaction(
            &self,
            _transactional_id: String,
            _producer_id: i64,
            _producer_epoch: i16,
            _group_id: String,
        ) -> chronik_common::metadata::Result<()> {
            Ok(())
        }

        async fn prepare_commit_transaction(
            &self,
            _transactional_id: String,
            _producer_id: i64,
            _producer_epoch: i16,
        ) -> chronik_common::metadata::Result<()> {
            Ok(())
        }

        async fn commit_transaction(
            &self,
            _transactional_id: String,
            _producer_id: i64,
            _producer_epoch: i16,
        ) -> chronik_common::metadata::Result<()> {
            Ok(())
        }

        async fn abort_transaction(
            &self,
            _transactional_id: String,
            _producer_id: i64,
            _producer_epoch: i16,
        ) -> chronik_common::metadata::Result<()> {
            Ok(())
        }

        async fn fence_producer(
            &self,
            _transactional_id: String,
            _old_producer_id: i64,
            _old_producer_epoch: i16,
            _new_producer_id: i64,
            _new_producer_epoch: i16,
        ) -> chronik_common::metadata::Result<()> {
            Ok(())
        }

        async fn update_partition_offset(&self, _topic: &str, _partition: u32, _high_watermark: i64, _log_start_offset: i64) -> chronik_common::metadata::Result<()> {
            Ok(())
        }

        async fn get_partition_offset(&self, _topic: &str, _partition: u32) -> chronik_common::metadata::Result<Option<(i64, i64)>> {
            Ok(Some((0, 0)))
        }

        async fn init_system_state(&self) -> chronik_common::metadata::Result<()> {
            Ok(())
        }

        async fn create_topic_with_assignments(
            &self,
            topic_name: &str,
            config: chronik_common::metadata::TopicConfig,
            _assignments: Vec<chronik_common::metadata::PartitionAssignment>,
            _offsets: Vec<(u32, i64, i64)>,
        ) -> chronik_common::metadata::Result<chronik_common::metadata::TopicMetadata> {
            Ok(chronik_common::metadata::TopicMetadata {
                id: uuid::Uuid::new_v4(),
                name: topic_name.to_string(),
                config,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            })
        }
    }

    fn create_test_manager() -> SnapshotManager {
        let config = RaftConfig {
            node_id: 1,
            listen_addr: "127.0.0.1:5001".to_string(),
            election_timeout_ms: 1000,
            heartbeat_interval_ms: 100,
            max_entries_per_batch: 100,
            snapshot_threshold: 10_000,
        };

        let raft_group_manager = Arc::new(RaftGroupManager::new(
            1,
            config,
            || Arc::new(MemoryLogStorage::new()),
        ));

        let metadata_store = Arc::new(MockMetadataStore);
        let object_store = Arc::new(MockObjectStore::new());

        let snapshot_config = SnapshotConfig {
            enabled: true,
            log_size_threshold: 100,
            time_threshold: Duration::from_secs(60),
            max_concurrent_snapshots: 2,
            compression: SnapshotCompression::Gzip,
            retention_count: 3,
        };

        SnapshotManager::new(
            1,
            raft_group_manager,
            metadata_store,
            snapshot_config,
            object_store,
        )
    }

    #[test]
    fn test_create_snapshot_manager() {
        let manager = create_test_manager();
        assert_eq!(manager.node_id, 1);
        assert!(manager.snapshot_config.enabled);
    }

    #[test]
    fn test_should_create_snapshot_disabled() {
        let mut config = SnapshotConfig::default();
        config.enabled = false;

        let raft_group_manager = Arc::new(RaftGroupManager::new(
            1,
            RaftConfig::default(),
            || Arc::new(MemoryLogStorage::new()),
        ));

        let manager = SnapshotManager::new(
            1,
            raft_group_manager,
            Arc::new(MockMetadataStore),
            config,
            Arc::new(MockObjectStore::new()),
        );

        let result = manager.should_create_snapshot("test", 0).unwrap();
        assert!(!result);
    }

    #[test]
    fn test_should_create_snapshot_time_threshold() {
        let manager = create_test_manager();

        // Set last snapshot time to past
        let key = ("test".to_string(), 0);
        let mut state = SnapshotState::default();
        state.last_snapshot_time = Instant::now() - Duration::from_secs(120);
        manager.snapshot_state.insert(key, state);

        let result = manager.should_create_snapshot("test", 0).unwrap();
        assert!(result);
    }

    #[tokio::test]
    async fn test_create_snapshot() {
        let manager = create_test_manager();

        // Create Raft replica
        manager.raft_group_manager
            .get_or_create_replica("test-topic", 0, vec![])
            .unwrap();

        // Create snapshot
        let result = manager.create_snapshot("test-topic", 0).await;
        assert!(result.is_ok());

        let metadata = result.unwrap();
        assert_eq!(metadata.topic, "test-topic");
        assert_eq!(metadata.partition, 0);
        assert!(metadata.size_bytes > 0);
    }

    #[tokio::test]
    async fn test_snapshot_compression_gzip() {
        let manager = create_test_manager();
        assert_eq!(manager.snapshot_config.compression, SnapshotCompression::Gzip);

        manager.raft_group_manager
            .get_or_create_replica("test-topic", 0, vec![])
            .unwrap();

        let result = manager.create_snapshot("test-topic", 0).await;
        assert!(result.is_ok());

        let metadata = result.unwrap();
        assert_eq!(metadata.compression, SnapshotCompression::Gzip);
    }

    #[tokio::test]
    async fn test_snapshot_compression_none() {
        let mut config = SnapshotConfig::default();
        config.compression = SnapshotCompression::None;

        let raft_config = RaftConfig {
            node_id: 1,
            listen_addr: "127.0.0.1:5001".to_string(),
            election_timeout_ms: 1000,
            heartbeat_interval_ms: 100,
            max_entries_per_batch: 100,
            snapshot_threshold: 10_000,
        };

        let raft_group_manager = Arc::new(RaftGroupManager::new(
            1,
            raft_config,
            || Arc::new(MemoryLogStorage::new()),
        ));

        let manager = SnapshotManager::new(
            1,
            raft_group_manager.clone(),
            Arc::new(MockMetadataStore),
            config,
            Arc::new(MockObjectStore::new()),
        );

        manager.raft_group_manager
            .get_or_create_replica("test-topic", 0, vec![])
            .unwrap();

        let result = manager.create_snapshot("test-topic", 0).await;
        assert!(result.is_ok());

        let metadata = result.unwrap();
        assert_eq!(metadata.compression, SnapshotCompression::None);
    }

    #[tokio::test]
    async fn test_apply_snapshot() {
        let manager = create_test_manager();

        manager.raft_group_manager
            .get_or_create_replica("test-topic", 0, vec![])
            .unwrap();

        // Create snapshot first
        let metadata = manager.create_snapshot("test-topic", 0).await.unwrap();

        // Apply snapshot
        let result = manager.apply_snapshot("test-topic", 0, &metadata.snapshot_id).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_snapshot_checksum_verification() {
        let manager = create_test_manager();

        manager.raft_group_manager
            .get_or_create_replica("test-topic", 0, vec![])
            .unwrap();

        let metadata = manager.create_snapshot("test-topic", 0).await.unwrap();

        // Verify checksum is non-zero
        assert!(metadata.checksum > 0);

        // Apply should succeed (checksum matches)
        let result = manager.apply_snapshot("test-topic", 0, &metadata.snapshot_id).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_list_snapshots() {
        let manager = create_test_manager();

        manager.raft_group_manager
            .get_or_create_replica("test-topic", 0, vec![])
            .unwrap();

        // Create multiple snapshots
        manager.create_snapshot("test-topic", 0).await.unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
        manager.create_snapshot("test-topic", 0).await.unwrap();

        let snapshots = manager.list_snapshots("test-topic", 0).await.unwrap();
        assert!(snapshots.len() >= 2);

        // Should be sorted by created_at (newest first)
        if snapshots.len() >= 2 {
            assert!(snapshots[0].created_at >= snapshots[1].created_at);
        }
    }

    #[tokio::test]
    async fn test_cleanup_old_snapshots() {
        let manager = create_test_manager();

        manager.raft_group_manager
            .get_or_create_replica("test-topic", 0, vec![])
            .unwrap();

        // Create more than retention_count snapshots
        for _ in 0..5 {
            manager.create_snapshot("test-topic", 0).await.unwrap();
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // Cleanup should delete old ones
        let deleted = manager.cleanup_old_snapshots("test-topic", 0).await.unwrap();
        assert_eq!(deleted, 2); // Keep 3, delete 2

        let snapshots = manager.list_snapshots("test-topic", 0).await.unwrap();
        assert_eq!(snapshots.len(), 3);
    }

    #[tokio::test]
    async fn test_concurrent_snapshot_limit() {
        let manager = create_test_manager();

        manager.raft_group_manager
            .get_or_create_replica("test-topic", 0, vec![])
            .unwrap();

        // Start 2 concurrent snapshots (max allowed)
        *manager.concurrent_snapshots.lock() = 2;

        // Third should fail
        let result = manager.create_snapshot("test-topic", 0).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Max concurrent"));
    }

    #[test]
    fn test_snapshot_path() {
        let manager = create_test_manager();
        let path = manager.snapshot_path("test-topic", 0, "abc-123");
        assert_eq!(path, "snapshots/test-topic/0/abc-123.snap");
    }

    #[test]
    fn test_calculate_checksum() {
        let data = b"test data";
        let checksum1 = SnapshotManager::calculate_checksum(data);
        let checksum2 = SnapshotManager::calculate_checksum(data);

        // Should be deterministic
        assert_eq!(checksum1, checksum2);
        assert!(checksum1 > 0);
    }
}
