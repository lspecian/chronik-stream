//! End-to-end cluster test for Phase 3
//!
//! This test verifies:
//! 1. Start 3-node cluster from scratch using ClusterCoordinator
//! 2. Create topic via Kafka protocol (any node)
//! 3. Produce to all partitions (using partition assignment)
//! 4. Consume from all partitions (with follower reads)
//! 5. Kill one node, verify cluster continues
//! 6. Restart killed node, verify it rejoins

use anyhow::Result;
use chronik_common::metadata::{MetadataStore, TopicConfig};
use chronik_config::cluster::{ClusterConfig, NodeConfig};
use chronik_protocol::{ApiKey, RequestHeader, ResponseHeader};
use chronik_raft::cluster_coordinator::ClusterCoordinator;
use chronik_raft::group_manager::RaftGroupManager;
use chronik_raft::partition_assigner::PartitionAssigner;
use chronik_raft::raft_meta_log::RaftMetaLog;
use chronik_raft::storage::RaftLogStorage;
use chronik_raft::RaftConfig;
use chronik_server::IntegratedKafkaServer;
use chronik_wal::group_commit::GroupCommitWal;
use chronik_wal::WalConfig;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio::net::TcpStream;
use tokio::time::{sleep, timeout};

/// Test node wrapper
struct TestNode {
    node_id: u64,
    data_dir: TempDir,
    kafka_server: Arc<IntegratedKafkaServer>,
    cluster_coordinator: Arc<ClusterCoordinator>,
    kafka_addr: SocketAddr,
    raft_addr: SocketAddr,
}

impl TestNode {
    async fn new(
        node_id: u64,
        cluster_config: ClusterConfig,
        raft_config: RaftConfig,
    ) -> Result<Self> {
        let data_dir = TempDir::new()?;

        // Create WAL for metadata
        let meta_wal_path = data_dir.path().join("meta_wal");
        std::fs::create_dir_all(&meta_wal_path)?;
        let meta_wal_config = WalConfig::default();
        let meta_wal = Arc::new(GroupCommitWal::new(meta_wal_path, meta_wal_config).await?);

        // Create RaftGroupManager
        let raft_manager = Arc::new(RaftGroupManager::new(
            node_id,
            raft_config.clone(),
            {
                let data_dir = data_dir.path().to_path_buf();
                Arc::new(move || -> Arc<dyn RaftLogStorage> {
                    let wal_path = data_dir.join(format!("raft_{}", uuid::Uuid::new_v4()));
                    std::fs::create_dir_all(&wal_path).unwrap();
                    let wal_config = WalConfig::default();
                    let wal = GroupCommitWal::new_blocking(wal_path, wal_config).unwrap();
                    Arc::new(wal)
                })
            },
        )?);

        // Start tick loop
        raft_manager.spawn_tick_loop()?;

        // Create metadata Raft replica
        let metadata_replica = raft_manager
            .get_or_create_replica(
                "__meta".to_string(),
                0,
                cluster_config.peers.iter().map(|p| p.id).collect(),
            )
            .await?;

        // Create RaftMetaLog
        let raft_meta_log = Arc::new(RaftMetaLog::new(node_id, metadata_replica)?);

        // Create ClusterCoordinator
        let cluster_coordinator = Arc::new(ClusterCoordinator::new(
            node_id,
            cluster_config.clone(),
            raft_manager.clone(),
            raft_meta_log.clone(),
        )?);

        // Create PartitionAssigner
        let partition_assigner = Arc::new(PartitionAssigner::new(
            cluster_config.clone(),
            raft_manager.clone(),
            raft_meta_log.clone(),
        )?);

        // Create Kafka server
        let kafka_addr: SocketAddr = cluster_config
            .peers
            .iter()
            .find(|p| p.id == node_id)
            .unwrap()
            .addr
            .parse()?;

        let kafka_server = IntegratedKafkaServer::new_with_raft(
            kafka_addr,
            data_dir.path().to_path_buf(),
            Some(raft_manager.clone()),
            raft_meta_log.clone(),
        )
        .await?;

        let raft_addr: SocketAddr = format!(
            "{}:{}",
            kafka_addr.ip(),
            cluster_config
                .peers
                .iter()
                .find(|p| p.id == node_id)
                .unwrap()
                .raft_port
        )
        .parse()?;

        Ok(Self {
            node_id,
            data_dir,
            kafka_server: Arc::new(kafka_server),
            cluster_coordinator,
            kafka_addr,
            raft_addr,
        })
    }

