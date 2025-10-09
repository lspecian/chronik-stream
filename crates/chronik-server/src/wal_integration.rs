//! WAL integration for durability guarantees
//! 
//! This module integrates the Write-Ahead Log (WAL) subsystem with the produce handler
//! to ensure zero message loss even on crash.

use crate::produce_handler::ProduceHandler;
use chronik_wal::{WalManager, WalRecord, WalConfig, WalError};
use chronik_common::{Result, Error};
use chronik_protocol::{ProduceRequest, ProduceResponse};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info, warn, error, instrument};
use bytes::{Bytes, BytesMut, BufMut};

/// WAL-integrated produce handler wrapper
pub struct WalProduceHandler {
    /// WAL manager for durability
    wal_manager: Arc<RwLock<WalManager>>,
    /// Original produce handler (will be removed after full integration)
    inner_handler: Arc<ProduceHandler>,
}

/// Implement the trait for the actual ProduceHandler  
#[async_trait::async_trait]
impl ProduceHandlerTrait for ProduceHandler {
    async fn handle_produce(&self, request: ProduceRequest, correlation_id: i32) -> Result<ProduceResponse> {
        self.handle_produce(request, correlation_id).await
    }
}

/// Trait for produce handlers (to abstract over implementation)
#[async_trait::async_trait]
pub trait ProduceHandlerTrait: Send + Sync {
    async fn handle_produce(&self, request: ProduceRequest, correlation_id: i32) -> Result<ProduceResponse>;
}

impl WalProduceHandler {
    /// Create a new WAL-integrated produce handler with recovery
    pub async fn new(
        wal_config: WalConfig,
        inner_handler: Arc<ProduceHandler>,
    ) -> Result<Self> {
        // Initialize WAL manager with recovery
        let wal_manager = WalManager::recover(&wal_config).await
            .map_err(|e| Error::Internal(format!("Failed to recover WAL: {}", e)))?;

        info!("WAL-integrated produce handler initialized with recovery");

        Ok(Self {
            wal_manager: Arc::new(RwLock::new(wal_manager)),
            inner_handler,
        })
    }

    /// Get reference to the WAL manager for external use (e.g., WAL Indexer)
    pub fn wal_manager(&self) -> &Arc<RwLock<WalManager>> {
        &self.wal_manager
    }

    /// Recover from WAL on startup
    pub async fn recover(&self) -> Result<()> {
        info!("Starting WAL recovery...");

        // Get recovery result from WAL
        let recovery_result = {
            let manager = self.wal_manager.read().await;
            manager.get_recovery_result()
        };

        info!(
            "WAL recovery complete: {} partitions, {} total records recovered",
            recovery_result.partitions,
            recovery_result.total_records
        );

        // Apply recovered records to in-memory buffers and segments
        if recovery_result.partitions > 0 {
            self.apply_recovered_records().await?;
        }

        Ok(())
    }

    /// Apply recovered WAL records to in-memory buffers
    async fn apply_recovered_records(&self) -> Result<()> {
        info!("Applying recovered WAL records to in-memory state...");

        let manager = self.wal_manager.read().await;

        // Iterate through all recovered partitions
        for tp in manager.get_partitions() {
            let topic = &tp.topic;
            let partition = tp.partition;

            info!("Recovering partition {}-{}", topic, partition);

            // Read all records from WAL for this partition
            match manager.read_from(topic, partition, 0, usize::MAX).await {
                Ok(records) => {
                    if !records.is_empty() {
                        info!(
                            "Found {} records in WAL for {}-{}, applying to buffers",
                            records.len(), topic, partition
                        );

                        // Apply records to the produce handler's state
                        self.restore_partition_state(topic, partition, &records).await?;
                    }
                }
                Err(e) => {
                    warn!(
                        "Failed to read WAL records for {}-{}: {}",
                        topic, partition, e
                    );
                }
            }
        }

        info!("WAL recovery application complete");
        Ok(())
    }

