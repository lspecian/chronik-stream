//! Leader Election per Partition (v2.5.0 Phase 5)
//!
//! This module implements automatic partition leader election when the current leader fails.
//!
//! Algorithm:
//! 1. Detect leader failure (heartbeat timeout)
//! 2. Check ISR (in-sync replicas) from RaftCluster metadata
//! 3. Elect first ISR member as new leader
//! 4. Propose SetPartitionLeader to Raft
//! 5. Update produce/fetch routing
//!
//! Usage:
//! ```rust
//! let elector = LeaderElector::new(raft_cluster.clone());
//!
//! // Start monitoring
//! elector.start_monitoring().await;
//!
//! // Trigger election (called when leader fails)
//! elector.elect_partition_leader("orders", 0).await?;
//! ```

use crate::raft_cluster::RaftCluster;
use crate::raft_metadata::{MetadataCommand, PartitionKey};
use anyhow::{Result, Context};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use dashmap::DashMap;
use tracing::{info, warn, error, debug};

/// Leader election timeout (how long before declaring leader dead)
const LEADER_TIMEOUT: Duration = Duration::from_secs(10);

/// Health check interval (how often to ping leaders)
const HEALTH_CHECK_INTERVAL: Duration = Duration::from_secs(3);

/// Partition health state
#[derive(Debug, Clone)]
struct PartitionHealth {
    /// Last time we received a heartbeat from the leader
    last_heartbeat: Instant,

    /// Current leader node ID
    current_leader: u64,

    /// Whether election is in progress
    electing: bool,
}

/// Leader elector for partition-level leader election
pub struct LeaderElector {
    /// RaftCluster for metadata queries and proposals
    raft_cluster: Arc<RaftCluster>,

    /// Per-partition health tracking
    partition_health: Arc<DashMap<PartitionKey, PartitionHealth>>,

    /// Shutdown signal
    shutdown: Arc<tokio::sync::Notify>,
}

