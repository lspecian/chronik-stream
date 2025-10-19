/// Integration test: Network partition handling (split-brain prevention)
///
/// Tests Raft's behavior under various network partition scenarios:
/// - Majority partition should elect leader
/// - Minority partition should not elect leader
/// - Partition healing should converge to single leader
/// - Split-brain prevention

mod common;

use anyhow::Result;
use common::*;
use std::time::Duration;
use tokio::time::sleep;

#[tokio::test]
async fn test_majority_partition_elects_leader() -> Result<()> {
    tracing_subscriber::fmt::init();

    // Use 5 nodes for clear majority/minority split
    let mut cluster = spawn_test_cluster_with_config(ClusterConfig {
        node_count: 5,
        use_toxiproxy: true,
        ..Default::default()
    }).await?;

    let initial_leader = cluster.wait_for_leader(Duration::from_secs(5)).await?;
    println!("Initial leader: node {}", initial_leader);

    // Partition: 3 nodes (majority) vs 2 nodes (minority)
    // Isolate nodes 4 and 5
    cluster.partition_network(vec![4, 5]).await?;
    println!("Network partitioned: [1,2,3] vs [4,5]");

    sleep(Duration::from_secs(2)).await;

    // Majority partition (nodes 1,2,3) should have a leader
    let mut majority_leader = None;
    for node_id in [1, 2, 3] {
        if let Some(node) = cluster.get_node(node_id) {
            if let Ok(info) = get_node_info(node.http_addr).await {
                if info.is_leader {
                    majority_leader = Some(node_id);
                    break;
                }
            }
        }
    }

    assert!(
        majority_leader.is_some(),
        "Majority partition should have a leader"
    );
    println!("Majority partition leader: node {}", majority_leader.unwrap());

    // Minority partition (nodes 4,5) should NOT have a leader
    for node_id in [4, 5] {
        if let Some(node) = cluster.get_node(node_id) {
            if let Ok(info) = get_node_info(node.http_addr).await {
                assert!(
                    !info.is_leader,
                    "Minority partition should not have leader"
                );
            }
        }
    }

    cluster.cleanup().await;
    Ok(())
}

#[tokio::test]
async fn test_partition_healing_converges() -> Result<()> {
    tracing_subscriber::fmt::init();

    let mut cluster = spawn_test_cluster_with_config(ClusterConfig {
        node_count: 5,
        use_toxiproxy: true,
        ..Default::default()
    }).await?;

    let initial_leader = cluster.wait_for_leader(Duration::from_secs(5)).await?;
    println!("Initial leader: node {}", initial_leader);

    // Create partition
    cluster.partition_network(vec![4, 5]).await?;
    println!("Network partitioned");

    sleep(Duration::from_secs(2)).await;

    // Heal partition
    cluster.heal_partition().await?;
    println!("Partition healed");

    // All nodes should eventually agree on a single leader
    let consensus_leader = cluster.wait_for_consensus(Duration::from_secs(10)).await?;
    println!("Consensus reached on leader: node {}", consensus_leader);

    // Verify all nodes agree
    for node in &cluster.nodes {
        if let Ok(info) = get_node_info(node.http_addr).await {
            assert_eq!(
                info.current_leader, consensus_leader,
                "Node {} disagrees on leader",
                node.node_id
            );
        }
    }

    cluster.cleanup().await;
    Ok(())
}

#[tokio::test]
async fn test_symmetric_partition() -> Result<()> {
    tracing_subscriber::fmt::init();

    // Use 6 nodes for symmetric partition (3 vs 3)
    let mut cluster = spawn_test_cluster_with_config(ClusterConfig {
        node_count: 6,
        use_toxiproxy: true,
        ..Default::default()
    }).await?;

    let initial_leader = cluster.wait_for_leader(Duration::from_secs(5)).await?;
    println!("Initial leader: node {}", initial_leader);

    // Partition: 3 vs 3 (no majority)
    cluster.partition_network(vec![4, 5, 6]).await?;
    println!("Symmetric partition: [1,2,3] vs [4,5,6]");

    sleep(Duration::from_secs(3)).await;

    // Neither partition should be able to elect new leader
    // (if leader was in one partition, it should remain)
    // (if leader was killed, no new leader should emerge)

    // Count leaders across all nodes
    let mut leader_count = 0;
    for node in &cluster.nodes {
        if let Ok(info) = get_node_info(node.http_addr).await {
            if info.is_leader {
                leader_count += 1;
            }
        }
    }

    // Should have at most 1 leader (the original one, if still alive)
    assert!(
        leader_count <= 1,
        "Should not have multiple leaders in symmetric partition"
    );

    // Heal partition
    cluster.heal_partition().await?;
    println!("Partition healed");

    // Should converge to single leader
    let consensus_leader = cluster.wait_for_consensus(Duration::from_secs(10)).await?;
    println!("Converged to leader: node {}", consensus_leader);

    cluster.cleanup().await;
    Ok(())
}

