//! RaftCluster wrapper (v2.5.0 Phase 2)
//!
//! This module wraps raft::RawNode with our MetadataStateMachine to provide
//! cluster metadata coordination (NOT data replication).
//!
//! Managed by this cluster:
//! - Cluster membership (which nodes are alive)
//! - Partition assignments (partition-0 → [node1, node2, node3])
//! - Partition leaders (partition-0 leader = node1)
//! - ISR tracking (in-sync replicas per partition)
//!
//! Usage:
//! ```rust
//! // Create Raft cluster
//! let cluster = RaftCluster::bootstrap(node_id, peers).await?;
//!
//! // Query partition metadata
//! let replicas = cluster.get_partition_replicas("orders", 0)?;
//! let leader = cluster.get_partition_leader("orders", 0)?;
//! ```

use crate::raft_metadata::{MetadataCommand, MetadataStateMachine, PartitionKey};
use anyhow::{Result, Context};
use std::sync::{Arc, RwLock};

use raft::{prelude::*, storage::MemStorage};

/// Raft cluster for metadata coordination
pub struct RaftCluster {
    /// Node ID in the cluster
    node_id: u64,

    /// Metadata state machine (shared with Raft)
    state_machine: Arc<RwLock<MetadataStateMachine>>,

    /// Raft node (only initialized if 'raft' feature is enabled)
    raft_node: Arc<RwLock<RawNode<MemStorage>>>,
}

impl RaftCluster {
    /// Bootstrap a new Raft cluster
    ///
    /// # Arguments
    /// - `node_id`: This node's ID in the cluster
    /// - `peers`: List of (node_id, address) for other nodes
    ///
    /// # Returns
    /// RaftCluster ready for metadata operations
    pub async fn bootstrap(node_id: u64, peers: Vec<(u64, String)>) -> Result<Self> {
        tracing::info!(
            "Bootstrapping Raft cluster: node_id={}, peers={:?}",
            node_id,
            peers
        );

        // Create Raft configuration
        let config = Config {
            id: node_id,
            election_tick: 10,
            heartbeat_tick: 3,
            max_size_per_msg: 1024 * 1024,
            max_inflight_msgs: 256,
            ..Default::default()
        };

        // Create storage
        let storage = MemStorage::new();

        // Create Raft node
        let raft_node = RawNode::new(&config, storage, &raft::default_logger())
            .context("Failed to create Raft node")?;

        // Create state machine
        let state_machine = Arc::new(RwLock::new(MetadataStateMachine::new()));

        tracing::info!("Raft cluster initialized successfully");

        Ok(Self {
            node_id,
            state_machine,
            raft_node: Arc::new(RwLock::new(raft_node)),
        })
    }

    /// Get partition replicas (nodes that should replicate this partition)
    pub fn get_partition_replicas(&self, topic: &str, partition: i32) -> Option<Vec<u64>> {
        let sm = self.state_machine.read().ok()?;
        sm.get_partition_replicas(topic, partition)
    }

    /// Get partition leader (node ID that's the leader for this partition)
    pub fn get_partition_leader(&self, topic: &str, partition: i32) -> Option<u64> {
        let sm = self.state_machine.read().ok()?;
        sm.get_partition_leader(topic, partition)
    }

    /// Get ISR (in-sync replicas) for a partition
    pub fn get_isr(&self, topic: &str, partition: i32) -> Option<Vec<u64>> {
        let sm = self.state_machine.read().ok()?;
        sm.get_isr(topic, partition)
    }

    /// Check if a node is in-sync for a partition
    pub fn is_in_sync(&self, topic: &str, partition: i32, node_id: u64) -> bool {
        self.state_machine
            .read()
            .ok()
            .map(|sm| sm.is_in_sync(topic, partition, node_id))
            .unwrap_or(false)
    }

    /// Propose a metadata command to the Raft cluster
    ///
    /// This serializes the command and proposes it via Raft consensus.
    /// Once committed, it will be applied to the state machine.
    pub async fn propose(&self, cmd: MetadataCommand) -> Result<()> {
        // Serialize command
        let data = bincode::serialize(&cmd)
            .context("Failed to serialize metadata command")?;

        // Propose to Raft
        let mut raft = self.raft_node.write()
            .map_err(|e| anyhow::anyhow!("Failed to acquire Raft lock: {}", e))?;

        raft.propose(vec![], data)
            .context("Failed to propose to Raft")?;

        Ok(())
    }

