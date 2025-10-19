//! WAL-backed Raft log storage
//!
//! This module implements persistent Raft log storage using Chronik's WAL
//! (Write-Ahead Log) system, providing durability and crash recovery.
//!
//! **Note**: This module requires the `wal-storage` feature to be enabled.

use chronik_raft::{RaftEntry, RaftError, RaftLogStorage, Result as RaftResult};
use async_trait::async_trait;

use chronik_wal::{GroupCommitWal, WalConfig};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{debug, info, warn};

/// Topic name for Raft internal logs
pub const RAFT_LOG_TOPIC: &str = "__raft_logs";

/// Persistent state that must survive crashes (Raft paper §5.2)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HardState {
    /// Latest term server has seen (increases monotonically)
    pub term: u64,

    /// Candidate that received vote in current term (or None)
    pub vote: Option<u64>,

    /// Index of highest log entry known to be committed
    pub commit: u64,
}

impl Default for HardState {
    fn default() -> Self {
        Self {
            term: 0,
            vote: None,
            commit: 0,
        }
    }
}

/// WAL-backed Raft log storage implementation
pub struct WalRaftStorage {
    /// Topic name (identifies the Raft group)
    topic: String,

    /// Partition ID
    partition: i32,

    /// WAL instance for durable log storage
    wal: Arc<GroupCommitWal>,

    /// In-memory index: log_index -> (wal_offset, term)
    /// Used for fast lookups without scanning WAL
    index: Arc<RwLock<BTreeMap<u64, (u64, u64)>>>,

    /// Persistent Raft state (term, vote, commit)
    hard_state: Arc<RwLock<HardState>>,

    /// Directory for state file
    state_dir: PathBuf,
}

impl WalRaftStorage {
    /// Create a new WAL-backed Raft storage
    ///
    /// # Arguments
    /// * `topic` - Topic name (Raft group identifier)
    /// * `partition` - Partition ID
    /// * `wal` - GroupCommitWal instance
    pub async fn new(topic: String, partition: i32, wal: Arc<GroupCommitWal>) -> RaftResult<Self> {
        let data_dir = wal.data_dir().to_path_buf();

        // State directory for hard state persistence
        let state_dir = data_dir.join("raft_state").join(&topic).join(partition.to_string());
        tokio::fs::create_dir_all(&state_dir)
            .await
            .map_err(|e| {
                RaftError::StorageError(format!("Failed to create state dir: {}", e))
            })?;

        let mut storage = Self {
            topic,
            partition,
            wal,
            index: Arc::new(RwLock::new(BTreeMap::new())),
            hard_state: Arc::new(RwLock::new(HardState::default())),
            state_dir,
        };

        // Recover from disk on startup
        storage.recover().await?;

        Ok(storage)
    }

    /// Recover Raft log from WAL on startup
    async fn recover(&mut self) -> RaftResult<()> {
        info!(
            "Recovering Raft log for {}/{} from WAL",
            self.topic, self.partition
        );

        // Recover hard state (term, vote, commit)
        self.recover_hard_state().await?;

        // Rebuild index from WAL
        let records = self
            .wal
            .read_all()
            .await
            .map_err(|e| RaftError::StorageError(format!("Failed to read WAL: {}", e)))?;

        let mut index = self.index.write();
        let mut recovered = 0;

        for (wal_offset, record) in records.iter().enumerate() {
            // Parse RaftEntry from record data
            if let Ok(entry) = RaftEntry::from_bytes(&record.data) {
                index.insert(entry.index, (wal_offset as u64, entry.term));
                recovered += 1;
            } else {
                warn!(
                    "Skipping corrupted Raft entry at WAL offset {}",
                    wal_offset
                );
            }
        }

        drop(index);

        info!(
            "Recovered {} Raft log entries for {}/{}",
            recovered, self.topic, self.partition
        );

        Ok(())
    }

