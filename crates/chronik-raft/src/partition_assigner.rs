//! Partition Assigner - Automatic partition assignment on cluster startup
//!
//! This module handles automatic partition assignment when a Raft cluster starts up.
//! It ensures every topic partition has Raft replicas assigned to cluster nodes.

use crate::{RaftGroupManager, Result};
use chronik_common::metadata::MetadataStore;
use chronik_common::partition_assignment::PartitionAssignment;
use chronik_common::types::NodeId;
use parking_lot::RwLock;
use std::sync::Arc;
use tracing::{debug, info, warn};

/// Key for storing partition assignment in metadata store
const PARTITION_ASSIGNMENT_KEY: &str = "__partition_assignment";

/// Configuration for partition assignment
#[derive(Debug, Clone)]
pub struct AssignmentConfig {
    /// This node's ID in the cluster
    pub node_id: NodeId,

    /// All node IDs in the cluster (including this node)
    pub all_nodes: Vec<NodeId>,

    /// Default replication factor for new topics
    pub default_replication_factor: i32,
}

impl AssignmentConfig {
    /// Create a new assignment configuration
    pub fn new(node_id: NodeId, all_nodes: Vec<NodeId>) -> Self {
        let default_replication_factor = 3.min(all_nodes.len() as i32);
        Self {
            node_id,
            all_nodes,
            default_replication_factor,
        }
    }

    /// Check if this node is in the cluster
    pub fn is_member(&self, node_id: NodeId) -> bool {
        self.all_nodes.contains(&node_id)
    }
}

/// Manages partition assignment across cluster nodes
pub struct PartitionAssigner {
    /// Assignment configuration
    assignment_config: AssignmentConfig,

    /// Raft group manager for creating replicas
    raft_group_manager: Arc<RaftGroupManager>,

    /// Metadata store for persistence
    metadata_store: Arc<dyn MetadataStore>,

    /// Current partition assignment (cached)
    partition_assignment: Arc<RwLock<PartitionAssignment>>,
}

impl PartitionAssigner {
    /// Create a new partition assigner
    ///
    /// # Arguments
    /// * `assignment_config` - Assignment configuration
    /// * `raft_group_manager` - Raft group manager
    /// * `metadata_store` - Metadata store
    pub fn new(
        assignment_config: AssignmentConfig,
        raft_group_manager: Arc<RaftGroupManager>,
        metadata_store: Arc<dyn MetadataStore>,
    ) -> Self {
        info!(
            "Creating PartitionAssigner for node {} in cluster with {} nodes",
            assignment_config.node_id,
            assignment_config.all_nodes.len()
        );

        Self {
            assignment_config,
            raft_group_manager,
            metadata_store,
            partition_assignment: Arc::new(RwLock::new(PartitionAssignment::new())),
        }
    }

    /// Assign partitions for all existing topics on cluster startup
    ///
    /// This is called once when the cluster starts up. It:
    /// 1. Loads existing assignment from metadata (if any)
    /// 2. For each topic without assignment, creates round-robin assignment
    /// 3. Creates Raft replicas on this node (if assigned)
    /// 4. Persists assignment to metadata
    ///
    /// # Returns
    /// Number of topics processed
    pub async fn assign_existing_topics(&self) -> Result<usize> {
        info!("Assigning partitions for existing topics on cluster startup");

        // Step 1: Load existing assignment from metadata
        self.load_assignment_from_metadata().await?;

        // Step 2: Get all topics from metadata
        let topics = self.metadata_store
            .list_topics()
            .await
            .map_err(|e| crate::RaftError::Config(format!("Failed to list topics: {}", e)))?;

        info!("Found {} topics in metadata", topics.len());

        let mut assigned_count = 0;

        // Step 3: Process each topic
        for topic_meta in topics {
            let topic = &topic_meta.name;
            let num_partitions = topic_meta.config.partition_count as i32;

            // Check if topic already has assignment
            if self.has_assignment(topic) {
                debug!("Topic {} already has assignment", topic);

                // Create replicas for partitions assigned to this node
                for partition in 0..num_partitions {
                    self.create_replica_if_assigned(topic, partition).await?;
                }
            } else {
                // Topic has no assignment - create one
                info!(
                    "Creating new assignment for topic {} ({} partitions)",
                    topic, num_partitions
                );

                self.assign_new_topic(topic, num_partitions).await?;
                assigned_count += 1;
            }
        }

        // Step 4: Persist updated assignment
        if assigned_count > 0 {
            self.save_assignment_to_metadata().await?;
            info!("Assigned {} new topics", assigned_count);
        }

        Ok(assigned_count)
    }

