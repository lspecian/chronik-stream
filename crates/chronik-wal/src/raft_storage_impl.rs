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
use std::path::{Path, PathBuf};
use tracing::{info, debug};
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

    /// Data directory for snapshots (Phase 5)
    data_dir: Arc<StdRwLock<PathBuf>>,
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
            data_dir: Arc::new(StdRwLock::new(PathBuf::new())),
        }
    }

    /// Recover Raft log from WAL segments (async, called once during bootstrap)
    ///
    /// Scans WAL files in wal/__meta/__raft_metadata/0/ directory and reconstructs:
    /// 1. In-memory Raft log entries (Vec<Entry>)
    /// 2. HardState (term, vote, commit)
    /// 3. ConfState (voters, learners)
    pub async fn recover(&self, data_dir: &Path) -> Result<()> {
        use std::fs;
        use std::io::Read;
        use protobuf::Message;

        info!("Starting Raft WAL recovery from {:?}", data_dir);

        // Store data_dir for snapshot operations
        *self.data_dir.write().unwrap() = data_dir.to_path_buf();

        // Phase 5: Load latest snapshot first (if available)
        let snapshot_opt = self.load_latest_snapshot().await?;
        let snapshot_index = if let Some(ref snapshot) = snapshot_opt {
            let metadata = snapshot.get_metadata();
            info!("✓ Loaded snapshot: index={}, term={}", metadata.index, metadata.term);
            metadata.index
        } else {
            info!("No snapshot found - will recover from WAL only");
            0
        };

        // WAL path for Raft metadata: wal/__meta/__raft_metadata/0/
        let wal_dir = data_dir.join("wal/__meta/__raft_metadata/0");

        if !wal_dir.exists() {
            info!("No Raft WAL directory found - starting with empty state");
            return Ok(());
        }

        // Scan all WAL segment files (wal_*.log)
        let mut wal_files = Vec::new();
        for entry in fs::read_dir(&wal_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("log") {
                wal_files.push(path);
            }
        }

        if wal_files.is_empty() {
            info!("No WAL files found - starting with empty state");
            return Ok(());
        }

        // Sort files by name (wal_0_0.log, wal_0_1.log, etc.)
        wal_files.sort();

        let mut recovered_entries: Vec<Entry> = Vec::new();
        let mut recovered_hard_state: Option<HardState> = None;
        let mut recovered_conf_state: Option<ConfState> = None;

        // Read each WAL file and deserialize records
        for wal_file in &wal_files {
            debug!("Reading Raft WAL file: {:?}", wal_file);

            let mut file = fs::File::open(wal_file)?;
            let mut contents = Vec::new();
            file.read_to_end(&mut contents)?;
            debug!("Read {} bytes from WAL file", contents.len());

            // Parse WAL records from file
            let mut offset = 0;
            while offset < contents.len() {
                debug!("Attempting to parse WAL record at offset {}/{}", offset, contents.len());

                // Try to parse a WalRecord
                match WalRecord::from_bytes(&contents[offset..]) {
                    Ok(record) => {
                        // Calculate size of this record by reading the length field
                        // WalRecord format: magic(2) + version(1) + flags(1) + length(4) + ...
                        // The 'length' field contains size of everything AFTER the 12-byte header
                        let length = match &record {
                            WalRecord::V1 { length, .. } => *length,
                            WalRecord::V2 { length, .. } => *length,
                        };
                        let record_size = (length as usize) + 12; // length + 12-byte header
                        debug!("Parsed WAL record: size={}, offset will advance to {}", record_size, offset + record_size);
                        offset += record_size;

                        // Get canonical data (only available for V2 records)
                        if let Some(data) = record.get_canonical_data() {
                            // Skip empty data
                            if data.is_empty() {
                                debug!("Skipping empty canonical_data at offset {}", offset);
                            }
                            // Try to parse as Entry first (most common)
                            // Use merge_from_bytes (implemented) not parse_from_bytes (unimplemented!)
                            else {
                                let mut entry = Entry::default();
                                if entry.merge_from_bytes(data).is_ok() && entry.index > 0 {
                                    debug!("Recovered Raft entry: index={}, term={}", entry.index, entry.term);
                                    recovered_entries.push(entry);
                                }
                                // Try to parse as HardState
                                else {
                                    let mut hs = HardState::default();
                                    if hs.merge_from_bytes(data).is_ok() && (hs.term > 0 || hs.vote > 0 || hs.commit > 0) {
                                        debug!("Recovered HardState: term={}, vote={}, commit={}", hs.term, hs.vote, hs.commit);
                                        recovered_hard_state = Some(hs);
                                    }
                                    // Try to parse as ConfState
                                    else {
                                        let mut cs = ConfState::default();
                                        if cs.merge_from_bytes(data).is_ok() && !cs.voters.is_empty() {
                                            debug!("Recovered ConfState: voters={:?}", cs.voters);
                                            recovered_conf_state = Some(cs);
                                        } else {
                                            debug!("Skipping unknown protobuf record at offset {} (len={})", offset, data.len());
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        // If we can't parse a record, we might be at the end of valid data
                        debug!("Failed to parse record at offset {}: {:?}", offset, e);
                        break;
                    }
                }
            }
        }

        // Restore in-memory state
        if !recovered_entries.is_empty() {
            // Sort entries by index
            recovered_entries.sort_by_key(|e| e.index);

            // Phase 5: Filter out entries covered by snapshot
            if snapshot_index > 0 {
                let entries_before_filter = recovered_entries.len();
                recovered_entries.retain(|e| e.index > snapshot_index);

                if entries_before_filter > recovered_entries.len() {
                    info!(
                        "Filtered {} entries covered by snapshot (index <= {})",
                        entries_before_filter - recovered_entries.len(),
                        snapshot_index
                    );
                }
            }

            if !recovered_entries.is_empty() {
                let first_idx = recovered_entries.first().map(|e| e.index).unwrap_or(1);
                let last_idx = recovered_entries.last().map(|e| e.index).unwrap_or(0);

                info!("✓ Recovered {} Raft entries (index {}-{})", recovered_entries.len(), first_idx, last_idx);

                // Update in-memory log
                *self.entries.write().unwrap() = recovered_entries;
                *self.first_index.write().unwrap() = first_idx;
            } else {
                // All entries were covered by snapshot
                info!("All WAL entries covered by snapshot - starting with empty log after snapshot");
                *self.first_index.write().unwrap() = snapshot_index + 1;
            }
        } else {
            if snapshot_index > 0 {
                info!("No Raft entries recovered - using snapshot state (first_index={})", snapshot_index + 1);
                *self.first_index.write().unwrap() = snapshot_index + 1;
            } else {
                info!("No Raft entries recovered - starting with empty log");
            }
        }

        // Restore HardState
        if let Some(hs) = recovered_hard_state {
            info!("✓ Recovered HardState: term={}, vote={}, commit={}", hs.term, hs.vote, hs.commit);

            let mut state = self.raft_state.write().unwrap();
            state.hard_state = hs;
        } else {
            info!("No HardState recovered - starting with default");
        }

        // Restore ConfState
        if let Some(cs) = recovered_conf_state {
            info!("✓ Recovered ConfState: voters={:?}, learners={:?}", cs.voters, cs.learners);

            let mut state = self.raft_state.write().unwrap();
            state.conf_state = cs;
        } else {
            info!("No ConfState recovered - starting with default");
        }

        info!("Raft WAL recovery complete");
        Ok(())
    }

    /// Set initial Raft state (called during bootstrap ONLY if no state was recovered)
    pub fn set_raft_state(&self, state: RaftState) {
        *self.raft_state.write().unwrap() = state;
    }

    /// Check if HardState was recovered (term > 0 means we have recovered state)
    pub fn has_recovered_state(&self) -> bool {
        let state = self.raft_state.read().unwrap();
        state.hard_state.term > 0 || state.hard_state.vote > 0 || state.hard_state.commit > 0
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

    /// Create a snapshot of current state (Phase 5)
    ///
    /// Returns a Raft Snapshot containing:
    /// - State machine data (serialized MetadataStateMachine)
    /// - Snapshot metadata (index, term, conf_state)
    pub async fn create_snapshot(
        &self,
        state_machine_data: Vec<u8>,
        applied_index: u64,
        applied_term: u64,
    ) -> Result<raft::prelude::Snapshot> {
        use raft::prelude::*;

        info!("Creating snapshot at index={}, term={}", applied_index, applied_term);

        // Get current ConfState
        let conf_state = {
            let state = self.raft_state.read().unwrap();
            state.conf_state.clone()
        };

        // Create snapshot metadata
        let mut snapshot_metadata = SnapshotMetadata::default();
        snapshot_metadata.set_index(applied_index);
        snapshot_metadata.set_term(applied_term);
        snapshot_metadata.set_conf_state(conf_state);

        // Create snapshot
        let mut snapshot = Snapshot::default();
        snapshot.set_data(state_machine_data);
        snapshot.set_metadata(snapshot_metadata);

        info!("✓ Created snapshot: index={}, term={}, data_size={} bytes",
            applied_index, applied_term, snapshot.get_data().len());

        Ok(snapshot)
    }

    /// Save snapshot to disk (Phase 5)
    ///
    /// Snapshot file format: {data_dir}/wal/__meta/__raft_metadata/snapshots/snapshot_{index}_{term}.snap
    pub async fn save_snapshot(&self, snapshot: &raft::prelude::Snapshot) -> Result<()> {
        use std::fs;
        use protobuf::Message;

        let metadata = snapshot.get_metadata();
        let index = metadata.index;
        let term = metadata.term;

        info!("Saving snapshot to disk: index={}, term={}", index, term);

        // Get data_dir
        let data_dir = self.data_dir.read().unwrap().clone();

        // Create snapshots directory
        let snapshot_dir = data_dir.join("wal/__meta/__raft_metadata/snapshots");
        fs::create_dir_all(&snapshot_dir)
            .context("Failed to create snapshots directory")?;

        // Snapshot filename: snapshot_{index}_{term}.snap
        let snapshot_file = snapshot_dir.join(format!("snapshot_{}_{}.snap", index, term));

        // Serialize snapshot using protobuf
        let snapshot_bytes = snapshot.write_to_bytes()
            .context("Failed to serialize snapshot")?;

        // Write to file with fsync
        fs::write(&snapshot_file, &snapshot_bytes)
            .context("Failed to write snapshot file")?;

        // Sync to ensure durability
        let file = fs::OpenOptions::new()
            .write(true)
            .open(&snapshot_file)?;
        file.sync_all()
            .context("Failed to fsync snapshot file")?;

        info!("✓ Saved snapshot to disk: {:?} ({} bytes)", snapshot_file, snapshot_bytes.len());

        // Update in-memory snapshot
        *self.snapshot.write() = snapshot.clone();

        Ok(())
    }

    /// Load latest snapshot from disk (Phase 5)
    ///
    /// Returns Some(snapshot) if found, None if no snapshots exist
    pub async fn load_latest_snapshot(&self) -> Result<Option<raft::prelude::Snapshot>> {
        use std::fs;
        use protobuf::Message;

        // Get data_dir
        let data_dir = self.data_dir.read().unwrap().clone();

        let snapshot_dir = data_dir.join("wal/__meta/__raft_metadata/snapshots");

        if !snapshot_dir.exists() {
            info!("No snapshots directory found");
            return Ok(None);
        }

        // Find all snapshot files
        let mut snapshot_files = Vec::new();
        for entry in fs::read_dir(&snapshot_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("snap") {
                snapshot_files.push(path);
            }
        }

        if snapshot_files.is_empty() {
            info!("No snapshot files found");
            return Ok(None);
        }

        // Sort by filename (snapshot_{index}_{term}.snap) - lexicographic sort works
        snapshot_files.sort();

        // Load the latest snapshot (last in sorted order)
        let latest_snapshot_file = snapshot_files.last().unwrap();

        info!("Loading latest snapshot from {:?}", latest_snapshot_file);

        let snapshot_bytes = fs::read(latest_snapshot_file)
            .context("Failed to read snapshot file")?;

        let mut snapshot = raft::prelude::Snapshot::default();
        snapshot.merge_from_bytes(&snapshot_bytes)
            .context("Failed to deserialize snapshot")?;

        let metadata = snapshot.get_metadata();
        info!("✓ Loaded snapshot: index={}, term={}, data_size={} bytes",
            metadata.index, metadata.term, snapshot.get_data().len());

        // Update in-memory snapshot
        *self.snapshot.write() = snapshot.clone();

        Ok(Some(snapshot))
    }

    /// Truncate Raft log entries up to snapshot index (Phase 5)
    ///
    /// Deletes log entries that are covered by the snapshot to free disk space
    pub async fn truncate_log_to_snapshot(&self, snapshot_index: u64) -> Result<()> {
        info!("Truncating Raft log up to index {}", snapshot_index);

        let mut entries = self.entries.write().unwrap();
        let mut first_index = self.first_index.write().unwrap();

        // Find position to truncate
        let old_first = *first_index;
        let new_first = snapshot_index + 1;

        if new_first <= old_first {
            info!("No truncation needed: snapshot_index={}, first_index={}", snapshot_index, old_first);
            return Ok(());
        }

        // Calculate how many entries to remove
        let entries_to_remove = (new_first - old_first) as usize;

        if entries_to_remove >= entries.len() {
            // Remove all entries
            let removed_count = entries.len();
            entries.clear();
            *first_index = new_first;
            info!("✓ Truncated all {} log entries, new first_index={}", removed_count, new_first);
        } else {
            // Remove first N entries
            entries.drain(0..entries_to_remove);
            *first_index = new_first;
            info!("✓ Truncated {} log entries, new first_index={}, remaining={}",
                entries_to_remove, new_first, entries.len());
        }

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
