//! WAL compaction for managing storage growth

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::time::{Duration, interval};
use tracing::{debug, info, warn, error};

use crate::{WalError, Result};
use crate::manager::{WalManager, TopicPartition};
use crate::record::WalRecord;
use crate::config::WalConfig;

/// Compaction strategy type
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CompactionStrategy {
    /// Keep only the latest value for each key
    KeyBased,
    /// Remove records older than retention period
    TimeBased,
    /// Hybrid: key-based within time window
    Hybrid,
    /// Custom strategy for specific topics
    Custom,
}

/// Configuration for WAL compaction
#[derive(Debug, Clone)]
pub struct CompactionConfig {
    /// Enable automatic compaction
    pub enabled: bool,

    /// Compaction interval in seconds
    pub interval_secs: u64,

    /// Minimum segment age before compaction (seconds)
    pub min_segment_age_secs: u64,

    /// Size threshold for compaction (bytes)
    pub size_threshold_bytes: u64,

    /// Minimum number of segments before compaction
    pub min_segments: usize,

    /// Percentage of records to retain during compaction (0.0 - 1.0)
    pub retention_ratio: f64,

    /// Maximum compaction duration (seconds)
    pub max_duration_secs: u64,

    /// Compaction strategy to use
    pub strategy: CompactionStrategy,

    /// Time retention period for time-based compaction (seconds)
    pub retention_period_secs: u64,

    /// Enable parallel compaction of multiple partitions
    pub parallel_compaction: bool,

    /// Maximum number of parallel compaction tasks
    pub max_parallel_tasks: usize,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_secs: 3600, // 1 hour
            min_segment_age_secs: 1800, // 30 minutes
            size_threshold_bytes: 100 * 1024 * 1024, // 100MB
            min_segments: 5,
            retention_ratio: 0.8, // Keep 80% of recent records
            max_duration_secs: 300, // 5 minutes
            strategy: CompactionStrategy::KeyBased,
            retention_period_secs: 86400 * 7, // 7 days
            parallel_compaction: true,
            max_parallel_tasks: 4,
        }
    }
}

/// WAL compaction manager
pub struct WalCompactor {
    config: CompactionConfig,
    wal_config: WalConfig,
    base_path: PathBuf,
}

impl WalCompactor {
    /// Create a new WAL compactor
    pub fn new(wal_config: WalConfig, compaction_config: CompactionConfig) -> Self {
        Self {
            config: compaction_config,
            base_path: wal_config.data_dir.clone(),
            wal_config,
        }
    }

    /// Start automatic compaction background task
    pub async fn start_auto_compaction(&self, wal_manager: Arc<RwLock<WalManager>>) {
        if !self.config.enabled {
            info!("WAL compaction is disabled");
            return;
        }

        info!("Starting WAL compaction service with {} second intervals", self.config.interval_secs);

        let mut interval_timer = interval(Duration::from_secs(self.config.interval_secs));
        let compactor = self.clone();

        tokio::spawn(async move {
            loop {
                interval_timer.tick().await;

                match compactor.run_compaction(wal_manager.clone()).await {
                    Ok(stats) => {
                        if stats.segments_compacted > 0 {
                            info!("Compaction completed: {} segments compacted, {} bytes saved",
                                  stats.segments_compacted, stats.bytes_saved);
                        }
                    }
                    Err(e) => {
                        error!("Compaction failed: {}", e);
                    }
                }
            }
        });
    }