    /// Assign a newly created topic
    ///
    /// # Arguments
    /// * `topic` - Topic name
    /// * `num_partitions` - Number of partitions
    ///
    /// # Returns
    /// Error if assignment fails
    pub async fn assign_new_topic(&self, topic: &str, num_partitions: i32) -> Result<()> {
        info!("Assigning new topic: {} ({} partitions)", topic, num_partitions);

        // Add topic to assignment using round-robin strategy
        {
            let mut assignment = self.partition_assignment.write();
            assignment
                .add_topic(
                    topic,
                    num_partitions,
                    self.assignment_config.default_replication_factor,
                    &self.assignment_config.all_nodes,
                )
                .map_err(|e| crate::RaftError::Config(format!("Assignment failed: {}", e)))?;
        }

        // Create replicas on this node (if assigned)
        for partition in 0..num_partitions {
            self.create_replica_if_assigned(topic, partition).await?;
        }

        // Persist assignment
        self.save_assignment_to_metadata().await?;

        Ok(())
    }

    /// Create a Raft replica if this node is assigned to the partition
    ///
    /// # Arguments
    /// * `topic` - Topic name
    /// * `partition` - Partition ID
    async fn create_replica_if_assigned(&self, topic: &str, partition: i32) -> Result<()> {
        // Check if this node is in the replica list
        let replicas = {
            let assignment = self.partition_assignment.read();
            assignment.get_replicas(topic, partition).map(|r| r.to_vec())
        };

        let Some(replica_nodes) = replicas else {
            warn!(
                "Partition {}-{} has no assignment (skipping replica creation)",
                topic, partition
            );
            return Ok(());
        };

        // Check if this node is assigned
        if !replica_nodes.contains(&self.assignment_config.node_id) {
            debug!(
                "Partition {}-{} not assigned to node {} (assigned to {:?})",
                topic, partition, self.assignment_config.node_id, replica_nodes
            );
            return Ok(());
        }

        // This node is assigned - create replica if not already exists
        if self.raft_group_manager.has_replica(topic, partition) {
            debug!(
                "Replica for {}-{} already exists on node {}",
                topic, partition, self.assignment_config.node_id
            );
            return Ok(());
        }

        // Build peer list (all replicas except this node)
        // Convert NodeId (u32) to u64 for Raft
        let peers: Vec<u64> = replica_nodes
            .iter()
            .filter(|&&node_id| node_id != self.assignment_config.node_id)
            .map(|&node_id| node_id as u64)
            .collect();

        info!(
            "Creating replica for {}-{} on node {} with peers {:?}",
            topic, partition, self.assignment_config.node_id, peers
        );

        // Create the replica via RaftGroupManager
        self.raft_group_manager
            .get_or_create_replica(topic, partition, peers)?;

        Ok(())
    }

    /// Check if a topic has partition assignment
    fn has_assignment(&self, topic: &str) -> bool {
        let assignment = self.partition_assignment.read();
        assignment.partition_count(topic) > 0
    }

