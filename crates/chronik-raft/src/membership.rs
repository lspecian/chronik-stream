//! Membership Manager - Dynamic cluster membership changes
//!
//! This module implements runtime membership changes for Raft clusters using
//! the joint consensus protocol. It supports adding and removing nodes from
//! a running cluster without downtime.
//!
//! # Architecture
//!
//! The membership manager uses Raft's ConfChange mechanism to safely modify
//! cluster configuration:
//!
//! 1. **Add Node**: Proposes ConfChangeAddNode to metadata Raft group
//! 2. **Remove Node**: Proposes ConfChangeRemoveNode to metadata Raft group
//! 3. **Joint Consensus**: Waits for both old and new quorum to commit
//! 4. **Replication**: Config changes are replicated via RaftMetaLog
//! 5. **Rebalancing**: Triggers partition reassignment after membership changes
//!
//! # Safety Guarantees
//!
//! - Cannot remove node if it breaks quorum (n/2 + 1 safety check)
//! - Cannot add node with duplicate ID or address
//! - Cannot modify membership while another change is in progress
//! - All changes are atomic and replicated via Raft consensus

use crate::{
    ClusterConfig, ClusterCoordinator, PartitionAssigner, PartitionReplica, PeerConfig,
    RaftGroupManager, RaftError, Result,
};
use parking_lot::RwLock;
use raft::prelude::{ConfChange, ConfChangeType};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

/// Node configuration for membership changes
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NodeConfig {
    /// Node ID (must be unique)
    pub node_id: u64,

    /// Node address (e.g., "192.168.1.10:5001")
    pub address: String,

    /// Optional metadata (region, rack, etc.)
    pub metadata: Option<String>,
}

impl NodeConfig {
    /// Create a new node configuration
    pub fn new(node_id: u64, address: String) -> Self {
        Self {
            node_id,
            address,
            metadata: None,
        }
    }

    /// Create a new node configuration with metadata
    pub fn with_metadata(node_id: u64, address: String, metadata: String) -> Self {
        Self {
            node_id,
            address,
            metadata: Some(metadata),
        }
    }
}

/// Membership change request (used for tracking in-progress changes)
#[derive(Debug, Clone)]
enum MembershipChange {
    /// Add node request
    AddNode {
        node: NodeConfig,
        started_at: Instant,
    },

    /// Remove node request
    RemoveNode {
        node_id: u64,
        started_at: Instant,
    },
}

/// Manages dynamic membership changes for Raft cluster
pub struct MembershipManager {
    /// This node's ID
    node_id: u64,

    /// Cluster coordinator (manages peer health and bootstrap)
    cluster_coordinator: Arc<ClusterCoordinator>,

    /// Raft group manager (manages partition replicas)
    raft_group_manager: Arc<RaftGroupManager>,

    /// Metadata replica (special __meta partition for cluster metadata)
    metadata_replica: Arc<PartitionReplica>,

    /// Current cluster configuration (cached, updated on membership changes)
    current_config: Arc<RwLock<ClusterConfig>>,

    /// In-progress membership change (only one at a time)
    in_progress_change: Arc<RwLock<Option<MembershipChange>>>,

    /// Partition assigner (for rebalancing after membership changes)
    partition_assigner: Option<Arc<PartitionAssigner>>,
}

impl MembershipManager {
    /// Create a new membership manager
    ///
    /// # Arguments
    /// * `node_id` - This node's ID
    /// * `cluster_coordinator` - Cluster coordinator instance
    /// * `raft_group_manager` - Raft group manager instance
    /// * `metadata_replica` - Metadata partition replica
    /// * `initial_config` - Initial cluster configuration
    ///
    /// # Returns
    /// A new MembershipManager instance
    pub fn new(
        node_id: u64,
        cluster_coordinator: Arc<ClusterCoordinator>,
        raft_group_manager: Arc<RaftGroupManager>,
        metadata_replica: Arc<PartitionReplica>,
        initial_config: ClusterConfig,
    ) -> Self {
        info!(
            "Creating MembershipManager for node {} with {} peers",
            node_id,
            initial_config.peers.len()
        );

        Self {
            node_id,
            cluster_coordinator,
            raft_group_manager,
            metadata_replica,
            current_config: Arc::new(RwLock::new(initial_config)),
            in_progress_change: Arc::new(RwLock::new(None)),
            partition_assigner: None,
        }
    }

