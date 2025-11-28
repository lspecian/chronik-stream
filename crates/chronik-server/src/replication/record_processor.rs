//! WAL record processing
//!
//! Extracted from `handle_connection()` to reduce complexity.
//! Handles metadata WAL records, normal WAL records, and ACK sending.

use anyhow::{Context, Result};
use bytes::{BufMut, BytesMut};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tracing::{error, info, warn};

use super::super::wal_replication::{WalReplicationRecord, WalAckMessage};
use super::super::raft_cluster::RaftCluster;

/// ACK magic number ('AK' in hex)
const ACK_MAGIC: u16 = 0x414B;

/// Current protocol version
const PROTOCOL_VERSION: u16 = 1;

/// Record processor
///
/// Handles WAL record processing and ACK protocol.
pub struct RecordProcessor;

impl RecordProcessor {
    /// Process metadata WAL record (__chronik_metadata)
    ///
    /// Complexity: < 25 (metadata event deserialization and application)
    pub async fn handle_metadata_record(
        produce_handler: &Arc<crate::produce_handler::ProduceHandler>,
        raft_cluster: &Option<Arc<RaftCluster>>,
        record: &WalReplicationRecord,
        metadata_store: &Option<Arc<dyn chronik_common::metadata::MetadataStore>>,
    ) -> Result<()> {
        use crate::raft_metadata::MetadataCommand;
        use chronik_common::metadata::{MetadataEvent as CommonMetadataEvent, MetadataEventPayload};

        // v2.2.9 Phase 7 FIX #5: Try deserializing as MetadataEvent first (new format)
        // Then fall back to MetadataCommand (legacy format)
        if let Ok(event) = CommonMetadataEvent::from_bytes(&record.data) {
            tracing::info!(
                "📥 Follower received MetadataEvent at offset {}: {:?}",
                record.base_offset,
                event.payload
            );

            // Apply to metadata_store if available
            if let Some(ref store) = metadata_store {
                if let Err(e) = store.apply_replicated_event(event.clone()).await {
                    tracing::warn!("Failed to apply replicated MetadataEvent: {}", e);
                } else {
                    match &event.payload {
                        MetadataEventPayload::PartitionAssigned { assignment } => {
                            tracing::info!(
                                "✅ Follower applied PartitionAssigned: {}-{}, leader={}, replicas={:?}",
                                assignment.topic, assignment.partition, assignment.leader_id, assignment.replicas
                            );
                        }
                        MetadataEventPayload::HighWatermarkUpdated { topic, partition, new_watermark } => {
                            tracing::info!(
                                "✅ Follower applied HighWatermarkUpdated: {}-{} => {}",
                                topic, partition, new_watermark
                            );
                        }
                        _ => {
                            tracing::debug!("✅ Follower applied MetadataEvent: {:?}", event.payload);
                        }
                    }
                }
            } else {
                tracing::warn!("Received MetadataEvent but no metadata_store configured!");
            }

            return Ok(());
        }

        // Legacy path: Try deserializing as MetadataCommand
        let cmd: MetadataCommand = bincode::deserialize(&record.data)
            .context("Failed to deserialize MetadataCommand from replicated data")?;

        tracing::debug!(
            "Phase 2.3: Follower received metadata replication at offset {}: {:?}",
            record.base_offset,
            cmd
        );

        // v2.2.9 Option 4: Partition metadata moved to WalMetadataStore
        // Most commands are now handled via MetadataEvent path above

        // Fire notification for any waiting threads
        // (Followers might have threads waiting for metadata updates via Phase 1 forwarding)
        if let Some(ref raft) = raft_cluster {
            let pending_brokers = raft.get_pending_brokers_notifications();

            match &cmd {
                MetadataCommand::RegisterBroker { broker_id, .. } => {
                    if let Some((_, notify)) = pending_brokers.remove(broker_id) {
                        notify.notify_waiters();
                        tracing::debug!("Notified waiting threads for broker {}", broker_id);
                    }
                }
                _ => {}
            }
        }

        Ok(())
    }

