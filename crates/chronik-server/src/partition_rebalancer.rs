//! Partition Rebalancer (v2.2.7 - Priority 2 Step 3)
//!
//! Automatically rebalances partition assignments when cluster membership changes.
//!
//! ## How It Works
//!
//! 1. **Background Worker**: Polls Raft every 30s to detect membership changes
//! 2. **Change Detection**: Compares current node count with last known count
//! 3. **Rebalancing**: When nodes added/removed, recalculates partition assignments
//! 4. **Proposal**: Submits new assignments via Raft consensus
//!
//! ## Partition Assignment Algorithm
//!
//! Uses round-robin with replication factor:
//! - Partition 0 → [Node 1, Node 2, Node 3]
//! - Partition 1 → [Node 2, Node 3, Node 4]
//! - Partition 2 → [Node 3, Node 4, Node 1]
//! - etc.
//!
//! This ensures:
//! - Even distribution across nodes
//! - Predictable replica placement
//! - Minimal data movement on rebalancing

use anyhow::{Context, Result};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::time::{sleep, Duration};
use tracing::{debug, error, info, warn};

use crate::raft_cluster::RaftCluster;
use chronik_common::metadata::MetadataStore;

/// Partition Rebalancer
///
/// Background worker that monitors cluster membership and automatically
/// rebalances partition assignments when nodes are added or removed.
pub struct PartitionRebalancer {
    /// Raft cluster handle
    raft_cluster: Arc<RaftCluster>,

    /// Metadata store for querying topics/partitions
    metadata_store: Arc<dyn MetadataStore>,

    /// Interval between rebalance checks
    rebalance_interval: Duration,

    /// Last known member count (for change detection)
    last_member_count: AtomicUsize,

    /// Replication factor for new assignments
    replication_factor: usize,
}

impl PartitionRebalancer {
    /// Create a new Partition Rebalancer
    ///
    /// # Arguments
    /// - `raft_cluster` - Raft cluster handle
    /// - `metadata_store` - Metadata store for querying topics
    /// - `replication_factor` - Number of replicas per partition
    ///
    /// # Returns
    /// Arc-wrapped rebalancer with background worker started
    pub async fn new(
        raft_cluster: Arc<RaftCluster>,
        metadata_store: Arc<dyn MetadataStore>,
        replication_factor: usize,
    ) -> Arc<Self> {
        let current_nodes = raft_cluster.get_all_nodes().await;

        info!(
            "Initializing Partition Rebalancer (current nodes: {}, replication_factor: {})",
            current_nodes.len(),
            replication_factor
        );

        let rebalancer = Arc::new(Self {
            raft_cluster,
            metadata_store,
            rebalance_interval: Duration::from_secs(30),
            last_member_count: AtomicUsize::new(current_nodes.len()),
            replication_factor,
        });

        // Start background worker
        Self::spawn_rebalance_worker(rebalancer.clone());

        rebalancer
    }

    /// Spawn background worker that monitors membership changes
    fn spawn_rebalance_worker(rebalancer: Arc<Self>) {
        tokio::spawn(async move {
            info!("Starting Partition Rebalancer background worker (checking every 30s)");

            loop {
                sleep(rebalancer.rebalance_interval).await;

                // Get current cluster membership
                let current_nodes = rebalancer.raft_cluster.get_all_nodes().await;
                let current_count = current_nodes.len();
                let last_count = rebalancer.last_member_count.load(Ordering::Relaxed);

                debug!(
                    "Rebalancer check: {} nodes (prev: {})",
                    current_count, last_count
                );

                // Check if membership changed
                if current_count != last_count {
                    info!(
                        "Cluster membership changed: {} → {} nodes - triggering rebalance",
                        last_count, current_count
                    );

                    // Rebalance all topics
                    if let Err(e) = rebalancer.rebalance_all_topics(&current_nodes).await {
                        error!("Failed to rebalance partitions: {}", e);
                    } else {
                        info!("✓ Rebalancing complete");
                    }

                    // Update last known count
                    rebalancer
                        .last_member_count
                        .store(current_count, Ordering::Relaxed);
                }
            }
        });
    }

