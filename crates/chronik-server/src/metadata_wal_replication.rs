//! Metadata WAL Replication - Async replication to followers (Phase 2)
//!
//! This module provides fire-and-forget replication of metadata commands to followers.
//! It reuses the existing WalReplicationManager infrastructure - no new ports, no new protocols!
//!
//! Architecture:
//! - Leader writes to local metadata WAL (fast, 1-2ms)
//! - Leader applies to state machine immediately
//! - Leader fires async replication task (this module)
//! - Replication uses EXISTING WalReplicationManager on EXISTING port (9291)
//! - Followers receive on EXISTING WalReceiver
//!
//! Key Design:
//! - Fire-and-forget (never blocks leader writes)
//! - Reuses proven WalReplicationManager (90K+ msg/s for partition data)
//! - Same TCP transport as partition data
//! - Special topic name "__chronik_metadata" for routing

use anyhow::{Result, Context};
use crate::metadata_wal::MetadataWal;
use crate::wal_replication::WalReplicationManager;
use crate::raft_metadata::MetadataCommand;
use std::sync::Arc;
use tracing::{debug, warn};

/// Metadata WAL Replicator
///
/// Thin wrapper around WalReplicationManager that handles metadata-specific replication.
/// Reuses all existing infrastructure - no new ports, no new protocols!
pub struct MetadataWalReplicator {
    /// Metadata WAL (for topic name and partition)
    wal: Arc<MetadataWal>,

    /// Existing WAL replication manager (reuses partition data infrastructure)
    replication_mgr: Arc<WalReplicationManager>,
}

impl MetadataWalReplicator {
    /// Create new metadata WAL replicator
    ///
    /// # Arguments
    /// - `wal`: Metadata WAL instance (provides topic name "__chronik_metadata")
    /// - `replication_mgr`: Existing WalReplicationManager (reuses partition data transport)
    ///
    /// # Returns
    /// Replicator ready to send metadata commands to followers
    pub fn new(
        wal: Arc<MetadataWal>,
        replication_mgr: Arc<WalReplicationManager>,
    ) -> Self {
        Self {
            wal,
            replication_mgr,
        }
    }

    /// Replicate metadata command to followers (async, fire-and-forget)
    ///
    /// This method returns immediately - replication happens in the background.
    /// Failures are logged but don't block the leader's write path.
    ///
    /// # Arguments
    /// - `cmd`: Metadata command to replicate
    /// - `offset`: WAL offset of this command (for ordering)
    ///
    /// # Returns
    /// Ok(()) if replication was queued successfully
    /// Err if serialization or queueing failed
    ///
    /// # Protocol
    /// - Serializes command to bincode
    /// - Sends via WalReplicationManager to "__chronik_metadata" topic, partition 0
    /// - Followers receive on existing WalReceiver
    /// - WalReceiver detects special topic name and routes to metadata handler
    pub async fn replicate(&self, cmd: &MetadataCommand, offset: i64) -> Result<()> {
        // Serialize command to bytes (same format as WAL write)
        let data = bincode::serialize(cmd)
            .context("Failed to serialize metadata command for replication")?;

        debug!(
            "Replicating metadata command to followers (offset={}): {:?}",
            offset,
            cmd
        );

        // Use existing WalReplicationManager with special topic name
        // This reuses ALL the existing infrastructure:
        // - Same TCP connections (port 9291)
        // - Same wire protocol (WAL frames)
        // - Same queue management (lock-free MPMC)
        // - Same retry logic
        // - Same metrics
        self.replication_mgr.replicate_partition(
            self.wal.topic_name().to_string(),  // "__chronik_metadata"
            self.wal.partition(),                // 0
            offset,
            offset,                              // leader_high_watermark (same as offset for metadata)
            data,                                // Vec<u8>
        ).await;

        debug!(
            "Metadata command queued for replication (topic='{}', partition={}, offset={})",
            self.wal.topic_name(),
            self.wal.partition(),
            offset
        );

        Ok(())
    }

    /// Spawn async replication task (fire-and-forget)
    ///
    /// This is a convenience method that spawns a background task for replication.
    /// The task returns immediately and doesn't block the caller.
    ///
    /// # Arguments
    /// - `cmd`: Metadata command to replicate
    /// - `offset`: WAL offset
    ///
    /// # Usage
    /// ```ignore
    /// // In RaftMetadataStore::create_topic():
    /// replicator.spawn_replicate(cmd.clone(), offset);
    /// // Returns immediately, replication happens in background
    /// ```
    pub fn spawn_replicate(&self, cmd: MetadataCommand, offset: i64) {
        let replicator = self.clone();
        tokio::spawn(async move {
            if let Err(e) = replicator.replicate(&cmd, offset).await {
                warn!(
                    "Metadata replication failed (offset={}): {}",
                    offset,
                    e
                );
                // Don't panic - replication is eventual consistency
                // Followers will catch up on reconnect or via Raft snapshot
            }
        });
    }
}

// Implement Clone for spawn_replicate convenience
impl Clone for MetadataWalReplicator {
    fn clone(&self) -> Self {
        Self {
            wal: Arc::clone(&self.wal),
            replication_mgr: Arc::clone(&self.replication_mgr),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[tokio::test]
    async fn test_metadata_wal_replicator_serialize() {
        // Test that we can serialize metadata commands correctly
        let cmd = MetadataCommand::CreateTopic {
            name: "test-topic".to_string(),
            partition_count: 3,
            replication_factor: 2,
            config: HashMap::new(),
        };

        let data = bincode::serialize(&cmd).unwrap();
        assert!(!data.is_empty());

        // Verify round-trip
        let cmd2: MetadataCommand = bincode::deserialize(&data).unwrap();
        match cmd2 {
            MetadataCommand::CreateTopic { name, partition_count, replication_factor, .. } => {
                assert_eq!(name, "test-topic");
                assert_eq!(partition_count, 3);
                assert_eq!(replication_factor, 2);
            }
            _ => panic!("Unexpected command type"),
        }
    }
}