    /// Recover hard state from disk
    async fn recover_hard_state(&self) -> RaftResult<()> {
        let state_path = self.state_dir.join("hard_state.bin");

        if !state_path.exists() {
            debug!("No existing hard state, using defaults");
            return Ok(());
        }

        let data = tokio::fs::read(&state_path)
            .await
            .map_err(|e| RaftError::StorageError(format!("Failed to read state: {}", e)))?;

        let state: HardState = bincode::deserialize(&data)
            .map_err(|e| RaftError::SerializationError(e.to_string()))?;

        *self.hard_state.write() = state.clone();

        info!(
            "Recovered hard state for {}/{}: term={}, vote={:?}, commit={}",
            self.topic, self.partition, state.term, state.vote, state.commit
        );

        Ok(())
    }

    /// Persist hard state to disk
    async fn persist_hard_state(&self) -> RaftResult<()> {
        let state = self.hard_state.read().clone();
        let data = bincode::serialize(&state)
            .map_err(|e| RaftError::SerializationError(e.to_string()))?;

        let state_path = self.state_dir.join("hard_state.bin");
        tokio::fs::write(&state_path, data)
            .await
            .map_err(|e| RaftError::StorageError(format!("Failed to write state: {}", e)))?;

        debug!(
            "Persisted hard state: term={}, vote={:?}, commit={}",
            state.term, state.vote, state.commit
        );

        Ok(())
    }

    /// Get current hard state (term, vote, commit)
    pub fn hard_state(&self) -> HardState {
        self.hard_state.read().clone()
    }

    /// Set hard state and persist to disk
    pub async fn set_hard_state(&self, state: HardState) -> RaftResult<()> {
        *self.hard_state.write() = state.clone();
        self.persist_hard_state().await?;
        Ok(())
    }

    /// Get current term
    pub fn term(&self) -> u64 {
        self.hard_state.read().term
    }

    /// Set term and persist
    pub async fn set_term(&self, term: u64) -> RaftResult<()> {
        self.hard_state.write().term = term;
        self.persist_hard_state().await
    }

    /// Get vote (candidate voted for in current term)
    pub fn vote(&self) -> Option<u64> {
        self.hard_state.read().vote
    }

    /// Set vote and persist
    pub async fn set_vote(&self, vote: Option<u64>) -> RaftResult<()> {
        self.hard_state.write().vote = vote;
        self.persist_hard_state().await
    }

    /// Get commit index
    pub fn commit(&self) -> u64 {
        self.hard_state.read().commit
    }

    /// Set commit index and persist
    pub async fn set_commit(&self, commit: u64) -> RaftResult<()> {
        self.hard_state.write().commit = commit;
        self.persist_hard_state().await
    }
}

#[async_trait]
impl RaftLogStorage for WalRaftStorage {
    async fn append(&self, entries: Vec<RaftEntry>) -> RaftResult<()> {
        if entries.is_empty() {
            return Ok(());
        }

        let mut index = self.index.write();
        let mut wal_offset = self.wal.next_offset().await;

        for entry in entries {
            // Serialize entry
            let data = entry.to_bytes()?;

            // Create WAL record
            let record = WalRecord::new_v2(entry.index as i64, self.partition, data);

            // Append to WAL (durable)
            self.wal
                .append_record(record)
                .await
                .map_err(|e| RaftError::StorageError(format!("WAL append failed: {}", e)))?;

            // Update index
            index.insert(entry.index, (wal_offset, entry.term));
            wal_offset += 1;

            debug!(
                "Appended Raft entry: index={}, term={}, wal_offset={}",
                entry.index, entry.term, wal_offset
            );
        }

        Ok(())
    }

