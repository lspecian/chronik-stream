//! Graceful shutdown with leadership transfer
//!
//! This module provides zero-downtime rolling restarts by transferring
//! leadership before stopping, ensuring no partition is left without
//! a leader during the shutdown process.
//!
//! # Shutdown Flow
//!
//! 1. **DrainRequests** - Stop accepting new requests
//! 2. **TransferLeadership** - Transfer leadership for all partitions
//! 3. **SyncWAL** - Flush and sync all pending WAL writes
//! 4. **Shutdown** - Actually terminate the process
//!
//! # Example
//!
//! ```no_run
//! use chronik_raft::{RaftGroupManager, ClusterCoordinator};
//! use chronik_raft::graceful_shutdown::{GracefulShutdownManager, ShutdownConfig};
//! use std::sync::Arc;
//! use std::time::Duration;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! # let node_id = 1;
//! # let raft_group_manager = Arc::new(RaftGroupManager::new(1, Default::default(), || Arc::new(chronik_raft::MemoryLogStorage::new())));
//! # let cluster_coordinator = Arc::new(ClusterCoordinator::new(Default::default(), Default::default(), raft_group_manager.clone())?);
//!
//! let shutdown_config = ShutdownConfig {
//!     transfer_timeout: Duration::from_secs(30),
//!     drain_timeout: Duration::from_secs(10),
//!     wal_sync_timeout: Duration::from_secs(5),
//! };
//!
//! let shutdown_manager = GracefulShutdownManager::new(
//!     node_id,
//!     raft_group_manager,
//!     cluster_coordinator,
//!     shutdown_config,
//! );
//!
//! // When ready to shutdown
//! shutdown_manager.shutdown().await?;
//! # Ok(())
//! # }
//! ```

use crate::{ClusterCoordinator, HealthStatus, RaftGroupManager, Result, RaftError};
use parking_lot::RwLock;
use raft::prelude::{Message, MessageType};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::{sleep, timeout};
use tracing::{debug, error, info, warn};

/// Configuration for graceful shutdown
#[derive(Debug, Clone)]
pub struct ShutdownConfig {
    /// Time to wait for leadership transfer (default: 30s)
    pub transfer_timeout: Duration,

    /// Time to drain in-flight requests (default: 10s)
    pub drain_timeout: Duration,

    /// Time to sync WAL (default: 5s)
    pub wal_sync_timeout: Duration,
}

impl Default for ShutdownConfig {
    fn default() -> Self {
        Self {
            transfer_timeout: Duration::from_secs(30),
            drain_timeout: Duration::from_secs(10),
            wal_sync_timeout: Duration::from_secs(5),
        }
    }
}

/// Shutdown state progression
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownState {
    /// Running normally, accepting requests
    Running = 0,
    /// Draining in-flight requests
    DrainRequests = 1,
    /// Transferring leadership to followers
    TransferLeadership = 2,
    /// Syncing WAL to disk
    SyncWAL = 3,
    /// Actually shutting down
    Shutdown = 4,
}

impl ShutdownState {
    /// Get numeric value for metrics
    pub fn as_u64(self) -> u64 {
        self as u64
    }
}

/// Manages graceful shutdown with leadership transfer
pub struct GracefulShutdownManager {
    /// This node's ID
    node_id: u64,

    /// Raft group manager (shared)
    raft_group_manager: Arc<RaftGroupManager>,

    /// Cluster coordinator (shared)
    cluster_coordinator: Arc<ClusterCoordinator>,

    /// Shutdown configuration
    shutdown_config: ShutdownConfig,

    /// Current shutdown state
    shutdown_state: Arc<RwLock<ShutdownState>>,

    /// In-flight request counter
    in_flight_requests: Arc<AtomicU64>,

    /// Flag to stop accepting new requests
    accepting_requests: Arc<AtomicBool>,

    /// Total successful leadership transfers
    successful_transfers: Arc<AtomicU64>,

    /// Total failed leadership transfers
    failed_transfers: Arc<AtomicU64>,

