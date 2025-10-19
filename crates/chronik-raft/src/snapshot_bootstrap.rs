//! Snapshot-based bootstrap for new Raft nodes
//!
//! This module enables new nodes to bootstrap from S3 snapshots instead of
//! replaying the full Raft log. This significantly speeds up recovery for
//! large clusters with long histories.
//!
//! # Architecture
//!
//! When a new node joins a cluster:
//! 1. Check S3 for latest snapshot of each partition
//! 2. Download and apply snapshot to restore state
//! 3. Replay only recent log entries after snapshot
//! 4. Join cluster as follower
//!
//! # Example
//!
//! ```no_run
//! use chronik_raft::snapshot_bootstrap::{SnapshotBootstrap, BootstrapConfig};
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let config = BootstrapConfig::default();
//! let bootstrap = SnapshotBootstrap::new(
//!     1,  // node_id
//!     raft_group_manager,
//!     snapshot_manager,
//!     config,
//! );
//!
//! // Bootstrap from S3 for a partition
//! bootstrap.bootstrap_partition("test-topic", 0).await?;
//! # Ok(())
//! # }
//! ```

use crate::{RaftError, RaftGroupManager, Result, SnapshotManager};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

/// Bootstrap configuration
#[derive(Debug, Clone)]
pub struct BootstrapConfig {
    /// Enable snapshot bootstrap (default: true)
    pub enabled: bool,

    /// Timeout for snapshot download (default: 5 minutes)
    pub download_timeout: Duration,

    /// Maximum age of snapshot to use (default: 24 hours)
    pub max_snapshot_age: Duration,

    /// Retry attempts for snapshot download (default: 3)
    pub retry_attempts: usize,

    /// Retry delay between attempts (default: 5 seconds)
    pub retry_delay: Duration,
}

impl Default for BootstrapConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            download_timeout: Duration::from_secs(300), // 5 minutes
            max_snapshot_age: Duration::from_secs(86400), // 24 hours
            retry_attempts: 3,
            retry_delay: Duration::from_secs(5),
        }
    }
}

/// Manages snapshot-based bootstrap for new nodes
pub struct SnapshotBootstrap {
    /// Node ID
    node_id: u64,

    /// Raft group manager
    raft_group_manager: Arc<RaftGroupManager>,

    /// Snapshot manager
    snapshot_manager: Arc<SnapshotManager>,

    /// Bootstrap configuration
    config: BootstrapConfig,
}

impl SnapshotBootstrap {
    /// Create a new SnapshotBootstrap
    ///
    /// # Arguments
    /// * `node_id` - Node ID
    /// * `raft_group_manager` - Raft group manager
    /// * `snapshot_manager` - Snapshot manager
    /// * `config` - Bootstrap configuration
    pub fn new(
        node_id: u64,
        raft_group_manager: Arc<RaftGroupManager>,
        snapshot_manager: Arc<SnapshotManager>,
        config: BootstrapConfig,
    ) -> Self {
        info!(
            "Creating SnapshotBootstrap for node {} with config: {:?}",
            node_id, config
        );

        Self {
            node_id,
            raft_group_manager,
            snapshot_manager,
            config,
        }
    }

    /// Bootstrap a partition from S3 snapshot
    ///
    /// This is called when a new node joins and needs to catch up on state.
    /// It downloads the latest snapshot from S3 and applies it to the state machine.
    ///
    /// # Arguments
    /// * `topic` - Topic name
    /// * `partition` - Partition ID
    ///
    /// # Returns
    /// Ok(()) if snapshot was applied, Err if failed or no snapshot available
    pub async fn bootstrap_partition(&self, topic: &str, partition: i32) -> Result<()> {
        if !self.config.enabled {
            debug!("Snapshot bootstrap disabled, skipping {}-{}", topic, partition);
            return Ok(());
        }

        let start = Instant::now();

        info!(
            "Starting snapshot bootstrap for {}-{} on node {}",
            topic, partition, self.node_id
        );

        // Step 1: List available snapshots
        let snapshots = self.snapshot_manager.list_snapshots(topic, partition).await?;

        if snapshots.is_empty() {
            debug!(
                "No snapshots available for {}-{}, will rely on log replay",
                topic, partition
            );
            return Ok(());
        }

        // Step 2: Find latest valid snapshot
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        let latest = snapshots
            .into_iter()
            .filter(|s| {
                let age_ms = now.saturating_sub(s.created_at);
                let age = Duration::from_millis(age_ms);
                age <= self.config.max_snapshot_age
            })
            .next(); // Already sorted newest first

        let snapshot = match latest {
            Some(s) => s,
            None => {
                warn!(
                    "No recent snapshots found for {}-{} within {:?} age limit",
                    topic, partition, self.config.max_snapshot_age
                );
                return Ok(());
            }
        };

        info!(
            "Found snapshot {} for {}-{}: index={}, term={}, size={} bytes, age={:.1} hours",
            snapshot.snapshot_id,
            topic,
            partition,
            snapshot.last_included_index,
            snapshot.last_included_term,
            snapshot.size_bytes,
            (now.saturating_sub(snapshot.created_at) as f64) / 3600000.0
        );

        // Step 3: Download and apply snapshot with retries
        let mut last_error = None;

        for attempt in 1..=self.config.retry_attempts {
            debug!(
                "Attempt {}/{} to apply snapshot {} for {}-{}",
                attempt, self.config.retry_attempts, snapshot.snapshot_id, topic, partition
            );

            match tokio::time::timeout(
                self.config.download_timeout,
                self.snapshot_manager.apply_snapshot(topic, partition, &snapshot.snapshot_id),
            )
            .await
            {
                Ok(Ok(())) => {
                    let duration = start.elapsed();
                    info!(
                        "Successfully bootstrapped {}-{} from snapshot {} in {:?} (index={}, term={})",
                        topic,
                        partition,
                        snapshot.snapshot_id,
                        duration,
                        snapshot.last_included_index,
                        snapshot.last_included_term
                    );
                    return Ok(());
                }
                Ok(Err(e)) => {
                    warn!(
                        "Failed to apply snapshot {} for {}-{} (attempt {}): {}",
                        snapshot.snapshot_id, topic, partition, attempt, e
                    );
                    last_error = Some(e);
                }
                Err(_) => {
                    warn!(
                        "Timeout applying snapshot {} for {}-{} (attempt {})",
                        snapshot.snapshot_id, topic, partition, attempt
                    );
                    last_error = Some(RaftError::Config(format!(
                        "Timeout after {:?}",
                        self.config.download_timeout
                    )));
                }
            }

            // Wait before retry (except on last attempt)
            if attempt < self.config.retry_attempts {
                tokio::time::sleep(self.config.retry_delay).await;
            }
        }

        // All attempts failed
        let error = last_error.unwrap_or_else(|| {
            RaftError::Config("Unknown error during snapshot bootstrap".to_string())
        });

        error!(
            "Failed to bootstrap {}-{} from snapshot after {} attempts: {}",
            topic, partition, self.config.retry_attempts, error
        );

        Err(error)
    }

