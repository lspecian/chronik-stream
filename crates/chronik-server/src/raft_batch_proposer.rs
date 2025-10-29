//! Raft batch proposer - proposes WAL batches to Raft for replication
//!
//! This module implements the batch proposer worker that receives notifications
//! from the WAL when batches are committed locally, and proposes them to Raft
//! for replication across the cluster.
//!
//! ## Architecture
//!
//! ```text
//! Producer → WAL (local fsync) → BatchNotify → RaftBatchProposer → Raft propose()
//!                                                     ↓
//!                                            ONE RwLock per batch!
//!                                           (vs 64 locks per batch before)
//! ```
//!
//! ## Performance Impact
//!
//! - Before: 64 concurrent producers × 40ms Raft = 2,560ms (25 msg/s)
//! - After: 1 batch proposal × 40ms = 40ms (1,600 msg/s!)
//!
//! ## Acks Semantics
//!
//! - acks=0,1: Return after WAL write (don't wait for Raft)
//! - acks=-1: Register waiter, wait for Raft commit notification

use chronik_wal::BatchMetadata;
use std::sync::Arc;
use tokio::sync::mpsc;
use dashmap::DashMap;
use tracing::{info, warn, error};

#[cfg(feature = "raft")]
use crate::raft_integration::RaftReplicaManager;

/// Batch proposer that receives WAL batch notifications and proposes to Raft
#[cfg(feature = "raft")]
pub struct RaftBatchProposer {
    /// Raft replica manager
    raft_manager: Arc<RaftReplicaManager>,

    /// Receive batch notifications from WAL
    batch_rx: mpsc::UnboundedReceiver<BatchMetadata>,

    /// Track pending batches for acks=-1 support
    /// Key: (topic, partition, offset), Value: Vec of oneshot senders
    pending_batches: Arc<DashMap<(String, i32, i64), Vec<tokio::sync::oneshot::Sender<Result<(), String>>>>>,
}

#[cfg(feature = "raft")]
impl RaftBatchProposer {
    /// Create a new batch proposer
    pub fn new(
        raft_manager: Arc<RaftReplicaManager>,
        batch_rx: mpsc::UnboundedReceiver<BatchMetadata>,
    ) -> Self {
        Self {
            raft_manager,
            batch_rx,
            pending_batches: Arc::new(DashMap::new()),
        }
    }

    /// Get a shared reference to pending_batches (for ProduceHandler to register waiters)
    pub fn pending_batches(&self) -> Arc<DashMap<(String, i32, i64), Vec<tokio::sync::oneshot::Sender<Result<(), String>>>>> {
        self.pending_batches.clone()
    }

    /// Run the batch proposer loop (background task)
    pub async fn run(mut self) {
        info!("RaftBatchProposer started - waiting for WAL batch notifications");

        while let Some(batch) = self.batch_rx.recv().await {
            info!(
                "📦 BATCH_RECEIVED: {}-{} offsets={}-{} records={}",
                batch.topic, batch.partition, batch.base_offset,
                batch.last_offset, batch.record_count
            );

            // Serialize batch metadata
            let serialized = match bincode::serialize(&batch) {
                Ok(s) => s,
                Err(e) => {
                    error!("Failed to serialize batch metadata: {}", e);
                    self.notify_batch_failed(&batch, format!("Serialization error: {}", e)).await;
                    continue;
                }
            };

            // Propose to Raft (ONE RwLock acquisition for entire batch!)
            let propose_start = std::time::Instant::now();
            match self.raft_manager
                .propose(&batch.topic, batch.partition, serialized)
                .await
            {
                Ok(raft_index) => {
                    let propose_duration = propose_start.elapsed();
                    info!(
                        "✅ BATCH_PROPOSED: {}-{} offsets={}-{} raft_index={} duration={:?}",
                        batch.topic, batch.partition,
                        batch.base_offset, batch.last_offset, raft_index, propose_duration
                    );
                    self.notify_batch_committed(&batch).await;
                }
                Err(e) => {
                    let propose_duration = propose_start.elapsed();
                    error!(
                        "❌ BATCH_PROPOSAL_FAILED: {}-{} offsets={}-{} error={} duration={:?}",
                        batch.topic, batch.partition,
                        batch.base_offset, batch.last_offset, e, propose_duration
                    );
                    self.notify_batch_failed(&batch, e.to_string()).await;
                }
            }
        }

        warn!("RaftBatchProposer stopped - batch notification channel closed");
    }

    /// Notify all waiting producers that batch was committed
    async fn notify_batch_committed(&self, batch: &BatchMetadata) {
        let mut notified_count = 0;

        // Remove all pending waiters for offsets in this batch
        for offset in batch.base_offset..=batch.last_offset {
            let key = (batch.topic.clone(), batch.partition, offset);
            if let Some((_, waiters)) = self.pending_batches.remove(&key) {
                for waiter in waiters {
                    let _ = waiter.send(Ok(()));
                    notified_count += 1;
                }
            }
        }

        if notified_count > 0 {
            info!(
                "📢 BATCH_NOTIFIED: Notified {} waiters for {}-{} offsets={}-{}",
                notified_count, batch.topic, batch.partition,
                batch.base_offset, batch.last_offset
            );
        }
    }

    /// Notify all waiting producers that batch proposal failed
    async fn notify_batch_failed(&self, batch: &BatchMetadata, error: String) {
        let mut notified_count = 0;

        for offset in batch.base_offset..=batch.last_offset {
            let key = (batch.topic.clone(), batch.partition, offset);
            if let Some((_, waiters)) = self.pending_batches.remove(&key) {
                for waiter in waiters {
                    let _ = waiter.send(Err(error.clone()));
                    notified_count += 1;
                }
            }
        }

        if notified_count > 0 {
            error!(
                "📢 BATCH_FAILED_NOTIFIED: Notified {} waiters of failure for {}-{} offsets={}-{}",
                notified_count, batch.topic, batch.partition,
                batch.base_offset, batch.last_offset
            );
        }
    }

    /// Register a waiter for when an offset is Raft-committed (for acks=-1)
    ///
    /// This is called by ProduceHandler when handling acks=-1 requests.
    /// The waiter will be notified when the batch containing this offset is committed.
    pub fn register_offset_waiter(
        pending_batches: Arc<DashMap<(String, i32, i64), Vec<tokio::sync::oneshot::Sender<Result<(), String>>>>>,
        topic: String,
        partition: i32,
        offset: i64,
    ) -> tokio::sync::oneshot::Receiver<Result<(), String>> {
        let (tx, rx) = tokio::sync::oneshot::channel();

        pending_batches
            .entry((topic, partition, offset))
            .or_insert_with(Vec::new)
            .push(tx);

        rx
    }
}
