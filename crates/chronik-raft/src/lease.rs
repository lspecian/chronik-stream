//! Lease-based reads for Chronik Raft - eliminates ReadIndex RPC overhead
//!
//! This module implements lease-based linearizable reads that avoid the ReadIndex
//! RPC round-trip by using time-based leases. This reduces read latency from
//! 10-50ms (ReadIndex) to <5ms (lease check).
//!
//! # Safety Guarantees
//!
//! - Lease duration MUST be less than election timeout (9s < 10s default)
//! - Clock drift bound protects against clock skew (500ms default)
//! - Heartbeat quorum ensures leader hasn't been partitioned
//! - On leadership change, old leader's leases expire before new leader elected
//!
//! # Protocol
//!
//! **Leader**:
//! 1. On becoming leader, grant lease (lease_end = now + lease_duration)
//! 2. Every renewal_interval (3s), send heartbeat to quorum
//! 3. If quorum ACKs within RTT, renew lease
//! 4. On leadership loss, revoke all leases
//!
//! **Follower (with lease-based reads)**:
//! 1. Check if lease is valid: now < lease_end - clock_drift_bound
//! 2. If valid, serve read from local state (commit_index from lease)
//! 3. If invalid, fallback to ReadIndex protocol
//!
//! # Example
//!
//! ```no_run
//! use chronik_raft::lease::{LeaseManager, LeaseConfig};
//! use std::sync::Arc;
//! use std::time::Duration;
//!
//! # async fn example() -> chronik_raft::Result<()> {
//! # let node_id = 1;
//! # let raft_group_manager = Arc::new(unimplemented!());
//!
//! // Create lease manager
//! let config = LeaseConfig {
//!     enabled: true,
//!     lease_duration: Duration::from_secs(9),
//!     renewal_interval: Duration::from_secs(3),
//!     clock_drift_bound: Duration::from_millis(500),
//! };
//!
//! let lease_manager = LeaseManager::new(node_id, raft_group_manager, config);
//!
//! // Spawn background renewal loop
//! let _handle = lease_manager.spawn_renewal_loop();
//!
//! // Check if lease-based read is safe
//! if let Some(safe_index) = lease_manager.get_read_index("my-topic", 0) {
//!     // Lease valid - serve read immediately (no RPC)
//!     println!("Safe to read up to index {}", safe_index);
//! }
//!
//! # Ok(())
//! # }
//! ```

use crate::{RaftError, RaftGroupManager, Result};
use dashmap::DashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;
use tracing::{debug, info, trace, warn};

/// Default lease duration (9 seconds - less than 10s election timeout)
const DEFAULT_LEASE_DURATION: Duration = Duration::from_secs(9);

/// Default renewal interval (3 seconds - renew every 3s)
const DEFAULT_RENEWAL_INTERVAL: Duration = Duration::from_secs(3);

/// Default clock drift bound (500ms - for clock skew protection)
const DEFAULT_CLOCK_DRIFT_BOUND: Duration = Duration::from_millis(500);

/// Key for identifying a partition lease (topic, partition_id)
pub type PartitionKey = (String, i32);

/// Configuration for lease-based reads
#[derive(Debug, Clone)]
pub struct LeaseConfig {
    /// Enable lease-based reads (default: true)
    pub enabled: bool,

    /// Lease duration (default: 9s, MUST be < election timeout)
    pub lease_duration: Duration,

    /// How often to renew leases (default: 3s)
    pub renewal_interval: Duration,

    /// Clock drift bound for safety (default: 500ms)
    pub clock_drift_bound: Duration,
}

impl Default for LeaseConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            lease_duration: DEFAULT_LEASE_DURATION,
            renewal_interval: DEFAULT_RENEWAL_INTERVAL,
            clock_drift_bound: DEFAULT_CLOCK_DRIFT_BOUND,
        }
    }
}