    async fn start(&self) -> Result<()> {
        // Spawn Kafka server
        let kafka_server = self.kafka_server.clone();
        let kafka_addr = self.kafka_addr;
        tokio::spawn(async move {
            kafka_server.run(kafka_addr).await.unwrap();
        });

        // Bootstrap cluster
        self.cluster_coordinator.bootstrap().await?;

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.kafka_server.shutdown().await?;
        Ok(())
    }

    async fn is_healthy(&self) -> bool {
        timeout(Duration::from_secs(1), TcpStream::connect(self.kafka_addr))
            .await
            .is_ok()
    }
}

/// Test cluster wrapper
struct TestCluster {
    nodes: HashMap<u64, TestNode>,
    cluster_config: ClusterConfig,
}

impl TestCluster {
    async fn new(num_nodes: usize, replication_factor: usize) -> Result<Self> {
        let mut peers = Vec::new();
        let base_kafka_port = 19092;
        let base_raft_port = 19093;

        for i in 0..num_nodes {
            peers.push(NodeConfig {
                id: (i + 1) as u64,
                addr: format!("127.0.0.1:{}", base_kafka_port + i),
                raft_port: (base_raft_port + i) as u16,
            });
        }

        let cluster_config = ClusterConfig {
            enabled: true,
            node_id: 0, // Will be set per node
            replication_factor,
            min_insync_replicas: (replication_factor / 2) + 1,
            peers: peers.clone(),
        };

        let raft_config = RaftConfig::default();

        let mut nodes = HashMap::new();
        for peer in &peers {
            let mut node_cluster_config = cluster_config.clone();
            node_cluster_config.node_id = peer.id;

            let node = TestNode::new(peer.id, node_cluster_config, raft_config.clone()).await?;
            nodes.insert(peer.id, node);
        }

        Ok(Self {
            nodes,
            cluster_config,
        })
    }

    async fn start_all(&mut self) -> Result<()> {
        for node in self.nodes.values() {
            node.start().await?;
        }

        // Wait for cluster to stabilize
        sleep(Duration::from_secs(2)).await;

        Ok(())
    }

    async fn stop_node(&mut self, node_id: u64) -> Result<()> {
        if let Some(node) = self.nodes.get(&node_id) {
            node.stop().await?;
        }
        Ok(())
    }

    async fn restart_node(&mut self, node_id: u64) -> Result<()> {
        if let Some(node) = self.nodes.get(&node_id) {
            node.start().await?;
            sleep(Duration::from_secs(1)).await;
        }
        Ok(())
    }

    fn get_kafka_addr(&self, node_id: u64) -> Option<SocketAddr> {
        self.nodes.get(&node_id).map(|n| n.kafka_addr)
    }

    async fn wait_for_leader(&self, topic: &str, partition: i32, timeout_secs: u64) -> Result<u64> {
        let start = std::time::Instant::now();
        loop {
            for node in self.nodes.values() {
                if node.cluster_coordinator.is_leader_for_partition(topic, partition)? {
                    return Ok(node.node_id);
                }
            }

            if start.elapsed().as_secs() > timeout_secs {
                anyhow::bail!("Timeout waiting for leader for {}/{}", topic, partition);
            }

            sleep(Duration::from_millis(100)).await;
        }
    }
}

#[tokio::test]
async fn test_cluster_bootstrap_and_metadata_replication() -> Result<()> {
    let mut cluster = TestCluster::new(3, 3).await?;
    cluster.start_all().await?;

    // Wait for metadata Raft group to elect leader
    let leader_id = cluster.wait_for_leader("__meta", 0, 10).await?;
    println!("Metadata leader: node {}", leader_id);

    // Create topic on any node (should replicate via Raft)
    let any_node = cluster.nodes.values().next().unwrap();
    let topic_config = TopicConfig {
        num_partitions: 3,
        replication_factor: 3,
        ..Default::default()
    };

    any_node
        .cluster_coordinator
        .metadata_store()
        .create_topic("test-topic", topic_config)
        .await?;

    // Verify topic exists on all nodes
    sleep(Duration::from_secs(1)).await;

    for node in cluster.nodes.values() {
        let topic = node
            .cluster_coordinator
            .metadata_store()
            .get_topic("test-topic")
            .await?;
        assert!(topic.is_some(), "Topic not found on node {}", node.node_id);
        assert_eq!(topic.unwrap().num_partitions, 3);
    }

    println!("✅ Topic replicated to all nodes");

    Ok(())
}