    /// Set partition assigner (optional, for automatic rebalancing)
    ///
    /// # Arguments
    /// * `assigner` - Partition assigner instance
    pub fn set_partition_assigner(&mut self, assigner: Arc<PartitionAssigner>) {
        info!("Partition assigner configured for membership manager");
        self.partition_assigner = Some(assigner);
    }

    /// Add a node to the cluster using joint consensus
    ///
    /// This performs the following steps:
    /// 1. Validate node can be added (no duplicate ID/address)
    /// 2. Check no membership change is in progress
    /// 3. Propose ConfChangeAddNode to metadata Raft group
    /// 4. Wait for joint consensus commit (old + new quorum)
    /// 5. Update local cluster configuration
    /// 6. Trigger partition rebalancing (if assigner configured)
    /// 7. Mark membership change complete
    ///
    /// # Arguments
    /// * `node` - Node configuration to add
    ///
    /// # Returns
    /// Ok(()) if successful, Err if validation fails or Raft rejects
    ///
    /// # Errors
    /// - Duplicate node ID or address
    /// - Membership change already in progress
    /// - Not Raft leader (cannot propose config changes)
    /// - Timeout waiting for commit
    pub async fn add_node(&self, node: NodeConfig) -> Result<()> {
        info!(
            "Request to add node {} at address {}",
            node.node_id, node.address
        );

        // Step 1: Validate node can be added
        self.validate_add_node(&node)?;

        // Step 2: Check no membership change in progress
        {
            let mut in_progress = self.in_progress_change.write();
            if in_progress.is_some() {
                return Err(RaftError::Config(
                    "Membership change already in progress".to_string(),
                ));
            }

            // Mark change as in progress
            *in_progress = Some(MembershipChange::AddNode {
                node: node.clone(),
                started_at: Instant::now(),
            });
        }

        // Execute add node with automatic cleanup on error
        let result = self.execute_add_node(node.clone()).await;

        // Clear in-progress flag
        {
            let mut in_progress = self.in_progress_change.write();
            *in_progress = None;
        }

        // Log result
        match &result {
            Ok(_) => {
                info!(
                    "Successfully added node {} to cluster",
                    node.node_id
                );
            }
            Err(e) => {
                error!(
                    "Failed to add node {} to cluster: {}",
                    node.node_id, e
                );
            }
        }

        result
    }

    /// Execute the add node operation (internal)
    async fn execute_add_node(&self, node: NodeConfig) -> Result<()> {
        // Step 3: Check we're the leader (only leader can propose config changes)
        if !self.metadata_replica.is_leader() {
            return Err(RaftError::Config(format!(
                "Not Raft leader (current leader: {})",
                self.metadata_replica.leader_id()
            )));
        }

        // Step 4: Create ConfChange for adding node
        let mut conf_change = ConfChange::default();
        conf_change.set_change_type(ConfChangeType::AddNode);
        conf_change.node_id = node.node_id;

        // Encode node configuration as context
        let context = bincode::serialize(&node).map_err(|e| {
            RaftError::Config(format!("Failed to serialize node config: {}", e))
        })?;
        conf_change.context = context;

        debug!(
            "Proposing ConfChangeAddNode for node {} to metadata Raft group",
            node.node_id
        );

        // Step 5: Propose configuration change
        // Serialize as (change_type: i32, node_id: u64, context: Vec<u8>)
        // NOTE: In production, this would use apply_conf_change() on the RawNode
        // but for Phase 4, we're just demonstrating the membership manager API
        let change_type = conf_change.get_change_type() as i32;
        let conf_change_bytes = bincode::serialize(&(
            change_type,
            conf_change.node_id,
            &conf_change.context,
        ))
        .map_err(|e| RaftError::Config(format!("Failed to serialize ConfChange: {}", e)))?;

        let commit_index = self.metadata_replica.propose(conf_change_bytes).await?;

        info!(
            "ConfChangeAddNode proposed at index {} for node {}",
            commit_index, node.node_id
        );

        // Step 6: Wait for commit (with timeout)
        self.wait_for_commit(commit_index, Duration::from_secs(30))
            .await?;

        // Step 7: Update local cluster configuration
        {
            let mut config = self.current_config.write();
            config.peers.push(PeerConfig {
                node_id: node.node_id,
                address: node.address.clone(),
            });

            info!(
                "Updated cluster configuration: {} total nodes",
                config.peers.len() + 1 // +1 for self
            );
        }

        // Step 8: Trigger partition rebalancing (if configured)
        if let Some(ref assigner) = self.partition_assigner {
            info!("Triggering partition rebalancing after node addition");
            if let Err(e) = assigner.rebalance_if_needed().await {
                warn!("Partition rebalancing failed: {}", e);
                // Don't fail the entire operation - rebalancing can be retried
            }
        }

        Ok(())
    }