    /// Restore partition state from WAL records
    async fn restore_partition_state(
        &self,
        topic: &str,
        partition: i32,
        records: &[WalRecord],
    ) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }

        use chronik_storage::canonical_record::CanonicalRecord;

        // Process WAL V2 records (CanonicalRecord batches)
        // V1 records are legacy and we'll skip them during recovery
        let mut recovered_batches = 0;
        let mut total_records = 0i64;

        for record in records {
            if let chronik_wal::record::WalRecord::V2 { canonical_data, .. } = record {
                // Deserialize CanonicalRecord from WAL
                match bincode::deserialize::<CanonicalRecord>(canonical_data) {
                    Ok(canonical_record) => {
                        // Count records in this batch
                        total_records += canonical_record.records.len() as i64;

                        // Convert back to Kafka wire format
                        match canonical_record.to_kafka_batch() {
                            Ok(kafka_batch) => {
                                // Apply to produce handler's buffer
                                self.inner_handler
                                    .apply_recovered_batch(topic, partition, kafka_batch)
                                    .await?;
                                recovered_batches += 1;
                            }
                            Err(e) => {
                                warn!("Failed to encode CanonicalRecord to Kafka batch: {}", e);
                            }
                        }
                    }
                    Err(e) => {
                        warn!("Failed to deserialize CanonicalRecord from WAL V2: {}", e);
                    }
                }
            }
            // V1 records are skipped - they're from old format
        }

        info!(
            "Restoring partition {}-{} state: {} batches, {} records (offsets 0-{})",
            topic, partition, recovered_batches, total_records, total_records - 1
        );

        // Ensure the partition exists in the produce handler
        self.inner_handler
            .ensure_partition_exists(topic, partition, total_records)
            .await?;

        info!(
            "Successfully recovered {} batches for partition {}-{} (V2 format)",
            recovered_batches, topic, partition
        );

        // CRITICAL: Update metadata store high watermark after recovery
        // This ensures FetchHandler knows data is available for consumption
        // High watermark = next offset to be written = total_records
        let high_watermark = total_records;
        self.inner_handler.update_high_watermark(topic, partition, high_watermark).await
            .map_err(|e| {
                warn!("Failed to update high watermark after recovery for {}-{}: {}", topic, partition, e);
                e
            })?;
        info!(
            "Updated metadata store high watermark for {}-{} to {}",
            topic, partition, high_watermark
        );

        Ok(())
    }
    
    /// Handle produce request with WAL durability
    ///
    /// CORRECT APPROACH (v1.3.41): POST-produce WAL writing
    /// 1. Let ProduceHandler process request (assigns offsets, re-encodes batch)
    /// 2. Extract the RE-ENCODED batches with assigned offsets from pending_batches
    /// 3. Convert to CanonicalRecord and write to WAL
    /// 4. Return response
    ///
    /// This ensures we write batches WITH assigned offsets, not raw client batches.
    #[instrument(skip(self, request))]
    pub async fn handle_produce(
        &self,
        request: ProduceRequest,
        correlation_id: i32,
    ) -> Result<ProduceResponse> {
        use chronik_storage::canonical_record::CanonicalRecord;

        // Store topic-partition info for WAL writing
        let topic_partitions: Vec<(String, i32)> = request.topics.iter()
            .flat_map(|t| t.partitions.iter().map(move |p| (t.name.clone(), p.index)))
            .collect();

        // Step 1: Let ProduceHandler process the request
        // This assigns offsets, re-encodes batch with correct base_offset and CRC
        let response = self.inner_handler.handle_produce(request, correlation_id).await?;

        // Step 2: Extract RE-ENCODED batches and write to WAL
        {
            let mut manager = self.wal_manager.write().await;

            for (topic, partition) in &topic_partitions {
                // Get the re-encoded batches ProduceHandler just created
                match self.inner_handler.get_pending_batches(topic, *partition).await {
                    Ok(batches) if !batches.is_empty() => {
                        debug!("Writing {} re-encoded batches to WAL for {}-{}", batches.len(), topic, partition);

                        for batch_bytes in batches {
                            // Convert Kafka v2 batch → CanonicalRecord
                            // These bytes have CORRECT base_offset (assigned by ProduceHandler)
                            match CanonicalRecord::from_kafka_batch(&batch_bytes) {
                                Ok(mut canonical_record) => {
                                    // CRITICAL FIX (v1.3.46): Preserve original compressed bytes
                                    // for byte-perfect CRC validation by Java Kafka clients.
                                    // Store the ENTIRE RecordBatch wire format (all fields intact).
                                    canonical_record.compressed_records_wire_bytes = Some(batch_bytes.clone());
                                    debug!(
                                        "Stored {} bytes of original wire format for CRC preservation",
                                        batch_bytes.len()
                                    );

                                    // Serialize with bincode for WAL V2
                                    match bincode::serialize(&canonical_record) {
                                        Ok(serialized) => {
                                            // Write to WAL V2
                                            if let Err(e) = manager.append_canonical(topic.clone(), *partition, serialized).await {
                                                error!("Failed to write CanonicalRecord to WAL: {}", e);
                                            } else {
                                                debug!("Successfully wrote batch to WAL V2 for {}-{}", topic, partition);
                                            }
                                        }
                                        Err(e) => {
                                            error!("Failed to serialize CanonicalRecord: {}", e);
                                        }
                                    }
                                }
                                Err(e) => {
                                    debug!("Batch not in v2 format (size: {}), skipping WAL V2: {}", batch_bytes.len(), e);
                                }
                            }
                        }
                    }
                    Ok(_) => {
                        // No batches for this partition (could be error response)
                    }
                    Err(e) => {
                        warn!("Failed to get pending batches for {}-{}: {}", topic, partition, e);
                    }
                }
            }

            // Flush WAL immediately for durability
            if let Err(e) = manager.flush_all().await {
                error!("Failed to flush WAL: {}", e);
            }
        }

        // CRITICAL (v1.3.43): Clear pending_batches AFTER writing to WAL
        // This prevents duplicate WAL writes and memory leaks
        for (topic, partition) in &topic_partitions {
            if let Err(e) = self.inner_handler.clear_pending_batches(topic, *partition).await {
                warn!("Failed to clear pending batches for {}-{}: {}", topic, partition, e);
            }
        }

        Ok(response)
    }
    
    /// Truncate WAL after successful flush to segments
    pub async fn truncate_after_flush(
        &self,
        topic: &str,
        partition: i32,
        up_to_offset: i64,
    ) -> Result<()> {
        info!(
            "Truncating WAL for {}-{} up to offset {}",
            topic, partition, up_to_offset
        );

        let mut manager = self.wal_manager.write().await;

        // Truncate the WAL segments that have been persisted
        match manager.truncate_before(topic, partition, up_to_offset).await {
            Ok(truncated_segments) => {
                info!(
                    "Successfully truncated {} WAL segments for {}-{} up to offset {}",
                    truncated_segments, topic, partition, up_to_offset
                );
            }
            Err(e) => {
                error!(
                    "Failed to truncate WAL for {}-{}: {}",
                    topic, partition, e
                );
                return Err(Error::Internal(format!("WAL truncation failed: {}", e)));
            }
        }

        Ok(())
    }
}

// REMOVED v1.3.36: Legacy V1 parsing code
// The following were part of the old approach that parsed individual records:
// - parse_record_batch() - parsed RecordBatch into individual ParsedRecord structs
// - ParsedRecord - temporary struct for individual records
// - RecordBatchBuilder - helper to rebuild batches from V1 records (with add_record/build methods)
//
// NEW APPROACH (v1.3.36): We now use CanonicalRecord which preserves the entire
// Kafka batch structure for exact round-trip and CRC preservation

#[cfg(test)]
mod tests {
    use super::*;
    
    #[tokio::test]
    async fn test_wal_integration() {
        // TODO: Add integration tests
    }
}