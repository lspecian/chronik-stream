//! Partition assignment logic for Chronik Raft clustering.
//!
//! Manages the assignment of topic partitions to nodes in the cluster, including:
//! - Replica placement across nodes
//! - Leader selection for each partition
//! - Round-robin distribution strategy
//! - JSON serialization for metadata persistence

use crate::error::{Error, Result};
use crate::types::NodeId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Key identifying a topic partition.
#[derive(Debug, Clone, Hash, Eq, PartialEq, Serialize, Deserialize)]
pub struct PartitionKey {
    pub topic: String,
    pub partition: i32,
}

impl PartitionKey {
    pub fn new(topic: impl Into<String>, partition: i32) -> Self {
        Self {
            topic: topic.into(),
            partition,
        }
    }
}

/// Assignment information for a single partition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartitionInfo {
    /// Ordered list of replica node IDs (first is preferred leader)
    pub replicas: Vec<NodeId>,
    /// Current leader node ID
    pub leader: NodeId,
}

/// Assignment entry for serialization.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct AssignmentEntry {
    topic: String,
    partition: i32,
    info: PartitionInfo,
}

/// Manages partition assignments across cluster nodes.
///
/// Maps each topic partition to:
/// - A list of replica nodes
/// - The current leader node
#[derive(Debug, Clone)]
pub struct PartitionAssignment {
    /// Map from (topic, partition) to assignment info
    assignments: HashMap<PartitionKey, PartitionInfo>,
}

impl PartitionAssignment {
    /// Create an empty partition assignment.
    pub fn new() -> Self {
        Self {
            assignments: HashMap::new(),
        }
    }

    /// Assign replicas to a partition.
    ///
    /// # Arguments
    /// * `topic` - Topic name
    /// * `partition` - Partition number
    /// * `replicas` - Ordered list of node IDs (first is preferred leader)
    ///
    /// # Returns
    /// Error if replicas list is empty or contains duplicates
    pub fn assign_partition(
        &mut self,
        topic: impl Into<String>,
        partition: i32,
        replicas: Vec<NodeId>,
    ) -> Result<()> {
        let topic = topic.into();

        // Validate replicas list
        if replicas.is_empty() {
            return Err(Error::Configuration(
                "Replicas list cannot be empty".to_string(),
            ));
        }

        // Check for duplicate replicas
        let mut unique = replicas.clone();
        unique.sort();
        unique.dedup();
        if unique.len() != replicas.len() {
            return Err(Error::Configuration(
                "Replicas list contains duplicates".to_string(),
            ));
        }

        // First replica is the preferred leader
        let leader = replicas[0];

        let key = PartitionKey::new(&topic, partition);
        let info = PartitionInfo { replicas, leader };

        self.assignments.insert(key, info);
        Ok(())
    }

    /// Get the replica list for a partition.
    ///
    /// Returns None if partition is not assigned.
    pub fn get_replicas(&self, topic: &str, partition: i32) -> Option<&[NodeId]> {
        let key = PartitionKey::new(topic, partition);
        self.assignments.get(&key).map(|info| info.replicas.as_slice())
    }

    /// Get the current leader for a partition.
    ///
    /// Returns None if partition is not assigned.
    pub fn get_leader(&self, topic: &str, partition: i32) -> Option<NodeId> {
        let key = PartitionKey::new(topic, partition);
        self.assignments.get(&key).map(|info| info.leader)
    }

    /// Update the leader for a partition.
    ///
    /// # Arguments
    /// * `topic` - Topic name
    /// * `partition` - Partition number
    /// * `node_id` - New leader node ID
    ///
    /// # Returns
    /// Error if partition is not assigned or node_id is not in replica list
    pub fn set_leader(
        &mut self,
        topic: impl Into<String>,
        partition: i32,
        node_id: NodeId,
    ) -> Result<()> {
        let topic = topic.into();
        let key = PartitionKey::new(&topic, partition);

        let info = self.assignments.get_mut(&key).ok_or_else(|| {
            Error::Configuration(format!(
                "Partition {}/{} is not assigned",
                topic, partition
            ))
        })?;

        // Verify node is in replica list
        if !info.replicas.contains(&node_id) {
            return Err(Error::Configuration(format!(
                "Node {} is not a replica for partition {}/{}",
                node_id, topic, partition
            )));
        }

        info.leader = node_id;
        Ok(())
    }