    /// Load partition assignment from metadata store
    ///
    /// # Returns
    /// Ok(true) if assignment was loaded, Ok(false) if no existing assignment
    async fn load_assignment_from_metadata(&self) -> Result<bool> {
        debug!("Loading partition assignment from metadata");

        // Try to get assignment from metadata store
        // For now, we'll use a custom key-value approach
        // TODO: Add get_raw_value/set_raw_value to MetadataStore trait

        // STUB: For now, create empty assignment
        // Phase 5 will implement proper persistence via metadata Raft group

        info!("No existing partition assignment found (creating new)");
        Ok(false)
    }

    /// Save partition assignment to metadata store
    async fn save_assignment_to_metadata(&self) -> Result<()> {
        debug!("Saving partition assignment to metadata");

        let assignment = self.partition_assignment.read();

        // Validate before saving
        assignment
            .validate()
            .map_err(|e| crate::RaftError::Config(format!("Invalid assignment: {}", e)))?;

        // Serialize to JSON
        let json = assignment
            .to_json()
            .map_err(|e| crate::RaftError::Config(format!("Serialization failed: {}", e)))?;

        info!(
            "Partition assignment serialized ({} bytes)",
            json.len()
        );

        // STUB: For now, just log the assignment
        // Phase 5 will implement proper persistence via metadata Raft group
        debug!("Assignment JSON: {}", json);

        Ok(())
    }

    /// Rebalance partitions if needed (future hook)
    ///
    /// Currently a no-op. Phase 5 will implement:
    /// - Detecting imbalanced assignments
    /// - Triggering replica migration
    /// - Updating assignment and metadata
    pub async fn rebalance_if_needed(&self) -> Result<()> {
        debug!("Rebalancing check (currently no-op)");
        // Phase 5 implementation
        Ok(())
    }

    /// Get current partition assignment (for inspection)
    pub fn get_assignment(&self) -> PartitionAssignment {
        self.partition_assignment.read().clone()
    }

    /// Get replicas for a partition
    pub fn get_replicas(&self, topic: &str, partition: i32) -> Option<Vec<NodeId>> {
        let assignment = self.partition_assignment.read();
        assignment.get_replicas(topic, partition).map(|r| r.to_vec())
    }

