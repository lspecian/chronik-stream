//! Synchronous RaftWalStorage that implements raft::Storage directly
//!
//! This module provides persistent Raft log storage using GroupCommitWal.
//! Key design: All read operations are synchronous using in-memory cache.
//! Only write operations (append) are async and happen in background.
//!
//! ## Architecture
//!
//! 1. **In-Memory Log**: Vec<Entry> cached in memory for fast sync reads
//! 2. **Async Persistence**: Appends happen via background task to WAL
//! 3. **Recovery**: On startup, scan WAL segments to rebuild in-memory log
//!
//! ## Synchronous raft::Storage Implementation
//!
//! - initial_state() - Read from in-memory RaftState
//! - entries() - Read from in-memory Vec<Entry>
//! - term() - Read from in-memory entries or snapshot
//! - first_index() - Read from in-memory
//! - last_index() - Read from in-memory
//! - snapshot() - Read from in-memory Snapshot
//!
//! ## Async Operations (called from message loop)
//!
//! - append() - Write entries to WAL in background
//! - apply_snapshot() - Apply snapshot to in-memory state
//! - compact() - Truncate log and create snapshot

use raft::{prelude::*, Storage, StorageError, RaftState};
use std::sync::{Arc, RwLock as StdRwLock};
use parking_lot::RwLock;
use std::path::Path;
use tracing::{info, warn, debug};
use anyhow::{Result, Context};
use protobuf::Message;

use crate::group_commit::GroupCommitWal;
use crate::record::WalRecord;

/// Topic name for Raft metadata log
const RAFT_TOPIC: &str = "__raft_metadata";
const RAFT_PARTITION: i32 = 0;

/// Synchronous RaftWalStorage implementing raft::Storage
///
/// All raft::Storage methods are synchronous and read from in-memory cache.
/// Writes happen asynchronously via background WAL persistence.
#[derive(Clone)]
pub struct RaftWalStorage {
    /// Underlying GroupCommitWal for persistence
    wal: Arc<GroupCommitWal>,

    /// In-memory Raft log entries (indexed from 1)
    /// This is the source of truth for raft::Storage reads
    entries: Arc<StdRwLock<Vec<Entry>>>,

    /// Raft hard state (term, vote, commit)
    raft_state: Arc<StdRwLock<RaftState>>,

    /// Snapshot (for log compaction)
    snapshot: Arc<RwLock<Snapshot>>,

    /// First index in log (after compaction)
    first_index: Arc<StdRwLock<u64>>,
}

impl RaftWalStorage {
    /// Create a new RaftWalStorage
    pub fn new(wal: Arc<GroupCommitWal>) -> Self {
        Self {
            wal,
            entries: Arc::new(StdRwLock::new(Vec::new())),
            raft_state: Arc::new(StdRwLock::new(RaftState {
                hard_state: HardState::default(),
                conf_state: ConfState::default(),
            })),
            snapshot: Arc::new(RwLock::new(Snapshot::default())),
            first_index: Arc::new(StdRwLock::new(1)),
        }
    }

    /// Recover Raft log from WAL segments (async, called once during bootstrap)
    ///
    /// TODO: Implement actual recovery by scanning WAL files
    /// For now, we start with empty state and rely on Raft's snapshot transfer
    pub async fn recover(&self, _data_dir: &Path) -> Result<()> {
        info!("Raft WAL recovery: Starting with empty state (TODO: implement WAL scanning)");
        info!("Raft will sync state from leader via snapshot transfer if needed");
        Ok(())
    }

    /// Set initial Raft state (called during bootstrap)
    pub fn set_raft_state(&self, state: RaftState) {
        *self.raft_state.write().unwrap() = state;
    }

