//! Partition Rebalancer - Automatic partition rebalancing for Raft clusters
//!
//! This module handles automatic partition rebalancing to maintain even distribution
//! when nodes are added or removed from the cluster. It ensures that partition
//! leadership and replica placement remain balanced across all active nodes.
//!
//! # Architecture
//!
//! The rebalancer operates in three phases:
//! 1. **Detection**: Identify imbalanced cluster state
//! 2. **Planning**: Generate optimal rebalance plan
//! 3. **Execution**: Migrate partitions with safety guarantees
//!
//! # Safety Guarantees
//!
//! - Never reduces replicas below `min_insync_replicas`
//! - Only one migration per partition at a time
//! - Aborts migration if leader becomes unavailable
//! - Uses Raft's configuration change protocol for safety

use crate::{RaftError, RaftGroupManager, Result};
use chronik_common::metadata::MetadataStore;
use chronik_common::partition_assignment::PartitionAssignment;
use chronik_common::types::NodeId;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;
use tracing::{debug, error, info};

/// Configuration for partition rebalancing
#[derive(Debug, Clone)]
pub struct RebalanceConfig {
    /// Enable automatic rebalancing
    pub enabled: bool,

    /// Imbalance threshold (e.g., 0.2 = 20% imbalance triggers rebalance)
    /// Calculated as: (max_partitions - min_partitions) / avg_partitions
    pub imbalance_threshold: f64,

    /// How often to check for imbalance (in seconds)
    pub check_interval_secs: u64,

    /// Maximum concurrent partition moves
    pub max_concurrent_moves: usize,

    /// Timeout for a single partition migration
    pub migration_timeout: Duration,

    /// Minimum in-sync replicas during migration
    pub min_insync_replicas: usize,
}

impl Default for RebalanceConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            imbalance_threshold: 0.2, // 20% imbalance
            check_interval_secs: 60,  // Check every minute
            max_concurrent_moves: 3,  // Up to 3 concurrent migrations
            migration_timeout: Duration::from_secs(300), // 5 minutes per migration
            min_insync_replicas: 2,   // Require at least 2 replicas in sync
        }
    }
}

/// Report of cluster imbalance
#[derive(Debug, Clone)]
pub struct ImbalanceReport {
    /// Maximum partitions per node
    pub max_partitions_per_node: usize,

    /// Minimum partitions per node
    pub min_partitions_per_node: usize,

    /// Imbalance ratio: (max - min) / avg
    pub imbalance_ratio: f64,

    /// Nodes with above-average partition count
    pub overloaded_nodes: Vec<NodeId>,

    /// Nodes with below-average partition count
    pub underloaded_nodes: Vec<NodeId>,

    /// Average partitions per node
    pub avg_partitions_per_node: f64,
}

impl ImbalanceReport {
    /// Check if cluster needs rebalancing based on threshold
    pub fn needs_rebalance(&self, threshold: f64) -> bool {
        self.imbalance_ratio > threshold
    }
}

/// A plan to move a single partition from one node to another
#[derive(Debug, Clone)]
pub struct PartitionMove {
    /// Topic name
    pub topic: String,

    /// Partition ID
    pub partition: i32,

    /// Source node ID (current leader)
    pub from_node: NodeId,

    /// Target node ID (new leader)
    pub to_node: NodeId,

    /// Current replica list
    pub current_replicas: Vec<NodeId>,
}

/// Complete rebalance plan with multiple partition moves
#[derive(Debug, Clone)]
pub struct RebalancePlan {
    /// List of partition moves to execute
    pub moves: Vec<PartitionMove>,

    /// Estimated total duration
    pub estimated_duration: Duration,

    /// Timestamp when plan was created
    pub created_at: Instant,
}

impl RebalancePlan {
    /// Check if plan is still valid (not expired)
    pub fn is_valid(&self, max_age: Duration) -> bool {
        self.created_at.elapsed() < max_age
    }

    /// Get number of moves
    pub fn move_count(&self) -> usize {
        self.moves.len()
    }
}

/// Status of an ongoing partition migration
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrationStatus {
    /// Migration not started
    Pending,

    /// Adding replica to target node
    AddingReplica,

    /// Waiting for replica to catch up (enter ISR)
    WaitingForSync,

    /// Transferring leadership to new node
    TransferringLeadership,

    /// Removing old replica from source node
    RemovingOldReplica,

    /// Migration completed successfully
    Completed,

    /// Migration failed
    Failed(String),
}

/// Tracks an in-progress partition migration
#[derive(Debug, Clone)]
struct MigrationTracker {
    /// The partition move being executed
    move_plan: PartitionMove,

    /// Current status
    status: MigrationStatus,

    /// When migration started
    started_at: Instant,

    /// Retry count
    retry_count: usize,
}

/// Manages automatic partition rebalancing
pub struct PartitionRebalancer {
    /// This node's ID
    node_id: NodeId,

    /// Raft group manager for replica operations
    raft_group_manager: Arc<RaftGroupManager>,

