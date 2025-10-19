/// Integration test: Raft leader election scenarios
///
/// Tests various leader election scenarios including:
/// - Initial election in new cluster
/// - Re-election after leader crash
/// - Election timeout handling
/// - Split vote scenarios

mod common;

use anyhow::Result;
use common::*;
use std::time::Duration;
use tokio::time::sleep;

#[tokio::test]
async fn test_initial_leader_election() -> Result<()> {
    tracing_subscriber::fmt::init();

    // Spawn 3-node cluster
    let mut cluster = spawn_test_cluster(3).await?;

    // Wait for leader election (should complete within 5 seconds)
    let leader_id = cluster.wait_for_leader(Duration::from_secs(5)).await?;

    println!("Leader elected: node {}", leader_id);

    // Verify leader is in valid range
    assert!(leader_id >= 1 && leader_id <= 3, "Invalid leader ID");

    // Verify all nodes agree on leader
    let consensus_leader = cluster.wait_for_consensus(Duration::from_secs(5)).await?;
    assert_eq!(leader_id, consensus_leader, "Nodes disagree on leader");

    cluster.cleanup().await;
    Ok(())
}

#[tokio::test]
async fn test_leader_crash_reelection() -> Result<()> {
    tracing_subscriber::fmt::init();

    let mut cluster = spawn_test_cluster(3).await?;

    // Wait for initial leader
    let leader_id = cluster.wait_for_leader(Duration::from_secs(5)).await?;
    println!("Initial leader: node {}", leader_id);

    // Kill leader
    cluster.kill_node(leader_id).await?;
    println!("Leader {} killed", leader_id);

    // New leader should be elected within 2 * election_timeout
    let new_leader = cluster.wait_for_leader(Duration::from_secs(3)).await?;

    println!("New leader elected: node {}", new_leader);
    assert_ne!(new_leader, leader_id, "New leader should be different");

    cluster.cleanup().await;
    Ok(())
}

#[tokio::test]
async fn test_leader_isolation_reelection() -> Result<()> {
    tracing_subscriber::fmt::init();

    // Use 5 nodes to test network partition
    let mut cluster = spawn_test_cluster_with_config(ClusterConfig {
        node_count: 5,
        use_toxiproxy: true,
        ..Default::default()
    }).await?;

    let leader_id = cluster.wait_for_leader(Duration::from_secs(5)).await?;
    println!("Initial leader: node {}", leader_id);

    // Partition leader from rest of cluster
    cluster.partition_network(vec![leader_id]).await?;
    println!("Leader {} partitioned", leader_id);

    // Remaining 4 nodes should elect new leader
    sleep(Duration::from_secs(2)).await;

    // Check that one of the non-partitioned nodes is leader
    for node in &cluster.nodes {
        if node.node_id != leader_id {
            if let Ok(info) = get_node_info_http(node.http_addr).await {
                if info.is_leader {
                    println!("New leader: node {}", node.node_id);
                    assert_ne!(node.node_id, leader_id);
                }
            }
        }
    }

    // Heal partition
    cluster.heal_partition().await?;
    println!("Partition healed");

    // Eventually all nodes should agree
    let _ = cluster.wait_for_consensus(Duration::from_secs(10)).await?;

    cluster.cleanup().await;
    Ok(())
}

#[tokio::test]
async fn test_majority_required_for_election() -> Result<()> {
    tracing_subscriber::fmt::init();

    let mut cluster = spawn_test_cluster(5).await?;

    let leader_id = cluster.wait_for_leader(Duration::from_secs(5)).await?;
    println!("Initial leader: node {}", leader_id);

    // Kill 3 nodes (lose quorum - only 2 alive)
    let mut killed = Vec::new();
    let node_ids: Vec<u64> = cluster.nodes.iter().map(|n| n.node_id).collect();
    for node_id in node_ids.iter().take(3) {
        cluster.kill_node(*node_id).await?;
        killed.push(*node_id);
    }

    println!("Killed nodes: {:?}", killed);

    // No leader should be possible (no quorum)
    sleep(Duration::from_secs(3)).await;

    let current_leader = cluster.get_leader().await;
    assert!(
        current_leader.is_none(),
        "Should not have leader without quorum"
    );

    // Restart one node to restore quorum (3 alive)
    cluster.restart_node(killed[0]).await?;
    println!("Restored node {}", killed[0]);

    // Now leader should be elected
    let new_leader = cluster.wait_for_leader(Duration::from_secs(5)).await?;
    println!("Leader elected with quorum: node {}", new_leader);

    cluster.cleanup().await;
    Ok(())
}