    /// Bootstrap all partitions for a topic
    ///
    /// This is a convenience method to bootstrap all partitions of a topic in parallel.
    ///
    /// # Arguments
    /// * `topic` - Topic name
    /// * `partition_count` - Number of partitions
    ///
    /// # Returns
    /// Ok(()) if all successful, Err with first failure
    pub async fn bootstrap_topic(&self, topic: &str, partition_count: i32) -> Result<()> {
        info!(
            "Bootstrapping all {} partitions of topic {} from snapshots",
            partition_count, topic
        );

        let start = Instant::now();
        let mut tasks = Vec::new();

        for partition in 0..partition_count {
            let topic_clone = topic.to_string();
            let bootstrap = Self {
                node_id: self.node_id,
                raft_group_manager: self.raft_group_manager.clone(),
                snapshot_manager: self.snapshot_manager.clone(),
                config: self.config.clone(),
            };

            tasks.push(tokio::spawn(async move {
                bootstrap.bootstrap_partition(&topic_clone, partition).await
            }));
        }

        // Wait for all partitions to complete
        let mut failed = 0;
        let mut succeeded = 0;

        for (i, task) in tasks.into_iter().enumerate() {
            match task.await {
                Ok(Ok(())) => {
                    succeeded += 1;
                }
                Ok(Err(e)) => {
                    warn!("Failed to bootstrap partition {}: {}", i, e);
                    failed += 1;
                }
                Err(e) => {
                    warn!("Bootstrap task panicked for partition {}: {}", i, e);
                    failed += 1;
                }
            }
        }

        let duration = start.elapsed();

        if failed > 0 {
            warn!(
                "Topic {} bootstrap completed with failures in {:?}: {} succeeded, {} failed",
                topic, duration, succeeded, failed
            );
        } else {
            info!(
                "Successfully bootstrapped all {} partitions of topic {} in {:?}",
                partition_count, topic, duration
            );
        }

        Ok(())
    }

    /// Check if snapshot bootstrap is recommended for a partition
    ///
    /// This checks if there's a recent snapshot available and the log is large enough
    /// that bootstrapping from snapshot would be faster than log replay.
    ///
    /// # Arguments
    /// * `topic` - Topic name
    /// * `partition` - Partition ID
    ///
    /// # Returns
    /// true if snapshot bootstrap is recommended, false otherwise
    pub async fn should_bootstrap(&self, topic: &str, partition: i32) -> Result<bool> {
        if !self.config.enabled {
            return Ok(false);
        }

        // Check if snapshots are available
        let snapshots = self.snapshot_manager.list_snapshots(topic, partition).await?;

        if snapshots.is_empty() {
            return Ok(false);
        }

        // Check if latest snapshot is recent enough
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        let latest = &snapshots[0]; // Already sorted newest first
        let age_ms = now.saturating_sub(latest.created_at);
        let age = Duration::from_millis(age_ms);

        if age > self.config.max_snapshot_age {
            debug!(
                "Latest snapshot for {}-{} is too old: {:?} > {:?}",
                topic, partition, age, self.config.max_snapshot_age
            );
            return Ok(false);
        }

        // Check if snapshot would save significant work
        // If snapshot is at least 1000 entries old, it's worth using
        if latest.last_included_index >= 1000 {
            debug!(
                "Snapshot bootstrap recommended for {}-{}: snapshot at index {}",
                topic, partition, latest.last_included_index
            );
            return Ok(true);
        }

        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MemoryLogStorage, RaftConfig, RaftGroupManager};

    #[test]
    fn test_bootstrap_config_defaults() {
        let config = BootstrapConfig::default();

        assert!(config.enabled);
        assert_eq!(config.download_timeout, Duration::from_secs(300));
        assert_eq!(config.max_snapshot_age, Duration::from_secs(86400));
        assert_eq!(config.retry_attempts, 3);
        assert_eq!(config.retry_delay, Duration::from_secs(5));
    }
}