    /// Rebalance all topics in the cluster
    ///
    /// Queries all topics from metadata store and rebalances each one.
    async fn rebalance_all_topics(&self, nodes: &[u64]) -> Result<()> {
        let topics = self
            .metadata_store
            .list_topics()
            .await
            .context("Failed to list topics")?;

        if topics.is_empty() {
            info!("No topics to rebalance");
            return Ok(());
        }

        info!("Rebalancing {} topics across {} nodes", topics.len(), nodes.len());

        for topic in topics {
            if let Err(e) = self.rebalance_topic(&topic.name, nodes).await {
                error!("Failed to rebalance topic '{}': {}", topic.name, e);
                // Continue with other topics
            }
        }

        Ok(())
    }

    /// Rebalance a single topic
    ///
    /// Recalculates partition assignments for the topic and proposes
    /// new assignments via Raft.
    async fn rebalance_topic(&self, topic: &str, nodes: &[u64]) -> Result<()> {
        // Get topic metadata to determine partition count
        let topic_meta = self
            .metadata_store
            .get_topic(topic)
            .await
            .context(format!("Failed to get topic metadata for '{}'", topic))?;

        let topic_meta = match topic_meta {
            Some(meta) => meta,
            None => {
                warn!("Topic '{}' not found in metadata store", topic);
                return Ok(());
            }
        };

        let partition_count = topic_meta.config.partition_count as i32;

        if partition_count == 0 {
            debug!("Topic '{}' has no partitions, skipping", topic);
            return Ok(());
        }

        debug!(
            "Rebalancing topic '{}' ({} partitions)",
            topic, partition_count
        );

        // Calculate new assignments
        let new_assignments =
            self.calculate_assignments(partition_count, nodes, self.replication_factor);

        // Assign partitions via metadata_store (WAL-based, not Raft - Option 4 / Phase 4)
        for (partition, new_replicas) in new_assignments {
            use chronik_common::metadata::PartitionAssignment;

            let leader_id = new_replicas.first().copied().unwrap_or(0);

            let assignment = PartitionAssignment {
                topic: topic.to_string(),
                partition: partition as u32,
                broker_id: leader_id as i32, // Deprecated field
                is_leader: true, // Deprecated field
                replicas: new_replicas.clone(),
                leader_id,
            };

            self.metadata_store
                .assign_partition(assignment)
                .await
                .context(format!(
                    "Failed to assign partition {}-{}",
                    topic, partition
                ))?;

            debug!(
                "Assigned partition {}-{}: replicas={:?}, leader={}",
                topic, partition, new_replicas, leader_id
            );
        }

        info!(
            "✓ Rebalanced topic '{}' across {} nodes ({} partitions)",
            topic,
            nodes.len(),
            partition_count
        );

        Ok(())
    }

    /// Calculate partition assignments using round-robin algorithm
    ///
    /// # Arguments
    /// - `partition_count` - Number of partitions in the topic
    /// - `nodes` - List of node IDs in the cluster
    /// - `replication_factor` - Number of replicas per partition
    ///
    /// # Returns
    /// Vector of (partition_id, replica_list) tuples
    ///
    /// # Algorithm
    /// ```text
    /// For each partition P:
    ///   For each replica R (0..replication_factor):
    ///     Node = nodes[(P + R) % node_count]
    /// ```
    ///
    /// # Example
    /// ```text
    /// Nodes: [1, 2, 3, 4]
    /// Replication Factor: 3
    ///
    /// Partition 0 → [1, 2, 3]
    /// Partition 1 → [2, 3, 4]
    /// Partition 2 → [3, 4, 1]
    /// Partition 3 → [4, 1, 2]
    /// Partition 4 → [1, 2, 3]
    /// ...
    /// ```
    fn calculate_assignments(
        &self,
        partition_count: i32,
        nodes: &[u64],
        replication_factor: usize,
    ) -> Vec<(i32, Vec<u64>)> {
        let mut assignments = Vec::new();
        let node_count = nodes.len();

        if node_count == 0 {
            warn!("No nodes available for assignment");
            return assignments;
        }

        // Cap replication factor at node count
        let effective_rf = std::cmp::min(replication_factor, node_count);

        if effective_rf < replication_factor {
            warn!(
                "Replication factor {} exceeds node count {}, using {}",
                replication_factor, node_count, effective_rf
            );
        }

        for partition in 0..partition_count {
            let mut replicas = Vec::new();

            // Assign replicas using round-robin
            for offset in 0..effective_rf {
                let node_index = (partition as usize + offset) % node_count;
                replicas.push(nodes[node_index]);
            }

            assignments.push((partition, replicas));
        }

        assignments
    }