    /// Write normal WAL record to local storage
    ///
    /// Complexity: < 25 (WAL write + watermark updates)
    pub async fn write_normal_record(
        wal_manager: &Arc<chronik_wal::WalManager>,
        record: &WalReplicationRecord,
        _raft_cluster: &Option<Arc<RaftCluster>>,
        produce_handler: &Option<Arc<crate::produce_handler::ProduceHandler>>,
        metadata_store: &Option<Arc<dyn chronik_common::metadata::MetadataStore>>,
    ) -> Result<()> {
        // Deserialize CanonicalRecord from data
        use chronik_storage::canonical_record::CanonicalRecord;
        let canonical_record: CanonicalRecord = bincode::deserialize(&record.data)
            .context("Failed to deserialize CanonicalRecord from replicated data")?;

        // Serialize back to bincode for WAL storage
        let serialized = bincode::serialize(&canonical_record)
            .context("Failed to serialize CanonicalRecord for WAL")?;

        // Write to local WAL (acks=1 for immediate fsync on follower)
        wal_manager.append_canonical_with_acks(
            record.topic.clone(),
            record.partition,
            serialized,
            record.base_offset,
            record.base_offset + record.record_count as i64 - 1,
            record.record_count as i32,
            1, // acks=1 for immediate fsync
        ).await
        .context("Failed to append replicated record to local WAL")?;

        // v2.2.9 FIX: Update high watermark on follower via ProduceHandler
        // This ensures watermarks are visible via ListOffsets API (which queries ProduceHandler, not Raft)
        // Trust the leader - we're receiving replicated state
        if let Some(ref handler) = produce_handler {
            let new_watermark = record.base_offset + record.record_count as i64;

            if let Err(e) = handler.update_high_watermark(&record.topic, record.partition, new_watermark).await {
                warn!("❌ Failed to update watermark for {}-{} to {}: {}",
                    record.topic, record.partition, new_watermark, e);
            } else {
                info!("✅ v2.2.9 Phase 7 DEBUG: Updated watermark for {}-{} to {}",
                    record.topic, record.partition, new_watermark);
            }
        } else {
            warn!("⚠️ v2.2.9 Phase 7 DEBUG: produce_handler is None! Cannot update watermark for {}-{}",
                record.topic, record.partition);
        }

        // v2.2.9 Phase 7 FIX: ALSO update MetadataStore partition offsets
        // ListOffsets API queries metadata_store, NOT ProduceHandler
        // This was the root cause of Bug #4: followers had correct ProduceHandler watermarks
        // but metadata_store offsets were 0, causing ListOffsets to timeout
        if let Some(ref store) = metadata_store {
            let new_watermark = record.base_offset + record.record_count as i64;
            if let Err(e) = store.update_partition_offset(
                &record.topic,
                record.partition as u32,
                new_watermark,
                0  // log_start_offset - we don't track this separately yet
            ).await {
                warn!("❌ Failed to update metadata_store offset for {}-{} to {}: {}",
                    record.topic, record.partition, new_watermark, e);
            } else {
                info!("✅ v2.2.9 Phase 7 FIX: Updated MetadataStore offset for {}-{} to {} (for ListOffsets API)",
                    record.topic, record.partition, new_watermark);
            }
        } else {
            warn!("⚠️ v2.2.9 Phase 7: metadata_store is None! ListOffsets API will fail on follower for {}-{}",
                record.topic, record.partition);
        }

        Ok(())
    }

    /// Send ACK frame back to leader
    ///
    /// Complexity: < 15 (ACK serialization and frame sending)
    pub async fn send_ack(stream: &mut TcpStream, ack_msg: &WalAckMessage) -> Result<()> {
        info!(
            "🔍 DEBUG send_ack: Serializing ACK for {}-{} offset {} from node {}",
            ack_msg.topic, ack_msg.partition, ack_msg.offset, ack_msg.node_id
        );

        // Serialize ACK message
        let serialized = bincode::serialize(ack_msg)
            .context("Failed to serialize ACK message")?;

        info!(
            "🔍 DEBUG send_ack: Serialized {} bytes, building frame with magic 0x{:04x}",
            serialized.len(), ACK_MAGIC
        );

        // Build ACK frame: [magic(2) | version(2) | length(4) | payload(N)]
        let mut frame = BytesMut::with_capacity(8 + serialized.len());
        frame.put_u16(ACK_MAGIC); // 'AK' magic number
        frame.put_u16(PROTOCOL_VERSION);
        frame.put_u32(serialized.len() as u32);
        frame.put_slice(&serialized);

        info!(
            "🔍 DEBUG send_ack: Frame built ({} bytes total), writing to stream",
            frame.len()
        );

        // Send frame
        stream.write_all(&frame).await
            .context("Failed to send ACK frame to leader")?;

        // CRITICAL: Flush to ensure ACK is actually sent over network
        stream.flush().await
            .context("Failed to flush ACK frame")?;

        info!(
            "🔍 DEBUG send_ack: Flushed {} bytes to leader",
            frame.len()
        );

        Ok(())
    }