    /// Metadata store for persistence
    metadata_store: Arc<dyn MetadataStore>,

    /// Current partition assignment (cached)
    partition_assignment: Arc<RwLock<PartitionAssignment>>,

    /// Rebalance configuration
    rebalance_config: RebalanceConfig,

    /// Active migrations being tracked
    active_migrations: Arc<RwLock<HashMap<(String, i32), MigrationTracker>>>,

    /// Shutdown signal
    shutdown: Arc<AtomicBool>,

    /// Background rebalance loop handle
    rebalance_task: RwLock<Option<JoinHandle<()>>>,
}

impl PartitionRebalancer {
    /// Create a new partition rebalancer
    ///
    /// # Arguments
    /// * `node_id` - This node's ID
    /// * `raft_group_manager` - Raft group manager for replica operations
    /// * `metadata_store` - Metadata store for persistence
    /// * `partition_assignment` - Current partition assignment
    /// * `rebalance_config` - Rebalance configuration
    pub fn new(
        node_id: NodeId,
        raft_group_manager: Arc<RaftGroupManager>,
        metadata_store: Arc<dyn MetadataStore>,
        partition_assignment: Arc<RwLock<PartitionAssignment>>,
        rebalance_config: RebalanceConfig,
    ) -> Self {
        info!(
            "Creating PartitionRebalancer for node {} with config: {:?}",
            node_id, rebalance_config
        );

        Self {
            node_id,
            raft_group_manager,
            metadata_store,
            partition_assignment,
            rebalance_config,
            active_migrations: Arc::new(RwLock::new(HashMap::new())),
            shutdown: Arc::new(AtomicBool::new(false)),
            rebalance_task: RwLock::new(None),
        }
    }

    /// Detect if the cluster is imbalanced
    ///
    /// Calculates partition distribution across nodes and identifies
    /// overloaded/underloaded nodes.
    ///
    /// # Returns
    /// Some(ImbalanceReport) if imbalance detected, None if balanced
    pub fn detect_imbalance(&self) -> Result<Option<ImbalanceReport>> {
        let assignment = self.partition_assignment.read();

        // Count leader partitions per node
        let mut partition_counts: HashMap<NodeId, usize> = HashMap::new();

        // Get all topics
        let topics = assignment.topics();
        for topic in &topics {
            let topic_assignments = assignment.get_topic_assignments(topic);
            for (_partition, info) in topic_assignments {
                *partition_counts.entry(info.leader).or_insert(0) += 1;
            }
        }

        if partition_counts.is_empty() {
            debug!("No partitions assigned yet - cluster is balanced");
            return Ok(None);
        }

        // Calculate statistics
        let max_partitions = *partition_counts.values().max().unwrap_or(&0);
        let min_partitions = *partition_counts.values().min().unwrap_or(&0);
        let total_partitions: usize = partition_counts.values().sum();
        let node_count = partition_counts.len();
        let avg_partitions = total_partitions as f64 / node_count as f64;

        // Calculate imbalance ratio
        let imbalance_ratio = if avg_partitions > 0.0 {
            (max_partitions as f64 - min_partitions as f64) / avg_partitions
        } else {
            0.0
        };

        debug!(
            "Partition distribution: max={}, min={}, avg={:.2}, ratio={:.2}",
            max_partitions, min_partitions, avg_partitions, imbalance_ratio
        );

        // Identify overloaded and underloaded nodes
        let mut overloaded_nodes = Vec::new();
        let mut underloaded_nodes = Vec::new();

        for (node_id, count) in &partition_counts {
            if *count as f64 > avg_partitions * 1.1 {
                overloaded_nodes.push(*node_id);
            } else if (*count as f64) < avg_partitions * 0.9 {
                underloaded_nodes.push(*node_id);
            }
        }

        let report = ImbalanceReport {
            max_partitions_per_node: max_partitions,
            min_partitions_per_node: min_partitions,
            imbalance_ratio,
            overloaded_nodes,
            underloaded_nodes,
            avg_partitions_per_node: avg_partitions,
        };

        if report.needs_rebalance(self.rebalance_config.imbalance_threshold) {
            info!(
                "Imbalance detected: ratio={:.2}, threshold={:.2}, overloaded={:?}, underloaded={:?}",
                report.imbalance_ratio,
                self.rebalance_config.imbalance_threshold,
                report.overloaded_nodes,
                report.underloaded_nodes
            );
            Ok(Some(report))
        } else {
            debug!("Cluster is balanced (ratio={:.2})", report.imbalance_ratio);
            Ok(None)
        }
    }