    /// Add a topic with partitions using round-robin assignment.
    ///
    /// # Arguments
    /// * `topic` - Topic name
    /// * `num_partitions` - Number of partitions
    /// * `replication_factor` - Number of replicas per partition
    /// * `nodes` - Available node IDs
    ///
    /// # Returns
    /// Error if replication_factor > nodes.len() or invalid parameters
    pub fn add_topic(
        &mut self,
        topic: impl Into<String>,
        num_partitions: i32,
        replication_factor: i32,
        nodes: &[NodeId],
    ) -> Result<()> {
        let topic = topic.into();

        // Validate inputs
        if num_partitions <= 0 {
            return Err(Error::Configuration(
                "Number of partitions must be positive".to_string(),
            ));
        }

        if replication_factor <= 0 {
            return Err(Error::Configuration(
                "Replication factor must be positive".to_string(),
            ));
        }

        if nodes.is_empty() {
            return Err(Error::Configuration(
                "Node list cannot be empty".to_string(),
            ));
        }

        if replication_factor as usize > nodes.len() {
            return Err(Error::Configuration(format!(
                "Replication factor ({}) exceeds number of nodes ({})",
                replication_factor,
                nodes.len()
            )));
        }

        // Use round-robin assignment strategy
        let assignments =
            round_robin(nodes, &topic, num_partitions, replication_factor)?;

        // Add each partition assignment
        for (partition, replicas) in assignments.into_iter().enumerate() {
            self.assign_partition(&topic, partition as i32, replicas)?;
        }

        Ok(())
    }

    /// Remove all partitions for a topic.
    ///
    /// Returns the number of partitions removed.
    pub fn remove_topic(&mut self, topic: &str) -> usize {
        let keys_to_remove: Vec<_> = self
            .assignments
            .keys()
            .filter(|key| key.topic == topic)
            .cloned()
            .collect();

        let count = keys_to_remove.len();
        for key in keys_to_remove {
            self.assignments.remove(&key);
        }
        count
    }

    /// Get all topics in the assignment.
    pub fn topics(&self) -> Vec<String> {
        let mut topics: Vec<_> = self
            .assignments
            .keys()
            .map(|key| key.topic.clone())
            .collect();
        topics.sort();
        topics.dedup();
        topics
    }

    /// Get partition count for a topic.
    pub fn partition_count(&self, topic: &str) -> i32 {
        self.assignments
            .keys()
            .filter(|key| key.topic == topic)
            .count() as i32
    }

    /// Get all partition assignments for a topic.
    pub fn get_topic_assignments(&self, topic: &str) -> Vec<(i32, &PartitionInfo)> {
        let mut result: Vec<_> = self
            .assignments
            .iter()
            .filter(|(key, _)| key.topic == topic)
            .map(|(key, info)| (key.partition, info))
            .collect();
        result.sort_by_key(|(partition, _)| *partition);
        result
    }

    /// Serialize to JSON string.
    pub fn to_json(&self) -> Result<String> {
        let entries: Vec<AssignmentEntry> = self
            .assignments
            .iter()
            .map(|(key, info)| AssignmentEntry {
                topic: key.topic.clone(),
                partition: key.partition,
                info: info.clone(),
            })
            .collect();

        serde_json::to_string_pretty(&entries)
            .map_err(|e| Error::Configuration(format!("JSON serialization failed: {}", e)))
    }

    /// Deserialize from JSON string.
    pub fn from_json(json: &str) -> Result<Self> {
        let entries: Vec<AssignmentEntry> = serde_json::from_str(json)
            .map_err(|e| Error::Configuration(format!("JSON deserialization failed: {}", e)))?;

        let mut assignment = PartitionAssignment::new();
        for entry in entries {
            let key = PartitionKey::new(&entry.topic, entry.partition);
            assignment.assignments.insert(key, entry.info);
        }

        Ok(assignment)
    }

    /// Validate the assignment.
    ///
    /// Checks:
    /// - All partitions have a leader
    /// - All leaders are in their replica lists
    /// - No duplicate replicas per partition
    pub fn validate(&self) -> Result<()> {
        for (key, info) in &self.assignments {
            // Check leader is in replica list
            if !info.replicas.contains(&info.leader) {
                return Err(Error::Configuration(format!(
                    "Leader {} for partition {}/{} is not in replica list",
                    info.leader, key.topic, key.partition
                )));
            }

            // Check for duplicate replicas
            let mut unique = info.replicas.clone();
            unique.sort();
            unique.dedup();
            if unique.len() != info.replicas.len() {
                return Err(Error::Configuration(format!(
                    "Partition {}/{} has duplicate replicas",
                    key.topic, key.partition
                )));
            }
        }
        Ok(())
    }
}