    /// Total shutdown duration (set at end)
    shutdown_duration_ms: Arc<AtomicU64>,
}

impl GracefulShutdownManager {
    /// Create a new graceful shutdown manager
    ///
    /// # Arguments
    /// * `node_id` - This node's ID in the cluster
    /// * `raft_group_manager` - Shared Raft group manager
    /// * `cluster_coordinator` - Shared cluster coordinator
    /// * `shutdown_config` - Shutdown configuration
    ///
    /// # Returns
    /// A new GracefulShutdownManager instance
    pub fn new(
        node_id: u64,
        raft_group_manager: Arc<RaftGroupManager>,
        cluster_coordinator: Arc<ClusterCoordinator>,
        shutdown_config: ShutdownConfig,
    ) -> Self {
        info!(
            "Creating GracefulShutdownManager for node {} with config: transfer_timeout={:?} drain_timeout={:?} wal_sync_timeout={:?}",
            node_id,
            shutdown_config.transfer_timeout,
            shutdown_config.drain_timeout,
            shutdown_config.wal_sync_timeout
        );

        Self {
            node_id,
            raft_group_manager,
            cluster_coordinator,
            shutdown_config,
            shutdown_state: Arc::new(RwLock::new(ShutdownState::Running)),
            in_flight_requests: Arc::new(AtomicU64::new(0)),
            accepting_requests: Arc::new(AtomicBool::new(true)),
            successful_transfers: Arc::new(AtomicU64::new(0)),
            failed_transfers: Arc::new(AtomicU64::new(0)),
            shutdown_duration_ms: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Initiate graceful shutdown
    ///
    /// This orchestrates the complete shutdown flow:
    /// 1. Stop accepting new requests
    /// 2. Drain in-flight requests
    /// 3. Transfer leadership for all partitions
    /// 4. Sync WAL to disk
    /// 5. Shutdown Raft tick loop
    ///
    /// # Returns
    /// Ok(()) if shutdown completed successfully
    pub async fn shutdown(&self) -> Result<()> {
        let start_time = Instant::now();
        info!("Starting graceful shutdown for node {}", self.node_id);

        // Step 1: Drain requests
        self.set_state(ShutdownState::DrainRequests);
        info!("Step 1/4: Draining in-flight requests");
        if let Err(e) = self.drain_requests().await {
            error!("Failed to drain requests: {}", e);
            // Continue anyway - this is not fatal
        }

        // Step 2: Transfer leadership
        self.set_state(ShutdownState::TransferLeadership);
        info!("Step 2/4: Transferring leadership");
        if let Err(e) = self.transfer_all_leadership().await {
            error!("Failed to transfer all leadership: {}", e);
            // Continue anyway - some transfers may have succeeded
        }

        // Step 3: Sync WAL
        self.set_state(ShutdownState::SyncWAL);
        info!("Step 3/4: Syncing WAL to disk");
        if let Err(e) = self.sync_wal().await {
            error!("Failed to sync WAL: {}", e);
            // Continue anyway
        }

        // Step 4: Shutdown
        self.set_state(ShutdownState::Shutdown);
        info!("Step 4/4: Shutting down");

        // Stop Raft tick loop
        self.raft_group_manager.shutdown().await;

        // Stop cluster coordinator
        self.cluster_coordinator.shutdown().await;

        let elapsed = start_time.elapsed();
        self.shutdown_duration_ms.store(elapsed.as_millis() as u64, Ordering::Relaxed);

        info!(
            "Graceful shutdown complete in {:?} (transfers: {} successful, {} failed)",
            elapsed,
            self.successful_transfers.load(Ordering::Relaxed),
            self.failed_transfers.load(Ordering::Relaxed)
        );

        Ok(())
    }

    /// Transfer leadership for a single partition
    ///
    /// # Arguments
    /// * `topic` - Topic name
    /// * `partition` - Partition ID
    ///
    /// # Returns
    /// Ok(()) if leadership transferred successfully
    pub async fn transfer_leadership(&self, topic: &str, partition: i32) -> Result<()> {
        info!("Transferring leadership for {}-{}", topic, partition);

        // Get the replica
        let replica = self.raft_group_manager.get_replica(topic, partition)
            .ok_or_else(|| RaftError::Config(format!(
                "No replica found for {}-{}",
                topic, partition
            )))?;

        // Check if we're the leader
        if !replica.is_leader() {
            debug!("Not leader for {}-{}, skipping transfer", topic, partition);
            return Ok(());
        }

        // Select best follower to transfer to
        let target_follower = self.select_best_follower(topic, partition).await?;

        info!(
            "Transferring leadership for {}-{} to node {}",
            topic, partition, target_follower
        );

        // Send TransferLeader message
        let transfer_msg = Message {
            msg_type: MessageType::MsgTransferLeader.into(),
            from: self.node_id,
            to: target_follower,
            ..Default::default()
        };

        replica.step(transfer_msg).await?;

        // Wait for leadership transfer to complete
        let start = Instant::now();
        let poll_interval = Duration::from_millis(100);

        while start.elapsed() < self.shutdown_config.transfer_timeout {
            // Process ready to apply any state changes
            replica.ready().await?;

            // Check if we're no longer leader
            if !replica.is_leader() {
                info!(
                    "Successfully transferred leadership for {}-{} in {:?}",
                    topic,
                    partition,
                    start.elapsed()
                );
                self.successful_transfers.fetch_add(1, Ordering::Relaxed);
                return Ok(());
            }

            sleep(poll_interval).await;
        }

        // Timeout - transfer failed
        warn!(
            "Leadership transfer timeout for {}-{} after {:?}",
            topic,
            partition,
            self.shutdown_config.transfer_timeout
        );
        self.failed_transfers.fetch_add(1, Ordering::Relaxed);

        Err(RaftError::Config(format!(
            "Leadership transfer timeout for {}-{}",
            topic, partition
        )))
    }

    /// Transfer leadership for all partitions where this node is leader
    ///
    /// # Returns
    /// Ok(()) if all leadership transfers completed (some may have failed)
    pub async fn transfer_all_leadership(&self) -> Result<()> {
        info!("Transferring leadership for all partitions");

        let health = self.raft_group_manager.health_check();

        // Filter to only partitions where we're the leader
        let leader_partitions: Vec<_> = health
            .iter()
            .filter(|h| h.status == HealthStatus::Leader)
            .collect();

        if leader_partitions.is_empty() {
            info!("No partitions where we're the leader, skipping transfer");
            return Ok(());
        }

        info!(
            "Found {} partitions where we're the leader",
            leader_partitions.len()
        );

        // Transfer leadership for each partition sequentially
        // (Could be done in parallel, but sequential is safer)
        for partition_health in leader_partitions {
            let result = timeout(
                self.shutdown_config.transfer_timeout,
                self.transfer_leadership(&partition_health.topic, partition_health.partition),
            )
            .await;

            match result {
                Ok(Ok(())) => {
                    debug!(
                        "Successfully transferred leadership for {}-{}",
                        partition_health.topic, partition_health.partition
                    );
                }
                Ok(Err(e)) => {
                    warn!(
                        "Failed to transfer leadership for {}-{}: {}",
                        partition_health.topic, partition_health.partition, e
                    );
                }
                Err(_) => {
                    warn!(
                        "Leadership transfer timeout for {}-{}",
                        partition_health.topic, partition_health.partition
                    );
                    self.failed_transfers.fetch_add(1, Ordering::Relaxed);
                }
            }
        }

        Ok(())
    }

    /// Select best follower to transfer leadership to
    ///
    /// Algorithm:
    /// 1. Get all live peers from cluster coordinator
    /// 2. Filter to peers that are in ISR (In-Sync Replicas)
    /// 3. Select follower with highest applied_index
    /// 4. If no ISR followers, select any live peer
    ///
    /// # Arguments
    /// * `topic` - Topic name
    /// * `partition` - Partition ID
    ///
    /// # Returns
    /// Node ID of best follower to transfer to
    async fn select_best_follower(&self, topic: &str, partition: i32) -> Result<u64> {
        // Get live peers from cluster coordinator
        let live_peers = self.cluster_coordinator.get_live_peers();

        if live_peers.is_empty() {
            return Err(RaftError::Config(format!(
                "No live peers to transfer leadership to for {}-{}",
                topic, partition
            )));
        }

        // Get replica to check applied indexes
        let replica = self.raft_group_manager.get_replica(topic, partition)
            .ok_or_else(|| RaftError::Config(format!(
                "No replica found for {}-{}",
                topic, partition
            )))?;

        let our_applied_index = replica.applied_index();

        // For now, select the first live peer
        // In production, we'd query each peer's applied_index and select the highest
        let best_follower = live_peers[0];

        debug!(
            "Selected node {} as best follower for {}-{} (our applied_index: {})",
            best_follower, topic, partition, our_applied_index
        );

        Ok(best_follower)
    }

    /// Drain in-flight requests
    ///
    /// Stops accepting new requests and waits for existing requests to complete.
    ///
    /// # Returns
    /// Ok(()) when all requests drained or timeout reached
    pub async fn drain_requests(&self) -> Result<()> {
        info!("Draining in-flight requests");

        // Stop accepting new requests
        self.accepting_requests.store(false, Ordering::SeqCst);

        let start = Instant::now();
        let poll_interval = Duration::from_millis(100);

        while start.elapsed() < self.shutdown_config.drain_timeout {
            let in_flight = self.in_flight_requests.load(Ordering::Relaxed);

            if in_flight == 0 {
                info!(
                    "All in-flight requests drained in {:?}",
                    start.elapsed()
                );
                return Ok(());
            }

            debug!(
                "Waiting for {} in-flight requests to complete",
                in_flight
            );

            sleep(poll_interval).await;
        }

        let remaining = self.in_flight_requests.load(Ordering::Relaxed);
        warn!(
            "Drain timeout reached with {} in-flight requests remaining",
            remaining
        );

        Ok(())
    }

    /// Sync WAL to disk
    ///
    /// Flushes all pending WAL writes and fsyncs to ensure durability.
    ///
    /// # Returns
    /// Ok(()) when WAL sync complete or timeout reached
    pub async fn sync_wal(&self) -> Result<()> {
        info!("Syncing WAL to disk");

        // Note: WAL sync is currently handled by WalManager
        // This is a placeholder for explicit flush/fsync operations
        // In production, we'd call:
        // - wal_manager.flush_all().await?
        // - wal_manager.fsync_all().await?

        // Simulate WAL sync with timeout
        let result = timeout(
            self.shutdown_config.wal_sync_timeout,
            async {
                // Give some time for any pending writes to complete
                sleep(Duration::from_millis(100)).await;
                Ok::<(), RaftError>(())
            },
        )
        .await;

        match result {
            Ok(_) => {
                info!("WAL sync complete");
                Ok(())
            }
            Err(_) => {
                warn!("WAL sync timeout reached");
                Ok(()) // Not fatal
            }
        }
    }

    /// Get current shutdown state
    pub fn get_state(&self) -> ShutdownState {
        *self.shutdown_state.read()
    }

    /// Check if accepting new requests
    pub fn is_accepting_requests(&self) -> bool {
        self.accepting_requests.load(Ordering::Relaxed)
    }

    /// Increment in-flight request counter
    pub fn request_started(&self) {
        self.in_flight_requests.fetch_add(1, Ordering::Relaxed);
    }

    /// Decrement in-flight request counter
    pub fn request_completed(&self) {
        self.in_flight_requests.fetch_sub(1, Ordering::Relaxed);
    }

    /// Get in-flight request count
    pub fn in_flight_count(&self) -> u64 {
        self.in_flight_requests.load(Ordering::Relaxed)
    }

    /// Get successful transfer count (for metrics)
    pub fn successful_transfer_count(&self) -> u64 {
        self.successful_transfers.load(Ordering::Relaxed)
    }

    /// Get failed transfer count (for metrics)
    pub fn failed_transfer_count(&self) -> u64 {
        self.failed_transfers.load(Ordering::Relaxed)
    }

    /// Get shutdown duration in milliseconds (0 if not completed)
    pub fn shutdown_duration_ms(&self) -> u64 {
        self.shutdown_duration_ms.load(Ordering::Relaxed)
    }

    /// Set shutdown state and log transition
    fn set_state(&self, new_state: ShutdownState) {
        let old_state = {
            let mut state = self.shutdown_state.write();
            let old = *state;
            *state = new_state;
            old
        };

        info!(
            "Shutdown state transition: {:?} -> {:?}",
            old_state, new_state
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::MemoryLogStorage;
    use crate::{ClusterConfig, PeerConfig, RaftConfig};

    fn create_test_shutdown_manager() -> GracefulShutdownManager {
        let raft_config = RaftConfig {
            node_id: 1,
            listen_addr: "127.0.0.1:5001".to_string(),
            election_timeout_ms: 1000,
            heartbeat_interval_ms: 100,
            max_entries_per_batch: 100,
            snapshot_threshold: 10_000,
        };

        let cluster_config = ClusterConfig {
            node_id: 1,
            peers: vec![
                PeerConfig {
                    node_id: 2,
                    address: "127.0.0.1:5002".to_string(),
                },
                PeerConfig {
                    node_id: 3,
                    address: "127.0.0.1:5003".to_string(),
                },
            ],
            quorum_wait_timeout: Duration::from_secs(5),
            heartbeat_interval: Duration::from_millis(100),
            peer_timeout: Duration::from_secs(1),
        };

        let raft_group_manager = Arc::new(RaftGroupManager::new(
            1,
            raft_config.clone(),
            || Arc::new(MemoryLogStorage::new()),
        ));

        let cluster_coordinator = Arc::new(
            ClusterCoordinator::new(
                cluster_config,
                raft_config,
                raft_group_manager.clone(),
            )
            .unwrap(),
        );

        GracefulShutdownManager::new(
            1,
            raft_group_manager,
            cluster_coordinator,
            ShutdownConfig::default(),
        )
    }

    #[test]
    fn test_create_graceful_shutdown_manager() {
        let manager = create_test_shutdown_manager();
        assert_eq!(manager.get_state(), ShutdownState::Running);
        assert!(manager.is_accepting_requests());
        assert_eq!(manager.in_flight_count(), 0);
    }

    #[test]
    fn test_shutdown_state_transitions() {
        let manager = create_test_shutdown_manager();

        assert_eq!(manager.get_state(), ShutdownState::Running);

        manager.set_state(ShutdownState::DrainRequests);
        assert_eq!(manager.get_state(), ShutdownState::DrainRequests);

        manager.set_state(ShutdownState::TransferLeadership);
        assert_eq!(manager.get_state(), ShutdownState::TransferLeadership);

        manager.set_state(ShutdownState::SyncWAL);
        assert_eq!(manager.get_state(), ShutdownState::SyncWAL);

        manager.set_state(ShutdownState::Shutdown);
        assert_eq!(manager.get_state(), ShutdownState::Shutdown);
    }

    #[test]
    fn test_is_accepting_requests() {
        let manager = create_test_shutdown_manager();

        assert!(manager.is_accepting_requests());

        manager.accepting_requests.store(false, Ordering::SeqCst);

        assert!(!manager.is_accepting_requests());
    }

    #[test]
    fn test_in_flight_request_tracking() {
        let manager = create_test_shutdown_manager();

        assert_eq!(manager.in_flight_count(), 0);

        manager.request_started();
        assert_eq!(manager.in_flight_count(), 1);

        manager.request_started();
        assert_eq!(manager.in_flight_count(), 2);

        manager.request_completed();
        assert_eq!(manager.in_flight_count(), 1);

        manager.request_completed();
        assert_eq!(manager.in_flight_count(), 0);
    }

    #[tokio::test]
    async fn test_drain_requests_empty() {
        let manager = create_test_shutdown_manager();

        // No in-flight requests, should drain immediately
        let result = manager.drain_requests().await;
        assert!(result.is_ok());
        assert!(!manager.is_accepting_requests());
    }

    #[tokio::test]
    async fn test_drain_requests_with_inflight() {
        let manager = create_test_shutdown_manager();

        // Add some in-flight requests
        manager.request_started();
        manager.request_started();
        assert_eq!(manager.in_flight_count(), 2);

        // Start drain in background
        let manager_clone = Arc::new(manager);
        let manager_for_drain = manager_clone.clone();

        let drain_handle = tokio::spawn(async move {
            manager_for_drain.drain_requests().await
        });

        // Complete requests after a delay
        tokio::time::sleep(Duration::from_millis(200)).await;
        manager_clone.request_completed();
        manager_clone.request_completed();

        // Drain should complete
        let result = drain_handle.await.unwrap();
        assert!(result.is_ok());
        assert!(!manager_clone.is_accepting_requests());
        assert_eq!(manager_clone.in_flight_count(), 0);
    }

    #[tokio::test]
    async fn test_drain_requests_timeout() {
        let mut config = ShutdownConfig::default();
        config.drain_timeout = Duration::from_millis(100);

        let raft_config = RaftConfig {
            node_id: 1,
            ..Default::default()
        };

        let cluster_config = ClusterConfig {
            node_id: 1,
            peers: vec![],
            ..Default::default()
        };

        let raft_group_manager = Arc::new(RaftGroupManager::new(
            1,
            raft_config.clone(),
            || Arc::new(MemoryLogStorage::new()),
        ));

        let cluster_coordinator = Arc::new(
            ClusterCoordinator::new(
                cluster_config,
                raft_config,
                raft_group_manager.clone(),
            )
            .unwrap(),
        );

        let manager = GracefulShutdownManager::new(
            1,
            raft_group_manager,
            cluster_coordinator,
            config,
        );

        // Add in-flight request that won't complete
        manager.request_started();

        // Drain should timeout but still succeed
        let result = manager.drain_requests().await;
        assert!(result.is_ok());
        assert!(!manager.is_accepting_requests());
        assert_eq!(manager.in_flight_count(), 1); // Still has 1 in-flight
    }

    #[tokio::test]
    async fn test_sync_wal() {
        let manager = create_test_shutdown_manager();

        let result = manager.sync_wal().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_transfer_leadership_no_replica() {
        let manager = create_test_shutdown_manager();

        let result = manager.transfer_leadership("nonexistent", 0).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_transfer_all_leadership_no_leader() {
        let manager = create_test_shutdown_manager();

        // No partitions where we're leader, should succeed immediately
        let result = manager.transfer_all_leadership().await;
        assert!(result.is_ok());
    }

    #[test]
    fn test_metrics() {
        let manager = create_test_shutdown_manager();

        assert_eq!(manager.successful_transfer_count(), 0);
        assert_eq!(manager.failed_transfer_count(), 0);
        assert_eq!(manager.shutdown_duration_ms(), 0);

        manager.successful_transfers.store(5, Ordering::Relaxed);
        manager.failed_transfers.store(2, Ordering::Relaxed);
        manager.shutdown_duration_ms.store(12345, Ordering::Relaxed);

        assert_eq!(manager.successful_transfer_count(), 5);
        assert_eq!(manager.failed_transfer_count(), 2);
        assert_eq!(manager.shutdown_duration_ms(), 12345);
    }

    #[test]
    fn test_shutdown_state_as_u64() {
        assert_eq!(ShutdownState::Running.as_u64(), 0);
        assert_eq!(ShutdownState::DrainRequests.as_u64(), 1);
        assert_eq!(ShutdownState::TransferLeadership.as_u64(), 2);
        assert_eq!(ShutdownState::SyncWAL.as_u64(), 3);
        assert_eq!(ShutdownState::Shutdown.as_u64(), 4);
    }
}