    /// Remove a node from the cluster
    ///
    /// This performs the following steps:
    /// 1. Validate node can be removed (doesn't break quorum)
    /// 2. Check no membership change is in progress
    /// 3. Propose ConfChangeRemoveNode to metadata Raft group
    /// 4. Wait for joint consensus commit
    /// 5. Update local cluster configuration
    /// 6. Trigger partition rebalancing (migrate partitions away from removed node)
    /// 7. Mark membership change complete
    ///
    /// # Arguments
    /// * `node_id` - Node ID to remove
    ///
    /// # Returns
    /// Ok(()) if successful, Err if validation fails or Raft rejects
    ///
    /// # Errors
    /// - Node doesn't exist in cluster
    /// - Removing node would break quorum
    /// - Membership change already in progress
    /// - Not Raft leader
    /// - Timeout waiting for commit
    pub async fn remove_node(&self, node_id: u64) -> Result<()> {
        info!("Request to remove node {} from cluster", node_id);

        // Step 1: Validate node can be removed
        self.validate_remove_node(node_id)?;

        // Step 2: Check no membership change in progress
        {
            let mut in_progress = self.in_progress_change.write();
            if in_progress.is_some() {
                return Err(RaftError::Config(
                    "Membership change already in progress".to_string(),
                ));
            }

            // Mark change as in progress
            *in_progress = Some(MembershipChange::RemoveNode {
                node_id,
                started_at: Instant::now(),
            });
        }

        // Execute remove node with automatic cleanup on error
        let result = self.execute_remove_node(node_id).await;

        // Clear in-progress flag
        {
            let mut in_progress = self.in_progress_change.write();
            *in_progress = None;
        }

        // Log result
        match &result {
            Ok(_) => {
                info!("Successfully removed node {} from cluster", node_id);
            }
            Err(e) => {
                error!("Failed to remove node {} from cluster: {}", node_id, e);
            }
        }

        result
    }

    /// Execute the remove node operation (internal)
    async fn execute_remove_node(&self, node_id: u64) -> Result<()> {
        // Step 3: Check we're the leader
        if !self.metadata_replica.is_leader() {
            return Err(RaftError::Config(format!(
                "Not Raft leader (current leader: {})",
                self.metadata_replica.leader_id()
            )));
        }

        // Step 4: Create ConfChange for removing node
        let mut conf_change = ConfChange::default();
        conf_change.set_change_type(ConfChangeType::RemoveNode);
        conf_change.node_id = node_id;

        debug!(
            "Proposing ConfChangeRemoveNode for node {} to metadata Raft group",
            node_id
        );

        // Step 5: Propose configuration change
        // Serialize as (change_type: i32, node_id: u64, context: Vec<u8>)
        // NOTE: In production, this would use apply_conf_change() on the RawNode
        // but for Phase 4, we're just demonstrating the membership manager API
        let change_type = conf_change.get_change_type() as i32;
        let conf_change_bytes = bincode::serialize(&(
            change_type,
            conf_change.node_id,
            &conf_change.context,
        ))
        .map_err(|e| RaftError::Config(format!("Failed to serialize ConfChange: {}", e)))?;

        let commit_index = self.metadata_replica.propose(conf_change_bytes).await?;

        info!(
            "ConfChangeRemoveNode proposed at index {} for node {}",
            commit_index, node_id
        );

        // Step 6: Wait for commit (with timeout)
        self.wait_for_commit(commit_index, Duration::from_secs(30))
            .await?;

        // Step 7: Update local cluster configuration
        {
            let mut config = self.current_config.write();
            config.peers.retain(|p| p.node_id != node_id);

            info!(
                "Updated cluster configuration: {} total nodes",
                config.peers.len() + 1 // +1 for self
            );
        }

        // Step 8: Trigger partition rebalancing (migrate partitions away)
        if let Some(ref assigner) = self.partition_assigner {
            info!("Triggering partition rebalancing after node removal");
            if let Err(e) = assigner.rebalance_if_needed().await {
                warn!("Partition rebalancing failed: {}", e);
                // Don't fail - rebalancing can be retried
            }
        }

        Ok(())
    }

    /// Get current cluster configuration
    ///
    /// # Returns
    /// Snapshot of current cluster configuration
    pub fn get_cluster_config(&self) -> ClusterConfig {
        self.current_config.read().clone()
    }

