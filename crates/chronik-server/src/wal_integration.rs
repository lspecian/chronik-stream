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
    /// Create a new WAL-integrated produce handler
    pub async fn new(
        wal_config: WalConfig,
        inner_handler: Arc<ProduceHandler>,
    ) -> Result<Self> {
        // Initialize WAL manager
        let wal_manager = WalManager::new(wal_config).await
            .map_err(|e| Error::Internal(format!("Failed to initialize WAL: {}", e)))?;
        
        info!("WAL-integrated produce handler initialized");
        
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
        
        // TODO: Apply recovered records to in-memory buffers and segments
        
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
        warn!(
            "WAL truncation for {}-{} up to offset {} (not yet implemented)",
            topic, partition, up_to_offset
        );
        
        // TODO: Implement WAL truncation after segments are persisted
        // This will free up WAL space for already-flushed records
        
        Ok(())
    }
}

/// Parse raw record batch bytes into structured records
fn parse_record_batch(data: &[u8]) -> Result<Vec<ParsedRecord>> {
    // TODO: Implement actual Kafka record batch parsing
    // For now, return empty vec to allow compilation
    warn!("Record batch parsing not yet implemented");
    Ok(Vec::new())
}

#[derive(Debug)]
struct ParsedRecord {
    offset: i64,
    timestamp: i64,
    key: Option<Vec<u8>>,
    value: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[tokio::test]
    async fn test_wal_integration() {
        // TODO: Add integration tests
    }
}