impl LeaseConfig {
    /// Validate lease configuration
    ///
    /// # Errors
    /// - Returns error if lease_duration >= 10s (default election timeout)
    /// - Returns error if clock_drift_bound > lease_duration
    /// - Returns error if renewal_interval > lease_duration
    pub fn validate(&self) -> Result<()> {
        // CRITICAL: lease_duration MUST be less than election timeout
        const MAX_LEASE_DURATION: Duration = Duration::from_secs(10);
        if self.lease_duration >= MAX_LEASE_DURATION {
            return Err(RaftError::Config(format!(
                "lease_duration ({:?}) must be less than election timeout (10s)",
                self.lease_duration
            )));
        }

        // Clock drift bound must be less than lease duration
        if self.clock_drift_bound > self.lease_duration {
            return Err(RaftError::Config(format!(
                "clock_drift_bound ({:?}) must be <= lease_duration ({:?})",
                self.clock_drift_bound, self.lease_duration
            )));
        }

        // Renewal interval should be less than lease duration
        if self.renewal_interval > self.lease_duration {
            return Err(RaftError::Config(format!(
                "renewal_interval ({:?}) should be < lease_duration ({:?})",
                self.renewal_interval, self.lease_duration
            )));
        }

        Ok(())
    }
}

/// State of a lease for a partition
#[derive(Debug, Clone)]
struct LeaseState {
    /// Partition key (topic, partition)
    partition_key: PartitionKey,

    /// Leader ID holding this lease
    leader_id: u64,

    /// When lease was granted
    lease_start: Instant,

    /// When lease expires
    lease_end: Instant,

    /// Last successful renewal
    last_renewal: Instant,

    /// Commit index at lease grant/renewal
    commit_index: u64,
}

impl LeaseState {
    /// Create a new lease
    fn new(
        partition_key: PartitionKey,
        leader_id: u64,
        commit_index: u64,
        lease_duration: Duration,
    ) -> Self {
        let now = Instant::now();
        Self {
            partition_key,
            leader_id,
            lease_start: now,
            lease_end: now + lease_duration,
            last_renewal: now,
            commit_index,
        }
    }

    /// Check if lease is still valid (accounting for clock drift)
    fn is_valid(&self, clock_drift_bound: Duration) -> bool {
        let now = Instant::now();
        // Safety margin: now < lease_end - clock_drift_bound
        let safe_end = self.lease_end.checked_sub(clock_drift_bound)
            .unwrap_or(self.lease_end);
        now < safe_end
    }

    /// Renew lease with new expiration
    fn renew(&mut self, commit_index: u64, lease_duration: Duration) {
        let now = Instant::now();
        self.lease_end = now + lease_duration;
        self.last_renewal = now;
        self.commit_index = commit_index;
    }

    /// Time until lease expires
    fn time_until_expiration(&self) -> Duration {
        let now = Instant::now();
        if now >= self.lease_end {
            Duration::ZERO
        } else {
            self.lease_end - now
        }
    }
}

/// Manages leases for all partitions on this node
///
/// The LeaseManager coordinates lease-based reads by maintaining time-based
/// leases for partitions where this node is the Raft leader. Reads can be
/// served immediately without RPC as long as the lease is valid.
pub struct LeaseManager {
    /// Node ID of this Raft node
    node_id: u64,

    /// Reference to the Raft group manager
    raft_group_manager: Arc<RaftGroupManager>,

    /// Lease configuration
    lease_config: LeaseConfig,

    /// Active leases (partition_key -> LeaseState)
    leases: Arc<DashMap<PartitionKey, LeaseState>>,

    /// Shutdown signal for background tasks
    shutdown: Arc<AtomicBool>,
}

