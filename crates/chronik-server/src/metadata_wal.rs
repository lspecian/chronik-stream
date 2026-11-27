//! Metadata WAL - Fast local WAL for metadata operations (Phase 2)
//!
//! This module provides a specialized WAL for cluster metadata operations.
//! Unlike Raft consensus (10-50ms), metadata WAL writes are local and fast (1-2ms).
//!
//! Architecture:
//! - Leader writes metadata commands to local WAL (durable, fast)
//! - Leader applies commands to state machine immediately
//! - Leader asynchronously replicates to followers (fire-and-forget)
//! - Followers receive replication and apply to their state machines
//!
//! Why this is faster than Raft:
//! - No quorum wait (1-2ms vs 10-50ms)
//! - Reuses proven GroupCommitWal infrastructure (90K+ msg/s proven)
//! - Reuses existing WalReplicationManager (works for partition data)
//!
//! Topic name convention:
//! - Uses "__chronik_metadata" as topic name for replication routing
//! - Partition 0 only (metadata is a single stream)

use anyhow::{Result, Context};
use chronik_wal::{GroupCommitWal, GroupCommitConfig, WalRecord};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use tracing::{debug, info, warn};
use chronik_common::metadata::WalMetadataStore;

/// Checkpoint of the node-local state, in the metadata WAL directory.
pub const CHECKPOINT_FILE: &str = "checkpoint.img";
const CHECKPOINT_MAGIC: &[u8] = b"CHRONIKCKPT1\n";
/// The restored base state (`metadata_dr::LOCAL_BASE_STATE_FILE`), by name.
const BASE_STATE_FILE_NAME: &str = "base-state.json.gz";
const DEFAULT_COMPACT_BYTES: u64 = 256 * 1024 * 1024;

/// What one [`MetadataWal::compact`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionReport {
    pub covered: i64,
    pub image_bytes: u64,
    pub deleted_segments: usize,
    pub freed_bytes: u64,
}

/// Metadata WAL - Special-purpose WAL for metadata operations
///
/// Provides fast local writes (1-2ms) compared to Raft consensus (10-50ms).
/// Uses "__chronik_metadata" as topic name for replication routing.
pub struct MetadataWal {
    /// Underlying GroupCommitWal (reuses existing infrastructure)
    wal: Arc<GroupCommitWal>,

    /// WAL directory path (for recovery)
    wal_dir: PathBuf,

    /// Topic name for replication routing (always "__chronik_metadata")
    topic_name: String,

    /// Partition ID (always 0, metadata is single stream)
    partition: i32,

    /// Next offset to assign (metadata WAL manages its own offset sequence)
    next_offset: AtomicI64,
}

impl MetadataWal {
    /// Create new metadata WAL
    ///
    /// # Arguments
    /// - `data_dir`: Base data directory (WAL will be created at `data_dir/metadata_wal/`)
    ///
    /// # Returns
    /// Metadata WAL instance ready for writes
    pub async fn new(data_dir: PathBuf) -> Result<Self> {
        let topic_name = "__chronik_metadata".to_string();
        let partition = 0;

        // Create WAL directory: data_dir/metadata_wal/
        let wal_dir = data_dir.join("metadata_wal");
        tokio::fs::create_dir_all(&wal_dir).await
            .context("Failed to create metadata WAL directory")?;

        info!("Creating metadata WAL at: {}", wal_dir.display());

        // Use GroupCommitWal with metadata-specific config
        // CHRONIK_METADATA_WAL_PROFILE overrides for metadata WAL only
        // CRITICAL: Metadata always defaults to HIGH profile for stability
        // (prevents regression where changing data WAL profile affected metadata)
        let config = if let Ok(profile) = std::env::var("CHRONIK_METADATA_WAL_PROFILE") {
            match profile.to_lowercase().as_str() {
                "low" | "small" | "container" => {
                    warn!("Using LOW profile for metadata WAL (not recommended for production)");
                    GroupCommitConfig::low_resource()
                }
                "medium" | "balanced" => {
                    info!("Using MEDIUM profile for metadata WAL");
                    GroupCommitConfig::medium_resource()
                }
                "high" | "aggressive" | "dedicated" => {
                    info!("Using HIGH profile for metadata WAL");
                    GroupCommitConfig::high_resource()
                }
                "ultra" | "maximum" | "throughput" => {
                    info!("Using ULTRA profile for metadata WAL");
                    GroupCommitConfig::ultra_resource()
                }
                _ => {
                    warn!("Unknown CHRONIK_METADATA_WAL_PROFILE '{}', using HIGH (default)", profile);
                    GroupCommitConfig::high_resource()
                }
            }
        } else {
            // Always default to HIGH for metadata (stable critical path)
            info!("Using HIGH profile for metadata WAL (default for stability)");
            GroupCommitConfig::high_resource()
        };

        // CRITICAL: Metadata WAL needs a dummy callback to prevent timeout warnings
        // Metadata writes are internal bookkeeping (high watermarks), not client produces
        // So we don't need to notify anyone - just let the commit complete silently
        let dummy_callback = |_topic: &str, _partition: i32, _min_offset: i64, _max_offset: i64| {
            // NO-OP: Metadata commits don't need notifications
        };

        let wal = GroupCommitWal::with_callback(wal_dir.clone(), config, Arc::new(dummy_callback));

        info!(
            "Metadata WAL created successfully (topic='{}', partition={})",
            topic_name,
            partition
        );

        Ok(Self {
            wal: Arc::new(wal),
            wal_dir,
            topic_name,
            partition,
            next_offset: AtomicI64::new(0),
        })
    }

