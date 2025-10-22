//! Integration tests for Raft cluster bootstrap
//!
//! Tests the ClusterCoordinator bootstrap process with simulated nodes.

use anyhow::Result;
use chronik_raft::{
    ClusterConfig, ClusterCoordinator, PeerConfig, RaftConfig, RaftGroupManager,
    MemoryLogStorage,
};
use std::sync::Arc;
use std::time::Duration;

/// Create a test Raft configuration
fn create_raft_config(node_id: u64) -> RaftConfig {
    RaftConfig {
        node_id,
        listen_addr: format!("127.0.0.1:{}", 5000 + node_id),
        election_timeout_ms: 300,
        heartbeat_interval_ms: 30,
        max_entries_per_batch: 100,
        snapshot_threshold: 10_000,
    }
}

/// Create a test cluster configuration
fn create_cluster_config(node_id: u64, peer_count: usize) -> ClusterConfig {
    let mut peers = Vec::new();

    // Add all peers (excluding self)
    for i in 1..=peer_count {
        let peer_id = i as u64;
        if peer_id != node_id {
            peers.push(PeerConfig {
                node_id: peer_id,
                address: format!("127.0.0.1:{}", 5000 + peer_id),
            });
        }
    }

    ClusterConfig {
        node_id,
        peers,
        quorum_wait_timeout: Duration::from_secs(2),
        heartbeat_interval: Duration::from_millis(100),
        peer_timeout: Duration::from_secs(1),
    }
}

#[tokio::test]
async fn test_single_node_bootstrap() -> Result<()> {
    // Single node cluster (no peers) - should be REJECTED by design
    // Raft requires 3+ nodes for quorum-based replication
    let cluster_config = ClusterConfig {
        node_id: 1,
        peers: vec![],
        quorum_wait_timeout: Duration::from_secs(5),
        heartbeat_interval: Duration::from_millis(100),
        peer_timeout: Duration::from_secs(1),
    };

    let raft_config = create_raft_config(1);
    let manager = Arc::new(RaftGroupManager::new(
        1,
        raft_config.clone(),
        || Arc::new(MemoryLogStorage::new()),
    ));

    // Creating ClusterCoordinator should FAIL for single-node clusters
    // because RaftGroupManager rejects single-node Raft
    let result = ClusterCoordinator::new(
        cluster_config,
        raft_config,
        manager.clone(),
    );

    // Expect error containing "Single-node Raft cluster rejected"
    assert!(result.is_err(), "Single-node Raft should be rejected");
    if let Err(err) = result {
        let err_msg = err.to_string();
        assert!(
            err_msg.contains("Single-node Raft cluster rejected") ||
            err_msg.contains("3+ nodes"),
            "Expected single-node rejection error, got: {}",
            err_msg
        );
    }

    Ok(())
}

#[tokio::test]
async fn test_quorum_calculation() -> Result<()> {
    // 3-node cluster
    let cluster_config = create_cluster_config(1, 3);
    let raft_config = create_raft_config(1);
    let manager = Arc::new(RaftGroupManager::new(
        1,
        raft_config.clone(),
        || Arc::new(MemoryLogStorage::new()),
    ));

    let coordinator = ClusterCoordinator::new(
        cluster_config,
        raft_config,
        manager,
    )?;

    assert_eq!(coordinator.get_cluster_size(), 3);
    assert_eq!(coordinator.get_quorum_size(), 2); // (3/2)+1 = 2

    Ok(())
}

#[tokio::test]
async fn test_metadata_partition_created() -> Result<()> {
    let cluster_config = create_cluster_config(1, 3);
    let raft_config = create_raft_config(1);
    let manager = Arc::new(RaftGroupManager::new(
        1,
        raft_config.clone(),
        || Arc::new(MemoryLogStorage::new()),
    ));

    let coordinator = ClusterCoordinator::new(
        cluster_config,
        raft_config,
        manager.clone(),
    )?;

    // Verify metadata partition was created
    assert!(manager.has_replica("__meta", 0));

    // Verify it's the special metadata partition
    let replica = coordinator.metadata_replica();
    assert_eq!(replica.topic(), "__meta");
    assert_eq!(replica.partition(), 0);

    Ok(())
}

#[tokio::test]
async fn test_peer_health_tracking() -> Result<()> {
    let cluster_config = create_cluster_config(1, 3);
    let raft_config = create_raft_config(1);
    let manager = Arc::new(RaftGroupManager::new(
        1,
        raft_config.clone(),
        || Arc::new(MemoryLogStorage::new()),
    ));

    let coordinator = ClusterCoordinator::new(
        cluster_config,
        raft_config,
        manager,
    )?;

    // Initially no peers are alive (haven't done health checks)
    let live_peers = coordinator.get_live_peers();
    assert_eq!(live_peers.len(), 0);

    // All peers should be tracked
    assert_eq!(coordinator.get_cluster_size(), 3);

    Ok(())
}

#[tokio::test]
async fn test_shutdown_graceful() -> Result<()> {
    let cluster_config = create_cluster_config(1, 3);
    let raft_config = create_raft_config(1);
    let manager = Arc::new(RaftGroupManager::new(
        1,
        raft_config.clone(),
        || Arc::new(MemoryLogStorage::new()),
    ));

    let coordinator = ClusterCoordinator::new(
        cluster_config,
        raft_config,
        manager,
    )?;

    // Shutdown should complete without errors
    coordinator.shutdown().await;

    Ok(())
}

#[tokio::test]
async fn test_bootstrap_timeout_without_peers() -> Result<()> {
    // 3-node cluster but no peers running
    let cluster_config = ClusterConfig {
        node_id: 1,
        peers: vec![
            PeerConfig {
                node_id: 2,
                address: "127.0.0.1:15002".to_string(), // Unreachable
            },
            PeerConfig {
                node_id: 3,
                address: "127.0.0.1:15003".to_string(), // Unreachable
            },
        ],
        quorum_wait_timeout: Duration::from_secs(2),
        heartbeat_interval: Duration::from_millis(100),
        peer_timeout: Duration::from_secs(1),
    };

    let raft_config = create_raft_config(1);
    let manager = Arc::new(RaftGroupManager::new(
        1,
        raft_config.clone(),
        || Arc::new(MemoryLogStorage::new()),
    ));

    let coordinator = ClusterCoordinator::new(
        cluster_config,
        raft_config,
        manager,
    )?;

    // Bootstrap should timeout (can't reach quorum of 2 without peers)
    let result = coordinator.bootstrap().await;
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("Quorum timeout"));

    // Cleanup
    coordinator.shutdown().await;

    Ok(())
}
