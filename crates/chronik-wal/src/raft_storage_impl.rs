//! RaftLogStorage implementation on GroupCommitWal
//!
//! This module implements the RaftLogStorage trait using the existing GroupCommitWal
//! infrastructure. This allows Chronik to reuse its WAL system for Raft consensus
//! without duplicate storage.
//!
//! ## Implementation Strategy
//!
//! 1. **Write Path**: RaftEntry → bincode serialize → WalRecord::V2 → GroupCommitWal
//! 2. **Read Path**: Scan WAL segments → filter by index range → deserialize RaftEntry
//! 3. **Index Tracking**: Use in-memory index for first/last lookups (O(1))
//! 4. **Compaction**: Leverage WAL segment rotation for log compaction
//!
//! ## Concurrency
//!
//! - All operations use GroupCommitWal's internal locking
//! - Index tracking uses DashMap for concurrent access
//! - No additional synchronization needed
//!
//! ## Durability
//!
//! - All appends use acks=1 (wait for fsync)
//! - Same durability guarantees as Kafka message writes
//! - Crash recovery via WAL segment scanning

use async_trait::async_trait;
use chronik_raft::{RaftEntry, RaftLogStorage, RaftError, Result, RAFT_TOPIC, RAFT_PARTITION};
use dashmap::DashMap;
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;
use tracing::{debug, info, warn};

use crate::group_commit::GroupCommitWal;
use crate::record::WalRecord;
use crate::error::WalError;

/// Convert WalError to RaftError
impl From<WalError> for RaftError {
    fn from(e: WalError) -> Self {
        RaftError::StorageError(e.to_string())
    }
}

/// RaftLogStorage implementation using GroupCommitWal
pub struct RaftWalStorage {
    /// Underlying GroupCommitWal
    wal: Arc<GroupCommitWal>,

    /// In-memory index tracking (index → segment_id)
    /// This cache avoids scanning all segments for index lookups
    index_cache: Arc<DashMap<u64, u64>>,

    /// First and last index tracking
    /// Cached for O(1) first_index() and last_index()
    first_index: Arc<parking_lot::RwLock<u64>>,
    last_index: Arc<parking_lot::RwLock<u64>>,
}

impl RaftWalStorage {
    /// Create a new RaftWalStorage
    pub fn new(wal: Arc<GroupCommitWal>) -> Self {
        Self {
            wal,
            index_cache: Arc::new(DashMap::new()),
            first_index: Arc::new(parking_lot::RwLock::new(0)),
            last_index: Arc::new(parking_lot::RwLock::new(0)),
        }
    }

    /// Recover index cache from existing WAL segments
    ///
    /// This scans all WAL files for the __raft topic and rebuilds the index cache.
    /// Should be called once during initialization.
    pub async fn recover(&self, data_dir: &Path) -> Result<()> {
        info!("Recovering Raft log index from WAL segments");

        let raft_dir = data_dir.join(RAFT_TOPIC).join(RAFT_PARTITION.to_string());

        if !raft_dir.exists() {
            info!("No existing Raft log found, starting fresh");
            return Ok(());
        }

        // Scan WAL segment files
        let mut entries = tokio::fs::read_dir(&raft_dir).await?;
        let mut segment_files = Vec::new();

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("log") {
                segment_files.push(path);
            }
        }

        // Sort by segment ID
        segment_files.sort();

        let mut recovered_count = 0;
        let mut min_index = u64::MAX;
        let mut max_index = 0u64;

        for segment_path in segment_files {
            // Read segment file
            let data = tokio::fs::read(&segment_path).await?;

            // Parse WAL records
            let mut offset = 0;
            while offset < data.len() {
                if data.len() - offset < 12 {
                    // Not enough data for header
                    break;
                }

                // Try to parse WalRecord
                match WalRecord::from_bytes(&data[offset..]) {
                    Ok(record) => {
                        if let WalRecord::V2 { base_offset, .. } = record {
                            let index = base_offset as u64;

                            // Extract segment ID from path (format: wal_PARTITION_SEGMENT.log)
                            let segment_id = segment_path
                                .file_stem()
                                .and_then(|s| s.to_str())
                                .and_then(|s| s.split('_').nth(2))
                                .and_then(|s| s.parse::<u64>().ok())
                                .unwrap_or(0);

                            self.index_cache.insert(index, segment_id);

                            min_index = min_index.min(index);
                            max_index = max_index.max(index);
                            recovered_count += 1;
                        }

                        // Move to next record
                        let record_len = record.get_length() as usize + 12; // 12-byte header
                        offset += record_len;
                    }
                    Err(e) => {
                        warn!(
                            "Failed to parse WAL record at offset {} in {:?}: {}",
                            offset, segment_path, e
                        );
                        break;
                    }
                }
            }
        }