    /// Append entries to in-memory log and persist to WAL (async, called from message loop)
    pub async fn append_entries(&self, entries: &[Entry]) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }

        debug!("Appending {} Raft entries to WAL", entries.len());

        // First, update in-memory log (synchronous)
        {
            let first_index = *self.first_index.read().unwrap();
            let mut log = self.entries.write().unwrap();

            for entry in entries {
                // Convert 1-based Raft index to 0-based Vec index
                let vec_idx = (entry.index - first_index) as usize;

                // Extend Vec if needed (fill gaps with default entries)
                if vec_idx >= log.len() {
                    log.resize(vec_idx + 1, Entry::default());
                }

                // Overwrite or append
                log[vec_idx] = entry.clone();
            }
        }

        // Then, persist to WAL (async)
        for entry in entries {
            let entry_bytes = entry.write_to_bytes()
                .context("Failed to encode Raft entry")?;

            let wal_record = WalRecord::new_v2(
                RAFT_TOPIC.to_string(),
                RAFT_PARTITION,
                entry_bytes,
                entry.index as i64,
                entry.index as i64,
                1,
            );

            self.wal
                .append(RAFT_TOPIC.to_string(), RAFT_PARTITION, wal_record, 1)
                .await
                .context("Failed to append Raft entry to WAL")?;
        }

        debug!("Successfully appended {} entries to WAL", entries.len());
        Ok(())
    }

    /// Persist HardState to WAL (async, called from message loop)
    pub async fn persist_hard_state(&self, hs: &HardState) -> Result<()> {
        debug!("Persisting HardState: term={}, vote={}, commit={}", hs.term, hs.vote, hs.commit);

        // Update in-memory first
        {
            let mut state = self.raft_state.write().unwrap();
            state.hard_state = hs.clone();
        }

        // Persist to WAL
        let hs_bytes = hs.write_to_bytes()
            .context("Failed to encode HardState")?;

        let wal_record = WalRecord::new_v2(
            RAFT_TOPIC.to_string(),
            RAFT_PARTITION,
            hs_bytes,
            0, // HardState doesn't have an offset
            0,
            1,
        );

        self.wal
            .append(RAFT_TOPIC.to_string(), RAFT_PARTITION, wal_record, 1)
            .await
            .context("Failed to persist HardState to WAL")?;

        debug!("Successfully persisted HardState");
        Ok(())
    }

    /// Persist ConfState to WAL (async, called from message loop)
    pub async fn persist_conf_state(&self, cs: &ConfState) -> Result<()> {
        debug!("Persisting ConfState: {} voters", cs.voters.len());

        // Update in-memory first
        {
            let mut state = self.raft_state.write().unwrap();
            state.conf_state = cs.clone();
        }

        // Persist to WAL
        let cs_bytes = cs.write_to_bytes()
            .context("Failed to encode ConfState")?;

        let wal_record = WalRecord::new_v2(
            RAFT_TOPIC.to_string(),
            RAFT_PARTITION,
            cs_bytes,
            0,
            0,
            1,
        );

        self.wal
            .append(RAFT_TOPIC.to_string(), RAFT_PARTITION, wal_record, 1)
            .await
            .context("Failed to persist ConfState to WAL")?;

        debug!("Successfully persisted ConfState");
        Ok(())
    }
}

impl Storage for RaftWalStorage {
    fn initial_state(&self) -> raft::Result<RaftState> {
        Ok(self.raft_state.read().unwrap().clone())
    }

    fn entries(
        &self,
        low: u64,
        high: u64,
        max_size: impl Into<Option<u64>>,
        _context: raft::GetEntriesContext,
    ) -> raft::Result<Vec<Entry>> {
        let log = self.entries.read().unwrap();

        if log.is_empty() {
            return Ok(Vec::new());
        }

        let first_index = *self.first_index.read().unwrap();

        if low < first_index {
            return Err(raft::Error::Store(StorageError::Compacted));
        }

        if high > log.len() as u64 + first_index {
            panic!(
                "index out of bound (last: {}, high: {})",
                log.len() as u64 + first_index,
                high
            );
        }

        let lo = (low - first_index) as usize;
        let hi = (high - first_index) as usize;

        if lo >= hi {
            return Ok(Vec::new());
        }

        let mut entries = log[lo..hi].to_vec();

        // Apply size limit if specified
        if let Some(max_size) = max_size.into() {
            let mut total_size = 0u64;
            let mut limit = entries.len();

            for (i, entry) in entries.iter().enumerate() {
                // Approximate size: 24 bytes overhead + data length
                let entry_size = 24 + entry.data.len() as u64;
                total_size += entry_size;
                if total_size > max_size {
                    limit = i;
                    break;
                }
            }

            entries.truncate(limit);
        }

        Ok(entries)
    }

    fn term(&self, idx: u64) -> raft::Result<u64> {
        // Check snapshot first
        let snapshot = self.snapshot.read();
        if idx == snapshot.get_metadata().index {
            return Ok(snapshot.get_metadata().term);
        }

        let log = self.entries.read().unwrap();
        let first_index = *self.first_index.read().unwrap();

        if idx < first_index {
            return Err(raft::Error::Store(StorageError::Compacted));
        }

        let offset = (idx - first_index) as usize;
        if offset >= log.len() {
            return Err(raft::Error::Store(StorageError::Unavailable));
        }

        Ok(log[offset].term)
    }

    fn first_index(&self) -> raft::Result<u64> {
        Ok(*self.first_index.read().unwrap())
    }

    fn last_index(&self) -> raft::Result<u64> {
        let log = self.entries.read().unwrap();
        let first_index = *self.first_index.read().unwrap();

        if log.is_empty() {
            Ok(first_index - 1)
        } else {
            Ok(first_index + log.len() as u64 - 1)
        }
    }

    fn snapshot(&self, _request_index: u64, _to: u64) -> raft::Result<Snapshot> {
        Ok(self.snapshot.read().clone())
    }
}