#[tokio::test]
async fn test_cascading_partitions() -> Result<()> {
    tracing_subscriber::fmt::init();

    let mut cluster = spawn_test_cluster_with_config(ClusterConfig {
        node_count: 7,
        use_toxiproxy: true,
        ..Default::default()
    }).await?;

    let initial_leader = cluster.wait_for_leader(Duration::from_secs(5)).await?;
    println!("Initial leader: node {}", initial_leader);

    // First partition: isolate 2 nodes
    cluster.partition_network(vec![6, 7]).await?;
    println!("First partition: [1,2,3,4,5] vs [6,7]");

    sleep(Duration::from_secs(2)).await;

    // Majority (5 nodes) should have leader
    let leader_after_first = cluster.get_leader().await;
    assert!(leader_after_first.is_some(), "Should have leader in majority");

    // Second partition: further isolate 2 more nodes (now 3 partitions)
    cluster.heal_partition().await?;
    sleep(Duration::from_millis(500)).await;
    cluster.partition_network(vec![4, 5, 6, 7]).await?;
    println!("Second partition: [1,2,3] vs [4,5,6,7]");

    sleep(Duration::from_secs(2)).await;

    // Still should have leader in partition with 4 nodes
    let leader_after_second = cluster.get_leader().await;
    assert!(
        leader_after_second.is_some(),
        "Should still have leader in majority partition"
    );

    // Heal all partitions
    cluster.heal_partition().await?;
    println!("All partitions healed");

    let final_leader = cluster.wait_for_consensus(Duration::from_secs(10)).await?;
    println!("Final leader after healing: node {}", final_leader);

    cluster.cleanup().await;
    Ok(())
}

#[tokio::test]
async fn test_partition_with_leader_in_minority() -> Result<()> {
    tracing_subscriber::fmt::init();

    let mut cluster = spawn_test_cluster_with_config(ClusterConfig {
        node_count: 5,
        use_toxiproxy: true,
        ..Default::default()
    }).await?;

    let leader_id = cluster.wait_for_leader(Duration::from_secs(5)).await?;
    println!("Initial leader: node {}", leader_id);

    // Partition leader into minority
    cluster.partition_network(vec![leader_id]).await?;
    println!("Leader {} partitioned into minority", leader_id);

    sleep(Duration::from_secs(3)).await;

    // Majority should elect new leader
    let mut new_leader = None;
    for node in &cluster.nodes {
        if node.node_id != leader_id {
            if let Ok(info) = get_node_info(node.http_addr).await {
                if info.is_leader {
                    new_leader = Some(node.node_id);
                    break;
                }
            }
        }
    }

    assert!(new_leader.is_some(), "Majority should elect new leader");
    assert_ne!(
        new_leader.unwrap(),
        leader_id,
        "New leader should be different"
    );

    println!("New leader in majority: node {}", new_leader.unwrap());

    // Old leader should step down
    if let Some(old_leader_node) = cluster.get_node(leader_id) {
        if let Ok(info) = get_node_info(old_leader_node.http_addr).await {
            assert!(
                !info.is_leader,
                "Old leader should step down in minority partition"
            );
        }
    }

    cluster.cleanup().await;
    Ok(())
}

#[tokio::test]
async fn test_flapping_network() -> Result<()> {
    tracing_subscriber::fmt::init();

    let mut cluster = spawn_test_cluster_with_config(ClusterConfig {
        node_count: 5,
        use_toxiproxy: true,
        election_timeout_ms: 2000, // Longer timeout for flapping test
        ..Default::default()
    }).await?;

    let initial_leader = cluster.wait_for_leader(Duration::from_secs(5)).await?;
    println!("Initial leader: node {}", initial_leader);

    // Rapidly partition and heal 5 times
    for i in 0..5 {
        println!("Flap iteration {}", i);

        // Partition
        cluster.partition_network(vec![4, 5]).await?;
        sleep(Duration::from_millis(500)).await;

        // Heal
        cluster.heal_partition().await?;
        sleep(Duration::from_millis(500)).await;
    }

    // After flapping, should eventually stabilize
    let final_leader = cluster.wait_for_consensus(Duration::from_secs(10)).await?;
    println!("Stable leader after flapping: node {}", final_leader);

    // Verify stability (no leader changes for 3 seconds)
    sleep(Duration::from_secs(3)).await;
    let still_leader = cluster.get_leader().await.unwrap();
    assert_eq!(
        still_leader, final_leader,
        "Leader should be stable after flapping"
    );

    cluster.cleanup().await;
    Ok(())
}

// Helper function
async fn get_node_info(http_addr: std::net::SocketAddr) -> Result<NodeInfo> {
    let url = format!("http://{}/api/v1/raft/info", http_addr);
    let resp = reqwest::get(&url).await?;
    let info = resp.json::<NodeInfo>().await?;
    Ok(info)
}

#[derive(Debug, serde::Deserialize)]
struct NodeInfo {
    node_id: u64,
    is_leader: bool,
    current_leader: u64,
    term: u64,
}
