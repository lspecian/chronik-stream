//! WAL integration for durability guarantees
//! 
//! This module integrates the Write-Ahead Log (WAL) subsystem with the produce handler
//! to ensure zero message loss even on crash.

use crate::produce_handler::ProduceHandler;
use chronik_wal::WalManager;
use chronik_common::Result;
use chronik_protocol::{ProduceRequest, ProduceResponse};
use std::sync::Arc;
use tracing::{error, info, instrument};

/// Exit code of a broker stopped by a failed WAL write.
pub const WAL_FAILURE_EXIT_CODE: i32 = 74; // EX_IOERR

/// What a failed WAL write or fsync does to the broker: it stops.
///
/// The failed partition is already fenced and every waiting producer has its
/// error (see `GroupCommitWal::set_failure_callback`), but a fenced partition
/// stays down until a restart, keeps its leadership in a cluster, and keeps
/// any producer that retries failing. After a failed fsync the file's tail
/// cannot be trusted anyway (the kernel may have dropped the pages and cleared
/// the error), and every partition shares the disk that failed. So the broker
/// stops, as Kafka does when its only log directory fails: on restart,
/// recovery reads what the files really hold and cuts a torn tail, and in a
/// cluster another replica leads meanwhile. Nothing acknowledged is lost: a
/// write is acknowledged only after its fsync succeeded, and producer
/// sequences live in memory, so a retry is written, not answered as a
/// duplicate of records that never reached the disk.
pub fn halt_on_wal_failure(wal: &'static str) -> chronik_wal::group_commit::FailureCallback {
    Arc::new(move |topic: &str, partition: i32, e: &chronik_wal::WalError| {
        error!(
            wal,
            topic,
            partition,
            error = %e,
            "WAL write failed; stopping the broker (exit {}). Fix the disk and restart: \
             recovery keeps every acknowledged record",
            WAL_FAILURE_EXIT_CODE
        );
        std::process::exit(WAL_FAILURE_EXIT_CODE);
    })
}

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

        // Also shutdown the inner ProduceHandler (flushes buffered partitions)
        if let Err(e) = self.inner_handler.shutdown().await {
            error!("ProduceHandler shutdown failed: {}", e);
        }

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
    

    #[tokio::test]
    async fn test_wal_integration() {
        // TODO: Add integration tests
    }
}