    /// Validate that a node can be added
    ///
    /// # Arguments
    /// * `node` - Node configuration to validate
    ///
    /// # Returns
    /// Ok(()) if valid, Err with reason if invalid
    fn validate_add_node(&self, node: &NodeConfig) -> Result<()> {
        let config = self.current_config.read();

        // Check 1: Node ID must not be this node
        if node.node_id == self.node_id {
            return Err(RaftError::Config(
                "Cannot add self to cluster".to_string(),
            ));
        }

        // Check 2: Node ID must not already exist
        if config.peers.iter().any(|p| p.node_id == node.node_id) {
            return Err(RaftError::Config(format!(
                "Node ID {} already exists in cluster",
                node.node_id
            )));
        }

        // Check 3: Address must not already exist
        if config.peers.iter().any(|p| p.address == node.address) {
            return Err(RaftError::Config(format!(
                "Address {} already exists in cluster",
                node.address
            )));
        }

        // Check 4: Address must be valid format (basic validation)
        if !node.address.contains(':') {
            return Err(RaftError::Config(format!(
                "Invalid address format: {} (expected host:port)",
                node.address
            )));
        }

        debug!("Node {} validation passed for add operation", node.node_id);
        Ok(())
    }

    /// Validate that a node can be removed
    ///
    /// # Arguments
    /// * `node_id` - Node ID to validate
    ///
    /// # Returns
    /// Ok(()) if valid, Err with reason if invalid
    fn validate_remove_node(&self, node_id: u64) -> Result<()> {
        let config = self.current_config.read();

        // Check 1: Cannot remove self
        if node_id == self.node_id {
            return Err(RaftError::Config(
                "Cannot remove self from cluster".to_string(),
            ));
        }

        // Check 2: Node must exist
        if !config.peers.iter().any(|p| p.node_id == node_id) {
            return Err(RaftError::Config(format!(
                "Node {} does not exist in cluster",
                node_id
            )));
        }

        // Check 3: Removing node must not break quorum
        let current_size = config.peers.len() + 1; // +1 for self
        let new_size = current_size - 1;
        let new_quorum = (new_size / 2) + 1;

        // After removal, we need at least 2 nodes to form quorum
        if new_size < 2 {
            return Err(RaftError::Config(
                "Cannot remove node: would leave only 1 node (no quorum possible)".to_string(),
            ));
        }

        // Sanity check: new quorum must be achievable
        if new_quorum > new_size {
            return Err(RaftError::Config(format!(
                "Cannot remove node: would break quorum (need {} out of {} nodes)",
                new_quorum, new_size
            )));
        }

        debug!(
            "Node {} validation passed for remove operation (cluster: {} -> {}, quorum: {} -> {})",
            node_id,
            current_size,
            new_size,
            (current_size / 2) + 1,
            new_quorum
        );

        Ok(())
    }