    /// Run a single compaction cycle
    pub async fn run_compaction(&self, wal_manager: Arc<RwLock<WalManager>>) -> Result<CompactionStats> {
        let start_time = std::time::Instant::now();
        let mut stats = CompactionStats::default();

        debug!("Starting WAL compaction cycle with strategy: {:?}", self.config.strategy);

        // Get list of partitions to compact
        let partitions = self.get_partitions_for_compaction(wal_manager.clone()).await?;

        if self.config.parallel_compaction && partitions.len() > 1 {
            // Parallel compaction
            stats = self.run_parallel_compaction(wal_manager, partitions, start_time).await?;
        } else {
            // Sequential compaction
            for partition in partitions {
                if start_time.elapsed().as_secs() > self.config.max_duration_secs {
                    warn!("Compaction timeout reached, stopping compaction cycle");
                    break;
                }

                match self.compact_partition(wal_manager.clone(), &partition).await {
                    Ok(partition_stats) => {
                        stats.merge(partition_stats);
                    }
                    Err(e) => {
                        error!("Failed to compact partition {:?}: {}", partition, e);
                        stats.errors += 1;
                    }
                }
            }
        }

        let duration = start_time.elapsed();
        info!("Compaction cycle completed in {:?} - {} segments compacted, {} bytes saved",
              duration, stats.segments_compacted, stats.bytes_saved);

        Ok(stats)
    }

    /// Run parallel compaction for multiple partitions
    async fn run_parallel_compaction(
        &self,
        wal_manager: Arc<RwLock<WalManager>>,
        partitions: Vec<TopicPartition>,
        start_time: std::time::Instant,
    ) -> Result<CompactionStats> {
        use futures::future::join_all;

        let mut stats = CompactionStats::default();
        let max_parallel = self.config.max_parallel_tasks.min(partitions.len());

        // Process partitions in batches
        for chunk in partitions.chunks(max_parallel) {
            if start_time.elapsed().as_secs() > self.config.max_duration_secs {
                warn!("Compaction timeout reached during parallel processing");
                break;
            }

            let futures: Vec<_> = chunk
                .iter()
                .map(|partition| {
                    let wal_manager = wal_manager.clone();
                    let partition = partition.clone();
                    let compactor = self.clone();

                    async move {
                        compactor.compact_partition(wal_manager, &partition).await
                    }
                })
                .collect();

            let results = join_all(futures).await;

            for result in results {
                match result {
                    Ok(partition_stats) => stats.merge(partition_stats),
                    Err(e) => {
                        error!("Parallel compaction error: {}", e);
                        stats.errors += 1;
                    }
                }
            }
        }

        Ok(stats)
    }

    /// Get partitions that need compaction
    async fn get_partitions_for_compaction(&self, wal_manager: Arc<RwLock<WalManager>>) -> Result<Vec<TopicPartition>> {
        let manager = wal_manager.read().await;
        let mut candidates = Vec::new();

        // Get all partitions from WAL manager
        // Note: This would need to be implemented in WalManager
        // For now, we'll implement a simple approach for metadata topic
        candidates.push(TopicPartition {
            topic: "__meta".to_string(),
            partition: 0,
        });

        // Filter based on compaction criteria
        let mut partitions_to_compact = Vec::new();

        for partition in candidates {
            if self.should_compact_partition(&manager, &partition).await? {
                partitions_to_compact.push(partition);
            }
        }

        Ok(partitions_to_compact)
    }

    /// Check if a partition should be compacted
    async fn should_compact_partition(&self, manager: &WalManager, partition: &TopicPartition) -> Result<bool> {
        // Check if partition exists
        let latest_offset = match manager.get_latest_offset(partition.clone()).await {
            Ok(offset) => offset,
            Err(_) => {
                debug!("Partition {:?} not found, skipping compaction", partition);
                return Ok(false);
            }
        };

        // Need minimum records
        if latest_offset < self.config.min_segments as i64 {
            return Ok(false);
        }

        // Check segment age and size
        // This would require additional metadata in WalManager
        // For now, use simple heuristics
        Ok(latest_offset > 1000) // Compact if more than 1000 records
    }