    /// Append pre-serialized bytes to WAL (v2.2.9 Option A)
    ///
    /// Used by WalMetadataStore to write serialized MetadataEvent bytes directly.
    /// This avoids double serialization (Event → Command → bytes).
    ///
    /// # Arguments
    /// - `data`: Pre-serialized bytes (typically serialized MetadataEvent)
    ///
    /// # Returns
    /// Offset of the written data in the WAL
    pub async fn append_bytes(&self, data: Vec<u8>) -> Result<i64> {
        // Allocate next offset
        let offset = self.next_offset.fetch_add(1, Ordering::SeqCst);

        // Capture data length before moving it into record
        let data_len = data.len();

        // Create WAL record
        let record = WalRecord::new_v2(
            self.topic_name.clone(),
            self.partition,
            data,
            offset,                // base_offset
            offset,                // last_offset (single command per record)
            1,                     // record_count (always 1 for metadata)
        );

        // Write to WAL (synchronous, but fast with group commit)
        // Use acks=1 for metadata (wait for fsync, but don't wait for replication)
        self.wal.append(
            self.topic_name.clone(),
            self.partition,
            record,
            1, // acks=1: wait for local fsync only
        ).await.context("Failed to append to metadata WAL")?;

        debug!(
            "Appended metadata bytes to WAL at offset {} ({} bytes)",
            offset, data_len
        );

        Ok(offset)
    }

    /// Get the next offset that will be assigned (v2.2.9)
    pub fn next_offset(&self) -> i64 {
        self.next_offset.load(Ordering::SeqCst)
    }

    /// Get topic name for replication routing
    ///
    /// Always returns "__chronik_metadata" - this is used by WalReplicationManager
    /// to route metadata replication correctly.
    pub fn topic_name(&self) -> &str {
        &self.topic_name
    }

    /// Get partition ID (always 0)
    pub fn partition(&self) -> i32 {
        self.partition
    }

    /// Recover metadata WAL state on startup: the events after `after` (the
    /// offset a checkpoint covers, if any), oldest first, and the next offset
    /// to assign — past every record on disk and past the checkpoint, so an
    /// offset is never reused after compaction deleted the records before it.
    pub async fn recover(&self, after: Option<i64>) -> Result<Vec<chronik_common::metadata::MetadataEvent>> {
        info!("Starting metadata WAL recovery...");
        let records = self.read_all_wal_records().await?;
        let floor = after.unwrap_or(-1);

        let mut max_offset = floor;
        for record in &records {
            if let WalRecord::V2 { last_offset, .. } = record {
                max_offset = max_offset.max(*last_offset);
            }
        }
        let next = max_offset + 1;
        self.next_offset.store(next, Ordering::SeqCst);
        info!("Restored next_offset to {} (recovered {} records, max offset {})", next, records.len(), max_offset);

        let mut events = Vec::new();
        let mut covered = 0usize;
        for record in records {
            match record {
                WalRecord::V2 { canonical_data, last_offset, .. } => {
                    if last_offset <= floor {
                        covered += 1;
                        continue;
                    }
                    match chronik_common::metadata::MetadataEvent::from_bytes(&canonical_data) {
                        Ok(event) => events.push(event),
                        Err(e) => {
                            tracing::warn!("Failed to deserialize metadata event: {} - skipping", e);
                            continue;
                        }
                    }
                }
                _ => {
                    tracing::warn!("Unexpected WalRecord V1 in metadata WAL - skipping");
                }
            }
        }
        if covered > 0 {
            info!("Skipped {covered} metadata WAL record(s) the checkpoint already covers");
        }
        info!("Successfully recovered {} metadata events from WAL", events.len());
        Ok(events)
    }

