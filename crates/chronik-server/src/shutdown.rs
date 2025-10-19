//! Graceful shutdown coordination for Chronik Server
//!
//! This module provides signal handling and coordinated shutdown across
//! all server components, including Raft leadership transfer for zero-downtime
//! rolling restarts.

use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;
use tokio::signal;
use tracing::{info, warn};

#[cfg(feature = "raft")]
use crate::raft_integration::RaftReplicaManager;

/// Shutdown coordinator for the Chronik server
///
/// This coordinates graceful shutdown across all components:
/// 1. Accept shutdown signal (SIGTERM/SIGINT)
/// 2. Stop accepting new requests
/// 3. Drain in-flight requests
/// 4. Transfer Raft leadership (if Raft enabled)
/// 5. Flush WAL
/// 6. Shutdown components
pub struct ShutdownCoordinator {
    /// Shutdown signal channel (receivers can await shutdown)
    shutdown_tx: tokio::sync::broadcast::Sender<()>,

    /// Raft replica manager (if Raft is enabled)
    #[cfg(feature = "raft")]
    raft_manager: Option<Arc<RaftReplicaManager>>,

    /// WAL manager for flushing
    wal_manager: Option<Arc<chronik_wal::WalManager>>,

    /// Shutdown timeout
    timeout: Duration,

    /// In-flight request counter
    in_flight: Arc<std::sync::atomic::AtomicU64>,

    /// Flag to stop accepting requests
    accepting_requests: Arc<std::sync::atomic::AtomicBool>,
}

impl ShutdownCoordinator {
    /// Create a new shutdown coordinator
    pub fn new() -> Self {
        let (shutdown_tx, _) = tokio::sync::broadcast::channel(1);

        Self {
            shutdown_tx,
            #[cfg(feature = "raft")]
            raft_manager: None,
            wal_manager: None,
            timeout: Duration::from_secs(30),
            in_flight: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            accepting_requests: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        }
    }

    /// Set Raft manager for leadership transfer
    #[cfg(feature = "raft")]
    pub fn set_raft_manager(&mut self, raft_manager: Arc<RaftReplicaManager>) {
        self.raft_manager = Some(raft_manager);
    }

    /// Set WAL manager for flushing
    pub fn set_wal_manager(&mut self, wal_manager: Arc<chronik_wal::WalManager>) {
        self.wal_manager = Some(wal_manager);
    }

    /// Set shutdown timeout
    pub fn set_timeout(&mut self, timeout: Duration) {
        self.timeout = timeout;
    }

    /// Get a shutdown receiver
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<()> {
        self.shutdown_tx.subscribe()
    }