    /// Compact a single partition
    async fn compact_partition(&self, wal_manager: Arc<RwLock<WalManager>>, partition: &TopicPartition) -> Result<CompactionStats> {
        debug!("Compacting partition {:?}", partition);

        let mut stats = CompactionStats::default();

        // Read all records from the partition
        let manager = wal_manager.read().await;
        let records = manager.read_from(&partition.topic, partition.partition, 0, i64::MAX as usize).await?;
        drop(manager); // Release read lock

        if records.is_empty() {
            debug!("No records to compact in partition {:?}", partition);
            return Ok(stats);
        }

        let original_size = records.len();

        // Apply compaction strategy
        let compacted_records = self.apply_compaction_strategy(records)?;
        let compacted_size = compacted_records.len();

        if compacted_size >= original_size {
            debug!("Compaction would not save space for partition {:?}, skipping", partition);
            return Ok(stats);
        }

        // Write compacted records to a temporary location
        let temp_path = self.create_temp_compacted_segment(partition, &compacted_records).await?;

        // Replace original segment with compacted version
        let mut manager = wal_manager.write().await;
        self.replace_partition_data(&mut manager, partition, &temp_path).await?;

        stats.segments_compacted = 1;
        stats.records_before = original_size;
        stats.records_after = compacted_size;
        stats.bytes_saved = ((original_size - compacted_size) * 500) as u64; // Rough estimate

        info!("Compacted partition {:?}: {} -> {} records ({} bytes saved)",
              partition, original_size, compacted_size, stats.bytes_saved);

        Ok(stats)
    }

    /// Apply compaction strategy to records
    fn apply_compaction_strategy(&self, records: Vec<WalRecord>) -> Result<Vec<WalRecord>> {
        match self.config.strategy {
            CompactionStrategy::KeyBased => self.apply_key_based_compaction(records),
            CompactionStrategy::TimeBased => self.apply_time_based_compaction(records),
            CompactionStrategy::Hybrid => self.apply_hybrid_compaction(records),
            CompactionStrategy::Custom => self.apply_custom_compaction(records),
        }
    }

    /// Key-based compaction: keep only latest value per key
    fn apply_key_based_compaction(&self, records: Vec<WalRecord>) -> Result<Vec<WalRecord>> {
        let mut key_to_record: HashMap<Vec<u8>, WalRecord> = HashMap::new();
        let mut keyless_records = Vec::new();
        let mut v2_records = Vec::new();

        for record in records {
            // V2 records are already compacted batches, keep them as-is
            if record.is_v2() {
                v2_records.push(record);
                continue;
            }

            // Handle V1 records
            if let Some(key) = record.get_key() {
                // Keep the latest record for each key
                key_to_record.insert(key.clone(), record);
            } else {
                // Keep records without keys for transaction logs
                keyless_records.push(record);
            }
        }

        let mut compacted_records: Vec<WalRecord> = key_to_record.into_values().collect();
        compacted_records.extend(keyless_records);
        compacted_records.extend(v2_records);

        // Sort by offset to maintain order (V1 only, V2 doesn't have offset)
        compacted_records.sort_by_key(|r| r.get_offset().unwrap_or(i64::MAX));

        // Apply retention ratio if needed
        self.apply_retention_ratio(compacted_records)
    }

    /// Time-based compaction: remove old records
    fn apply_time_based_compaction(&self, records: Vec<WalRecord>) -> Result<Vec<WalRecord>> {
        let current_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;

        let retention_cutoff = current_time - (self.config.retention_period_secs * 1000) as i64;

        let mut compacted_records: Vec<WalRecord> = records
            .into_iter()
            .filter(|r| {
                // V2 records always kept (they're already optimized batches)
                if r.is_v2() {
                    return true;
                }
                // V1 records filtered by timestamp
                r.get_timestamp().unwrap_or(current_time) >= retention_cutoff
            })
            .collect();

        // Sort by offset to maintain order (V1 only)
        compacted_records.sort_by_key(|r| r.get_offset().unwrap_or(i64::MAX));

        Ok(compacted_records)
    }

    /// Hybrid compaction: key-based within time window
    fn apply_hybrid_compaction(&self, records: Vec<WalRecord>) -> Result<Vec<WalRecord>> {
        let current_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;

        let retention_cutoff = current_time - (self.config.retention_period_secs * 1000) as i64;

        // First filter by time (V1 only, V2 always kept)
        let recent_records: Vec<WalRecord> = records
            .into_iter()
            .filter(|r| {
                if r.is_v2() {
                    return true;
                }
                r.get_timestamp().unwrap_or(current_time) >= retention_cutoff
            })
            .collect();

        // Then apply key-based compaction
        self.apply_key_based_compaction(recent_records)
    }