#[tokio::test]
async fn test_partition_assignment_and_leadership() -> Result<()> {
    let mut cluster = TestCluster::new(3, 3).await?;
    cluster.start_all().await?;

    // Wait for metadata leader
    cluster.wait_for_leader("__meta", 0, 10).await?;

    // Create topic with 6 partitions
    let any_node = cluster.nodes.values().next().unwrap();
    let topic_config = TopicConfig {
        num_partitions: 6,
        replication_factor: 3,
        ..Default::default()
    };

    any_node
        .cluster_coordinator
        .metadata_store()
        .create_topic("multi-partition-topic", topic_config)
        .await?;

    sleep(Duration::from_secs(2)).await;

    // Verify each partition has a leader
    let mut leaders_by_node = HashMap::new();
    for partition in 0..6 {
        let leader_id = cluster
            .wait_for_leader("multi-partition-topic", partition, 10)
            .await?;
        *leaders_by_node.entry(leader_id).or_insert(0) += 1;
        println!(
            "Partition {} leader: node {}",
            partition, leader_id
        );
    }

    // Verify leadership is distributed (round-robin should give each node 2 partitions)
    assert_eq!(leaders_by_node.len(), 3, "Leaders should be distributed");
    for (node_id, count) in &leaders_by_node {
        assert_eq!(*count, 2, "Node {} should lead 2 partitions", node_id);
    }

    println!("✅ Partition leadership distributed evenly");

    Ok(())
}

#[tokio::test]
async fn test_produce_and_consume_with_replication() -> Result<()> {
    let mut cluster = TestCluster::new(3, 3).await?;
    cluster.start_all().await?;

    // Wait for metadata leader
    cluster.wait_for_leader("__meta", 0, 10).await?;

    // Create topic
    let any_node = cluster.nodes.values().next().unwrap();
    let topic_config = TopicConfig {
        num_partitions: 3,
        replication_factor: 3,
        ..Default::default()
    };

    any_node
        .cluster_coordinator
        .metadata_store()
        .create_topic("replicated-topic", topic_config)
        .await?;

    sleep(Duration::from_secs(2)).await;

    // Produce to partition 0 (via leader)
    let leader_id = cluster.wait_for_leader("replicated-topic", 0, 10).await?;
    let leader_node = cluster.nodes.get(&leader_id).unwrap();

    // TODO: Use actual Kafka protocol to produce
    // For now, just verify leader is ready
    assert!(
        leader_node
            .cluster_coordinator
            .is_leader_for_partition("replicated-topic", 0)?,
        "Node {} should be leader for partition 0",
        leader_id
    );

    println!("✅ Leader ready for produce requests");

    // TODO: Verify follower reads work
    // This requires implementing follower read logic in FetchHandler

    Ok(())
}

#[tokio::test]
async fn test_node_failure_and_recovery() -> Result<()> {
    let mut cluster = TestCluster::new(3, 3).await?;
    cluster.start_all().await?;

    // Wait for metadata leader
    let original_leader_id = cluster.wait_for_leader("__meta", 0, 10).await?;
    println!("Original metadata leader: node {}", original_leader_id);

    // Create topic
    let any_node = cluster.nodes.values().next().unwrap();
    let topic_config = TopicConfig {
        num_partitions: 3,
        replication_factor: 3,
        ..Default::default()
    };

    any_node
        .cluster_coordinator
        .metadata_store()
        .create_topic("fault-tolerant-topic", topic_config)
        .await?;

    sleep(Duration::from_secs(2)).await;

    // Kill the leader node
    println!("Stopping node {}", original_leader_id);
    cluster.stop_node(original_leader_id).await?;

    sleep(Duration::from_secs(3)).await;

    // Verify new leader elected
    let new_leader_id = cluster.wait_for_leader("__meta", 0, 10).await?;
    assert_ne!(
        new_leader_id, original_leader_id,
        "New leader should be elected"
    );
    println!("New metadata leader: node {}", new_leader_id);

    // Verify cluster still works (can create topic)
    let topic_config2 = TopicConfig {
        num_partitions: 2,
        replication_factor: 2, // Only 2 nodes alive
        ..Default::default()
    };

    let working_node = cluster.nodes.get(&new_leader_id).unwrap();
    working_node
        .cluster_coordinator
        .metadata_store()
        .create_topic("after-failure-topic", topic_config2)
        .await?;

    sleep(Duration::from_secs(1)).await;

    // Verify topic exists on remaining nodes
    for node in cluster.nodes.values() {
        if node.node_id == original_leader_id {
            continue; // Skip stopped node
        }

        let topic = node
            .cluster_coordinator
            .metadata_store()
            .get_topic("after-failure-topic")
            .await?;
        assert!(
            topic.is_some(),
            "Topic not found on node {} after failure",
            node.node_id
        );
    }

    println!("✅ Cluster survived node failure");

    // Restart failed node
    println!("Restarting node {}", original_leader_id);
    cluster.restart_node(original_leader_id).await?;

    sleep(Duration::from_secs(3)).await;

    // Verify restarted node rejoins and syncs metadata
    let restarted_node = cluster.nodes.get(&original_leader_id).unwrap();
    let topic = restarted_node
        .cluster_coordinator
        .metadata_store()
        .get_topic("after-failure-topic")
        .await?;
    assert!(
        topic.is_some(),
        "Restarted node should have synced metadata"
    );

    println!("✅ Restarted node rejoined cluster and synced");

    Ok(())
}