    /// The checkpoint written by [`Self::compact`]: the WAL offset it covers
    /// and the node-local state image.
    pub fn load_checkpoint(&self) -> Result<Option<(i64, Vec<u8>)>> {
        let path = self.wal_dir.join(CHECKPOINT_FILE);
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let Some(rest) = bytes.strip_prefix(CHECKPOINT_MAGIC) else {
            anyhow::bail!("{} is not a metadata checkpoint", path.display());
        };
        anyhow::ensure!(rest.len() >= 8, "{} is truncated", path.display());
        let covered = i64::from_be_bytes(rest[..8].try_into().expect("8 bytes"));
        Ok(Some((covered, rest[8..].to_vec())))
    }

    /// Compact the metadata WAL: write a checkpoint of `store` that covers
    /// every record appended so far, then delete the segments it makes
    /// redundant (and a restored base state, which it includes). Boot loads
    /// the checkpoint and replays only later records.
    ///
    /// Crash-safe in every order: the checkpoint is written to a temporary
    /// file, synced and renamed before anything is deleted, and records it
    /// covers are skipped by offset if their segment is still there.
    pub async fn compact(&self, store: &WalMetadataStore) -> Result<CompactionReport> {
        let started = std::time::Instant::now();
        let ((covered, active), image) = store
            .checkpoint(|| async {
                let active = self
                    .wal
                    .seal_partition(&self.topic_name, self.partition)
                    .await
                    .map_err(|e| chronik_common::metadata::MetadataError::StorageError(format!("sealing the metadata WAL: {e}")))?;
                Ok((self.next_offset() - 1, active))
            })
            .await
            .map_err(|e| anyhow::anyhow!("checkpointing metadata: {e}"))?;

        let path = self.wal_dir.join(CHECKPOINT_FILE);
        let tmp = self.wal_dir.join(format!("{CHECKPOINT_FILE}.tmp"));
        let mut bytes = Vec::with_capacity(CHECKPOINT_MAGIC.len() + 8 + image.len());
        bytes.extend_from_slice(CHECKPOINT_MAGIC);
        bytes.extend_from_slice(&covered.to_be_bytes());
        bytes.extend_from_slice(&image);
        let wal_dir = self.wal_dir.clone();
        let image_bytes = bytes.len() as u64;
        tokio::task::spawn_blocking(move || -> Result<()> {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
            f.write_all(&bytes)?;
            f.sync_all()?;
            std::fs::rename(&tmp, &path).with_context(|| format!("renaming to {}", path.display()))?;
            std::fs::File::open(&wal_dir)?.sync_all()?;
            Ok(())
        })
        .await??;

        // The checkpoint includes a restored base state.
        let base = self.wal_dir.join(BASE_STATE_FILE_NAME);
        if base.exists() {
            std::fs::remove_file(&base).with_context(|| format!("removing {}", base.display()))?;
        }

        let mut deleted = 0usize;
        let mut freed = 0u64;
        for (segment_id, path, size) in self.segment_files()? {
            if active.is_some_and(|a| segment_id >= a) {
                continue;
            }
            self.wal
                .delete_segment_file(&self.topic_name, self.partition, segment_id)
                .with_context(|| format!("deleting {}", path.display()))?;
            deleted += 1;
            freed += size;
        }
        let report = CompactionReport { covered, image_bytes, deleted_segments: deleted, freed_bytes: freed };
        info!(
            "Compacted the metadata WAL: checkpoint at offset {} ({} bytes), {} segment(s) deleted ({} bytes) in {:?}",
            covered, image_bytes, deleted, freed, started.elapsed()
        );
        Ok(report)
    }