impl LeaderElector {
    /// Create a new leader elector
    pub fn new(raft_cluster: Arc<RaftCluster>) -> Self {
        Self {
            raft_cluster,
            partition_health: Arc::new(DashMap::new()),
            shutdown: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// Start monitoring partition leaders (background task)
    pub fn start_monitoring(&self) {
        let raft_cluster = self.raft_cluster.clone();
        let partition_health = self.partition_health.clone();
        let shutdown = self.shutdown.clone();

        tokio::spawn(async move {
            info!("Leader election monitor started");

            loop {
                tokio::select! {
                    _ = tokio::time::sleep(HEALTH_CHECK_INTERVAL) => {
                        // Check all partitions for leader health
                        Self::check_partition_leaders(
                            &raft_cluster,
                            &partition_health
                        ).await;
                    }
                    _ = shutdown.notified() => {
                        info!("Leader election monitor shutting down");
                        break;
                    }
                }
            }
        });
    }

    /// Check all partition leaders for health
    async fn check_partition_leaders(
        raft_cluster: &Arc<RaftCluster>,
        partition_health: &Arc<DashMap<PartitionKey, PartitionHealth>>,
    ) {
        // Get all partitions from raft metadata
        let partitions = raft_cluster.list_all_partitions();

        for (topic, partition) in partitions {
            // Get current leader
            let leader = match raft_cluster.get_partition_leader(&topic, partition) {
                Some(l) => l,
                None => {
                    // No leader assigned yet, trigger election
                    debug!("No leader for {}-{}, triggering election", topic, partition);
                    Self::trigger_election(
                        raft_cluster,
                        partition_health,
                        &topic,
                        partition
                    ).await;
                    continue;
                }
            };

            // Check health state
            let needs_election = partition_health
                .get(&(topic.clone(), partition))
                .map(|health| {
                    let elapsed = health.last_heartbeat.elapsed();
                    elapsed > LEADER_TIMEOUT && !health.electing
                })
                .unwrap_or(true);  // No health record = need election

            if needs_election {
                warn!(
                    "Leader timeout for {}-{} (leader={}), triggering election",
                    topic, partition, leader
                );
                Self::trigger_election(
                    raft_cluster,
                    partition_health,
                    &topic,
                    partition
                ).await;
            }
        }
    }

    /// Trigger leader election for a partition
    async fn trigger_election(
        raft_cluster: &Arc<RaftCluster>,
        partition_health: &Arc<DashMap<PartitionKey, PartitionHealth>>,
        topic: &str,
        partition: i32,
    ) {
        let key = (topic.to_string(), partition);

        // Mark as electing to prevent duplicate elections
        partition_health.insert(
            key.clone(),
            PartitionHealth {
                last_heartbeat: Instant::now(),
                current_leader: 0,  // Unknown during election
                electing: true,
            },
        );

        // Elect new leader
        match Self::elect_leader_from_isr(raft_cluster, topic, partition).await {
            Ok(new_leader) => {
                info!(
                    "✅ Elected new leader for {}-{}: node {}",
                    topic, partition, new_leader
                );

                // Update health state
                partition_health.insert(
                    key,
                    PartitionHealth {
                        last_heartbeat: Instant::now(),
                        current_leader: new_leader,
                        electing: false,
                    },
                );
            }
            Err(e) => {
                error!(
                    "❌ Failed to elect leader for {}-{}: {}",
                    topic, partition, e
                );

                // Clear electing flag to allow retry
                partition_health.remove(&key);
            }
        }
    }

    /// Elect a new leader from ISR members
    async fn elect_leader_from_isr(
        raft_cluster: &Arc<RaftCluster>,
        topic: &str,
        partition: i32,
    ) -> Result<u64> {
        // Get ISR for this partition
        let isr = raft_cluster
            .get_isr(topic, partition)
            .context("No ISR found for partition")?;

        if isr.is_empty() {
            // Fallback: Get replicas if ISR is empty
            let replicas = raft_cluster
                .get_partition_replicas(topic, partition)
                .context("No replicas found for partition")?;

            if replicas.is_empty() {
                anyhow::bail!("No replicas or ISR for partition {}-{}", topic, partition);
            }

            // Pick first replica
            let new_leader = replicas[0];
            warn!(
                "ISR empty for {}-{}, electing first replica as leader: {}",
                topic, partition, new_leader
            );

            // Propose to Raft
            raft_cluster
                .propose_set_partition_leader(topic, partition, new_leader)
                .await?;

            return Ok(new_leader);
        }

        // Pick first ISR member as new leader (Kafka-style)
        let new_leader = isr[0];

        info!(
            "Electing first ISR member as leader for {}-{}: {} (ISR: {:?})",
            topic, partition, new_leader, isr
        );

        // Propose to Raft
        raft_cluster
            .propose_set_partition_leader(topic, partition, new_leader)
            .await?;

        Ok(new_leader)
    }

    /// Record a heartbeat from a partition leader (called by produce/fetch handlers)
    pub fn record_heartbeat(&self, topic: &str, partition: i32, leader_node_id: u64) {
        let key = (topic.to_string(), partition);

        self.partition_health.insert(
            key,
            PartitionHealth {
                last_heartbeat: Instant::now(),
                current_leader: leader_node_id,
                electing: false,
            },
        );
    }

    /// Manually trigger election for a partition (for testing or admin ops)
    pub async fn elect_partition_leader(&self, topic: &str, partition: i32) -> Result<u64> {
        info!("Manual leader election triggered for {}-{}", topic, partition);

        Self::elect_leader_from_isr(
            &self.raft_cluster,
            topic,
            partition
        ).await
    }

    /// Shutdown the monitor
    pub fn shutdown(&self) {
        self.shutdown.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_leader_election_from_isr() {
        // This test would require a mock RaftCluster
        // Skipping for now, will test with integration tests
    }
}