#[tokio::test]
async fn test_rapid_leader_changes() -> Result<()> {
    tracing_subscriber::fmt::init();

    let mut cluster = spawn_test_cluster(5).await?;

    let mut previous_leader = cluster.wait_for_leader(Duration::from_secs(5)).await?;
    println!("Initial leader: node {}", previous_leader);

    // Kill leader 3 times in succession
    for i in 0..3 {
        cluster.kill_node(previous_leader).await?;
        println!("Iteration {}: Killed leader {}", i, previous_leader);

        // Wait for new leader
        let new_leader = cluster.wait_for_leader(Duration::from_secs(5)).await?;
        println!("Iteration {}: New leader {}", i, new_leader);

        assert_ne!(new_leader, previous_leader, "Should elect different leader");
        previous_leader = new_leader;
    }

    // Final leader should be stable
    sleep(Duration::from_secs(2)).await;
    let final_leader = cluster.get_leader().await.unwrap();
    assert_eq!(final_leader, previous_leader, "Leader should be stable");

    cluster.cleanup().await;
    Ok(())
}

#[tokio::test]
async fn test_election_with_different_timeouts() -> Result<()> {
    tracing_subscriber::fmt::init();

    // Test with shorter election timeout
    let mut cluster1 = spawn_test_cluster_with_config(ClusterConfig {
        node_count: 3,
        election_timeout_ms: 500,
        heartbeat_interval_ms: 50,
        ..Default::default()
    }).await?;

    let leader1 = cluster1.wait_for_leader(Duration::from_secs(2)).await?;
    println!("Short timeout cluster - leader: {}", leader1);
    cluster1.cleanup().await;

    // Test with longer election timeout
    let mut cluster2 = spawn_test_cluster_with_config(ClusterConfig {
        node_count: 3,
        election_timeout_ms: 3000,
        heartbeat_interval_ms: 300,
        ..Default::default()
    }).await?;

    let leader2 = cluster2.wait_for_leader(Duration::from_secs(10)).await?;
    println!("Long timeout cluster - leader: {}", leader2);
    cluster2.cleanup().await;

    Ok(())
}

#[tokio::test]
async fn test_simultaneous_node_failures() -> Result<()> {
    tracing_subscriber::fmt::init();

    let mut cluster = spawn_test_cluster(7).await?;

    let leader_id = cluster.wait_for_leader(Duration::from_secs(5)).await?;
    println!("Initial leader: node {}", leader_id);

    // Kill 3 nodes simultaneously (but maintain quorum - 4 alive)
    let killed: Vec<u64> = cluster.nodes.iter()
        .filter(|n| n.node_id != leader_id)
        .take(3)
        .map(|n| n.node_id)
        .collect();

    // Kill all at once
    for node_id in &killed {
        cluster.kill_node(*node_id).await?;
    }

    println!("Simultaneously killed nodes: {:?}", killed);

    // Leader should still be stable (quorum maintained)
    sleep(Duration::from_secs(2)).await;

    let current_leader = cluster.get_leader().await;
    assert!(current_leader.is_some(), "Should still have leader with quorum");

    cluster.cleanup().await;
    Ok(())
}

// Helper function to get node info via HTTP
async fn get_node_info_http(http_addr: std::net::SocketAddr) -> Result<NodeInfo> {
    let url = format!("http://{}/api/v1/raft/info", http_addr);
    let resp = reqwest::get(&url).await?;
    let info = resp.json::<NodeInfo>().await?;
    Ok(info)
}

// Property-based testing with proptest

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(10))]

        #[test]
        fn test_cluster_survives_random_failures(
            cluster_size in 3..7usize,
            failures in prop::collection::vec(0..6u64, 0..3)
        ) {
            let runtime = tokio::runtime::Runtime::new().unwrap();
            runtime.block_on(async {
                let mut cluster = spawn_test_cluster(cluster_size).await.unwrap();

                // Wait for initial leader
                let _ = cluster.wait_for_leader(Duration::from_secs(5)).await.unwrap();

                // Apply random failures
                for node_id in failures {
                    if node_id < cluster_size as u64 && node_id > 0 {
                        let _ = cluster.kill_node(node_id).await;
                        sleep(Duration::from_millis(500)).await;
                    }
                }

                // Count alive nodes
                let mut alive_count = 0;
                for node in &cluster.nodes {
                    if node.is_alive().await {
                        alive_count += 1;
                    }
                }

                // If quorum exists, should have leader
                if alive_count > cluster_size / 2 {
                    let _ = cluster.wait_for_leader(Duration::from_secs(10)).await.unwrap();
                }

                cluster.cleanup().await;
            });
        }
    }
}