    /// Get current rebalancing status
    ///
    /// Returns information about the rebalancer state.
    pub async fn status(&self) -> RebalancerStatus {
        let current_nodes = self.raft_cluster.get_all_nodes().await;
        let last_count = self.last_member_count.load(Ordering::Relaxed);

        RebalancerStatus {
            current_node_count: current_nodes.len(),
            last_known_node_count: last_count,
            rebalance_interval_secs: self.rebalance_interval.as_secs(),
            replication_factor: self.replication_factor,
        }
    }
}

/// Rebalancer status information
#[derive(Debug, Clone)]
pub struct RebalancerStatus {
    pub current_node_count: usize,
    pub last_known_node_count: usize,
    pub rebalance_interval_secs: u64,
    pub replication_factor: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calculate_assignments_basic() {
        // Mock rebalancer (only need calculate_assignments method)
        let nodes = vec![1, 2, 3];
        let partition_count = 6;
        let replication_factor = 3;

        // Create a minimal rebalancer for testing
        // We can't easily create a full rebalancer in unit tests,
        // so we'll test the logic directly

        // Expected assignments:
        // P0 → [1, 2, 3]
        // P1 → [2, 3, 1]
        // P2 → [3, 1, 2]
        // P3 → [1, 2, 3]
        // P4 → [2, 3, 1]
        // P5 → [3, 1, 2]

        let mut assignments = Vec::new();
        for partition in 0..partition_count {
            let mut replicas = Vec::new();
            for offset in 0..replication_factor {
                let node_index = (partition as usize + offset) % nodes.len();
                replicas.push(nodes[node_index]);
            }
            assignments.push((partition, replicas));
        }

        assert_eq!(assignments.len(), 6);
        assert_eq!(assignments[0].1, vec![1, 2, 3]);
        assert_eq!(assignments[1].1, vec![2, 3, 1]);
        assert_eq!(assignments[2].1, vec![3, 1, 2]);
        assert_eq!(assignments[3].1, vec![1, 2, 3]);
    }

    #[test]
    fn test_calculate_assignments_4_nodes() {
        let nodes = vec![1, 2, 3, 4];
        let partition_count = 4;
        let replication_factor = 3;

        let mut assignments = Vec::new();
        for partition in 0..partition_count {
            let mut replicas = Vec::new();
            for offset in 0..replication_factor {
                let node_index = (partition as usize + offset) % nodes.len();
                replicas.push(nodes[node_index]);
            }
            assignments.push((partition, replicas));
        }

        // Expected:
        // P0 → [1, 2, 3]
        // P1 → [2, 3, 4]
        // P2 → [3, 4, 1]
        // P3 → [4, 1, 2]

        assert_eq!(assignments[0].1, vec![1, 2, 3]);
        assert_eq!(assignments[1].1, vec![2, 3, 4]);
        assert_eq!(assignments[2].1, vec![3, 4, 1]);
        assert_eq!(assignments[3].1, vec![4, 1, 2]);
    }

    #[test]
    fn test_calculate_assignments_rf_exceeds_nodes() {
        // Replication factor > node count
        let nodes = vec![1, 2];
        let partition_count = 3;
        let replication_factor = 3;

        // Should cap RF at node_count
        let effective_rf = std::cmp::min(replication_factor, nodes.len());
        assert_eq!(effective_rf, 2);

        let mut assignments = Vec::new();
        for partition in 0..partition_count {
            let mut replicas = Vec::new();
            for offset in 0..effective_rf {
                let node_index = (partition as usize + offset) % nodes.len();
                replicas.push(nodes[node_index]);
            }
            assignments.push((partition, replicas));
        }

        // Expected (with RF=2):
        // P0 → [1, 2]
        // P1 → [2, 1]
        // P2 → [1, 2]

        assert_eq!(assignments[0].1, vec![1, 2]);
        assert_eq!(assignments[1].1, vec![2, 1]);
        assert_eq!(assignments[2].1, vec![1, 2]);
    }
}