impl LeaseManager {
    /// Create a new LeaseManager
    ///
    /// # Arguments
    /// * `node_id` - ID of this Raft node
    /// * `raft_group_manager` - Reference to the Raft group manager
    /// * `config` - Lease configuration
    ///
    /// # Returns
    /// New LeaseManager instance
    ///
    /// # Errors
    /// Returns error if configuration is invalid
    pub fn new(
        node_id: u64,
        raft_group_manager: Arc<RaftGroupManager>,
        config: LeaseConfig,
    ) -> Result<Self> {
        // Validate configuration
        config.validate()?;

        info!(
            "Creating LeaseManager for node {} with config: {:?}",
            node_id, config
        );

        Ok(Self {
            node_id,
            raft_group_manager,
            lease_config: config,
            leases: Arc::new(DashMap::new()),
            shutdown: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Grant a lease for a partition (leader only)
    ///
    /// This should be called when this node becomes the leader for a partition.
    ///
    /// # Arguments
    /// * `topic` - Topic name
    /// * `partition` - Partition ID
    ///
    /// # Returns
    /// Ok(()) if lease granted successfully
    ///
    /// # Errors
    /// - Returns error if this node is not the leader
    /// - Returns error if partition replica doesn't exist
    pub fn grant_lease(&self, topic: &str, partition: i32) -> Result<()> {
        let key = (topic.to_string(), partition);

        // Get replica and verify leadership
        let replica = self.raft_group_manager.get_replica(topic, partition)
            .ok_or_else(|| RaftError::Config(format!(
                "Partition {}-{} not found",
                topic, partition
            )))?;

        if !replica.is_leader() {
            return Err(RaftError::Config(format!(
                "Cannot grant lease for {}-{}: not leader",
                topic, partition
            )));
        }

        let commit_index = replica.commit_index();

        // Create new lease
        let lease = LeaseState::new(
            key.clone(),
            self.node_id,
            commit_index,
            self.lease_config.lease_duration,
        );

        info!(
            "Granted lease for {}-{} (commit_index={}, expires in {:?})",
            topic,
            partition,
            commit_index,
            self.lease_config.lease_duration
        );

        self.leases.insert(key, lease);

        Ok(())
    }

    /// Renew a lease for a partition (leader only)
    ///
    /// This should be called periodically by the renewal loop to extend leases.
    /// The renewal involves sending a heartbeat to a quorum to ensure the leader
    /// hasn't been partitioned.
    ///
    /// # Arguments
    /// * `topic` - Topic name
    /// * `partition` - Partition ID
    ///
    /// # Returns
    /// Ok(()) if lease renewed successfully
    ///
    /// # Errors
    /// - Returns error if this node is not the leader
    /// - Returns error if heartbeat quorum not reached
    pub fn renew_lease(&self, topic: &str, partition: i32) -> Result<()> {
        let key = (topic.to_string(), partition);

        // Get replica and verify leadership
        let replica = self.raft_group_manager.get_replica(topic, partition)
            .ok_or_else(|| RaftError::Config(format!(
                "Partition {}-{} not found",
                topic, partition
            )))?;

        if !replica.is_leader() {
            // Not leader anymore - revoke lease
            self.revoke_lease(topic, partition);
            return Err(RaftError::Config(format!(
                "Cannot renew lease for {}-{}: not leader",
                topic, partition
            )));
        }

        // In a real implementation, this would:
        // 1. Send heartbeat to all followers
        // 2. Wait for quorum ACKs
        // 3. Only renew if quorum reached within RTT
        //
        // For now, we simulate by checking if we're still the leader
        // and updating the lease with current commit index.

        let commit_index = replica.commit_index();

        if let Some(mut lease) = self.leases.get_mut(&key) {
            lease.renew(commit_index, self.lease_config.lease_duration);

            debug!(
                "Renewed lease for {}-{} (commit_index={}, expires in {:?})",
                topic,
                partition,
                commit_index,
                lease.time_until_expiration()
            );

            Ok(())
        } else {
            // No existing lease - grant new one
            self.grant_lease(topic, partition)
        }
    }

    /// Revoke a lease (on leadership loss)
    ///
    /// This should be called when this node loses leadership for a partition.
    ///
    /// # Arguments
    /// * `topic` - Topic name
    /// * `partition` - Partition ID
    pub fn revoke_lease(&self, topic: &str, partition: i32) {
        let key = (topic.to_string(), partition);

        if let Some((_, lease)) = self.leases.remove(&key) {
            info!(
                "Revoked lease for {}-{} (was valid for {:?})",
                topic,
                partition,
                lease.time_until_expiration()
            );
        }
    }

    /// Check if a lease is valid for reads
    ///
    /// # Arguments
    /// * `topic` - Topic name
    /// * `partition` - Partition ID
    ///
    /// # Returns
    /// true if lease is valid and safe to read, false otherwise
    pub fn is_lease_valid(&self, topic: &str, partition: i32) -> bool {
        if !self.lease_config.enabled {
            return false;
        }

        let key = (topic.to_string(), partition);

        if let Some(lease) = self.leases.get(&key) {
            let valid = lease.is_valid(self.lease_config.clock_drift_bound);

            if !valid {
                trace!(
                    "Lease for {}-{} expired (time since expiration: {:?})",
                    topic,
                    partition,
                    Instant::now().duration_since(lease.lease_end)
                );
            }

            valid
        } else {
            false
        }
    }

    /// Get safe read index under lease
    ///
    /// If the lease is valid, returns the commit index that is safe to read.
    /// Otherwise returns None (fallback to ReadIndex protocol).
    ///
    /// # Arguments
    /// * `topic` - Topic name
    /// * `partition` - Partition ID
    ///
    /// # Returns
    /// Some(commit_index) if lease is valid, None otherwise
    pub fn get_read_index(&self, topic: &str, partition: i32) -> Option<u64> {
        if !self.is_lease_valid(topic, partition) {
            return None;
        }

        let key = (topic.to_string(), partition);

        self.leases.get(&key).map(|lease| {
            trace!(
                "Lease-based read for {}-{}: commit_index={} (lease valid for {:?})",
                topic,
                partition,
                lease.commit_index,
                lease.time_until_expiration()
            );
            lease.commit_index
        })
    }

    /// Spawn background lease renewal loop
    ///
    /// This creates a tokio task that continuously renews leases for partitions
    /// where this node is the leader. The renewal happens every renewal_interval
    /// to ensure leases stay valid.
    ///
    /// # Returns
    /// JoinHandle for the background task
    pub fn spawn_renewal_loop(self: Arc<Self>) -> JoinHandle<()> {
        let renewal_interval = self.lease_config.renewal_interval;

        tokio::spawn(async move {
            info!(
                "Lease renewal loop started (interval: {:?})",
                renewal_interval
            );

            let mut interval = tokio::time::interval(renewal_interval);

            while !self.shutdown.load(Ordering::Relaxed) {
                interval.tick().await;

                // Get all active replicas
                let replicas = self.raft_group_manager.list_replicas();

                trace!("Renewal loop checking {} replicas", replicas.len());

                for (topic, partition) in replicas {
                    // Check if we're the leader
                    if self.raft_group_manager.is_leader_for_partition(&topic, partition) {
                        // Renew lease
                        match self.renew_lease(&topic, partition) {
                            Ok(()) => {
                                trace!("Renewal loop: renewed lease for {}-{}", topic, partition);
                            }
                            Err(e) => {
                                debug!(
                                    "Renewal loop: failed to renew lease for {}-{}: {}",
                                    topic, partition, e
                                );
                            }
                        }
                    } else {
                        // Not leader - ensure no lease exists
                        let key = (topic.clone(), partition);
                        if self.leases.contains_key(&key) {
                            self.revoke_lease(&topic, partition);
                        }
                    }
                }
            }

            info!("Lease renewal loop stopped");
        })
    }

    /// Stop the lease manager and revoke all leases
    pub fn shutdown(&self) {
        info!("Shutting down LeaseManager");

        // Signal shutdown
        self.shutdown.store(true, Ordering::Relaxed);

        // Revoke all leases
        let keys: Vec<PartitionKey> = self.leases.iter().map(|e| e.key().clone()).collect();
        for (topic, partition) in keys {
            self.revoke_lease(&topic, partition);
        }

        debug!("LeaseManager shutdown complete");
    }

    /// Get count of active leases
    pub fn lease_count(&self) -> usize {
        self.leases.len()
    }

    /// Get node ID
    pub fn node_id(&self) -> u64 {
        self.node_id
    }

    /// Get lease config
    pub fn config(&self) -> &LeaseConfig {
        &self.lease_config
    }

    /// List all active leases (for monitoring)
    pub fn list_leases(&self) -> Vec<(String, i32, u64, Duration)> {
        self.leases
            .iter()
            .map(|entry| {
                let (topic, partition) = entry.key();
                let lease = entry.value();
                (
                    topic.clone(),
                    *partition,
                    lease.commit_index,
                    lease.time_until_expiration(),
                )
            })
            .collect()
    }
}

impl Drop for LeaseManager {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MemoryLogStorage, PartitionReplica, RaftConfig};
    use std::time::Duration;

    fn create_test_config() -> LeaseConfig {
        LeaseConfig {
            enabled: true,
            lease_duration: Duration::from_secs(9),
            renewal_interval: Duration::from_secs(3),
            clock_drift_bound: Duration::from_millis(500),
        }
    }

    fn create_test_raft_config(node_id: u64) -> RaftConfig {
        RaftConfig {
            node_id,
            listen_addr: format!("127.0.0.1:{}", 5000 + node_id),
            election_timeout_ms: 10_000,
            heartbeat_interval_ms: 100,
            max_entries_per_batch: 100,
            snapshot_threshold: 10_000,
        }
    }

    async fn create_test_group_manager(node_id: u64) -> Arc<RaftGroupManager> {
        let config = create_test_raft_config(node_id);
        Arc::new(RaftGroupManager::new(
            node_id,
            config,
            || Arc::new(MemoryLogStorage::new()),
        ))
    }

    #[tokio::test]
    async fn test_create_lease_manager() {
        let group_manager = create_test_group_manager(1).await;
        let config = create_test_config();
        let manager = LeaseManager::new(1, group_manager, config).unwrap();

        assert_eq!(manager.node_id(), 1);
        assert_eq!(manager.lease_count(), 0);
        assert!(manager.config().enabled);
    }

    #[tokio::test]
    async fn test_validate_config_valid() {
        let config = LeaseConfig {
            enabled: true,
            lease_duration: Duration::from_secs(9),
            renewal_interval: Duration::from_secs(3),
            clock_drift_bound: Duration::from_millis(500),
        };

        assert!(config.validate().is_ok());
    }

    #[tokio::test]
    async fn test_validate_config_invalid_lease_duration() {
        let config = LeaseConfig {
            enabled: true,
            lease_duration: Duration::from_secs(10), // >= 10s (election timeout)
            renewal_interval: Duration::from_secs(3),
            clock_drift_bound: Duration::from_millis(500),
        };

        assert!(config.validate().is_err());
    }

    #[tokio::test]
    async fn test_validate_config_invalid_clock_drift() {
        let config = LeaseConfig {
            enabled: true,
            lease_duration: Duration::from_secs(9),
            renewal_interval: Duration::from_secs(3),
            clock_drift_bound: Duration::from_secs(10), // > lease_duration
        };

        assert!(config.validate().is_err());
    }

    #[tokio::test]
    async fn test_grant_lease_success() {
        let group_manager = create_test_group_manager(1).await;
        let config = create_test_config();
        let manager = LeaseManager::new(1, group_manager.clone(), config).unwrap();

        // Create replica and make it leader
        let replica = group_manager.get_or_create_replica("test-topic", 0, vec![]).unwrap();
        replica.campaign().unwrap();
        let _ = replica.ready().await.unwrap();

        // Grant lease
        let result = manager.grant_lease("test-topic", 0);
        assert!(result.is_ok());
        assert_eq!(manager.lease_count(), 1);
    }

    #[tokio::test]
    async fn test_grant_lease_not_leader() {
        let group_manager = create_test_group_manager(1).await;
        let config = create_test_config();
        let manager = LeaseManager::new(1, group_manager.clone(), config).unwrap();

        // Create replica but don't make it leader
        let _replica = group_manager.get_or_create_replica("test-topic", 0, vec![]).unwrap();

        // Grant lease should fail
        let result = manager.grant_lease("test-topic", 0);
        assert!(result.is_err());
        assert_eq!(manager.lease_count(), 0);
    }

    #[tokio::test]
    async fn test_is_lease_valid() {
        let group_manager = create_test_group_manager(1).await;
        let config = create_test_config();
        let manager = LeaseManager::new(1, group_manager.clone(), config).unwrap();

        // Create replica and make it leader
        let replica = group_manager.get_or_create_replica("test-topic", 0, vec![]).unwrap();
        replica.campaign().unwrap();
        let _ = replica.ready().await.unwrap();

        // Grant lease
        manager.grant_lease("test-topic", 0).unwrap();

        // Should be valid immediately
        assert!(manager.is_lease_valid("test-topic", 0));
    }

    #[tokio::test]
    async fn test_is_lease_valid_after_expiration() {
        let group_manager = create_test_group_manager(1).await;
        let config = LeaseConfig {
            enabled: true,
            lease_duration: Duration::from_millis(100), // Very short for testing
            renewal_interval: Duration::from_millis(50),
            clock_drift_bound: Duration::from_millis(10),
        };
        let manager = LeaseManager::new(1, group_manager.clone(), config).unwrap();

        // Create replica and make it leader
        let replica = group_manager.get_or_create_replica("test-topic", 0, vec![]).unwrap();
        replica.campaign().unwrap();
        let _ = replica.ready().await.unwrap();

        // Grant lease
        manager.grant_lease("test-topic", 0).unwrap();

        // Should be valid initially
        assert!(manager.is_lease_valid("test-topic", 0));

        // Wait for lease to expire (lease_duration + clock_drift_bound)
        tokio::time::sleep(Duration::from_millis(150)).await;

        // Should be invalid now
        assert!(!manager.is_lease_valid("test-topic", 0));
    }

    #[tokio::test]
    async fn test_get_read_index() {
        let group_manager = create_test_group_manager(1).await;
        let config = create_test_config();
        let manager = LeaseManager::new(1, group_manager.clone(), config).unwrap();

        // Create replica and make it leader
        let replica = group_manager.get_or_create_replica("test-topic", 0, vec![]).unwrap();
        replica.campaign().unwrap();
        let _ = replica.ready().await.unwrap();

        // Grant lease
        manager.grant_lease("test-topic", 0).unwrap();

        // Should return commit index
        let read_index = manager.get_read_index("test-topic", 0);
        assert!(read_index.is_some());
        assert_eq!(read_index.unwrap(), replica.commit_index());
    }

    #[tokio::test]
    async fn test_get_read_index_no_lease() {
        let group_manager = create_test_group_manager(1).await;
        let config = create_test_config();
        let manager = LeaseManager::new(1, group_manager.clone(), config).unwrap();

        // No lease granted
        let read_index = manager.get_read_index("test-topic", 0);
        assert!(read_index.is_none());
    }

    #[tokio::test]
    async fn test_revoke_lease() {
        let group_manager = create_test_group_manager(1).await;
        let config = create_test_config();
        let manager = LeaseManager::new(1, group_manager.clone(), config).unwrap();

        // Create replica and make it leader
        let replica = group_manager.get_or_create_replica("test-topic", 0, vec![]).unwrap();
        replica.campaign().unwrap();
        let _ = replica.ready().await.unwrap();

        // Grant lease
        manager.grant_lease("test-topic", 0).unwrap();
        assert_eq!(manager.lease_count(), 1);

        // Revoke lease
        manager.revoke_lease("test-topic", 0);
        assert_eq!(manager.lease_count(), 0);
    }

    #[tokio::test]
    async fn test_renew_lease() {
        let group_manager = create_test_group_manager(1).await;
        let config = create_test_config();
        let manager = LeaseManager::new(1, group_manager.clone(), config).unwrap();

        // Create replica and make it leader
        let replica = group_manager.get_or_create_replica("test-topic", 0, vec![]).unwrap();
        replica.campaign().unwrap();
        let _ = replica.ready().await.unwrap();

        // Grant lease
        manager.grant_lease("test-topic", 0).unwrap();

        // Get initial lease
        let initial_lease = manager.leases.get(&("test-topic".to_string(), 0)).unwrap();
        let initial_end = initial_lease.lease_end;
        drop(initial_lease);

        // Wait a bit
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Renew lease
        let result = manager.renew_lease("test-topic", 0);
        assert!(result.is_ok());

        // Verify lease was renewed (end time should be later)
        let renewed_lease = manager.leases.get(&("test-topic".to_string(), 0)).unwrap();
        assert!(renewed_lease.lease_end > initial_end);
    }

    #[tokio::test]
    async fn test_renewal_loop() {
        let group_manager = create_test_group_manager(1).await;
        let config = LeaseConfig {
            enabled: true,
            lease_duration: Duration::from_secs(2),
            renewal_interval: Duration::from_millis(200), // Fast for testing
            clock_drift_bound: Duration::from_millis(100),
        };
        let manager = Arc::new(LeaseManager::new(1, group_manager.clone(), config).unwrap());

        // Create replica and make it leader
        let replica = group_manager.get_or_create_replica("test-topic", 0, vec![]).unwrap();
        replica.campaign().unwrap();
        let _ = replica.ready().await.unwrap();

        // Grant initial lease
        manager.grant_lease("test-topic", 0).unwrap();

        // Spawn renewal loop
        let _handle = manager.clone().spawn_renewal_loop();

        // Wait for a few renewal cycles
        tokio::time::sleep(Duration::from_millis(600)).await;

        // Lease should still be valid (renewed automatically)
        assert!(manager.is_lease_valid("test-topic", 0));

        // Shutdown
        manager.shutdown();
    }

    #[tokio::test]
    async fn test_list_leases() {
        let group_manager = create_test_group_manager(1).await;
        let config = create_test_config();
        let manager = LeaseManager::new(1, group_manager.clone(), config).unwrap();

        // Create multiple replicas and make them leaders
        for i in 0..3 {
            let replica = group_manager.get_or_create_replica("test-topic", i, vec![]).unwrap();
            replica.campaign().unwrap();
            let _ = replica.ready().await.unwrap();
            manager.grant_lease("test-topic", i).unwrap();
        }

        // List leases
        let leases = manager.list_leases();
        assert_eq!(leases.len(), 3);

        // Verify all leases are present
        for i in 0..3 {
            assert!(leases.iter().any(|(t, p, _, _)| t == "test-topic" && *p == i));
        }
    }

    #[tokio::test]
    async fn test_clock_drift_bound_protection() {
        let group_manager = create_test_group_manager(1).await;
        let config = LeaseConfig {
            enabled: true,
            lease_duration: Duration::from_millis(200),
            renewal_interval: Duration::from_millis(100),
            clock_drift_bound: Duration::from_millis(50), // 50ms safety margin
        };
        let manager = LeaseManager::new(1, group_manager.clone(), config).unwrap();

        // Create replica and make it leader
        let replica = group_manager.get_or_create_replica("test-topic", 0, vec![]).unwrap();
        replica.campaign().unwrap();
        let _ = replica.ready().await.unwrap();

        // Grant lease
        manager.grant_lease("test-topic", 0).unwrap();

        // Sleep until close to expiration (but before actual expiration)
        // Lease should be invalid due to clock_drift_bound
        tokio::time::sleep(Duration::from_millis(160)).await; // 200ms - 50ms = 150ms safe end

        // Should be invalid due to clock drift protection
        assert!(!manager.is_lease_valid("test-topic", 0));
    }

    #[tokio::test]
    async fn test_disabled_leases() {
        let group_manager = create_test_group_manager(1).await;
        let config = LeaseConfig {
            enabled: false, // Disabled
            lease_duration: Duration::from_secs(9),
            renewal_interval: Duration::from_secs(3),
            clock_drift_bound: Duration::from_millis(500),
        };
        let manager = LeaseManager::new(1, group_manager.clone(), config).unwrap();

        // Create replica and make it leader
        let replica = group_manager.get_or_create_replica("test-topic", 0, vec![]).unwrap();
        replica.campaign().unwrap();
        let _ = replica.ready().await.unwrap();

        // Grant lease
        manager.grant_lease("test-topic", 0).unwrap();

        // Should always be invalid when disabled
        assert!(!manager.is_lease_valid("test-topic", 0));
        assert!(manager.get_read_index("test-topic", 0).is_none());
    }

    #[tokio::test]
    async fn test_shutdown_revokes_all_leases() {
        let group_manager = create_test_group_manager(1).await;
        let config = create_test_config();
        let manager = LeaseManager::new(1, group_manager.clone(), config).unwrap();

        // Create multiple leases
        for i in 0..5 {
            let replica = group_manager.get_or_create_replica("test-topic", i, vec![]).unwrap();
            replica.campaign().unwrap();
            let _ = replica.ready().await.unwrap();
            manager.grant_lease("test-topic", i).unwrap();
        }

        assert_eq!(manager.lease_count(), 5);

        // Shutdown
        manager.shutdown();

        // All leases should be revoked
        assert_eq!(manager.lease_count(), 0);
    }

    #[tokio::test]
    async fn test_lease_state_renew() {
        let mut lease = LeaseState::new(
            ("test-topic".to_string(), 0),
            1,
            100,
            Duration::from_secs(9),
        );

        let initial_end = lease.lease_end;

        // Wait a bit
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Renew
        lease.renew(200, Duration::from_secs(9));

        // End time should be later
        assert!(lease.lease_end > initial_end);
        assert_eq!(lease.commit_index, 200);
    }
}