#[tokio::test]
async fn test_split_brain_prevention() -> Result<()> {
    let mut cluster = TestCluster::new(3, 3).await?;
    cluster.start_all().await?;

    // Wait for metadata leader
    let leader_id = cluster.wait_for_leader("__meta", 0, 10).await?;

    // Kill 2 nodes (no quorum)
    let nodes_to_kill: Vec<u64> = cluster
        .nodes
        .keys()
        .filter(|&&id| id != leader_id)
        .take(2)
        .copied()
        .collect();

    for node_id in &nodes_to_kill {
        cluster.stop_node(*node_id).await?;
    }

    sleep(Duration::from_secs(2)).await;

    // Verify remaining node cannot accept writes (no quorum)
    let remaining_node = cluster.nodes.get(&leader_id).unwrap();
    let topic_config = TopicConfig {
        num_partitions: 1,
        replication_factor: 1,
        ..Default::default()
    };

    let result = timeout(
        Duration::from_secs(2),
        remaining_node
            .cluster_coordinator
            .metadata_store()
            .create_topic("no-quorum-topic", topic_config),
    )
    .await;

    assert!(
        result.is_err() || result.unwrap().is_err(),
        "Should not accept writes without quorum"
    );

    println!("✅ Split-brain prevented (no writes without quorum)");

    Ok(())
}

#[tokio::test]
async fn test_concurrent_metadata_operations() -> Result<()> {
    let mut cluster = TestCluster::new(3, 3).await?;
    cluster.start_all().await?;

    // Wait for metadata leader
    cluster.wait_for_leader("__meta", 0, 10).await?;

    // Spawn 10 concurrent topic creations from different nodes
    let mut handles = Vec::new();
    for i in 0..10 {
        let node = cluster.nodes.values().nth(i % 3).unwrap();
        let metadata_store = node.cluster_coordinator.metadata_store();

        let handle = tokio::spawn(async move {
            let topic_config = TopicConfig {
                num_partitions: 3,
                replication_factor: 3,
                ..Default::default()
            };

            metadata_store
                .create_topic(&format!("concurrent-topic-{}", i), topic_config)
                .await
        });

        handles.push(handle);
    }

    // Wait for all to complete
    let results = futures::future::join_all(handles).await;

    // All should succeed (Raft serializes conflicting writes)
    for (i, result) in results.iter().enumerate() {
        assert!(
            result.is_ok() && result.as_ref().unwrap().is_ok(),
            "Topic creation {} failed",
            i
        );
    }

    sleep(Duration::from_secs(2)).await;

    // Verify all 10 topics exist on all nodes
    for node in cluster.nodes.values() {
        for i in 0..10 {
            let topic = node
                .cluster_coordinator
                .metadata_store()
                .get_topic(&format!("concurrent-topic-{}", i))
                .await?;
            assert!(
                topic.is_some(),
                "Topic {} not found on node {}",
                i,
                node.node_id
            );
        }
    }

    println!("✅ Concurrent metadata operations succeeded");

    Ok(())
}