    /// Check if accepting new requests
    pub fn is_accepting_requests(&self) -> bool {
        self.accepting_requests.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Increment in-flight request counter
    pub fn request_started(&self) {
        self.in_flight.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Decrement in-flight request counter
    pub fn request_completed(&self) {
        self.in_flight.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Wait for shutdown signal and coordinate graceful shutdown
    pub async fn wait_for_signal_and_shutdown(self: Arc<Self>) -> Result<()> {
        // Wait for SIGTERM or SIGINT
        #[cfg(unix)]
        {
            let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate())?;
            let mut sigint = signal::unix::signal(signal::unix::SignalKind::interrupt())?;

            tokio::select! {
                _ = sigterm.recv() => {
                    info!("Received SIGTERM, initiating graceful shutdown");
                }
                _ = sigint.recv() => {
                    info!("Received SIGINT (Ctrl+C), initiating graceful shutdown");
                }
            }
        }

        #[cfg(not(unix))]
        {
            signal::ctrl_c().await?;
            info!("Received Ctrl+C, initiating graceful shutdown");
        }

        // Execute shutdown
        self.shutdown().await
    }

    /// Execute graceful shutdown
    ///
    /// This orchestrates the complete shutdown flow:
    /// 1. Stop accepting new requests
    /// 2. Drain in-flight requests
    /// 3. Transfer Raft leadership (if enabled)
    /// 4. Flush WAL
    /// 5. Signal shutdown to all components
    pub async fn shutdown(&self) -> Result<()> {
        let start = std::time::Instant::now();
        info!("Starting graceful shutdown sequence");

        // Step 1: Stop accepting new requests
        info!("Step 1/4: Stopping new request acceptance");
        self.accepting_requests.store(false, std::sync::atomic::Ordering::SeqCst);

        // Step 2: Drain in-flight requests
        info!("Step 2/4: Draining in-flight requests");
        let drain_timeout = Duration::from_secs(10);
        let drain_start = std::time::Instant::now();

        while drain_start.elapsed() < drain_timeout {
            let in_flight = self.in_flight.load(std::sync::atomic::Ordering::Relaxed);
            if in_flight == 0 {
                info!("All in-flight requests drained in {:?}", drain_start.elapsed());
                break;
            }

            info!("Waiting for {} in-flight requests to complete", in_flight);
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        let remaining = self.in_flight.load(std::sync::atomic::Ordering::Relaxed);
        if remaining > 0 {
            warn!("Drain timeout reached with {} in-flight requests remaining", remaining);
        }

        // Step 3: Transfer Raft leadership (if Raft enabled)
        #[cfg(feature = "raft")]
        {
            if let Some(ref raft_manager) = self.raft_manager {
                if raft_manager.config.enabled {
                    info!("Step 3/4: Transferring Raft leadership");
                    if let Err(e) = self.transfer_raft_leadership(raft_manager).await {
                        warn!("Failed to transfer all leadership: {}", e);
                        // Continue anyway - some transfers may have succeeded
                    }
                } else {
                    info!("Step 3/4: Skipping Raft leadership transfer (Raft not enabled)");
                }
            } else {
                info!("Step 3/4: Skipping Raft leadership transfer (no Raft manager)");
            }
        }

        #[cfg(not(feature = "raft"))]
        {
            info!("Step 3/4: Skipping Raft leadership transfer (Raft feature not compiled)");
        }

        // Step 4: Flush WAL
        info!("Step 4/4: Flushing WAL to disk");
        if let Some(ref wal_manager) = self.wal_manager {
            if let Err(e) = wal_manager.flush_all().await {
                warn!("Failed to flush WAL: {}", e);
                // Continue anyway
            } else {
                info!("WAL flush complete");
            }
        }

        // Broadcast shutdown signal to all components
        let _ = self.shutdown_tx.send(());

        info!("Graceful shutdown complete in {:?}", start.elapsed());
        Ok(())
    }

    /// Transfer Raft leadership for all partitions where this node is leader
    #[cfg(feature = "raft")]
    async fn transfer_raft_leadership(&self, raft_manager: &RaftReplicaManager) -> Result<()> {
        use chronik_raft::RaftError;

        info!("Transferring Raft leadership for all partitions");

        // Get all partitions where we're leader
        let leader_partitions: Vec<_> = raft_manager
            .list_partitions()
            .into_iter()
            .filter(|(topic, partition)| raft_manager.is_leader(topic, *partition))
            .collect();

        if leader_partitions.is_empty() {
            info!("No partitions where we're the leader, skipping transfer");
            return Ok(());
        }

        info!("Found {} partitions where we're the leader", leader_partitions.len());

        let mut successful = 0;
        let mut failed = 0;

        // Transfer leadership for each partition
        for (topic, partition) in leader_partitions {
            match self.transfer_partition_leadership(raft_manager, &topic, partition).await {
                Ok(()) => {
                    info!("Successfully transferred leadership for {}-{}", topic, partition);
                    successful += 1;
                }
                Err(e) => {
                    warn!("Failed to transfer leadership for {}-{}: {}", topic, partition, e);
                    failed += 1;
                }
            }
        }

        info!(
            "Leadership transfer complete: {} successful, {} failed",
            successful, failed
        );

        Ok(())
    }

    /// Transfer leadership for a single partition
    #[cfg(feature = "raft")]
    async fn transfer_partition_leadership(
        &self,
        raft_manager: &RaftReplicaManager,
        topic: &str,
        partition: i32,
    ) -> Result<(), chronik_raft::RaftError> {
        use chronik_raft::Message; use chronik_raft::MessageType;
        use chronik_raft::RaftError;

        info!("Transferring leadership for {}-{}", topic, partition);

        // Get the replica
        let replica = raft_manager
            .get_replica(topic, partition)
            .ok_or_else(|| RaftError::Config(format!("No replica for {}-{}", topic, partition)))?;

        // Check if we're the leader
        if !replica.is_leader() {
            info!("Not leader for {}-{}, skipping transfer", topic, partition);
            return Ok(());
        }

        // Select best follower (for now, use leader_id logic to find a peer)
        // In production, query each peer's applied_index and select the highest
        let target_follower = self.select_best_follower(raft_manager, topic, partition).await?;

        info!(
            "Transferring leadership for {}-{} to node {}",
            topic, partition, target_follower
        );

        // Send TransferLeader message to self (Raft will handle the transfer)
        let transfer_msg = Message {
            msg_type: MessageType::MsgTransferLeader.into(),
            from: raft_manager.config.raft_config.node_id,
            to: target_follower,
            ..Default::default()
        };

        replica.step(transfer_msg).await?;

        // Wait for leadership transfer to complete
        let transfer_timeout = Duration::from_secs(5);
        let start = std::time::Instant::now();

        while start.elapsed() < transfer_timeout {
            // Process ready to apply state changes
            replica.ready().await?;

            // Check if we're no longer leader
            if !replica.is_leader() {
                info!(
                    "Successfully transferred leadership for {}-{} in {:?}",
                    topic,
                    partition,
                    start.elapsed()
                );
                return Ok(());
            }

            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        // Timeout - transfer failed
        warn!(
            "Leadership transfer timeout for {}-{} after {:?}",
            topic, partition, transfer_timeout
        );

        Err(RaftError::Config(format!(
            "Leadership transfer timeout for {}-{}",
            topic, partition
        )))
    }

    /// Select best follower to transfer leadership to
    #[cfg(feature = "raft")]
    async fn select_best_follower(
        &self,
        raft_manager: &RaftReplicaManager,
        _topic: &str,
        _partition: i32,
    ) -> Result<u64, chronik_raft::RaftError> {
        // Get live peers from Raft manager
        let peers = raft_manager.get_peers().await;

        if peers.is_empty() {
            return Err(chronik_raft::RaftError::Config(
                "No peers available for leadership transfer".to_string(),
            ));
        }

        // For now, select the first peer
        // In production, query each peer's applied_index and select the highest
        let best_follower = peers[0];

        info!("Selected node {} as best follower for leadership transfer", best_follower);
        Ok(best_follower)
    }
}

impl Default for ShutdownCoordinator {
    fn default() -> Self {
        Self::new()
    }
}