    /// Process WAL record (orchestration function)
    ///
    /// Complexity: < 20 (decision tree + delegation to handlers)
    pub async fn process_record(
        record: WalReplicationRecord,
        wal_manager: &Arc<chronik_wal::WalManager>,
        raft_cluster: &Option<Arc<RaftCluster>>,
        produce_handler: &Option<Arc<crate::produce_handler::ProduceHandler>>,
        metadata_store: &Option<Arc<dyn chronik_common::metadata::MetadataStore>>,
        isr_ack_tracker: &Option<Arc<crate::isr_ack_tracker::IsrAckTracker>>,
        stream: &mut TcpStream,
        node_id: u64,
    ) -> Result<()> {
        // Phase 2.3: Special handling for metadata WAL replication
        if record.topic == "__chronik_metadata" && record.partition == 0 {
            if let Some(ref ph) = produce_handler {
                if let Err(e) = Self::handle_metadata_record(ph, raft_cluster, &record, metadata_store).await {
                    error!(
                        "Failed to apply metadata WAL record: {}",
                        e
                    );
                } else {
                    info!(
                        "METADATA✓ Replicated: {}-{} offset {} ({} bytes)",
                        record.topic,
                        record.partition,
                        record.base_offset,
                        record.data.len()
                    );
                }
            } else {
                warn!(
                    "Received metadata WAL replication but no ProduceHandler configured (v2.2.9 - watermarks won't update!)"
                );
            }
            return Ok(()); // Skip normal WAL write for metadata
        }

        // v2.2.9 FIX: Accept all WAL replication from leader (trust the leader)
        //
        // REMOVED: Raft metadata check that was causing race condition data loss
        // - Metadata is replicated via __chronik_metadata WAL topic (fast, microseconds)
        // - Partition assignments arrive before or concurrently with partition data
        // - The old Raft check rejected data if metadata hadn't synced yet (race condition)
        // - Result: 100% data loss on followers despite acks=all
        //
        // NEW APPROACH: Trust the leader to send correct replicas
        // - Leader already knows which nodes are replicas (from its own metadata)
        // - Leader only sends to nodes in the replica set
        // - If we receive WAL data, the leader thinks we should have it → accept it
        // - Metadata will arrive via __chronik_metadata (already implemented above)
        //
        // Safety: This is safe because:
        // 1. Leader sends metadata via __chronik_metadata BEFORE or WITH partition data
        // 2. Even if there's a race, metadata arrives within milliseconds
        // 3. Accepting "extra" data is better than losing data (can always clean up later)
        // 4. WAL compaction will remove data for partitions we don't own anymore
        //
        // See docs/RAFT_VS_WAL_METADATA_REDUNDANCY_ANALYSIS.md for full analysis

        info!(
            "✅ Accepting WAL replication for {}-{} from leader (node {})",
            record.topic, record.partition, node_id
        );

        // Write to local WAL (normal partition data)
        if let Err(e) = Self::write_normal_record(wal_manager, &record, raft_cluster, produce_handler, metadata_store).await {
            error!(
                "Failed to write replicated WAL record to local WAL for {}-{}: {}",
                record.topic, record.partition, e
            );
        } else {
            info!(
                "WAL✓ Replicated: {}-{} offset {} ({} records, {} bytes)",
                record.topic,
                record.partition,
                record.base_offset,
                record.record_count,
                record.data.len()
            );

            // v2.2.7 Phase 4: Send ACK back to leader for acks=-1 support
            if let Some(ref _tracker) = isr_ack_tracker {
                info!(
                    "🔍 DEBUG: Preparing to send ACK for {}-{} offset {} from node {}",
                    record.topic, record.partition, record.base_offset, node_id
                );

                let ack_msg = WalAckMessage {
                    topic: record.topic.clone(),
                    partition: record.partition,
                    offset: record.base_offset,
                    node_id,
                };

                // Send ACK frame back to leader
                // NOTE: ACK is sent over TCP to leader's ACK reader
                // The ACK reader on the leader will call tracker.record_ack()
                // We do NOT call tracker.record_ack() here (that would be wrong - followers don't track their own ACKs)
                if let Err(e) = Self::send_ack(stream, &ack_msg).await {
                    error!(
                        "❌ Failed to send ACK for {}-{} offset {}: {}",
                        record.topic, record.partition, record.base_offset, e
                    );
                } else {
                    info!(
                        "✅ ACK SENT to leader: {}-{} offset {} from node {}",
                        record.topic, record.partition, record.base_offset, node_id
                    );
                }
            } else {
                warn!(
                    "⚠️ DEBUG: No ISR ACK tracker configured - ACKs will NOT be sent for {}-{} offset {}",
                    record.topic, record.partition, record.base_offset
                );
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ack_magic_constant() {
        assert_eq!(ACK_MAGIC, 0x414B);
    }

    #[test]
    fn test_protocol_version() {
        assert_eq!(PROTOCOL_VERSION, 1);
    }
}