    async fn get(&self, index: u64) -> RaftResult<Option<RaftEntry>> {
        let index_map = self.index.read();
        let (wal_offset, _term) = match index_map.get(&index) {
            Some(val) => *val,
            None => return Ok(None),
        };
        drop(index_map);

        // Read from WAL
        let records = self
            .wal
            .read_all()
            .await
            .map_err(|e| RaftError::StorageError(format!("WAL read failed: {}", e)))?;

        if wal_offset as usize >= records.len() {
            return Ok(None);
        }

        let record = &records[wal_offset as usize];
        let entry = RaftEntry::from_bytes(&record.data)?;

        Ok(Some(entry))
    }

    async fn range(&self, start: u64, end: u64) -> RaftResult<Vec<RaftEntry>> {
        if start > end {
            return Err(RaftError::InvalidRange { start, end });
        }

        let index_map = self.index.read();
        let offsets: Vec<_> = index_map
            .range(start..end)
            .map(|(_, (offset, _))| *offset)
            .collect();
        drop(index_map);

        if offsets.is_empty() {
            return Ok(Vec::new());
        }

        // Read all records from WAL
        let records = self
            .wal
            .read_all()
            .await
            .map_err(|e| RaftError::StorageError(format!("WAL read failed: {}", e)))?;

        let mut entries = Vec::with_capacity(offsets.len());
        for offset in offsets {
            if (offset as usize) < records.len() {
                if let Ok(entry) = RaftEntry::from_bytes(&records[offset as usize].data) {
                    entries.push(entry);
                }
            }
        }

        Ok(entries)
    }

    async fn first_index(&self) -> RaftResult<u64> {
        let index = self.index.read();
        Ok(index.keys().next().copied().unwrap_or(0))
    }

    async fn last_index(&self) -> RaftResult<u64> {
        let index = self.index.read();
        Ok(index.keys().next_back().copied().unwrap_or(0))
    }

