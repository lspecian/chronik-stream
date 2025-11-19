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
use crate::raft_metadata::MetadataCommand;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use tracing::{debug, info};

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

        // Use GroupCommitWal with default config
        // Can be overridden via CHRONIK_WAL_PROFILE environment variable
        // Default: high profile (10K batches, 100ms flush, 50MB buffer)
        let config = GroupCommitConfig::default();
        let wal = GroupCommitWal::new(wal_dir.clone(), config);

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

    /// Append metadata command to WAL
    ///
    /// This is a durable, synchronous write that returns immediately after fsync (1-2ms).
    /// Uses group commit for efficiency - multiple concurrent writes are batched together.
    ///
    /// # Arguments
    /// - `cmd`: Metadata command to persist
    ///
    /// # Returns
    /// Offset of the written command in the WAL
    pub async fn append(&self, cmd: &MetadataCommand) -> Result<i64> {
        // Serialize command to bytes (bincode)
        let data = bincode::serialize(cmd)
            .context("Failed to serialize metadata command")?;

        // Use common append_bytes implementation
        self.append_bytes(data).await
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

    /// Get reference to underlying GroupCommitWal
    ///
    /// Useful for advanced operations like recovery, compaction, etc.
    pub fn wal(&self) -> &Arc<GroupCommitWal> {
        &self.wal
    }

    /// Recover metadata WAL state on startup
    ///
    /// This method:
    /// 1. Reads all WAL segments to find the latest offset
    /// 2. Restores next_offset to continue from where we left off
    /// 3. Returns all metadata events for state machine replay
    ///
    /// # Returns
    /// Vec of MetadataEvents in order (oldest to newest)
    pub async fn recover(&self) -> Result<Vec<chronik_common::metadata::MetadataEvent>> {
        info!("Starting metadata WAL recovery...");

        // Read all WAL records from disk
        let records = self.read_all_wal_records().await?;

        if records.is_empty() {
            info!("No metadata WAL records found - starting fresh");
            return Ok(Vec::new());
        }

        // Find the highest offset to restore next_offset
        let mut max_offset = -1i64;
        for record in &records {
            match record {
                WalRecord::V2 { last_offset, .. } => {
                    if *last_offset > max_offset {
                        max_offset = *last_offset;
                    }
                }
                _ => {}
            }
        }

        // Restore next_offset (next write should be max_offset + 1)
        let next = max_offset + 1;
        self.next_offset.store(next, Ordering::SeqCst);
        info!("Restored next_offset to {} (recovered {} records, max offset {})",
            next, records.len(), max_offset);

        // Convert WalRecords to MetadataEvents
        let mut events = Vec::new();
        for record in records {
            match record {
                WalRecord::V2 { canonical_data, .. } => {
                    // Deserialize bytes to MetadataEvent
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

        info!("Successfully recovered {} metadata events from WAL", events.len());
        Ok(events)
    }

    /// Read all WAL records from disk (internal helper for recovery)
    async fn read_all_wal_records(&self) -> Result<Vec<WalRecord>> {
        use tokio::fs;
        use tokio::io::AsyncReadExt;

        // WAL files are stored at: wal_dir/__chronik_metadata/0/wal_*.log
        let partition_dir = self.wal_dir.join(&self.topic_name).join(self.partition.to_string());

        // Check if directory exists
        if !partition_dir.exists() {
            debug!("Partition directory does not exist: {}", partition_dir.display());
            return Ok(Vec::new());
        }

        // Scan for all WAL files in the partition directory
        let mut entries = fs::read_dir(&partition_dir).await
            .context(format!("Failed to read partition directory: {}", partition_dir.display()))?;

        let mut wal_files = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("log") {
                wal_files.push(path);
            }
        }

        // Sort files by name (wal_0_0.log, wal_0_1.log, etc.)
        wal_files.sort();

        if wal_files.is_empty() {
            debug!("No WAL files found in {}", partition_dir.display());
            return Ok(Vec::new());
        }

        // Read and parse all WAL files
        let num_files = wal_files.len();
        let mut all_records = Vec::new();
        for wal_file in wal_files {
            debug!("Reading WAL file: {}", wal_file.display());

            // Read entire file into memory
            let mut file = fs::File::open(&wal_file).await
                .context(format!("Failed to open WAL file: {}", wal_file.display()))?;
            let mut buffer = Vec::new();
            file.read_to_end(&mut buffer).await
                .context("Failed to read WAL file")?;

            // Parse all records from buffer
            let mut offset = 0;
            while offset < buffer.len() {
                match WalRecord::from_bytes(&buffer[offset..]) {
                    Ok(record) => {
                        let record_len = record.to_bytes()?.len();
                        all_records.push(record);
                        offset += record_len;
                    }
                    Err(e) => {
                        debug!("Failed to parse WAL record at offset {}: {} - end of file or corrupt", offset, e);
                        break;
                    }
                }
            }
        }

        debug!("Successfully read {} WAL records from {} files", all_records.len(), num_files);
        Ok(all_records)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[tokio::test]
    async fn test_metadata_wal_basic() {
        let temp_dir = tempfile::tempdir().unwrap();
        let wal = MetadataWal::new(temp_dir.path().to_path_buf()).await.unwrap();

        // Test topic name and partition
        assert_eq!(wal.topic_name(), "__chronik_metadata");
        assert_eq!(wal.partition(), 0);

        // Test append
        let cmd = MetadataCommand::CreateTopic {
            name: "test-topic".to_string(),
            partition_count: 3,
            replication_factor: 2,
            config: HashMap::new(),
        };

        let offset = wal.append(&cmd).await.unwrap();
        assert_eq!(offset, 0); // First write
    }

    #[tokio::test]
    async fn test_metadata_wal_multiple_writes() {
        let temp_dir = tempfile::tempdir().unwrap();
        let wal = MetadataWal::new(temp_dir.path().to_path_buf()).await.unwrap();

        // Write multiple commands
        let cmd1 = MetadataCommand::RegisterBroker {
            broker_id: 1,
            host: "localhost".to_string(),
            port: 9092,
            rack: None,
        };

        let cmd2 = MetadataCommand::RegisterBroker {
            broker_id: 2,
            host: "localhost".to_string(),
            port: 9093,
            rack: None,
        };

        let offset1 = wal.append(&cmd1).await.unwrap();
        let offset2 = wal.append(&cmd2).await.unwrap();

        assert_eq!(offset1, 0);
        assert_eq!(offset2, 1);
    }
}