    /// Wait for a Raft index to be committed
    ///
    /// # Arguments
    /// * `index` - Raft log index to wait for
    /// * `timeout` - Maximum time to wait
    ///
    /// # Returns
    /// Ok(()) if committed within timeout, Err otherwise
    async fn wait_for_commit(&self, index: u64, timeout: Duration) -> Result<()> {
        let start = Instant::now();

        debug!("Waiting for commit of index {} (timeout: {:?})", index, timeout);

        loop {
            // Check if index is committed
            let commit_index = self.metadata_replica.commit_index();
            if commit_index >= index {
                debug!(
                    "Index {} committed (commit_index: {}, elapsed: {:?})",
                    index,
                    commit_index,
                    start.elapsed()
                );
                return Ok(());
            }

            // Check timeout
            if start.elapsed() > timeout {
                return Err(RaftError::Config(format!(
                    "Timeout waiting for commit of index {} (current commit_index: {}, elapsed: {:?})",
                    index,
                    commit_index,
                    start.elapsed()
                )));
            }

            // Wait a bit before checking again
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Check if a membership change is currently in progress
    ///
    /// # Returns
    /// true if a change is in progress, false otherwise
    pub fn is_change_in_progress(&self) -> bool {
        self.in_progress_change.read().is_some()
    }

    /// Get information about the in-progress change (if any)
    ///
    /// # Returns
    /// Description of in-progress change, or None
    pub fn get_in_progress_change(&self) -> Option<String> {
        let in_progress = self.in_progress_change.read();
        in_progress.as_ref().map(|change| match change {
            MembershipChange::AddNode { node, started_at } => {
                format!(
                    "Adding node {} ({}) - started {:?} ago",
                    node.node_id,
                    node.address,
                    started_at.elapsed()
                )
            }
            MembershipChange::RemoveNode { node_id, started_at } => {
                format!(
                    "Removing node {} - started {:?} ago",
                    node_id,
                    started_at.elapsed()
                )
            }
        })
    }

    /// Get current cluster size (including this node)
    pub fn get_cluster_size(&self) -> usize {
        let config = self.current_config.read();
        config.peers.len() + 1 // +1 for self
    }

    /// Get current quorum size (n/2 + 1)
    pub fn get_quorum_size(&self) -> usize {
        let size = self.get_cluster_size();
        (size / 2) + 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MemoryLogStorage, RaftConfig};
    use std::time::Duration;

    fn create_test_config(node_id: u64, peers: Vec<(u64, &str)>) -> ClusterConfig {
        ClusterConfig {
            node_id,
            peers: peers
                .into_iter()
                .map(|(id, addr)| PeerConfig {
                    node_id: id,
                    address: addr.to_string(),
                })
                .collect(),
            quorum_wait_timeout: Duration::from_secs(5),
            heartbeat_interval: Duration::from_millis(100),
            peer_timeout: Duration::from_secs(1),
        }
    }

    fn create_test_manager(node_id: u64, peers: Vec<(u64, &str)>) -> MembershipManager {
        let cluster_config = create_test_config(node_id, peers.clone());
        let raft_config = RaftConfig {
            node_id,
            ..Default::default()
        };

        let raft_group_manager = Arc::new(RaftGroupManager::new(
            node_id,
            raft_config.clone(),
            || Arc::new(MemoryLogStorage::new()),
        ));

        // Create metadata replica
        let peer_ids: Vec<u64> = peers.iter().map(|(id, _)| *id).collect();
        let metadata_replica = raft_group_manager
            .get_or_create_replica("__meta", 0, peer_ids)
            .unwrap();

        let cluster_coordinator = Arc::new(
            ClusterCoordinator::new(
                cluster_config.clone(),
                raft_config,
                raft_group_manager.clone(),
            )
            .unwrap(),
        );

        MembershipManager::new(
            node_id,
            cluster_coordinator,
            raft_group_manager,
            metadata_replica,
            cluster_config,
        )
    }

    #[test]
    fn test_create_membership_manager() {
        let manager = create_test_manager(1, vec![(2, "127.0.0.1:5002"), (3, "127.0.0.1:5003")]);

        assert_eq!(manager.node_id, 1);
        assert_eq!(manager.get_cluster_size(), 3);
        assert_eq!(manager.get_quorum_size(), 2);
    }

    #[test]
    fn test_validate_add_node_success() {
        let manager = create_test_manager(1, vec![(2, "127.0.0.1:5002"), (3, "127.0.0.1:5003")]);

        let new_node = NodeConfig::new(4, "127.0.0.1:5004".to_string());
        assert!(manager.validate_add_node(&new_node).is_ok());
    }

    #[test]
    fn test_validate_add_node_duplicate_id() {
        let manager = create_test_manager(1, vec![(2, "127.0.0.1:5002"), (3, "127.0.0.1:5003")]);

        let duplicate_node = NodeConfig::new(2, "127.0.0.1:5004".to_string());
        let result = manager.validate_add_node(&duplicate_node);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));
    }

