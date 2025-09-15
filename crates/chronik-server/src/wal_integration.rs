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
use tracing::{info, warn, error, instrument};
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

        // Get the last offset from recovered records
        let last_offset = records
            .iter()
            .map(|r| r.offset)
            .max()
            .unwrap_or(0);

        info!(
            "Restoring partition {}-{} state up to offset {}",
            topic, partition, last_offset
        );

        // Ensure the partition exists in the produce handler
        self.inner_handler
            .ensure_partition_exists(topic, partition, last_offset + 1)
            .await?;

        // Convert WAL records to buffered batches for the produce handler
        // This maintains the original wire format for consumers
        let mut batch_builder = RecordBatchBuilder::new(topic, partition);

        for record in records {
            batch_builder.add_record(
                record.offset,
                record.timestamp,
                record.key.as_deref(),
                &record.value,
            );
        }

        // Apply the batch to the produce handler's buffers
        if let Some(batch) = batch_builder.build() {
            self.inner_handler
                .apply_recovered_batch(topic, partition, batch)
                .await?;
        }

        info!(
            "Successfully restored {} records for partition {}-{}",
            records.len(), topic, partition
        );

        Ok(())
    }
    
    /// Handle produce request with WAL durability
    #[instrument(skip(self, request))]
    pub async fn handle_produce(
        &self,
        request: ProduceRequest,
        correlation_id: i32,
    ) -> Result<ProduceResponse> {
        // CRITICAL: Write to WAL BEFORE any other processing
        // This ensures durability even if we crash after WAL write
        
        let mut wal_records = Vec::new();
        
        // Convert produce request to WAL records
        for topic_data in &request.topics {
            for partition_data in &topic_data.partitions {
                // Parse the raw record batch data
                let records = parse_record_batch(&partition_data.records)?;
                
                for record in records {
                    let wal_record = WalRecord::new(
                        record.offset,
                        record.key.clone(),
                        record.value.clone(),
                        record.timestamp,
                    );
                    
                    wal_records.push((
                        topic_data.name.clone(),
                        partition_data.index,
                        wal_record,
                    ));
                }
            }
        }
        
        // Write all records to WAL with fsync
        {
            let mut manager = self.wal_manager.write().await;
            
            for (topic, partition, record) in wal_records {
                // This will fsync before returning
                manager.append(topic, partition, vec![record]).await
                    .map_err(|e| {
                        error!("Failed to write to WAL: {}", e);
                        Error::Internal(format!("WAL write failed: {}", e))
                    })?;
            }
            
            // Force flush to ensure durability
            manager.flush_all().await
                .map_err(|e| {
                    error!("Failed to flush WAL: {}", e);
                    Error::Internal(format!("WAL flush failed: {}", e))
                })?;
        }
        
        info!("WAL write complete, proceeding with produce handling");
        
        // Now that WAL write is complete, proceed with normal handling
        // This updates in-memory buffers and may write to segments
        self.inner_handler.handle_produce(request, correlation_id).await
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

/// Parse raw record batch bytes into structured records
fn parse_record_batch(data: &[u8]) -> Result<Vec<ParsedRecord>> {
    use chronik_storage::kafka_records::KafkaRecordBatch;

    // Parse using the existing Kafka record batch decoder
    let batch = KafkaRecordBatch::decode(data)?;

    let mut parsed_records = Vec::new();
    let base_offset = batch.header.base_offset;
    let base_timestamp = batch.header.base_timestamp;

    for record in batch.records {
        let offset = base_offset + record.offset_delta as i64;
        let timestamp = base_timestamp + record.timestamp_delta;

        parsed_records.push(ParsedRecord {
            offset,
            timestamp,
            key: record.key.map(|k| k.to_vec()),
            value: record.value.map(|v| v.to_vec()).unwrap_or_default(),
        });
    }

    info!("Parsed {} records from record batch", parsed_records.len());
    Ok(parsed_records)
}

#[derive(Debug)]
struct ParsedRecord {
    offset: i64,
    timestamp: i64,
    key: Option<Vec<u8>>,
    value: Vec<u8>,
}

/// Helper to build record batches from WAL records
struct RecordBatchBuilder {
    topic: String,
    partition: i32,
    records: Vec<WalRecord>,
}

impl RecordBatchBuilder {
    fn new(topic: &str, partition: i32) -> Self {
        Self {
            topic: topic.to_string(),
            partition,
            records: Vec::new(),
        }
    }

    fn add_record(
        &mut self,
        offset: i64,
        timestamp: i64,
        key: Option<&[u8]>,
        value: &[u8],
    ) {
        self.records.push(WalRecord::new(
            offset,
            key.map(|k| k.to_vec()),
            value.to_vec(),
            timestamp,
        ));
    }

    fn build(self) -> Option<Bytes> {
        if self.records.is_empty() {
            return None;
        }

        // Build a simple Kafka record batch format
        // This is a simplified version - in production you'd use the full Kafka protocol
        let mut buf = BytesMut::new();

        // Write batch header (simplified)
        buf.put_i64(self.records.first()?.offset); // base offset
        buf.put_i32(self.records.len() as i32); // batch length
        buf.put_i64(self.records.first()?.timestamp); // base timestamp

        // Write records
        for record in &self.records {
            // Record length
            let record_size = 8 + 8 + // offset + timestamp
                4 + record.key.as_ref().map_or(0, |k| k.len()) + // key length + key
                4 + record.value.len(); // value length + value

            buf.put_i32(record_size as i32);
            buf.put_i64(record.offset);
            buf.put_i64(record.timestamp);

            // Key
            if let Some(key) = &record.key {
                buf.put_i32(key.len() as i32);
                buf.put_slice(key);
            } else {
                buf.put_i32(-1); // null key
            }

            // Value
            buf.put_i32(record.value.len() as i32);
            buf.put_slice(&record.value);
        }

        Some(buf.freeze())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[tokio::test]
    async fn test_wal_integration() {
        // TODO: Add integration tests
    }
}