impl Default for PartitionAssignment {
    fn default() -> Self {
        Self::new()
    }
}

/// Round-robin assignment strategy.
///
/// Distributes partitions evenly across nodes, ensuring:
/// - Each partition has `replication_factor` replicas
/// - Replicas for a partition are on different nodes
/// - Leadership is balanced across nodes
///
/// # Algorithm
/// For each partition:
/// 1. Assign leader using: partition % num_nodes
/// 2. Assign replicas in round-robin order starting from leader
///
/// # Example
/// 3 nodes [1,2,3], 9 partitions, RF=3:
/// - Partition 0: [1,2,3] (leader=1)
/// - Partition 1: [2,3,1] (leader=2)
/// - Partition 2: [3,1,2] (leader=3)
/// - Partition 3: [1,2,3] (leader=1)
/// - ...
pub fn round_robin(
    nodes: &[NodeId],
    _topic: &str,
    num_partitions: i32,
    replication_factor: i32,
) -> Result<Vec<Vec<NodeId>>> {
    if nodes.is_empty() {
        return Err(Error::Configuration(
            "Node list cannot be empty".to_string(),
        ));
    }

    if replication_factor as usize > nodes.len() {
        return Err(Error::Configuration(format!(
            "Replication factor ({}) exceeds number of nodes ({})",
            replication_factor,
            nodes.len()
        )));
    }

    let mut assignments = Vec::with_capacity(num_partitions as usize);

    for partition in 0..num_partitions {
        let mut replicas = Vec::with_capacity(replication_factor as usize);

        // Start from leader position (partition % num_nodes)
        let leader_idx = (partition as usize) % nodes.len();

        // Assign replicas in round-robin order
        for i in 0..replication_factor {
            let node_idx = (leader_idx + i as usize) % nodes.len();
            replicas.push(nodes[node_idx]);
        }

        assignments.push(replicas);
    }

    Ok(assignments)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_round_robin_3_nodes_9_partitions() {
        let nodes = vec![1, 2, 3];
        let result = round_robin(&nodes, "test", 9, 3).unwrap();

        assert_eq!(result.len(), 9);

        // Partition 0: [1,2,3], leader=1
        assert_eq!(result[0], vec![1, 2, 3]);

        // Partition 1: [2,3,1], leader=2
        assert_eq!(result[1], vec![2, 3, 1]);

        // Partition 2: [3,1,2], leader=3
        assert_eq!(result[2], vec![3, 1, 2]);

        // Partition 3: [1,2,3], leader=1
        assert_eq!(result[3], vec![1, 2, 3]);

        // Verify leadership balance
        let leader_counts = count_leaders(&result);
        assert_eq!(leader_counts.get(&1), Some(&3));
        assert_eq!(leader_counts.get(&2), Some(&3));
        assert_eq!(leader_counts.get(&3), Some(&3));
    }

    #[test]
    fn test_round_robin_5_nodes_10_partitions() {
        let nodes = vec![1, 2, 3, 4, 5];
        let result = round_robin(&nodes, "test", 10, 3).unwrap();

        assert_eq!(result.len(), 10);

        // Verify all partitions have 3 replicas
        for replicas in &result {
            assert_eq!(replicas.len(), 3);
        }

        // Verify leadership balance (each node should lead 2 partitions)
        let leader_counts = count_leaders(&result);
        assert_eq!(leader_counts.get(&1), Some(&2));
        assert_eq!(leader_counts.get(&2), Some(&2));
        assert_eq!(leader_counts.get(&3), Some(&2));
        assert_eq!(leader_counts.get(&4), Some(&2));
        assert_eq!(leader_counts.get(&5), Some(&2));

        // Verify no duplicate replicas
        for replicas in &result {
            let mut unique = replicas.clone();
            unique.sort();
            unique.dedup();
            assert_eq!(unique.len(), replicas.len());
        }
    }

    #[test]
    fn test_partition_assignment_basic() {
        let mut assignment = PartitionAssignment::new();

        // Add topic with 9 partitions, RF=3, 3 nodes
        let nodes = vec![1, 2, 3];
        assignment.add_topic("orders", 9, 3, &nodes).unwrap();

        // Verify partition count
        assert_eq!(assignment.partition_count("orders"), 9);

        // Verify partition 0
        let replicas = assignment.get_replicas("orders", 0).unwrap();
        assert_eq!(replicas, &[1, 2, 3]);
        assert_eq!(assignment.get_leader("orders", 0), Some(1));

        // Verify partition 1
        let replicas = assignment.get_replicas("orders", 1).unwrap();
        assert_eq!(replicas, &[2, 3, 1]);
        assert_eq!(assignment.get_leader("orders", 1), Some(2));

        // Verify partition 2
        let replicas = assignment.get_replicas("orders", 2).unwrap();
        assert_eq!(replicas, &[3, 1, 2]);
        assert_eq!(assignment.get_leader("orders", 2), Some(3));
    }

    #[test]
    fn test_set_leader() {
        let mut assignment = PartitionAssignment::new();
        assignment.add_topic("test", 3, 3, &[1, 2, 3]).unwrap();

        // Change leader for partition 0 from 1 to 2
        assignment.set_leader("test", 0, 2).unwrap();
        assert_eq!(assignment.get_leader("test", 0), Some(2));

        // Try to set invalid leader (not in replica list)
        let result = assignment.set_leader("test", 0, 99);
        assert!(result.is_err());
    }

    #[test]
    fn test_remove_topic() {
        let mut assignment = PartitionAssignment::new();
        assignment.add_topic("test", 5, 2, &[1, 2, 3]).unwrap();

        assert_eq!(assignment.partition_count("test"), 5);

        let removed = assignment.remove_topic("test");
        assert_eq!(removed, 5);
        assert_eq!(assignment.partition_count("test"), 0);
    }

    #[test]
    fn test_json_serialization() {
        let mut assignment = PartitionAssignment::new();
        assignment.add_topic("test", 3, 2, &[1, 2, 3]).unwrap();

        // Serialize to JSON
        let json = assignment.to_json().unwrap();
        assert!(!json.is_empty());

        // Deserialize from JSON
        let restored = PartitionAssignment::from_json(&json).unwrap();

        // Verify same data
        assert_eq!(restored.partition_count("test"), 3);
        assert_eq!(
            restored.get_replicas("test", 0),
            assignment.get_replicas("test", 0)
        );
        assert_eq!(
            restored.get_leader("test", 0),
            assignment.get_leader("test", 0)
        );
    }

    #[test]
    fn test_validate() {
        let mut assignment = PartitionAssignment::new();
        assignment.add_topic("test", 3, 2, &[1, 2, 3]).unwrap();

        // Valid assignment should pass
        assert!(assignment.validate().is_ok());

        // Manually break it by setting invalid leader
        assignment.assign_partition("broken", 0, vec![1, 2]).unwrap();
        assignment.set_leader("broken", 0, 1).unwrap();
        // Manually corrupt the leader
        let key = PartitionKey::new("broken", 0);
        assignment.assignments.get_mut(&key).unwrap().leader = 99;

        // Should fail validation
        assert!(assignment.validate().is_err());
    }

    #[test]
    fn test_replication_factor_validation() {
        let mut assignment = PartitionAssignment::new();

        // RF > nodes should fail
        let result = assignment.add_topic("test", 3, 5, &[1, 2, 3]);
        assert!(result.is_err());

        // RF == nodes should succeed
        let result = assignment.add_topic("test", 3, 3, &[1, 2, 3]);
        assert!(result.is_ok());
    }

    #[test]
    fn test_duplicate_replicas_rejected() {
        let mut assignment = PartitionAssignment::new();

        // Try to assign duplicate replicas
        let result = assignment.assign_partition("test", 0, vec![1, 2, 1]);
        assert!(result.is_err());
    }

    #[test]
    fn test_get_topic_assignments() {
        let mut assignment = PartitionAssignment::new();
        assignment.add_topic("orders", 3, 2, &[1, 2, 3]).unwrap();

        let topic_assignments = assignment.get_topic_assignments("orders");
        assert_eq!(topic_assignments.len(), 3);

        // Verify sorted by partition
        assert_eq!(topic_assignments[0].0, 0);
        assert_eq!(topic_assignments[1].0, 1);
        assert_eq!(topic_assignments[2].0, 2);
    }

    #[test]
    fn test_topics_list() {
        let mut assignment = PartitionAssignment::new();
        assignment.add_topic("orders", 3, 2, &[1, 2]).unwrap();
        assignment.add_topic("users", 5, 2, &[1, 2]).unwrap();

        let topics = assignment.topics();
        assert_eq!(topics, vec!["orders", "users"]);
    }

    // Helper function to count leaders
    fn count_leaders(assignments: &[Vec<NodeId>]) -> HashMap<NodeId, usize> {
        let mut counts = HashMap::new();
        for replicas in assignments {
            if let Some(&leader) = replicas.first() {
                *counts.entry(leader).or_insert(0) += 1;
            }
        }
        counts
    }
}