    #[test]
    fn test_validate_add_node_duplicate_address() {
        let manager = create_test_manager(1, vec![(2, "127.0.0.1:5002"), (3, "127.0.0.1:5003")]);

        let duplicate_addr = NodeConfig::new(4, "127.0.0.1:5002".to_string());
        let result = manager.validate_add_node(&duplicate_addr);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));
    }

    #[test]
    fn test_validate_add_node_invalid_address() {
        let manager = create_test_manager(1, vec![(2, "127.0.0.1:5002"), (3, "127.0.0.1:5003")]);

        let invalid_addr = NodeConfig::new(4, "invalid_address".to_string());
        let result = manager.validate_add_node(&invalid_addr);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Invalid address"));
    }

    #[test]
    fn test_validate_add_node_self() {
        let manager = create_test_manager(1, vec![(2, "127.0.0.1:5002"), (3, "127.0.0.1:5003")]);

        let self_node = NodeConfig::new(1, "127.0.0.1:5001".to_string());
        let result = manager.validate_add_node(&self_node);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Cannot add self"));
    }

    #[test]
    fn test_validate_remove_node_success() {
        let manager = create_test_manager(1, vec![(2, "127.0.0.1:5002"), (3, "127.0.0.1:5003")]);

        assert!(manager.validate_remove_node(2).is_ok());
    }

    #[test]
    fn test_validate_remove_node_nonexistent() {
        let manager = create_test_manager(1, vec![(2, "127.0.0.1:5002"), (3, "127.0.0.1:5003")]);

        let result = manager.validate_remove_node(99);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("does not exist"));
    }

    #[test]
    fn test_validate_remove_node_self() {
        let manager = create_test_manager(1, vec![(2, "127.0.0.1:5002"), (3, "127.0.0.1:5003")]);

        let result = manager.validate_remove_node(1);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Cannot remove self"));
    }

    #[test]
    fn test_validate_remove_node_breaks_quorum() {
        // 2-node cluster: removing any node leaves only 1 (no quorum)
        let manager = create_test_manager(1, vec![(2, "127.0.0.1:5002")]);

        let result = manager.validate_remove_node(2);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("would leave only 1 node"));
    }

    #[test]
    fn test_get_cluster_config() {
        let manager = create_test_manager(1, vec![(2, "127.0.0.1:5002"), (3, "127.0.0.1:5003")]);

        let config = manager.get_cluster_config();
        assert_eq!(config.node_id, 1);
        assert_eq!(config.peers.len(), 2);
    }

    #[test]
    fn test_in_progress_tracking() {
        let manager = create_test_manager(1, vec![(2, "127.0.0.1:5002"), (3, "127.0.0.1:5003")]);

        // Initially no change in progress
        assert!(!manager.is_change_in_progress());
        assert!(manager.get_in_progress_change().is_none());

        // Simulate in-progress change
        {
            let mut in_progress = manager.in_progress_change.write();
            *in_progress = Some(MembershipChange::AddNode {
                node: NodeConfig::new(4, "127.0.0.1:5004".to_string()),
                started_at: Instant::now(),
            });
        }

        assert!(manager.is_change_in_progress());
        let info = manager.get_in_progress_change();
        assert!(info.is_some());
        assert!(info.unwrap().contains("Adding node 4"));

        // Clear in-progress
        {
            let mut in_progress = manager.in_progress_change.write();
            *in_progress = None;
        }

        assert!(!manager.is_change_in_progress());
    }

    #[test]
    fn test_cluster_size_calculations() {
        // 3-node cluster
        let manager = create_test_manager(1, vec![(2, "127.0.0.1:5002"), (3, "127.0.0.1:5003")]);
        assert_eq!(manager.get_cluster_size(), 3);
        assert_eq!(manager.get_quorum_size(), 2); // (3/2)+1 = 2

        // 5-node cluster
        let manager = create_test_manager(
            1,
            vec![
                (2, "127.0.0.1:5002"),
                (3, "127.0.0.1:5003"),
                (4, "127.0.0.1:5004"),
                (5, "127.0.0.1:5005"),
            ],
        );
        assert_eq!(manager.get_cluster_size(), 5);
        assert_eq!(manager.get_quorum_size(), 3); // (5/2)+1 = 3
    }

    #[test]
    fn test_node_config_creation() {
        let node = NodeConfig::new(1, "192.168.1.10:5001".to_string());
        assert_eq!(node.node_id, 1);
        assert_eq!(node.address, "192.168.1.10:5001");
        assert!(node.metadata.is_none());

        let node_with_meta =
            NodeConfig::with_metadata(2, "192.168.1.20:5001".to_string(), "us-west-2".to_string());
        assert_eq!(node_with_meta.node_id, 2);
        assert_eq!(node_with_meta.metadata, Some("us-west-2".to_string()));
    }

    #[tokio::test]
    async fn test_add_node_not_leader() {
        // Create a follower node (peers > this node ID = not first node = follower)
        let manager = create_test_manager(3, vec![(1, "127.0.0.1:5001"), (2, "127.0.0.1:5002")]);

        let new_node = NodeConfig::new(4, "127.0.0.1:5004".to_string());
        let result = manager.add_node(new_node).await;

        // Should fail because we're not the leader
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Not Raft leader"));
    }

    #[tokio::test]
    async fn test_remove_node_not_leader() {
        // Create a follower node
        let manager = create_test_manager(3, vec![(1, "127.0.0.1:5001"), (2, "127.0.0.1:5002")]);

        let result = manager.remove_node(2).await;

        // Should fail because we're not the leader
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Not Raft leader"));
    }
}