    async fn truncate_after(&self, index: u64) -> RaftResult<()> {
        let mut index_map = self.index.write();

        // Remove entries after index
        index_map.retain(|&k, _| k <= index);

        debug!("Truncated Raft log after index {}", index);

        // Note: We don't actually truncate the WAL file here for safety.
        // The index acts as the source of truth for which entries are valid.
        // WAL compaction/cleanup happens separately in background.

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_create_wal_storage() {
        let temp_dir = TempDir::new().unwrap();
        let storage = WalRaftStorage::new("test-topic".to_string(), 0, temp_dir.path())
            .await
            .unwrap();

        assert_eq!(storage.topic, "test-topic");
        assert_eq!(storage.partition, 0);
    }

    #[tokio::test]
    async fn test_append_and_get() {
        let temp_dir = TempDir::new().unwrap();
        let storage = WalRaftStorage::new("test-topic".to_string(), 0, temp_dir.path())
            .await
            .unwrap();

        // Append entries
        let entries = vec![
            RaftEntry::new(1, 1, b"entry1".to_vec()),
            RaftEntry::new(1, 2, b"entry2".to_vec()),
            RaftEntry::new(2, 3, b"entry3".to_vec()),
        ];

        storage.append(entries.clone()).await.unwrap();

        // Get individual entries
        let entry1 = storage.get(1).await.unwrap().unwrap();
        assert_eq!(entry1.index, 1);
        assert_eq!(entry1.term, 1);
        assert_eq!(entry1.data, b"entry1");

        let entry3 = storage.get(3).await.unwrap().unwrap();
        assert_eq!(entry3.index, 3);
        assert_eq!(entry3.term, 2);
    }

    #[tokio::test]
    async fn test_range() {
        let temp_dir = TempDir::new().unwrap();
        let storage = WalRaftStorage::new("test-topic".to_string(), 0, temp_dir.path())
            .await
            .unwrap();

        // Append entries
        let entries = vec![
            RaftEntry::new(1, 1, b"entry1".to_vec()),
            RaftEntry::new(1, 2, b"entry2".to_vec()),
            RaftEntry::new(2, 3, b"entry3".to_vec()),
            RaftEntry::new(2, 4, b"entry4".to_vec()),
        ];

        storage.append(entries).await.unwrap();

        // Get range [2, 4)
        let range = storage.range(2, 4).await.unwrap();
        assert_eq!(range.len(), 2);
        assert_eq!(range[0].index, 2);
        assert_eq!(range[1].index, 3);
    }

    #[tokio::test]
    async fn test_first_last_index() {
        let temp_dir = TempDir::new().unwrap();
        let storage = WalRaftStorage::new("test-topic".to_string(), 0, temp_dir.path())
            .await
            .unwrap();

        // Empty log
        assert_eq!(storage.first_index().await.unwrap(), 0);
        assert_eq!(storage.last_index().await.unwrap(), 0);

        // After appending
        let entries = vec![
            RaftEntry::new(1, 5, b"entry5".to_vec()),
            RaftEntry::new(1, 6, b"entry6".to_vec()),
            RaftEntry::new(2, 7, b"entry7".to_vec()),
        ];

        storage.append(entries).await.unwrap();

        assert_eq!(storage.first_index().await.unwrap(), 5);
        assert_eq!(storage.last_index().await.unwrap(), 7);
    }

    #[tokio::test]
    async fn test_truncate_after() {
        let temp_dir = TempDir::new().unwrap();
        let storage = WalRaftStorage::new("test-topic".to_string(), 0, temp_dir.path())
            .await
            .unwrap();

        // Append entries
        let entries = vec![
            RaftEntry::new(1, 1, b"entry1".to_vec()),
            RaftEntry::new(1, 2, b"entry2".to_vec()),
            RaftEntry::new(2, 3, b"entry3".to_vec()),
            RaftEntry::new(2, 4, b"entry4".to_vec()),
        ];

        storage.append(entries).await.unwrap();

        // Truncate after index 2
        storage.truncate_after(2).await.unwrap();

        // Verify entries 3 and 4 are gone
        assert_eq!(storage.last_index().await.unwrap(), 2);
        assert!(storage.get(3).await.unwrap().is_none());
        assert!(storage.get(4).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_hard_state_persistence() {
        let temp_dir = TempDir::new().unwrap();

        // Create storage and set hard state
        {
            let storage = WalRaftStorage::new("test-topic".to_string(), 0, temp_dir.path())
                .await
                .unwrap();

            let state = HardState {
                term: 5,
                vote: Some(3),
                commit: 10,
            };

            storage.set_hard_state(state.clone()).await.unwrap();

            assert_eq!(storage.hard_state(), state);
        }

        // Recreate storage and verify recovery
        {
            let storage = WalRaftStorage::new("test-topic".to_string(), 0, temp_dir.path())
                .await
                .unwrap();

            let state = storage.hard_state();
            assert_eq!(state.term, 5);
            assert_eq!(state.vote, Some(3));
            assert_eq!(state.commit, 10);
        }
    }

    #[tokio::test]
    async fn test_recovery() {
        let temp_dir = TempDir::new().unwrap();

        // Create storage and append entries
        {
            let storage = WalRaftStorage::new("test-topic".to_string(), 0, temp_dir.path())
                .await
                .unwrap();

            let entries = vec![
                RaftEntry::new(1, 1, b"entry1".to_vec()),
                RaftEntry::new(1, 2, b"entry2".to_vec()),
                RaftEntry::new(2, 3, b"entry3".to_vec()),
            ];

            storage.append(entries).await.unwrap();
        }

        // Recreate storage (simulates crash recovery)
        {
            let storage = WalRaftStorage::new("test-topic".to_string(), 0, temp_dir.path())
                .await
                .unwrap();

            // Verify entries were recovered
            assert_eq!(storage.first_index().await.unwrap(), 1);
            assert_eq!(storage.last_index().await.unwrap(), 3);

            let entry2 = storage.get(2).await.unwrap().unwrap();
            assert_eq!(entry2.data, b"entry2");
        }
    }
}