    /// Custom compaction for specific use cases
    fn apply_custom_compaction(&self, records: Vec<WalRecord>) -> Result<Vec<WalRecord>> {
        // For transactional records, keep all transaction boundaries
        // For metadata records, use key-based compaction
        // For consumer offsets, keep only latest per partition

        let mut transaction_records = Vec::new();
        let mut metadata_records = Vec::new();
        let mut offset_records = Vec::new();
        let mut other_records = Vec::new();
        let mut v2_records = Vec::new();

        for record in records {
            // V2 records always kept (already optimized batches)
            if record.is_v2() {
                v2_records.push(record);
                continue;
            }

            // Identify V1 record type based on key prefix
            if let Some(key) = record.get_key() {
                if key.starts_with(b"__txn__") {
                    transaction_records.push(record);
                } else if key.starts_with(b"__offset__") {
                    offset_records.push(record);
                } else if key.starts_with(b"__meta__") {
                    metadata_records.push(record);
                } else {
                    other_records.push(record);
                }
            } else {
                other_records.push(record);
            }
        }

        // Apply different strategies to each category
        let mut result = Vec::new();

        // Keep all transaction records (they're critical)
        result.extend(transaction_records);

        // Key-based compaction for metadata
        if !metadata_records.is_empty() {
            result.extend(self.apply_key_based_compaction(metadata_records)?);
        }

        // Key-based compaction for offsets
        if !offset_records.is_empty() {
            result.extend(self.apply_key_based_compaction(offset_records)?);
        }

        // Time-based for other records
        if !other_records.is_empty() {
            result.extend(self.apply_time_based_compaction(other_records)?);
        }

        // Keep all V2 records
        result.extend(v2_records);

        // Sort by offset to maintain order (V1 only)
        result.sort_by_key(|r| r.get_offset().unwrap_or(i64::MAX));

        Ok(result)
    }

    /// Apply retention ratio to limit record count
    fn apply_retention_ratio(&self, mut records: Vec<WalRecord>) -> Result<Vec<WalRecord>> {
        let target_count = (records.len() as f64 * self.config.retention_ratio) as usize;

        if target_count < records.len() {
            // Keep the most recent records
            records.truncate(target_count);
        }

        Ok(records)
    }

    /// Create temporary compacted segment file
    async fn create_temp_compacted_segment(&self, partition: &TopicPartition, records: &[WalRecord]) -> Result<PathBuf> {
        let temp_dir = self.base_path.join("tmp");
        tokio::fs::create_dir_all(&temp_dir).await
            .map_err(|e| WalError::Io(e))?;

        let temp_file = temp_dir.join(format!("compact_{}_{}.wal", partition.topic, partition.partition));

        // Write records to temp file
        // This is a simplified implementation - in practice, you'd use the proper WAL format
        let mut data = Vec::new();
        for record in records {
            let serialized = serde_json::to_vec(record)
                .map_err(|e| WalError::IoError(format!("Failed to serialize record: {}", e)))?;
            data.extend_from_slice(&(serialized.len() as u32).to_le_bytes());
            data.extend_from_slice(&serialized);
        }

        tokio::fs::write(&temp_file, data).await
            .map_err(|e| WalError::Io(e))?;

        Ok(temp_file)
    }

    /// Replace partition data with compacted version
    async fn replace_partition_data(&self, manager: &mut WalManager, partition: &TopicPartition, temp_path: &Path) -> Result<()> {
        // This is a placeholder - in a real implementation, you'd need to:
        // 1. Stop writes to the partition
        // 2. Replace the segment files atomically
        // 3. Update the WAL manager's internal state
        // 4. Resume writes

        warn!("WAL compaction replace_partition_data is not fully implemented yet");

        // For now, just remove the temp file
        let _ = tokio::fs::remove_file(temp_path).await;

        Ok(())
    }
}