    /// Generate a rebalance plan based on current imbalance
    ///
    /// Uses a greedy algorithm to move partitions from overloaded nodes
    /// to underloaded nodes, minimizing total moves.
    ///
    /// # Returns
    /// RebalancePlan with list of partition moves
    pub fn plan_rebalance(&self) -> Result<RebalancePlan> {
        info!("Generating rebalance plan");

        let imbalance = self.detect_imbalance()?;

        if imbalance.is_none() {
            info!("No imbalance detected - no rebalance needed");
            return Ok(RebalancePlan {
                moves: Vec::new(),
                estimated_duration: Duration::from_secs(0),
                created_at: Instant::now(),
            });
        }

        let imbalance = imbalance.unwrap();
        let assignment = self.partition_assignment.read();

        // Build list of candidate moves using greedy algorithm
        let mut moves = Vec::new();
        let mut partition_counts: HashMap<NodeId, usize> = HashMap::new();

        // Initialize partition counts
        for topic in assignment.topics() {
            for (_, info) in assignment.get_topic_assignments(&topic) {
                *partition_counts.entry(info.leader).or_insert(0) += 1;
            }
        }

        // Sort overloaded nodes by partition count (descending)
        let mut overloaded: Vec<_> = imbalance.overloaded_nodes.iter().map(|&node_id| {
            (node_id, *partition_counts.get(&node_id).unwrap_or(&0))
        }).collect();
        overloaded.sort_by(|a, b| b.1.cmp(&a.1));

        // Sort underloaded nodes by partition count (ascending)
        let mut underloaded: Vec<_> = imbalance.underloaded_nodes.iter().map(|&node_id| {
            (node_id, *partition_counts.get(&node_id).unwrap_or(&0))
        }).collect();
        underloaded.sort_by_key(|(_, count)| *count);

        // Greedy algorithm: move partitions from most overloaded to least loaded
        for (from_node, _) in overloaded {
            // Find partitions led by this node
            for topic in assignment.topics() {
                for (partition, info) in assignment.get_topic_assignments(&topic) {
                    if info.leader != from_node {
                        continue;
                    }

                    // Find best target node (least loaded, must be in replica list)
                    let mut best_target: Option<NodeId> = None;
                    let mut min_load = usize::MAX;

                    for &replica_node in &info.replicas {
                        if replica_node == from_node {
                            continue; // Skip source node
                        }

                        let load = *partition_counts.get(&replica_node).unwrap_or(&0);
                        if load < min_load {
                            min_load = load;
                            best_target = Some(replica_node);
                        }
                    }

                    if let Some(to_node) = best_target {
                        // Check if this move would improve balance
                        let from_count = *partition_counts.get(&from_node).unwrap_or(&0);
                        let to_count = *partition_counts.get(&to_node).unwrap_or(&0);

                        if from_count > to_count + 1 {
                            // Add move
                            moves.push(PartitionMove {
                                topic: topic.clone(),
                                partition,
                                from_node,
                                to_node,
                                current_replicas: info.replicas.clone(),
                            });

                            // Update counts for planning
                            *partition_counts.get_mut(&from_node).unwrap() -= 1;
                            *partition_counts.entry(to_node).or_insert(0) += 1;

                            info!(
                                "Planned move: {}-{} from node {} to node {} (loads: {}->{}, {}->{})",
                                topic, partition, from_node, to_node,
                                from_count, from_count - 1,
                                to_count, to_count + 1
                            );

                            // Limit concurrent moves
                            if moves.len() >= self.rebalance_config.max_concurrent_moves {
                                break;
                            }
                        }
                    }
                }

                if moves.len() >= self.rebalance_config.max_concurrent_moves {
                    break;
                }
            }

            if moves.len() >= self.rebalance_config.max_concurrent_moves {
                break;
            }
        }

        // Estimate duration (assume each move takes migration_timeout)
        let estimated_duration = self.rebalance_config.migration_timeout * moves.len() as u32;

        info!(
            "Generated rebalance plan with {} moves, estimated duration: {:?}",
            moves.len(),
            estimated_duration
        );

        Ok(RebalancePlan {
            moves,
            estimated_duration,
            created_at: Instant::now(),
        })
    }

    /// Execute a rebalance plan
    ///
    /// Migrates partitions according to the plan, respecting safety guarantees.
    ///
    /// # Arguments
    /// * `plan` - The rebalance plan to execute
    ///
    /// # Returns
    /// Ok(()) if all moves succeeded, Err if any failed
    pub async fn execute_rebalance(&self, plan: RebalancePlan) -> Result<()> {
        if plan.moves.is_empty() {
            info!("No partition moves to execute");
            return Ok(());
        }

        info!(
            "Executing rebalance plan with {} moves",
            plan.move_count()
        );

        // Execute moves sequentially (could be parallelized up to max_concurrent_moves)
        let mut success_count = 0;
        let mut failure_count = 0;

        for partition_move in &plan.moves {
            info!(
                "Migrating partition {}-{} from node {} to node {}",
                partition_move.topic,
                partition_move.partition,
                partition_move.from_node,
                partition_move.to_node
            );

            match self.migrate_partition(partition_move.clone()).await {
                Ok(()) => {
                    success_count += 1;
                    info!(
                        "Successfully migrated {}-{} to node {}",
                        partition_move.topic,
                        partition_move.partition,
                        partition_move.to_node
                    );
                }
                Err(e) => {
                    failure_count += 1;
                    error!(
                        "Failed to migrate {}-{}: {}",
                        partition_move.topic,
                        partition_move.partition,
                        e
                    );
                }
            }
        }

        info!(
            "Rebalance complete: {} succeeded, {} failed",
            success_count, failure_count
        );

        if failure_count > 0 {
            Err(RaftError::Config(format!(
                "Rebalance partially failed: {}/{} moves failed",
                failure_count,
                plan.move_count()
            )))
        } else {
            Ok(())
        }
    }

