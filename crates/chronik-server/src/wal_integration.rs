//! WAL integration for durability guarantees
//! 
//! This module integrates the Write-Ahead Log (WAL) subsystem with the produce handler
//! to ensure zero message loss even on crash.

use crate::produce_handler::ProduceHandler;
use chronik_wal::WalManager;
use chronik_common::Result;
use chronik_protocol::{ProduceRequest, ProduceResponse};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, instrument};
use bytes::{Bytes, BytesMut, BufMut};

/// WAL-integrated produce handler wrapper
/// v1.3.47+: Uses Arc<WalManager> directly for lock-free concurrent access
pub struct WalProduceHandler {
    /// WAL manager for durability (no RwLock - WalManager uses DashMap internally)
    wal_manager: Arc<WalManager>,
    /// Original produce handler (will be removed after full integration)
    inner_handler: Arc<ProduceHandler>,
}

impl WalProduceHandler {

    /// Create a passthrough wrapper for backward compatibility (v1.3.47+)
    /// WAL writes are now handled inline in ProduceHandler
    pub fn new_passthrough(
        wal_manager: Arc<WalManager>,
        inner_handler: Arc<ProduceHandler>,
    ) -> Self {
        // v1.3.53+: GroupCommitWal handles flushing internally via background workers
        // No need for periodic flusher anymore

        info!("WAL passthrough handler initialized (v1.3.53: GroupCommitWal)");

        Self {
            wal_manager,
            inner_handler,
        }
    }

    /// Get reference to the WAL manager for external use (e.g., WAL Indexer)
    /// v1.3.47+: Returns Arc<WalManager> directly (no RwLock - WalManager uses DashMap internally)
    pub fn wal_manager(&self) -> &Arc<WalManager> {
        &self.wal_manager
    }

    /// Handle produce request (v1.3.47: passthrough, WAL now handled inline)
    ///
    /// This is now a simple passthrough since WAL writes happen inline in ProduceHandler.
    /// Kept for backward compatibility with code that expects WalProduceHandler.
    #[instrument(skip(self, request))]
    pub async fn handle_produce(
        &self,
        request: ProduceRequest,
        correlation_id: i32,
    ) -> Result<ProduceResponse> {
        // WAL is now written inline in ProduceHandler before high watermark update
        // This eliminates race conditions and guarantees durability
        self.inner_handler.handle_produce(request, correlation_id).await
    }

    /// Passthrough for a request whose origin and connection order are known;
    /// see [`ProduceHandler::handle_produce_ordered`].
    pub async fn handle_produce_ordered(
        &self,
        request: ProduceRequest,
        correlation_id: i32,
        forwarded: bool,
        ticket: Option<Arc<crate::produce_order::ProduceTicket>>,
    ) -> Result<ProduceResponse> {
        self.inner_handler
            .handle_produce_ordered(request, correlation_id, forwarded, ticket)
            .await
    }
    
    /// Shutdown the WAL and flush all pending segments to disk
    pub async fn shutdown(&self) {
        info!("Shutting down WAL produce handler...");

        // Shutdown the WAL manager (flushes pending commits and seals active segments)
        self.wal_manager.shutdown().await;

        // Also shutdown the inner ProduceHandler
        self.inner_handler.shutdown().await;

        info!("WAL produce handler shutdown complete");
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