        if recovered_count > 0 {
            *self.first_index.write() = min_index;
            *self.last_index.write() = max_index;
            info!(
                "Recovered {} Raft log entries (index range: {}-{})",
                recovered_count, min_index, max_index
            );
        }

        Ok(())
    }

    /// Update index tracking after append
    fn update_index(&self, index: u64, segment_id: u64) {
        self.index_cache.insert(index, segment_id);

        let mut first = self.first_index.write();
        let mut last = self.last_index.write();

        if *first == 0 || index < *first {
            *first = index;
        }
        if index > *last {
            *last = index;
        }
    }

    /// Read Raft entries from WAL segments
    async fn read_entries_from_wal(&self, range: Range<u64>) -> Result<Vec<RaftEntry>> {
        if range.start >= range.end {
            return Ok(Vec::new());
        }

        // For now, we'll scan segments sequentially
        // Future optimization: use index_cache to find exact segments
        let wal_dir = self
            .wal
            .base_dir()
            .join(RAFT_TOPIC)
            .join(RAFT_PARTITION.to_string());

        if !wal_dir.exists() {
            return Ok(Vec::new());
        }

        let mut entries = tokio::fs::read_dir(&wal_dir).await?;
        let mut segment_files = Vec::new();

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("log") {
                segment_files.push(path);
            }
        }

        segment_files.sort();

        let mut result = Vec::new();

        for segment_path in segment_files {
            let data = tokio::fs::read(&segment_path).await?;
            let mut offset = 0;

            while offset < data.len() {
                if data.len() - offset < 12 {
                    break;
                }

                match WalRecord::from_bytes(&data[offset..]) {
                    Ok(record) => {
                        let record_len = record.get_length() as usize + 12;

                        if let WalRecord::V2 {
                            base_offset,
                            canonical_data,
                            ..
                        } = record
                        {
                            let index = base_offset as u64;

                            // Check if this entry is in the requested range
                            if index >= range.start && index < range.end {
                                match RaftEntry::from_bytes(&canonical_data) {
                                    Ok(entry) => result.push(entry),
                                    Err(e) => {
                                        warn!("Failed to deserialize RaftEntry at index {}: {}", index, e);
                                    }
                                }
                            }

                            // If we've passed the end of the range, we can stop
                            if index >= range.end {
                                return Ok(result);
                            }
                        }

                        offset += record_len;
                    }
                    Err(e) => {
                        warn!(
                            "Failed to parse WAL record at offset {} in {:?}: {}",
                            offset, segment_path, e
                        );
                        break;
                    }
                }
            }
        }

        Ok(result)
    }
}