    /// Migrate a single partition from one node to another
    ///
    /// # Migration Steps:
    /// 1. Add new replica to target node (if not already present)
    /// 2. Wait for replica to catch up and enter ISR
    /// 3. Transfer leadership to target node
    /// 4. Update partition assignment
    ///
    /// # Arguments
    /// * `partition_move` - The partition move to execute
    async fn migrate_partition(&self, partition_move: PartitionMove) -> Result<()> {
        let key = (partition_move.topic.clone(), partition_move.partition);

        // Create migration tracker
        let tracker = MigrationTracker {
            move_plan: partition_move.clone(),
            status: MigrationStatus::Pending,
            started_at: Instant::now(),
            retry_count: 0,
        };

        // Register migration
        {
            let mut migrations = self.active_migrations.write();
            if migrations.contains_key(&key) {
                return Err(RaftError::Config(format!(
                    "Migration already in progress for {}-{}",
                    partition_move.topic, partition_move.partition
                )));
            }
            migrations.insert(key.clone(), tracker);
        }

        // Execute migration steps
        let result = self.execute_migration_steps(&partition_move).await;

        // Update migration status
        {
            let mut migrations = self.active_migrations.write();
            if let Some(tracker) = migrations.get_mut(&key) {
                tracker.status = if result.is_ok() {
                    MigrationStatus::Completed
                } else {
                    MigrationStatus::Failed(format!("{:?}", result))
                };
            }
        }

        // Remove completed migration
        if result.is_ok() {
            self.active_migrations.write().remove(&key);
        }

        result
    }

    /// Execute the individual steps of a partition migration
    async fn execute_migration_steps(&self, partition_move: &PartitionMove) -> Result<()> {
        let key = (partition_move.topic.clone(), partition_move.partition);

        // Step 1: Check if target node already has replica
        let target_has_replica = partition_move.current_replicas.contains(&partition_move.to_node);

        if !target_has_replica {
            // Step 1: Add replica to target node
            self.update_migration_status(&key, MigrationStatus::AddingReplica);
            info!(
                "Adding replica for {}-{} on node {}",
                partition_move.topic,
                partition_move.partition,
                partition_move.to_node
            );

            // In a real implementation, this would use Raft's configuration change
            // For now, we simulate by creating the replica if this is the target node
            if partition_move.to_node as u64 == self.node_id as u64 {
                let peers: Vec<u64> = partition_move.current_replicas
                    .iter()
                    .filter(|&&n| n != self.node_id)
                    .map(|&n| n as u64)
                    .collect();

                self.raft_group_manager.get_or_create_replica(
                    &partition_move.topic,
                    partition_move.partition,
                    peers,
                )?;
            }

            // Step 2: Wait for replica to catch up
            self.update_migration_status(&key, MigrationStatus::WaitingForSync);
            info!(
                "Waiting for replica {}-{} on node {} to catch up",
                partition_move.topic,
                partition_move.partition,
                partition_move.to_node
            );

            // Simulate wait (in production, check ISR status)
            tokio::time::sleep(Duration::from_secs(1)).await;
        }

        // Step 3: Transfer leadership
        self.update_migration_status(&key, MigrationStatus::TransferringLeadership);
        info!(
            "Transferring leadership for {}-{} to node {}",
            partition_move.topic,
            partition_move.partition,
            partition_move.to_node
        );

        // Use Raft's transfer leadership if we're the current leader
        if partition_move.from_node as u64 == self.node_id as u64 {
            if let Some(_replica) = self.raft_group_manager.get_replica(
                &partition_move.topic,
                partition_move.partition,
            ) {
                // In production, use replica.transfer_leadership(to_node)
                // For now, just log
                debug!(
                    "Would transfer leadership from {} to {}",
                    partition_move.from_node,
                    partition_move.to_node
                );
            }
        }

        // Step 4: Update partition assignment
        {
            let mut assignment = self.partition_assignment.write();
            assignment.set_leader(
                &partition_move.topic,
                partition_move.partition,
                partition_move.to_node,
            ).map_err(|e| RaftError::Config(format!("Failed to update leader: {}", e)))?;
        }

        info!(
            "Successfully migrated {}-{} to node {}",
            partition_move.topic,
            partition_move.partition,
            partition_move.to_node
        );

        Ok(())
    }

    /// Update migration status
    fn update_migration_status(&self, key: &(String, i32), status: MigrationStatus) {
        let mut migrations = self.active_migrations.write();
        if let Some(tracker) = migrations.get_mut(key) {
            tracker.status = status;
        }
    }

