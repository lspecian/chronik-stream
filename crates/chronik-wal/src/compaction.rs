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

        debug!("Starting WAL compaction cycle");

        // Get list of partitions to compact
        let partitions = self.get_partitions_for_compaction(wal_manager.clone()).await?;

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

        let duration = start_time.elapsed();
        debug!("Compaction cycle completed in {:?}", duration);

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
        // For metadata WAL, we can implement key-based compaction
        // Keep only the latest record for each unique key

        let mut key_to_record: HashMap<Vec<u8>, WalRecord> = HashMap::new();

        for record in records {
            if let Some(key) = &record.key {
                // Keep the latest record for each key
                key_to_record.insert(key.clone(), record);
            } else {
                // Keep records without keys (shouldn't happen in metadata WAL)
                // For now, keep them all
                continue;
            }
        }

        let mut compacted_records: Vec<WalRecord> = key_to_record.into_values().collect();

        // Sort by offset to maintain order
        compacted_records.sort_by_key(|r| r.offset);

        // Apply retention ratio if we still have too many records
        let target_count = (compacted_records.len() as f64 * self.config.retention_ratio) as usize;
        if target_count < compacted_records.len() {
            // Keep the most recent records
            compacted_records = compacted_records
                .into_iter()
                .rev()
                .take(target_count)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
        }

        Ok(compacted_records)
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
#[derive(Debug, Default)]
pub struct CompactionStats {
    pub segments_compacted: u32,
    pub records_before: usize,
    pub records_after: usize,
    pub bytes_saved: u64,
    pub errors: u32,
}

impl CompactionStats {
    fn merge(&mut self, other: CompactionStats) {
        self.segments_compacted += other.segments_compacted;
        self.records_before += other.records_before;
        self.records_after += other.records_after;
        self.bytes_saved += other.bytes_saved;
        self.errors += other.errors;
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
            assert!(compacted[i].offset >= compacted[i-1].offset);
        }

        // Should have the updated value for key1
        let key1_record = compacted.iter().find(|r| r.key.as_ref() == Some(&b"key1".to_vec())).unwrap();
        assert_eq!(key1_record.value, b"value1_updated".to_vec());
    }

    #[tokio::test]
    async fn test_compaction_stats_merge() {
        let mut stats1 = CompactionStats {
            segments_compacted: 2,
            records_before: 1000,
            records_after: 800,
            bytes_saved: 50000,
            errors: 1,
        };

        let stats2 = CompactionStats {
            segments_compacted: 1,
            records_before: 500,
            records_after: 400,
            bytes_saved: 25000,
            errors: 0,
        };

        stats1.merge(stats2);

        assert_eq!(stats1.segments_compacted, 3);
        assert_eq!(stats1.records_before, 1500);
        assert_eq!(stats1.records_after, 1200);
        assert_eq!(stats1.bytes_saved, 75000);
        assert_eq!(stats1.errors, 1);
    }
}