#[async_trait]
impl RaftLogStorage for RaftWalStorage {
    async fn append(&self, entries: Vec<RaftEntry>) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }

        debug!("Appending {} Raft entries to WAL", entries.len());

        for entry in entries {
            let index = entry.index;
            let canonical_data = entry.to_bytes()?;

            // Create V2 WAL record
            let wal_record = WalRecord::new_v2(
                RAFT_TOPIC.to_string(),
                RAFT_PARTITION,
                canonical_data,
                index as i64,
                index as i64,
                1, // Single entry per record
            );

            // Append with acks=1 (wait for fsync)
            self.wal
                .append(RAFT_TOPIC.to_string(), RAFT_PARTITION, wal_record, 1)
                .await?;

            // Update index tracking
            // TODO: Get actual segment_id from WAL
            self.update_index(index, 0);
        }

        Ok(())
    }

    async fn get(&self, index: u64) -> Result<Option<RaftEntry>> {
        debug!("Getting Raft entry at index {}", index);

        let entries = self.read_entries_from_wal(index..index + 1).await?;

        Ok(entries.into_iter().next())
    }

    async fn range(&self, start: u64, end: u64) -> Result<Vec<RaftEntry>> {
        debug!("Getting Raft entries in range {}..{}", start, end);

        if start > end {
            return Err(RaftError::InvalidRange { start, end });
        }

        self.read_entries_from_wal(start..end).await
    }

    async fn first_index(&self) -> Result<u64> {
        Ok(*self.first_index.read())
    }

    async fn last_index(&self) -> Result<u64> {
        Ok(*self.last_index.read())
    }

    async fn truncate_after(&self, index: u64) -> Result<()> {
        info!("Truncating Raft log after index {}", index);

        // Remove from index cache
        let last = *self.last_index.read();
        for idx in (index + 1)..=last {
            self.index_cache.remove(&idx);
        }

        // Update last_index
        let mut last_idx = self.last_index.write();
        if *last_idx > index {
            *last_idx = index;
        }

        // Note: Actual WAL segment deletion happens through segment rotation
        // The segments will be cleaned up by WalIndexer after they're sealed
        info!("Raft log truncation: removed entries after index {}", index);

        Ok(())
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::group_commit::GroupCommitConfig;
    use std::path::PathBuf;
    use tempfile::TempDir;

    async fn create_test_storage() -> (RaftWalStorage, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let wal = Arc::new(GroupCommitWal::new(
            temp_dir.path().to_path_buf(),
            GroupCommitConfig::default(),
        ));
        let storage = RaftWalStorage::new(wal);
        (storage, temp_dir)
    }

    #[tokio::test]
    async fn test_append_and_get_entry() {
        let (storage, _temp) = create_test_storage().await;

        let entry = RaftEntry::new(1, 100, b"test-data".to_vec());

        storage.append(vec![entry.clone()]).await.unwrap();

        let retrieved = storage.get(100).await.unwrap();
        assert_eq!(retrieved, Some(entry));
    }

    #[tokio::test]
    async fn test_get_missing_entry() {
        let (storage, _temp) = create_test_storage().await;

        let retrieved = storage.get(999).await.unwrap();
        assert_eq!(retrieved, None);
    }

    #[tokio::test]
    async fn test_append_multiple_entries() {
        let (storage, _temp) = create_test_storage().await;

        let entries = vec![
            RaftEntry::new(1, 100, b"data1".to_vec()),
            RaftEntry::new(1, 101, b"data2".to_vec()),
            RaftEntry::new(2, 102, b"data3".to_vec()),
        ];

        storage.append(entries.clone()).await.unwrap();

        let retrieved = storage.range(100, 103).await.unwrap();
        assert_eq!(retrieved.len(), 3);
        assert_eq!(retrieved, entries);
    }

    #[tokio::test]
    async fn test_first_and_last_index() {
        let (storage, _temp) = create_test_storage().await;

        // Empty log
        assert_eq!(storage.first_index().await.unwrap(), 0);
        assert_eq!(storage.last_index().await.unwrap(), 0);

        // Add entries
        let entries = vec![
            RaftEntry::new(1, 100, b"data1".to_vec()),
            RaftEntry::new(1, 101, b"data2".to_vec()),
            RaftEntry::new(1, 102, b"data3".to_vec()),
        ];

        storage.append(entries).await.unwrap();

        assert_eq!(storage.first_index().await.unwrap(), 100);
        assert_eq!(storage.last_index().await.unwrap(), 102);
    }

    #[tokio::test]
    async fn test_get_entries_range() {
        let (storage, _temp) = create_test_storage().await;

        let entries = vec![
            RaftEntry::new(1, 100, b"data1".to_vec()),
            RaftEntry::new(1, 101, b"data2".to_vec()),
            RaftEntry::new(1, 102, b"data3".to_vec()),
            RaftEntry::new(2, 103, b"data4".to_vec()),
            RaftEntry::new(2, 104, b"data5".to_vec()),
        ];

        storage.append(entries).await.unwrap();

        // Get middle range
        let retrieved = storage.range(101, 104).await.unwrap();
        assert_eq!(retrieved.len(), 3);
        assert_eq!(retrieved[0].index, 101);
        assert_eq!(retrieved[1].index, 102);
        assert_eq!(retrieved[2].index, 103);
    }

    #[tokio::test]
    async fn test_truncate_after() {
        let (storage, _temp) = create_test_storage().await;

        let entries = vec![
            RaftEntry::new(1, 100, b"data1".to_vec()),
            RaftEntry::new(1, 101, b"data2".to_vec()),
            RaftEntry::new(1, 102, b"data3".to_vec()),
            RaftEntry::new(2, 103, b"data4".to_vec()),
            RaftEntry::new(2, 104, b"data5".to_vec()),
        ];

        storage.append(entries).await.unwrap();

        // Truncate after 101 (removes 102, 103, 104)
        storage.truncate_after(101).await.unwrap();

        // last_index should now be 101
        assert_eq!(storage.first_index().await.unwrap(), 100);
        assert_eq!(storage.last_index().await.unwrap(), 101);
    }

    #[tokio::test]
    async fn test_invalid_range() {
        let (storage, _temp) = create_test_storage().await;

        // start > end should error
        let result = storage.range(100, 99).await;
        assert!(result.is_err());
    }
}