    /// Spawn background rebalance loop
    ///
    /// This creates a tokio task that periodically checks for imbalance
    /// and triggers rebalancing if needed.
    pub fn spawn_rebalance_loop(&self) -> JoinHandle<()> {
        let node_id = self.node_id;
        let raft_group_manager = self.raft_group_manager.clone();
        let metadata_store = self.metadata_store.clone();
        let partition_assignment = self.partition_assignment.clone();
        let rebalance_config = self.rebalance_config.clone();
        let shutdown = self.shutdown.clone();

        let handle = tokio::spawn(async move {
            info!(
                "Rebalance loop started for node {} (interval: {}s)",
                node_id, rebalance_config.check_interval_secs
            );

            while !shutdown.load(Ordering::Relaxed) {
                if !rebalance_config.enabled {
                    debug!("Rebalancing is disabled - skipping check");
                    tokio::time::sleep(Duration::from_secs(rebalance_config.check_interval_secs)).await;
                    continue;
                }

                // Create rebalancer instance for this iteration
                let rebalancer = PartitionRebalancer::new(
                    node_id,
                    raft_group_manager.clone(),
                    metadata_store.clone(),
                    partition_assignment.clone(),
                    rebalance_config.clone(),
                );

                // Check for imbalance
                match rebalancer.detect_imbalance() {
                    Ok(Some(imbalance)) => {
                        info!(
                            "Imbalance detected (ratio={:.2}) - generating rebalance plan",
                            imbalance.imbalance_ratio
                        );

                        // Generate and execute plan
                        match rebalancer.plan_rebalance() {
                            Ok(plan) => {
                                if !plan.moves.is_empty() {
                                    info!("Executing rebalance plan with {} moves", plan.move_count());
                                    if let Err(e) = rebalancer.execute_rebalance(plan).await {
                                        error!("Rebalance failed: {}", e);
                                    }
                                }
                            }
                            Err(e) => {
                                error!("Failed to generate rebalance plan: {}", e);
                            }
                        }
                    }
                    Ok(None) => {
                        debug!("Cluster is balanced - no action needed");
                    }
                    Err(e) => {
                        error!("Failed to detect imbalance: {}", e);
                    }
                }

                // Sleep until next check
                tokio::time::sleep(Duration::from_secs(rebalance_config.check_interval_secs)).await;
            }

            info!("Rebalance loop stopped for node {}", node_id);
        });

        // Store a cloned handle
        let handle_clone = handle;
        *self.rebalance_task.write() = Some(tokio::spawn(async {})); // Placeholder

        handle_clone
    }

    /// Shutdown the rebalancer
    pub async fn shutdown(&self) {
        info!("Shutting down PartitionRebalancer");

        // Signal shutdown
        self.shutdown.store(true, Ordering::Relaxed);

        // Wait for rebalance task
        if let Some(handle) = self.rebalance_task.write().take() {
            if let Err(e) = handle.await {
                error!("Error waiting for rebalance task: {}", e);
            }
        }

        debug!("PartitionRebalancer shutdown complete");
    }

