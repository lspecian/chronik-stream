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
use chronik_common::metadata::MetadataStore;  // v2.2.9 Phase 7 FIX: Import trait for methods

/// Metadata WAL Replicator
///
/// Thin wrapper around WalReplicationManager that handles metadata-specific replication.
/// Reuses all existing infrastructure - no new ports, no new protocols!
pub struct MetadataWalReplicator {
    /// Metadata WAL (for topic name and partition)
    wal: Arc<MetadataWal>,

    /// Existing WAL replication manager (reuses partition data infrastructure)
    replication_mgr: Arc<WalReplicationManager>,

    /// Event bus for metadata changes (event-based replication)
    event_bus: Arc<crate::metadata_events::MetadataEventBus>,

    /// Metadata store (v2.2.9 Phase 7 FIX: needed to apply events on followers)
    metadata_store: Arc<chronik_common::metadata::WalMetadataStore>,
}

impl MetadataWalReplicator {
    /// Create new metadata WAL replicator
    ///
    /// # Arguments
    /// - `wal`: Metadata WAL instance (provides topic name "__chronik_metadata")
    /// - `replication_mgr`: Existing WalReplicationManager (reuses partition data transport)
    /// - `event_bus`: Event bus for metadata changes
    /// - `metadata_store`: Metadata store to apply events on followers
    ///
    /// # Returns
    /// Replicator ready to send metadata commands to followers
    pub fn new(
        wal: Arc<MetadataWal>,
        replication_mgr: Arc<WalReplicationManager>,
        event_bus: Arc<crate::metadata_events::MetadataEventBus>,
        metadata_store: Arc<chronik_common::metadata::WalMetadataStore>,
    ) -> Self {
        Self {
            wal,
            replication_mgr,
            event_bus,
            metadata_store,
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

    /// Start listening for metadata events and replicate them
    ///
    /// This is the NEW event-driven architecture. RaftMetadataStore emits events,
    /// and this listener picks them up and replicates to followers.
    ///
    /// This method should be called once during server startup (in IntegratedServer).
    pub fn start_event_listener(self: Arc<Self>) {
        let mut rx = self.event_bus.subscribe();

        tokio::spawn(async move {
            tracing::info!("📡 MetadataWalReplicator event listener started");

            loop {
                match rx.recv().await {
                    Ok(event) => {
                        tracing::info!("📩 MetadataWalReplicator received event: {:?}", event);
                        if let Err(e) = self.handle_event(event).await {
                            tracing::warn!("Failed to handle metadata event: {}", e);
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => {
                        tracing::warn!("MetadataWalReplicator lagged by {} events (replication queue slow)", count);
                        // Continue - events are stored in WAL anyway, followers will catch up
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        tracing::warn!("MetadataWalReplicator event bus closed - stopping listener");
                        break;
                    }
                }
            }

            tracing::warn!("📡 MetadataWalReplicator event listener stopped");
        });
    }

    /// Handle a metadata event by replicating it
    async fn handle_event(&self, event: crate::metadata_events::MetadataEvent) -> Result<()> {
        use crate::metadata_events::MetadataEvent;

        match event {
            // v2.2.9 Phase 7 FIX #4: Replicate PartitionAssigned events to followers via WalReplicationManager
            // The leader receives this from local event bus AFTER write_and_apply() completes
            // So local state is ALREADY applied - we just need to replicate to followers
            MetadataEvent::PartitionAssigned { topic, partition, replicas, leader } => {
                tracing::info!("📡 Replicating PartitionAssigned to followers: {}-{}, leader={}, replicas={:?}",
                    topic, partition, leader, replicas);

                // Create the partition assignment struct for serialization
                let assignment = chronik_common::metadata::PartitionAssignment {
                    topic: topic.clone(),
                    partition: partition as u32,
                    broker_id: leader as i32,  // Deprecated field
                    is_leader: true,  // Deprecated field
                    replicas: replicas.clone(),
                    leader_id: leader,
                };

                // Create MetadataEvent for replication (same format as WAL writes)
                let metadata_event = chronik_common::metadata::MetadataEvent::new(
                    chronik_common::metadata::MetadataEventPayload::PartitionAssigned {
                        assignment,
                    },
                );

                // Serialize and replicate via WalReplicationManager
                // v2.2.9 Phase 7 FIX #7: Use broadcast_metadata() instead of replicate_partition()
                // replicate_partition() tries to deserialize as CanonicalRecord, which fails for MetadataEvent
                match metadata_event.to_bytes() {
                    Ok(data) => {
                        // Broadcast to all followers via __chronik_metadata topic
                        self.replication_mgr.broadcast_metadata(
                            self.wal.topic_name().to_string(),  // "__chronik_metadata"
                            self.wal.partition(),                // 0
                            0,  // dummy offset (event-based, not WAL-offset-based)
                            data,
                        ).await;
                        tracing::info!("✅ PartitionAssigned broadcast to followers: {}-{}", topic, partition);
                    }
                    Err(e) => {
                        tracing::warn!("Failed to serialize PartitionAssigned {}-{}: {}", topic, partition, e);
                    }
                }
            }

            // v2.2.9 Option 4: Partition metadata moved to WalMetadataStore
            MetadataEvent::LeaderChanged { topic, partition, new_leader } => {
                tracing::debug!("Skipping LeaderChanged replication (now in WalMetadataStore): {}-{} => {}",
                    topic, partition, new_leader);
                // let cmd = MetadataCommand::SetPartitionLeader {
                //     topic,
                //     partition,
                //     leader: new_leader,
                // };
                // self.replicate_command(cmd).await?;
            }

            // v2.2.9 Option 4: Partition metadata moved to WalMetadataStore
            MetadataEvent::ISRUpdated { topic, partition, isr: _ } => {
                tracing::debug!("Skipping ISRUpdated replication (now in WalMetadataStore): {}-{}",
                    topic, partition);
                // let cmd = MetadataCommand::UpdateISR {
                //     topic,
                //     partition,
                //     isr,
                // };
                // self.replicate_command(cmd).await?;
            }

            // CRITICAL PERFORMANCE FIX v2.2.9+:
            // HighWatermarkUpdated events are LOCAL notifications only - they're already applied in ProduceHandler
            // This listener's job is ONLY to replicate to followers, NOT to re-apply locally
            // Re-applying = double lock acquisition = 36x slowdown!
            //
            // Followers apply when they receive via WalReceiver, not from this event bus
            MetadataEvent::HighWatermarkUpdated { topic, partition, offset } => {
                tracing::debug!("📤 HighWatermarkUpdated (local notification only, no action): {}-{} => {}",
                    topic, partition, offset);
                // NO-OP: Already applied by ProduceHandler, replication happens via WalReplicationManager
            }

            MetadataEvent::TopicCreated { topic, num_partitions } => {
                tracing::info!("📡 Replicating TopicCreated: {} with {} partitions",
                    topic, num_partitions);
                // Topics are created via separate CreateTopic command
                // Partitions are assigned via PartitionAssigned events
                // So we don't need to replicate this event directly
            }

            MetadataEvent::TopicDeleted { topic } => {
                tracing::info!("📡 Replicating TopicDeleted: {}", topic);
                // TODO: Implement when DeleteTopic command is added
            }

            // v2.2.15 CRITICAL FIX: Replicate BrokerRegistered events to followers
            // Without this, followers don't know about brokers and return fallback metadata
            // This was the root cause of cluster mode client timeouts!
            MetadataEvent::BrokerRegistered { broker_id, host, port, rack } => {
                tracing::info!("📡 Replicating BrokerRegistered to followers: broker_id={}, {}:{}",
                    broker_id, host, port);

                // Create BrokerMetadata for serialization
                use chronik_common::metadata::traits::{BrokerMetadata, BrokerStatus};
                let broker_metadata = BrokerMetadata {
                    broker_id,
                    host: host.clone(),
                    port,
                    rack: rack.clone(),
                    status: BrokerStatus::Online,
                    created_at: chrono::Utc::now(),
                    updated_at: chrono::Utc::now(),
                };

                // Create MetadataEvent for replication (same format as WAL writes)
                let metadata_event = chronik_common::metadata::MetadataEvent::new(
                    chronik_common::metadata::MetadataEventPayload::BrokerRegistered {
                        metadata: broker_metadata,
                    },
                );

                // Serialize and broadcast to followers
                match metadata_event.to_bytes() {
                    Ok(data) => {
                        self.replication_mgr.broadcast_metadata(
                            self.wal.topic_name().to_string(),  // "__chronik_metadata"
                            self.wal.partition(),                // 0
                            0,  // dummy offset (event-based)
                            data,
                        ).await;
                        tracing::info!("✅ BrokerRegistered broadcast to followers: broker_id={}", broker_id);
                    }
                    Err(e) => {
                        tracing::warn!("Failed to serialize BrokerRegistered for broker {}: {}", broker_id, e);
                    }
                }
            }
        }

        Ok(())
    }

    /// Replicate a metadata command to followers (internal helper)
    async fn replicate_command(&self, cmd: MetadataCommand) -> Result<()> {
        // Note: The command is already written to WAL by RaftMetadataStore with its real offset
        // We're just sending a copy to followers here. The offset doesn't matter for
        // event-driven replication since we're just fire-and-forgetting.
        // Use 0 as a dummy offset since the actual offset is in the WAL already.
        let offset = 0;

        self.replicate(&cmd, offset).await
    }
}

// Implement Clone for spawn_replicate convenience
impl Clone for MetadataWalReplicator {
    fn clone(&self) -> Self {
        Self {
            wal: Arc::clone(&self.wal),
            replication_mgr: Arc::clone(&self.replication_mgr),
            event_bus: Arc::clone(&self.event_bus),
            metadata_store: Arc::clone(&self.metadata_store),  // v2.2.9 Phase 7 FIX
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