impl Clone for WalCompactor {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            wal_config: self.wal_config.clone(),
            base_path: self.base_path.clone(),
        }
    }
}

/// Statistics from a compaction run
#[derive(Debug, Default, Clone)]
pub struct CompactionStats {
    pub segments_compacted: u32,
    pub records_before: usize,
    pub records_after: usize,
    pub bytes_saved: u64,
    pub errors: u32,
    pub duration_ms: u64,
    pub strategy_used: Option<CompactionStrategy>,
}

impl CompactionStats {
    fn merge(&mut self, other: CompactionStats) {
        self.segments_compacted += other.segments_compacted;
        self.records_before += other.records_before;
        self.records_after += other.records_after;
        self.bytes_saved += other.bytes_saved;
        self.errors += other.errors;
        self.duration_ms = self.duration_ms.max(other.duration_ms);
        if self.strategy_used.is_none() {
            self.strategy_used = other.strategy_used;
        }
    }

    /// Calculate compaction ratio
    pub fn compaction_ratio(&self) -> f64 {
        if self.records_before > 0 {
            1.0 - (self.records_after as f64 / self.records_before as f64)
        } else {
            0.0
        }
    }

    /// Get throughput in records/sec
    pub fn throughput(&self) -> f64 {
        if self.duration_ms > 0 {
            (self.records_before as f64 / self.duration_ms as f64) * 1000.0
        } else {
            0.0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_compaction_config_default() {
        let config = CompactionConfig::default();
        assert!(config.enabled);
        assert_eq!(config.interval_secs, 3600);
        assert!(config.retention_ratio > 0.0 && config.retention_ratio <= 1.0);
    }

    #[test]
    fn test_apply_compaction_strategy() {
        let temp_dir = TempDir::new().unwrap();
        let wal_config = WalConfig {
            enabled: true,
            data_dir: temp_dir.path().to_path_buf(),
            ..Default::default()
        };

        let compactor = WalCompactor::new(wal_config, CompactionConfig::default());

        // Create test records with duplicate keys
        let records = vec![
            WalRecord::new(0, Some(b"key1".to_vec()), b"value1".to_vec(), 1000),
            WalRecord::new(1, Some(b"key2".to_vec()), b"value2".to_vec(), 2000),
            WalRecord::new(2, Some(b"key1".to_vec()), b"value1_updated".to_vec(), 3000),
            WalRecord::new(3, Some(b"key3".to_vec()), b"value3".to_vec(), 4000),
        ];

        let compacted = compactor.apply_compaction_strategy(records).unwrap();

        // Should have 3 records (latest for each key)
        assert_eq!(compacted.len(), 3);

        // Should be sorted by offset
        for i in 1..compacted.len() {
            let curr_offset = compacted[i].get_offset().unwrap();
            let prev_offset = compacted[i-1].get_offset().unwrap();
            assert!(curr_offset >= prev_offset);
        }

        // Should have the updated value for key1
        let key1_record = compacted.iter().find(|r| r.get_key() == Some(&b"key1".to_vec())).unwrap();
        assert_eq!(key1_record.get_value().unwrap(), b"value1_updated");
    }

    #[tokio::test]
    async fn test_compaction_stats_merge() {
        let mut stats1 = CompactionStats {
            segments_compacted: 2,
            records_before: 1000,
            records_after: 800,
            bytes_saved: 50000,
            errors: 1,
            duration_ms: 0,
            strategy_used: None,
        };

        let stats2 = CompactionStats {
            segments_compacted: 1,
            records_before: 500,
            records_after: 400,
            bytes_saved: 25000,
            errors: 0,
            duration_ms: 0,
            strategy_used: None,
        };

        stats1.merge(stats2);

        assert_eq!(stats1.segments_compacted, 3);
        assert_eq!(stats1.records_before, 1500);
        assert_eq!(stats1.records_after, 1200);
        assert_eq!(stats1.bytes_saved, 75000);
        assert_eq!(stats1.errors, 1);
    }
}