    /// Get leader for a partition
    pub fn get_leader(&self, topic: &str, partition: i32) -> Option<NodeId> {
        let assignment = self.partition_assignment.read();
        assignment.get_leader(topic, partition)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MemoryLogStorage, RaftConfig};
    use chronik_common::metadata::{
        ConsumerGroupMetadata, ConsumerOffset, MetadataError, PartitionAssignment as MetadataPartitionAssignment,
        Result as MetadataResult, SegmentMetadata, TopicConfig, TopicMetadata, BrokerMetadata, BrokerStatus,
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

        fn add_test_topic(&self, name: &str, partitions: u32) {
            let meta = TopicMetadata {
                id: uuid::Uuid::new_v4(),
                name: name.to_string(),
                config: TopicConfig {
                    partition_count: partitions,
                    replication_factor: 3,
                    ..Default::default()
                },
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            };
            self.topics.write().insert(name.to_string(), meta);
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

    fn create_test_setup(node_id: u64, all_nodes: Vec<u64>) -> (
        AssignmentConfig,
        Arc<RaftGroupManager>,
        Arc<TestMetadataStore>,
    ) {
        let assignment_config = AssignmentConfig::new(node_id as u32, all_nodes.into_iter().map(|n| n as u32).collect());

        let raft_config = RaftConfig {
            node_id,
            ..Default::default()
        };

        let raft_manager = Arc::new(RaftGroupManager::new(
            node_id,
            raft_config,
            || Arc::new(MemoryLogStorage::new()),
        ));

        let metadata_store = Arc::new(TestMetadataStore::new());

        (assignment_config, raft_manager, metadata_store)
    }

    #[tokio::test]
    async fn test_create_assigner() {
        let (assignment_config, raft_manager, metadata_store) = create_test_setup(1, vec![1, 2, 3]);

        let assigner = PartitionAssigner::new(
            assignment_config.clone(),
            raft_manager,
            metadata_store,
        );

        assert_eq!(assigner.assignment_config.node_id, 1);
        assert_eq!(assigner.assignment_config.all_nodes.len(), 3);
    }

    #[tokio::test]
    async fn test_assign_new_topic() {
        let (assignment_config, raft_manager, metadata_store) = create_test_setup(1, vec![1, 2, 3]);

        let assigner = PartitionAssigner::new(
            assignment_config.clone(),
            raft_manager.clone(),
            metadata_store,
        );

        // Assign a new topic with 9 partitions
        assigner.assign_new_topic("orders", 9).await.unwrap();

        // Verify assignment was created
        let assignment = assigner.get_assignment();
        assert_eq!(assignment.partition_count("orders"), 9);

        // Verify round-robin assignment
        // Node 1 should be leader for partitions 0, 3, 6
        assert_eq!(assigner.get_leader("orders", 0), Some(1));
        assert_eq!(assigner.get_leader("orders", 3), Some(1));
        assert_eq!(assigner.get_leader("orders", 6), Some(1));

        // Verify replicas were created on node 1 for partitions it's assigned to
        // Partition 0: [1,2,3] - node 1 should have replica
        assert!(raft_manager.has_replica("orders", 0));

        // Partition 1: [2,3,1] - node 1 should have replica
        assert!(raft_manager.has_replica("orders", 1));

        // Total replicas should be 9 (one for each partition)
        assert_eq!(raft_manager.replica_count(), 9);
    }

    #[tokio::test]
    async fn test_assign_existing_topics() {
        let (assignment_config, raft_manager, metadata_store) = create_test_setup(1, vec![1, 2, 3]);

        // Add some topics to metadata
        metadata_store.add_test_topic("topic-a", 3);
        metadata_store.add_test_topic("topic-b", 6);

        let assigner = PartitionAssigner::new(
            assignment_config.clone(),
            raft_manager.clone(),
            metadata_store,
        );

        // Assign all existing topics
        let count = assigner.assign_existing_topics().await.unwrap();
        assert_eq!(count, 2); // 2 topics assigned

        // Verify assignments
        let assignment = assigner.get_assignment();
        assert_eq!(assignment.partition_count("topic-a"), 3);
        assert_eq!(assignment.partition_count("topic-b"), 6);

        // Verify replicas created
        assert!(raft_manager.has_replica("topic-a", 0));
        assert!(raft_manager.has_replica("topic-b", 0));
    }

    #[tokio::test]
    async fn test_node_assignment_filtering() {
        // Test that node 2 only gets replicas for partitions it's assigned to
        let (assignment_config, raft_manager, metadata_store) = create_test_setup(2, vec![1, 2, 3]);

        let assigner = PartitionAssigner::new(
            assignment_config.clone(),
            raft_manager.clone(),
            metadata_store,
        );

        // Assign topic with 3 partitions
        assigner.assign_new_topic("test", 3).await.unwrap();

        // Round-robin: [1,2,3], [2,3,1], [3,1,2]
        // Node 2 is in all 3 partitions, so should have 3 replicas
        assert_eq!(raft_manager.replica_count(), 3);

        assert!(raft_manager.has_replica("test", 0)); // [1,2,3]
        assert!(raft_manager.has_replica("test", 1)); // [2,3,1]
        assert!(raft_manager.has_replica("test", 2)); // [3,1,2]
    }

    #[tokio::test]
    async fn test_rebalance_noop() {
        let (assignment_config, raft_manager, metadata_store) = create_test_setup(1, vec![1, 2, 3]);

        let assigner = PartitionAssigner::new(
            assignment_config.clone(),
            raft_manager,
            metadata_store,
        );

        // Should not error (currently no-op)
        assert!(assigner.rebalance_if_needed().await.is_ok());
    }
}