    /// Apply committed Raft entries to the state machine
    ///
    /// This should be called by the Raft message processing loop when
    /// entries are committed.
    pub fn apply_committed_entries(&self, entries: &[Entry]) -> Result<()> {
        let mut sm = self.state_machine.write()
            .map_err(|e| anyhow::anyhow!("Failed to acquire state machine lock: {}", e))?;

        for entry in entries {
            // Skip empty entries (configuration changes)
            if entry.data.is_empty() {
                continue;
            }

            // Deserialize command
            let cmd: MetadataCommand = bincode::deserialize(&entry.data)
                .context("Failed to deserialize metadata command")?;

            // Apply to state machine
            sm.apply(cmd)?;
        }

        Ok(())
    }

    /// Get this node's ID
    pub fn node_id(&self) -> u64 {
        self.node_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_raft_cluster_bootstrap() {
        // Bootstrap a 3-node cluster
        let peers = vec![
            (2, "localhost:9093".to_string()),
            (3, "localhost:9094".to_string()),
        ];

        let cluster = RaftCluster::bootstrap(1, peers).await.unwrap();

        assert_eq!(cluster.node_id(), 1);
    }

    #[tokio::test]
    async fn test_metadata_queries() {
        let cluster = RaftCluster::bootstrap(1, vec![]).await.unwrap();

        // Initially no partition assignments
        assert_eq!(cluster.get_partition_replicas("test", 0), None);
        assert_eq!(cluster.get_partition_leader("test", 0), None);

        // Propose partition assignment
        cluster.propose(MetadataCommand::AssignPartition {
            topic: "test".to_string(),
            partition: 0,
            replicas: vec![1, 2, 3],
        }).await.unwrap();

        // Note: In real usage, this would require Raft consensus and applying
        // committed entries. For this test, we just verify the API works.
    }

    #[tokio::test]
    #[cfg(not(feature = "raft"))]
    async fn test_raft_disabled() {
        let result = RaftCluster::bootstrap(1, vec![]).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Raft feature is not enabled"));
    }
}

/// Configuration for running a Raft cluster node
pub struct RaftClusterConfig {
    pub node_id: u64,
    pub raft_addr: String,
    pub peers: Vec<(u64, String)>,
    pub bootstrap: bool,
    pub kafka_port: u16,
    pub advertised_addr: String,
    pub data_dir: String,
}

/// Run a Raft cluster node (CLI entry point)
///
/// This function bootstraps a Raft cluster and starts the integrated Kafka server
/// with Raft metadata coordination.
pub async fn run_raft_cluster(config: RaftClusterConfig) -> Result<()> {
    use crate::integrated_server::{IntegratedKafkaServer, IntegratedServerConfig};
    use std::net::SocketAddr;
    
    tracing::info!(
        "Starting Raft cluster node: id={}, kafka_port={}, raft_addr={}",
        config.node_id,
        config.kafka_port,
        config.raft_addr
    );

    // Step 1: Bootstrap RaftCluster for metadata coordination
    let raft_cluster = Arc::new(RaftCluster::bootstrap(config.node_id, config.peers).await?);
    
    tracing::info!("Raft cluster bootstrapped successfully");

    // Step 2: Create IntegratedKafkaServer configuration
    let server_config = IntegratedServerConfig {
        node_id: config.node_id as i32,
        advertised_host: config.advertised_addr.clone(),
        advertised_port: config.kafka_port as i32,
        data_dir: config.data_dir,
        enable_indexing: false,
        enable_compression: true,
        auto_create_topics: true,
        num_partitions: 1,
        replication_factor: 1,
        use_wal_metadata: true,
        enable_wal_indexing: false,
        wal_indexing_interval_secs: 300,
        object_store_config: None,
        enable_metadata_dr: false,
        metadata_upload_interval_secs: 60,
        cluster_config: None,  // TODO(Phase 3): Wire RaftCluster to cluster_config
    };

    // Step 3: Create and start Kafka server with Raft cluster
    let cluster_for_bg = Arc::clone(&raft_cluster);  // Clone for background task
    let server = IntegratedKafkaServer::new(server_config, Some(raft_cluster)).await?;

    // Bind Kafka server
    let kafka_addr = format!("0.0.0.0:{}", config.kafka_port);

    tracing::info!("Starting Kafka server on {}", kafka_addr);

    // Step 4: Start Raft background task (heartbeats, leader election)
    let cluster_clone = cluster_for_bg;
    tokio::spawn(async move {
        // TODO: Implement Raft tick loop
        loop {
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            // cluster_clone.tick().await;
        }
    });

    // Step 5: Run Kafka server (blocks until shutdown)
    server.run(&kafka_addr).await?;

    Ok(())
}