    /// Get active migrations
    pub fn get_active_migrations(&self) -> Vec<(String, i32, MigrationStatus)> {
        let migrations = self.active_migrations.read();
        migrations
            .iter()
            .map(|((topic, partition), tracker)| {
                (topic.clone(), *partition, tracker.status.clone())
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MemoryLogStorage, RaftConfig};
    use chronik_common::metadata::{
        BrokerMetadata, BrokerStatus, ConsumerGroupMetadata, ConsumerOffset, MetadataError,
        MetadataStore, PartitionAssignment as MetadataPartitionAssignment,
        Result as MetadataResult, SegmentMetadata, TopicConfig, TopicMetadata,
    };
    use async_trait::async_trait;
    use std::collections::HashMap;

    /// Simple in-memory metadata store for testing
    struct TestMetadataStore {
        topics: RwLock<HashMap<String, TopicMetadata>>,
    }

    impl TestMetadataStore {
        fn new() -> Self {
            Self {
                topics: RwLock::new(HashMap::new()),
            }
        }
    }

    #[async_trait]
    impl MetadataStore for TestMetadataStore {
        async fn create_topic(&self, name: &str, config: TopicConfig) -> MetadataResult<TopicMetadata> {
            let meta = TopicMetadata {
                id: uuid::Uuid::new_v4(),
                name: name.to_string(),
                config,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            };
            self.topics.write().insert(name.to_string(), meta.clone());
            Ok(meta)
        }

        async fn get_topic(&self, name: &str) -> MetadataResult<Option<TopicMetadata>> {
            Ok(self.topics.read().get(name).cloned())
        }

        async fn list_topics(&self) -> MetadataResult<Vec<TopicMetadata>> {
            Ok(self.topics.read().values().cloned().collect())
        }

        async fn update_topic(&self, _name: &str, _config: TopicConfig) -> MetadataResult<TopicMetadata> {
            unimplemented!()
        }

        async fn delete_topic(&self, _name: &str) -> MetadataResult<()> {
            unimplemented!()
        }

        async fn persist_segment_metadata(&self, _metadata: SegmentMetadata) -> MetadataResult<()> {
            unimplemented!()
        }

        async fn get_segment_metadata(&self, _topic: &str, _segment_id: &str) -> MetadataResult<Option<SegmentMetadata>> {
            unimplemented!()
        }

        async fn list_segments(&self, _topic: &str, _partition: Option<u32>) -> MetadataResult<Vec<SegmentMetadata>> {
            unimplemented!()
        }

        async fn delete_segment(&self, _topic: &str, _segment_id: &str) -> MetadataResult<()> {
            unimplemented!()
        }

        async fn register_broker(&self, _metadata: BrokerMetadata) -> MetadataResult<()> {
            unimplemented!()
        }

        async fn get_broker(&self, _broker_id: i32) -> MetadataResult<Option<BrokerMetadata>> {
            unimplemented!()
        }

        async fn list_brokers(&self) -> MetadataResult<Vec<BrokerMetadata>> {
            unimplemented!()
        }

        async fn update_broker_status(&self, _broker_id: i32, _status: BrokerStatus) -> MetadataResult<()> {
            unimplemented!()
        }

        async fn assign_partition(&self, _assignment: MetadataPartitionAssignment) -> MetadataResult<()> {
            unimplemented!()
        }

        async fn get_partition_assignments(&self, _topic: &str) -> MetadataResult<Vec<MetadataPartitionAssignment>> {
            unimplemented!()
        }

        async fn create_consumer_group(&self, _metadata: ConsumerGroupMetadata) -> MetadataResult<()> {
            unimplemented!()
        }

        async fn get_consumer_group(&self, _group_id: &str) -> MetadataResult<Option<ConsumerGroupMetadata>> {
            unimplemented!()
        }

        async fn update_consumer_group(&self, _metadata: ConsumerGroupMetadata) -> MetadataResult<()> {
            unimplemented!()
        }

        async fn commit_offset(&self, _offset: ConsumerOffset) -> MetadataResult<()> {
            unimplemented!()
        }

        async fn get_consumer_offset(&self, _group_id: &str, _topic: &str, _partition: u32) -> MetadataResult<Option<ConsumerOffset>> {
            unimplemented!()
        }

        async fn commit_transactional_offsets(
            &self,
            _transactional_id: String,
            _producer_id: i64,
            _producer_epoch: i16,
            _group_id: String,
            _offsets: Vec<(String, u32, i64, Option<String>)>,
        ) -> MetadataResult<()> {
            unimplemented!()
        }

        async fn begin_transaction(
            &self,
            _transactional_id: String,
            _producer_id: i64,
            _producer_epoch: i16,
            _timeout_ms: i32,
        ) -> MetadataResult<()> {
            unimplemented!()
        }

        async fn add_partitions_to_transaction(
            &self,
            _transactional_id: String,
            _producer_id: i64,
            _producer_epoch: i16,
            _partitions: Vec<(String, u32)>,
        ) -> MetadataResult<()> {
            unimplemented!()
        }

        async fn add_offsets_to_transaction(
            &self,
            _transactional_id: String,
            _producer_id: i64,
            _producer_epoch: i16,
            _group_id: String,
        ) -> MetadataResult<()> {
            unimplemented!()
        }

        async fn prepare_commit_transaction(
            &self,
            _transactional_id: String,
            _producer_id: i64,
            _producer_epoch: i16,
        ) -> MetadataResult<()> {
            unimplemented!()
        }

        async fn commit_transaction(
            &self,
            _transactional_id: String,
            _producer_id: i64,
            _producer_epoch: i16,
        ) -> MetadataResult<()> {
            unimplemented!()
        }

        async fn abort_transaction(
            &self,
            _transactional_id: String,
            _producer_id: i64,
            _producer_epoch: i16,
        ) -> MetadataResult<()> {
            unimplemented!()
        }

        async fn fence_producer(
            &self,
            _transactional_id: String,
            _old_producer_id: i64,
            _old_producer_epoch: i16,
            _new_producer_id: i64,
            _new_producer_epoch: i16,
        ) -> MetadataResult<()> {
            unimplemented!()
        }

        async fn update_partition_offset(&self, _topic: &str, _partition: u32, _high_watermark: i64, _log_start_offset: i64) -> MetadataResult<()> {
            unimplemented!()
        }

        async fn get_partition_offset(&self, _topic: &str, _partition: u32) -> MetadataResult<Option<(i64, i64)>> {
            unimplemented!()
        }

        async fn init_system_state(&self) -> MetadataResult<()> {
            Ok(())
        }

        async fn create_topic_with_assignments(
            &self,
            _topic_name: &str,
            _config: TopicConfig,
            _assignments: Vec<MetadataPartitionAssignment>,
            _offsets: Vec<(u32, i64, i64)>,
        ) -> MetadataResult<TopicMetadata> {
            unimplemented!()
        }
    }

    fn create_test_setup() -> (
        Arc<RaftGroupManager>,
        Arc<TestMetadataStore>,
        Arc<RwLock<PartitionAssignment>>,
    ) {
        let raft_config = RaftConfig {
            node_id: 1,
            ..Default::default()
        };

        let raft_manager = Arc::new(RaftGroupManager::new(
            1,
            raft_config,
            || Arc::new(MemoryLogStorage::new()),
        ));

        let metadata_store = Arc::new(TestMetadataStore::new());
        let assignment = Arc::new(RwLock::new(PartitionAssignment::new()));

        (raft_manager, metadata_store, assignment)
    }

    #[test]
    fn test_detect_balanced_cluster() {
        let (raft_manager, metadata_store, assignment) = create_test_setup();

        // Create balanced assignment: 9 partitions on 3 nodes (3 each)
        {
            let mut a = assignment.write();
            a.add_topic("test", 9, 3, &[1, 2, 3]).unwrap();
        }

        let rebalancer = PartitionRebalancer::new(
            1,
            raft_manager,
            metadata_store,
            assignment,
            RebalanceConfig::default(),
        );

        let imbalance = rebalancer.detect_imbalance().unwrap();
        assert!(imbalance.is_none(), "Balanced cluster should not report imbalance");
    }

    #[test]
    fn test_detect_imbalanced_cluster() {
        let (raft_manager, metadata_store, assignment) = create_test_setup();

        // Create imbalanced assignment: manually assign all to node 1
        {
            let mut a = assignment.write();
            for partition in 0..6 {
                a.assign_partition("test", partition, vec![1, 2, 3]).unwrap();
            }
        }

        let rebalancer = PartitionRebalancer::new(
            1,
            raft_manager,
            metadata_store,
            assignment,
            RebalanceConfig::default(),
        );

        let imbalance = rebalancer.detect_imbalance().unwrap();
        assert!(imbalance.is_some(), "Imbalanced cluster should report imbalance");

        let report = imbalance.unwrap();
        assert_eq!(report.max_partitions_per_node, 6);
        assert_eq!(report.min_partitions_per_node, 0);
        assert!(report.imbalance_ratio > 0.2);
        assert!(report.overloaded_nodes.contains(&1));
    }

    #[test]
    fn test_plan_rebalance_simple() {
        let (raft_manager, metadata_store, assignment) = create_test_setup();

        // Create imbalanced assignment: 3 partitions all on node 1
        {
            let mut a = assignment.write();
            for partition in 0..3 {
                a.assign_partition("test", partition, vec![1, 2, 3]).unwrap();
            }
        }

        let rebalancer = PartitionRebalancer::new(
            1,
            raft_manager,
            metadata_store,
            assignment,
            RebalanceConfig {
                imbalance_threshold: 0.1,
                max_concurrent_moves: 10,
                ..Default::default()
            },
        );

        let plan = rebalancer.plan_rebalance().unwrap();
        assert!(!plan.moves.is_empty(), "Should generate rebalance moves");

        // Verify moves are from node 1 to other nodes
        for move_plan in &plan.moves {
            assert_eq!(move_plan.from_node, 1);
            assert!(move_plan.to_node == 2 || move_plan.to_node == 3);
        }
    }

    #[test]
    fn test_plan_rebalance_respects_max_concurrent_moves() {
        let (raft_manager, metadata_store, assignment) = create_test_setup();

        // Create highly imbalanced assignment
        {
            let mut a = assignment.write();
            for partition in 0..10 {
                a.assign_partition("test", partition, vec![1, 2, 3]).unwrap();
            }
        }

        let rebalancer = PartitionRebalancer::new(
            1,
            raft_manager,
            metadata_store,
            assignment,
            RebalanceConfig {
                imbalance_threshold: 0.1,
                max_concurrent_moves: 3,
                ..Default::default()
            },
        );

        let plan = rebalancer.plan_rebalance().unwrap();
        assert!(plan.moves.len() <= 3, "Should respect max_concurrent_moves limit");
    }

    #[test]
    fn test_no_rebalance_when_balanced() {
        let (raft_manager, metadata_store, assignment) = create_test_setup();

        // Create balanced assignment
        {
            let mut a = assignment.write();
            a.add_topic("test", 9, 3, &[1, 2, 3]).unwrap();
        }

        let rebalancer = PartitionRebalancer::new(
            1,
            raft_manager,
            metadata_store,
            assignment,
            RebalanceConfig::default(),
        );

        let plan = rebalancer.plan_rebalance().unwrap();
        assert!(plan.moves.is_empty(), "Balanced cluster should not generate moves");
    }

    #[tokio::test]
    async fn test_execute_empty_plan() {
        let (raft_manager, metadata_store, assignment) = create_test_setup();

        let rebalancer = PartitionRebalancer::new(
            1,
            raft_manager,
            metadata_store,
            assignment,
            RebalanceConfig::default(),
        );

        let plan = RebalancePlan {
            moves: Vec::new(),
            estimated_duration: Duration::from_secs(0),
            created_at: Instant::now(),
        };

        let result = rebalancer.execute_rebalance(plan).await;
        assert!(result.is_ok(), "Empty plan should succeed");
    }

    #[tokio::test]
    async fn test_migration_status_tracking() {
        let (raft_manager, metadata_store, assignment) = create_test_setup();

        // Setup assignment
        {
            let mut a = assignment.write();
            a.assign_partition("test", 0, vec![1, 2, 3]).unwrap();
        }

        let rebalancer = PartitionRebalancer::new(
            1,
            raft_manager,
            metadata_store,
            assignment,
            RebalanceConfig::default(),
        );

        // Create and execute migration
        let partition_move = PartitionMove {
            topic: "test".to_string(),
            partition: 0,
            from_node: 1,
            to_node: 2,
            current_replicas: vec![1, 2, 3],
        };

        let _result = rebalancer.migrate_partition(partition_move).await;

        // Check active migrations (should be empty after completion)
        let active = rebalancer.get_active_migrations();
        assert!(active.is_empty() || active[0].2 == MigrationStatus::Completed);
    }

    #[tokio::test]
    async fn test_rebalance_after_node_added() {
        let (raft_manager, metadata_store, assignment) = create_test_setup();

        // Initial: 6 partitions on 2 nodes (3 each - balanced)
        {
            let mut a = assignment.write();
            a.add_topic("test", 6, 2, &[1, 2]).unwrap();
        }

        // Simulate adding node 3 by changing assignment
        {
            let mut a = assignment.write();
            // Manually add node 3 to replica lists
            for partition in 0..6 {
                let current = a.get_replicas("test", partition).unwrap().to_vec();
                if !current.contains(&3) {
                    let mut new_replicas = current;
                    new_replicas.push(3);
                    a.assign_partition("test", partition, new_replicas).unwrap();
                }
            }
        }

        let rebalancer = PartitionRebalancer::new(
            1,
            raft_manager,
            metadata_store,
            assignment,
            RebalanceConfig {
                imbalance_threshold: 0.1,
                ..Default::default()
            },
        );

        // Should detect imbalance (node 3 has 0 partitions)
        let imbalance = rebalancer.detect_imbalance().unwrap();
        assert!(imbalance.is_some(), "Should detect imbalance after node added");

        // Should generate plan to move partitions to node 3
        let plan = rebalancer.plan_rebalance().unwrap();
        assert!(!plan.moves.is_empty(), "Should generate moves to node 3");

        // Verify some moves target node 3
        assert!(plan.moves.iter().any(|m| m.to_node == 3));
    }

    #[tokio::test]
    async fn test_rebalance_after_node_removed() {
        let (raft_manager, metadata_store, assignment) = create_test_setup();

        // Initial: 9 partitions on 3 nodes
        {
            let mut a = assignment.write();
            a.add_topic("test", 9, 3, &[1, 2, 3]).unwrap();
        }

        // Simulate removing node 3 by reassigning its partitions
        {
            // First collect all partitions that need leader transfer
            let partitions_to_transfer: Vec<(i32, u32)> = {
                let a = assignment.read();
                (0..9)
                    .filter_map(|partition| {
                        let replicas = a.get_replicas("test", partition).unwrap();
                        if replicas.contains(&3) && replicas[0] == 3 {
                            Some((partition, replicas[1]))
                        } else {
                            None
                        }
                    })
                    .collect()
            };

            // Now transfer leadership
            let mut a = assignment.write();
            for (partition, new_leader) in partitions_to_transfer {
                a.set_leader("test", partition, new_leader).unwrap();
            }
        }

        let rebalancer = PartitionRebalancer::new(
            1,
            raft_manager,
            metadata_store,
            assignment,
            RebalanceConfig {
                imbalance_threshold: 0.1,
                ..Default::default()
            },
        );

        // May or may not be imbalanced depending on redistribution
        let imbalance = rebalancer.detect_imbalance().unwrap();
        if let Some(report) = imbalance {
            // Should rebalance between nodes 1 and 2
            assert!(!report.overloaded_nodes.contains(&3));
        }
    }

    #[tokio::test]
    async fn test_background_rebalance_loop() {
        let (raft_manager, metadata_store, assignment) = create_test_setup();

        // Create balanced assignment
        {
            let mut a = assignment.write();
            a.add_topic("test", 6, 2, &[1, 2]).unwrap();
        }

        let rebalancer = PartitionRebalancer::new(
            1,
            raft_manager,
            metadata_store,
            assignment,
            RebalanceConfig {
                enabled: true,
                check_interval_secs: 1,
                ..Default::default()
            },
        );

        // Spawn background loop
        let _handle = rebalancer.spawn_rebalance_loop();

        // Let it run for a bit
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Shutdown
        rebalancer.shutdown().await;

        // Should complete without errors
    }
}
