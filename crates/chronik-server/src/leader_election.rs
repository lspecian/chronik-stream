//! Event-Driven Leader Election per Partition (v2.2.7)
//!
//! This module implements Redpanda-style event-driven partition leader election.
//!
//! **KEY DESIGN CHANGE (v2.2.7):**
//! - NO continuous monitoring or background health checks
//! - Elections triggered ONLY by actual failure detection events:
//!   1. WAL replication stream timeout (follower detects dead leader)
//!   2. Raft node failure notification
//!   3. Manual administrative action
//!
//! **Why Event-Driven?**
//! - Eliminates 3-second polling loop overhead
//! - Reduces Raft proposals (only on actual failures)
//! - Follows Redpanda's design: heartbeats are implicit in WAL streams
//! - More Kafka-like: elections on failure, not continuous monitoring
//!
//! Algorithm:
//! 1. WAL follower detects stream timeout (leader dead)
//! 2. Check ISR (in-sync replicas) from RaftCluster metadata
//! 3. Elect first ISR member as new leader
//! 4. Propose SetPartitionLeader to Raft (only Raft leader can do this)
//! 5. Update produce/fetch routing
//!
//! Usage:
//! ```rust
//! let elector = LeaderElector::new(raft_cluster.clone());
//!
//! // NO start_monitoring() call - it's event-driven!
//!
//! // Trigger election when WAL stream fails:
//! elector.trigger_election_on_timeout("orders", 0).await?;
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
/// Used by WAL replication to detect failures
pub const LEADER_TIMEOUT: Duration = Duration::from_secs(10);

/// Track ongoing elections to prevent duplicates
#[derive(Debug, Clone)]
struct ElectionState {
    /// Timestamp when election started
    started_at: Instant,

    /// Whether election is in progress
    in_progress: bool,
}

/// Event-driven leader elector for partition-level leader election (v2.2.7)
///
/// This struct provides leader election capabilities WITHOUT continuous monitoring.
/// Elections are triggered by external events (WAL timeouts, Raft notifications, etc.)
pub struct LeaderElector {
    /// RaftCluster for metadata queries and proposals
    raft_cluster: Arc<RaftCluster>,

    /// Track ongoing elections to prevent duplicates
    /// Key: (topic, partition), Value: election state
    ongoing_elections: Arc<DashMap<PartitionKey, ElectionState>>,
}

impl LeaderElector {
    /// Create a new event-driven leader elector (v2.2.7)
    ///
    /// No background tasks are started - elections happen only when triggered by events
    pub fn new(raft_cluster: Arc<RaftCluster>) -> Self {
        info!("LeaderElector created in EVENT-DRIVEN mode (no continuous monitoring)");
        Self {
            raft_cluster,
            ongoing_elections: Arc::new(DashMap::new()),
        }
    }

    /// Trigger leader election for a partition on timeout (v2.2.7)
    ///
    /// This is the main entry point for event-driven elections.
    /// Called by WAL replication when a follower detects the leader has stopped sending.
    ///
    /// **IMPORTANT**: This should only be called by the Raft leader node, as only
    /// the Raft leader can propose metadata changes.
    pub async fn trigger_election_on_timeout(
        &self,
        topic: &str,
        partition: i32,
        reason: &str,
    ) -> Result<u64> {
        // CRITICAL: Only Raft leader can propose partition leader changes
        if !self.raft_cluster.am_i_leader() {
            debug!(
                "Skipping election for {}-{}: this node is not the Raft leader (reason: {})",
                topic, partition, reason
            );
            anyhow::bail!("Cannot trigger election: not Raft leader");
        }

        let key = (topic.to_string(), partition);

        // Check if election already in progress
        if let Some(state) = self.ongoing_elections.get(&key) {
            if state.in_progress && state.started_at.elapsed() < Duration::from_secs(30) {
                debug!(
                    "Election already in progress for {}-{} (started {:?} ago)",
                    topic, partition, state.started_at.elapsed()
                );
                anyhow::bail!("Election already in progress");
            }
        }

        warn!(
            "Triggering leader election for {}-{}: {}",
            topic, partition, reason
        );

        // Mark election as in progress
        self.ongoing_elections.insert(
            key.clone(),
            ElectionState {
                started_at: Instant::now(),
                in_progress: true,
            },
        );

        // Elect new leader
        let result = Self::elect_leader_from_isr(&self.raft_cluster, topic, partition).await;

        // Clear election state
        self.ongoing_elections.remove(&key);

        match result {
            Ok(new_leader) => {
                info!(
                    "✅ Elected new leader for {}-{}: node {} (reason: {})",
                    topic, partition, new_leader, reason
                );
                Ok(new_leader)
            }
            Err(e) => {
                error!(
                    "❌ Failed to elect leader for {}-{}: {} (reason: {})",
                    topic, partition, e, reason
                );
                Err(e)
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

    /// Manually trigger election for a partition (for testing or admin ops)
    ///
    /// This bypasses the normal timeout-based triggering and immediately elects a new leader.
    pub async fn elect_partition_leader(&self, topic: &str, partition: i32) -> Result<u64> {
        self.trigger_election_on_timeout(topic, partition, "manual trigger").await
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