    /// Compact whenever the WAL on disk reaches `CHRONIK_METADATA_WAL_COMPACT_BYTES`
    /// (default 256MB): checked a minute apart, from 30 s after boot, so a
    /// node that booted on a long WAL compacts it right away.
    pub fn spawn_compaction(self: &Arc<Self>, store: Arc<WalMetadataStore>) -> tokio::task::JoinHandle<()> {
        let wal = self.clone();
        let threshold = chronik_common::env::bytes("CHRONIK_METADATA_WAL_COMPACT_BYTES").unwrap_or(DEFAULT_COMPACT_BYTES).max(1);
        tokio::spawn(async move {
            let start = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
            let mut tick = tokio::time::interval_at(start, std::time::Duration::from_secs(60));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let size = wal.wal_bytes();
                if size < threshold {
                    continue;
                }
                info!("Metadata WAL is {size} bytes (compacting at {threshold})");
                if let Err(e) = wal.compact(&store).await {
                    warn!("Compacting the metadata WAL failed (retrying in a minute): {e:#}");
                }
            }
        })
    }

    /// Bytes of WAL segments on disk.
    pub fn wal_bytes(&self) -> u64 {
        self.segment_files().map(|files| files.iter().map(|(_, _, size)| size).sum()).unwrap_or(0)
    }

    /// `(segment id, path, size)` of every segment file.
    fn segment_files(&self) -> Result<Vec<(u64, PathBuf, u64)>> {
        let dir = self.wal_dir.join(&self.topic_name).join(self.partition.to_string());
        let prefix = format!("wal_{}_", self.partition);
        let mut files = Vec::new();
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(files),
            Err(e) => return Err(e).with_context(|| format!("listing {}", dir.display())),
        };
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let Some(id) = name
                .to_str()
                .and_then(|n| n.strip_prefix(&prefix))
                .and_then(|n| n.strip_suffix(".log"))
                .and_then(|n| n.parse::<u64>().ok())
            else {
                continue;
            };
            files.push((id, entry.path(), entry.metadata().map(|m| m.len()).unwrap_or(0)));
        }
        files.sort_by_key(|(id, _, _)| *id);
        Ok(files)
    }

    /// Read all WAL records from disk (internal helper for recovery)
    ///
    /// v2.2.19: Bug #5 FIXED - Properly reads GroupCommitWal file format
    /// Uses the same parsing logic as WalManager.read_from()
    async fn read_all_wal_records(&self) -> Result<Vec<WalRecord>> {
        use byteorder::{LittleEndian, ReadBytesExt};
        use std::io::Cursor as IoCursor;

        // Metadata WAL directory structure:
        // wal_dir/__chronik_metadata/0/wal_0_{segment_id}.log
        let partition_dir = self.wal_dir
            .join(&self.topic_name)
            .join(self.partition.to_string());

        if !partition_dir.exists() {
            debug!("Metadata WAL partition directory does not exist: {:?} - starting fresh", partition_dir);
            return Ok(Vec::new());
        }

        // Find all WAL segment files
        let mut wal_files = Vec::new();
        let mut entries = tokio::fs::read_dir(&partition_dir).await
            .context("Failed to read metadata WAL directory")?;

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if let Some(filename) = path.file_name().and_then(|n| n.to_str()) {
                // Match pattern: wal_{partition}_{segment_id}.log
                if filename.starts_with(&format!("wal_{}_", self.partition)) && filename.ends_with(".log") {
                    wal_files.push(path);
                }
            }
        }

        if wal_files.is_empty() {
            debug!("No metadata WAL files found in: {:?} - starting fresh", partition_dir);
            return Ok(Vec::new());
        }

        // Sort by segment ID (extract from filename)
        wal_files.sort_by_key(|path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .and_then(|s| {
                    s.strip_prefix(&format!("wal_{}_", self.partition))
                        .and_then(|rest| rest.strip_suffix(".log"))
                        .and_then(|seg_id| seg_id.parse::<u64>().ok())
                })
                .unwrap_or(0)
        });

        info!("Found {} metadata WAL segment files for recovery: {:?}", wal_files.len(), wal_files);

        let mut records = Vec::new();

        // Read from all segment files
        for wal_file_path in &wal_files {
            let file_data = tokio::fs::read(&wal_file_path).await
                .with_context(|| format!("Failed to read metadata WAL file: {:?}", wal_file_path))?;

            if file_data.is_empty() {
                continue;
            }

            debug!("Reading {} bytes from metadata WAL file: {:?}", file_data.len(), wal_file_path);

            // Parse V2 WAL records from file (same format as WalManager.read_from)
            let mut cursor = 0;

            while cursor < file_data.len() {
                if cursor + 12 > file_data.len() {
                    break;
                }

                let record_start = cursor;
                let mut rdr = IoCursor::new(&file_data[cursor..]);

                let magic = rdr.read_u16::<LittleEndian>().unwrap();
                let version = rdr.read_u8().unwrap();
                let flags = rdr.read_u8().unwrap();
                let length = rdr.read_u32::<LittleEndian>().unwrap() as usize;
                let crc32 = rdr.read_u32::<LittleEndian>().unwrap();

                if magic != 0xCA7E || version != 2 || length == 0 {
                    debug!("Invalid WAL record at cursor {}: magic={:x}, version={}, length={}",
                           cursor, magic, version, length);
                    break;
                }

                // Parse V2 record body with graceful handling of truncated files
                let topic_len = match rdr.read_u16::<LittleEndian>() {
                    Ok(len) => len as usize,
                    Err(e) => {
                        debug!("Failed to read topic_len at cursor {}: {} - likely truncated", cursor, e);
                        break;
                    }
                };

                let mut topic_bytes = vec![0u8; topic_len];
                if let Err(e) = std::io::Read::read_exact(&mut rdr, &mut topic_bytes) {
                    debug!("Failed to read topic bytes at cursor {}: {} - likely truncated", cursor, e);
                    break;
                }

                let record_topic = match String::from_utf8(topic_bytes) {
                    Ok(s) => s,
                    Err(e) => {
                        debug!("Invalid UTF-8 in topic at cursor {}: {}", cursor, e);
                        break;
                    }
                };

                let record_partition = match rdr.read_i32::<LittleEndian>() {
                    Ok(p) => p,
                    Err(e) => {
                        debug!("Failed to read partition at cursor {}: {}", cursor, e);
                        break;
                    }
                };

                let canonical_data_len = match rdr.read_u32::<LittleEndian>() {
                    Ok(len) => len as usize,
                    Err(e) => {
                        debug!("Failed to read canonical_data_len at cursor {}: {}", cursor, e);
                        break;
                    }
                };

                let mut canonical_data = vec![0u8; canonical_data_len];
                if let Err(e) = std::io::Read::read_exact(&mut rdr, &mut canonical_data) {
                    debug!("Failed to read canonical data at cursor {}: {} - likely truncated", cursor, e);
                    break;
                }

                let base_offset = match rdr.read_i64::<LittleEndian>() {
                    Ok(o) => o,
                    Err(e) => {
                        debug!("Failed to read base_offset at cursor {}: {}", cursor, e);
                        break;
                    }
                };

                let last_offset = match rdr.read_i64::<LittleEndian>() {
                    Ok(o) => o,
                    Err(e) => {
                        debug!("Failed to read last_offset at cursor {}: {}", cursor, e);
                        break;
                    }
                };

                let record_count = match rdr.read_i32::<LittleEndian>() {
                    Ok(c) => c,
                    Err(e) => {
                        debug!("Failed to read record_count at cursor {}: {}", cursor, e);
                        break;
                    }
                };

                let record = WalRecord::V2 {
                    magic,
                    version,
                    flags,
                    length: length as u32,
                    crc32,
                    topic: record_topic,
                    partition: record_partition,
                    canonical_data,
                    base_offset,
                    last_offset,
                    record_count,
                };
                records.push(record);

                cursor = record_start + rdr.position() as usize;
            }

            debug!("Completed reading metadata WAL segment {:?}: {} records so far", wal_file_path, records.len());
        }

        info!("✅ Metadata WAL recovery complete: {} records from {} segment files (Bug #5 FIXED)",
              records.len(), wal_files.len());

        Ok(records)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn appends_get_consecutive_offsets() {
        let temp_dir = tempfile::tempdir().unwrap();
        let wal = MetadataWal::new(temp_dir.path().to_path_buf()).await.unwrap();
        assert_eq!(wal.topic_name(), "__chronik_metadata");
        assert_eq!(wal.partition(), 0);
        assert_eq!(wal.append_bytes(b"first".to_vec()).await.unwrap(), 0);
        assert_eq!(wal.append_bytes(b"second".to_vec()).await.unwrap(), 1);
    }

    use chronik_common::metadata::{ConsumerOffset, MetadataStore};

    /// A metadata WAL and the store writing to it, as the builder wires them,
    /// after loading the checkpoint and replaying the rest.
    async fn open(dir: &std::path::Path) -> (Arc<MetadataWal>, Arc<WalMetadataStore>) {
        let wal = Arc::new(MetadataWal::new(dir.to_path_buf()).await.unwrap());
        let w = wal.clone();
        let append: chronik_common::metadata::WalAppendFn = Arc::new(move |bytes| {
            let w = w.clone();
            Box::pin(async move { w.append_bytes(bytes).await.map_err(|e| e.to_string()) })
        });
        let store = WalMetadataStore::new(1, append);
        let covered = match wal.load_checkpoint().unwrap() {
            Some((covered, image)) => {
                store.import_state(&image).await.unwrap();
                Some(covered)
            }
            None => None,
        };
        let events = wal.recover(covered).await.unwrap();
        store.replay_events(events).await.unwrap();
        (wal, Arc::new(store))
    }

    async fn commit(store: &WalMetadataStore, partition: u32, offset: i64) {
        store
            .commit_offset(ConsumerOffset {
                group_id: "g".into(),
                topic: "t".into(),
                partition,
                offset,
                metadata: None,
                commit_timestamp: chrono::Utc::now(),
            })
            .await
            .unwrap();
    }

    async fn offset(store: &WalMetadataStore, partition: u32) -> Option<i64> {
        store.get_consumer_offset("g", "t", partition).await.unwrap().map(|o| o.offset)
    }

    /// After compaction the node boots from the checkpoint plus the records
    /// after it, to the same state, with the covered segments gone.
    #[tokio::test(flavor = "multi_thread")]
    async fn compaction_keeps_the_state_and_drops_the_covered_segments() {
        let dir = tempfile::tempdir().unwrap();
        {
            let (_wal, store) = open(dir.path()).await;
            for i in 0..200 {
                commit(&store, i % 4, i as i64).await;
            }
        }
        // A second run: segments from both runs are on disk.
        let (wal, store) = open(dir.path()).await;
        commit(&store, 0, 1_000).await;
        assert_eq!(wal.segment_files().unwrap().len(), 2);
        let report = wal.compact(&store).await.unwrap();
        assert_eq!(report.covered, 200, "covers every record written so far");
        assert_eq!(report.deleted_segments, 2);
        assert!(wal.segment_files().unwrap().iter().all(|(id, _, _)| *id >= 2), "a covered segment survived");
        commit(&store, 1, 2_000).await;
        drop((wal, store));

        let (wal, store) = open(dir.path()).await;
        assert_eq!(offset(&store, 0).await, Some(1_000), "from the checkpoint");
        assert_eq!(offset(&store, 1).await, Some(2_000), "replayed after the checkpoint");
        assert_eq!(offset(&store, 2).await, Some(198));
        assert_eq!(offset(&store, 3).await, Some(199));
        assert_eq!(wal.next_offset(), 202);
    }

    /// Compacting when nothing was written since boot deletes every segment;
    /// offsets still continue past the checkpoint, so later records are not
    /// mistaken for covered ones at the next boot.
    #[tokio::test(flavor = "multi_thread")]
    async fn offsets_continue_past_a_checkpoint_that_left_no_segment() {
        let dir = tempfile::tempdir().unwrap();
        {
            let (_wal, store) = open(dir.path()).await;
            for i in 0..10 {
                commit(&store, 0, i).await;
            }
        }
        {
            let (wal, store) = open(dir.path()).await;
            let report = wal.compact(&store).await.unwrap();
            assert_eq!((report.covered, report.deleted_segments), (9, 1));
            assert!(wal.segment_files().unwrap().is_empty());
        }
        let (wal, store) = open(dir.path()).await;
        assert_eq!(wal.next_offset(), 10);
        commit(&store, 0, 77).await;
        drop((wal, store));
        let (_wal, store) = open(dir.path()).await;
        assert_eq!(offset(&store, 0).await, Some(77), "a record after the checkpoint was skipped");
    }

    /// The restored base state the checkpoint replaces is the one
    /// `metadata_dr` writes.
    #[test]
    fn the_base_state_is_the_one_a_restore_writes() {
        assert_eq!(
            std::path::Path::new("metadata_wal").join(BASE_STATE_FILE_NAME),
            std::path::Path::new(crate::metadata_dr::LOCAL_BASE_STATE_FILE)
        );
    }
}
