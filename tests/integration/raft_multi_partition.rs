//! Integration test for Phase 2: Multi-partition Raft replication
//!
//! Tests that multiple independent Raft partitions can operate simultaneously
//! on the same cluster nodes without interfering with each other.
//!
//! Key test scenarios:
//! 1. Multi-partition independence: Each partition has separate Raft group and leader
//! 2. Balanced leadership: Leaders distributed across nodes
//! 3. Partition isolation: Failure in one partition doesn't affect others
//! 4. Follower reads: Consumers can read from any replica after commit
//! 5. No message loss: All messages survive leader failover per partition
//! 6. Concurrent operations: Multiple partitions process messages simultaneously

use anyhow::{Context, Result};
use chronik_common::partition_assignment::{PartitionAssignment, round_robin};
use chronik_raft::{RaftGroupManager, RaftConfig, MemoryLogStorage};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::sleep;
use tracing::{debug, info, warn};

/// Create a 3-node cluster with RaftGroupManagers
fn create_cluster_managers() -> Vec<(u64, Arc<RaftGroupManager>)> {
    let node_ids = vec![1, 2, 3];
    let mut managers = Vec::new();

    for &node_id in &node_ids {
        let config = RaftConfig {
            node_id,
            listen_addr: format!("127.0.0.1:{}", 5000 + node_id),
            election_timeout_ms: 1000,
            heartbeat_interval_ms: 100,
            max_entries_per_batch: 100,
            snapshot_threshold: 10_000,
        };

        let manager = Arc::new(RaftGroupManager::new(
            node_id,
            config,
            || Arc::new(MemoryLogStorage::new()),
        ));

        managers.push((node_id, manager));
    }

    managers
}

/// Setup partition assignment for a topic
fn setup_partition_assignment(
    topic: &str,
    num_partitions: i32,
    replication_factor: i32,
    nodes: &[u64],
) -> Result<PartitionAssignment> {
    let mut assignment = PartitionAssignment::new();
    assignment.add_topic(topic, num_partitions, replication_factor, nodes)?;
    Ok(assignment)
}

/// Create replicas on all nodes according to partition assignment
fn create_partition_replicas(
    managers: &[(u64, Arc<RaftGroupManager>)],
    assignment: &PartitionAssignment,
    topic: &str,
) -> Result<()> {
    let topic_assignments = assignment.get_topic_assignments(topic);

    for (partition, info) in topic_assignments {
        // Create replica on each node in the replica list
        for &replica_node_id in &info.replicas {
            let manager = managers
                .iter()
                .find(|(node_id, _)| *node_id == replica_node_id)
                .map(|(_, mgr)| mgr)
                .ok_or_else(|| anyhow::anyhow!("Node {} not found", replica_node_id))?;

            // Peers are all other replicas (excluding self)
            let peers: Vec<u64> = info.replicas
                .iter()
                .filter(|&&id| id != replica_node_id)
                .copied()
                .collect();

            manager.get_or_create_replica(topic, partition, peers)?;
            debug!(
                "Created replica for {}-{} on node {} with peers {:?}",
                topic, partition, replica_node_id, peers
            );
        }
    }

    Ok(())
}

/// Process one tick cycle for all managers
async fn tick_all_managers(managers: &[(u64, Arc<RaftGroupManager>)]) -> Result<()> {
    // Collect all messages from all managers
    let mut all_messages = Vec::new();

    for (_node_id, manager) in managers {
        manager.tick_all()?;
        let ready_results = manager.ready_all().await?;

        for ((topic, partition), (messages, _committed)) in ready_results {
            for msg in messages {
                // Tag message with source partition
                all_messages.push((topic.clone(), partition, msg));
            }
        }
    }

    // Route messages to appropriate replicas
    for (topic, partition, msg) in all_messages {
        let to = msg.to;

        // Find the manager for the destination node
        if let Some((_node_id, manager)) = managers.iter().find(|(node_id, _)| *node_id == to) {
            manager.route_message(&topic, partition, msg).await?;
        }
    }

    Ok(())
}

/// Wait for leader election on a specific partition
async fn wait_for_partition_leader(
    managers: &[(u64, Arc<RaftGroupManager>)],
    topic: &str,
    partition: i32,
    timeout: Duration,
) -> Result<u64> {
    let start = Instant::now();
    let mut iter = 0;

    loop {
        if start.elapsed() > timeout {
            // Log final state
            for (node_id, manager) in managers {
                if let Some(replica) = manager.get_replica(topic, partition) {
                    warn!(
                        "Node {} partition {}-{}: role={:?}, leader={}, term={}",
                        node_id,
                        topic,
                        partition,
                        replica.role(),
                        replica.leader_id(),
                        replica.term()
                    );
                }
            }
            anyhow::bail!(
                "Timeout waiting for leader election on partition {}-{}",
                topic,
                partition
            );
        }

        // Tick all managers
        tick_all_managers(managers).await?;

        // Log state every 20 iterations
        if iter % 20 == 0 {
            for (node_id, manager) in managers {
                if let Some(replica) = manager.get_replica(topic, partition) {
                    debug!(
                        "Node {} partition {}-{}: role={:?}, leader={}, term={}",
                        node_id,
                        topic,
                        partition,
                        replica.role(),
                        replica.leader_id(),
                        replica.term()
                    );
                }
            }
        }

        // Check if any replica is the leader
        for (node_id, manager) in managers {
            if manager.is_leader_for_partition(topic, partition) {
                info!("Leader elected for {}-{}: node {}", topic, partition, node_id);
                return Ok(*node_id);
            }
        }

        iter += 1;
        sleep(Duration::from_millis(50)).await;
    }
}

/// Wait for all partitions to elect leaders
async fn wait_for_all_partition_leaders(
    managers: &[(u64, Arc<RaftGroupManager>)],
    topic: &str,
    num_partitions: i32,
    timeout: Duration,
) -> Result<HashMap<i32, u64>> {
    let mut leaders = HashMap::new();

    for partition in 0..num_partitions {
        let leader_id = wait_for_partition_leader(managers, topic, partition, timeout).await?;
        leaders.insert(partition, leader_id);
    }

    Ok(leaders)
}

/// Wait for a specific partition to commit up to an index
async fn wait_for_partition_commit(
    managers: &[(u64, Arc<RaftGroupManager>)],
    topic: &str,
    partition: i32,
    index: u64,
    timeout: Duration,
) -> Result<()> {
    let start = Instant::now();

    loop {
        if start.elapsed() > timeout {
            anyhow::bail!(
                "Timeout waiting for commit index {} on partition {}-{}",
                index,
                topic,
                partition
            );
        }

        tick_all_managers(managers).await?;

        // Check if all replicas have committed
        let mut all_committed = true;
        for (_node_id, manager) in managers {
            if let Some(replica) = manager.get_replica(topic, partition) {
                if replica.commit_index() < index {
                    all_committed = false;
                    break;
                }
            }
        }

        if all_committed {
            info!("Partition {}-{} committed up to index {}", topic, partition, index);
            return Ok(());
        }

        sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn test_partition_assignment_balance() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    info!("TEST: Partition assignment balance");

    // Test round_robin assignment
    let nodes = vec![1, 2, 3];
    let assignments = round_robin(&nodes, "test-topic", 9, 3)?;

    // Verify 9 partitions
    assert_eq!(assignments.len(), 9);

    // Count leadership distribution
    let mut leader_counts = HashMap::new();
    for replicas in &assignments {
        let leader = replicas[0]; // First replica is leader
        *leader_counts.entry(leader).or_insert(0) += 1;
    }

    // Verify balanced leadership (each node should lead 3 partitions)
    for node_id in &nodes {
        let count = leader_counts.get(node_id).unwrap_or(&0);
        assert_eq!(
            *count, 3,
            "Node {} should lead 3 partitions, but leads {}",
            node_id, count
        );
    }

    info!("SUCCESS: Leadership is balanced across all nodes");

    Ok(())
}

#[tokio::test]
async fn test_multi_partition_produce_consume() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    info!("TEST: Multi-partition produce and consume");

    // 1. Create 3-node cluster
    let managers = create_cluster_managers();
    let nodes: Vec<u64> = managers.iter().map(|(id, _)| *id).collect();

    // 2. Create partition assignment (3 partitions, RF=3)
    let topic = "test-multi";
    let assignment = setup_partition_assignment(topic, 3, 3, &nodes)?;

    // 3. Create replicas on all nodes
    create_partition_replicas(&managers, &assignment, topic)?;

    // 4. Wait for leaders on all partitions
    let leaders = wait_for_all_partition_leaders(&managers, topic, 3, Duration::from_secs(10)).await?;

    info!("Partition leaders elected: {:?}", leaders);

    // 5. Verify each partition has a different leader
    let unique_leaders: std::collections::HashSet<_> = leaders.values().copied().collect();
    assert_eq!(
        unique_leaders.len(),
        3,
        "All 3 partitions should have different leaders"
    );

    // 6. Propose messages to each partition
    let mut proposed_indices = HashMap::new();
    for partition in 0..3 {
        let leader_id = leaders.get(&partition).unwrap();
        let manager = managers
            .iter()
            .find(|(node_id, _)| node_id == leader_id)
            .map(|(_, mgr)| mgr)
            .unwrap();

        let data = format!("message for partition {}", partition).into_bytes();
        let index = manager.propose(topic, partition, data).await?;

        info!("Proposed to partition {} at index {}", partition, index);
        proposed_indices.insert(partition, index);
    }

    // 7. Wait for all partitions to commit
    for (partition, index) in &proposed_indices {
        wait_for_partition_commit(&managers, topic, *partition, *index, Duration::from_secs(5)).await?;
    }

    // 8. Verify all replicas have committed
    for (partition, expected_index) in &proposed_indices {
        for (node_id, manager) in &managers {
            if let Some(replica) = manager.get_replica(topic, *partition) {
                assert!(
                    replica.commit_index() >= *expected_index,
                    "Node {} partition {} should have commit_index >= {}",
                    node_id,
                    partition,
                    expected_index
                );
            }
        }
    }

    info!("SUCCESS: All partitions processed messages independently");

    Ok(())
}

#[tokio::test]
async fn test_multi_partition_independence() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    info!("TEST: Multi-partition independence (partition 0 failure doesn't affect others)");

    // 1. Setup: 3 nodes, 3 partitions, RF=3
    let managers = create_cluster_managers();
    let nodes: Vec<u64> = managers.iter().map(|(id, _)| *id).collect();

    let topic = "test-independence";
    let assignment = setup_partition_assignment(topic, 3, 3, &nodes)?;
    create_partition_replicas(&managers, &assignment, topic)?;

    // 2. Wait for leaders on all partitions
    let leaders = wait_for_all_partition_leaders(&managers, topic, 3, Duration::from_secs(10)).await?;

    info!("Initial leaders: {:?}", leaders);

    let partition0_leader = *leaders.get(&0).unwrap();
    let partition1_leader = *leaders.get(&1).unwrap();
    let partition2_leader = *leaders.get(&2).unwrap();

    // 3. Produce to all partitions
    for partition in 0..3 {
        let leader_id = leaders.get(&partition).unwrap();
        let manager = managers
            .iter()
            .find(|(node_id, _)| node_id == leader_id)
            .map(|(_, mgr)| mgr)
            .unwrap();

        let data = format!("initial message for partition {}", partition).into_bytes();
        let index = manager.propose(topic, partition, data).await?;
        wait_for_partition_commit(&managers, topic, partition, index, Duration::from_secs(5)).await?;
    }

    info!("All partitions have initial messages committed");

    // 4. Kill leader of partition 0
    let managers_after_kill: Vec<_> = managers
        .into_iter()
        .filter(|(node_id, _)| *node_id != partition0_leader)
        .collect();

    info!("Killed partition 0 leader (node {})", partition0_leader);

    // 5. Wait for new leader election on partition 0
    sleep(Duration::from_secs(2)).await; // Election timeout

    let new_partition0_leader = wait_for_partition_leader(
        &managers_after_kill,
        topic,
        0,
        Duration::from_secs(5),
    )
    .await?;

    assert_ne!(
        new_partition0_leader, partition0_leader,
        "Partition 0 should have new leader"
    );

    info!("Partition 0 elected new leader: {}", new_partition0_leader);

    // 6. Verify partitions 1 and 2 UNAFFECTED (still have same leaders)
    for partition in 1..=2 {
        for (node_id, manager) in &managers_after_kill {
            if let Some(replica) = manager.get_replica(topic, partition) {
                if replica.is_leader() {
                    let expected_leader = if partition == 1 {
                        partition1_leader
                    } else {
                        partition2_leader
                    };

                    assert_eq!(
                        *node_id, expected_leader,
                        "Partition {} leader should still be node {}",
                        partition, expected_leader
                    );

                    info!("Partition {} still has original leader {}", partition, node_id);
                }
            }
        }
    }

    // 7. Produce more messages to all partitions after failover
    for partition in 0..3 {
        // Find current leader
        let mut leader_id = None;
        for (node_id, manager) in &managers_after_kill {
            if manager.is_leader_for_partition(topic, partition) {
                leader_id = Some(*node_id);
                break;
            }
        }

        let leader_id = leader_id.expect(&format!("No leader for partition {}", partition));
        let manager = managers_after_kill
            .iter()
            .find(|(node_id, _)| *node_id == leader_id)
            .map(|(_, mgr)| mgr)
            .unwrap();

        let data = format!("message after failover for partition {}", partition).into_bytes();
        let index = manager.propose(topic, partition, data).await?;

        info!("Proposed to partition {} after failover at index {}", partition, index);

        wait_for_partition_commit(&managers_after_kill, topic, partition, index, Duration::from_secs(5))
            .await?;
    }

    // 8. Verify no message loss (all surviving nodes have all messages)
    for (node_id, manager) in &managers_after_kill {
        for partition in 0..3 {
            if let Some(replica) = manager.get_replica(topic, partition) {
                // Each partition should have at least 2 messages (initial + after failover)
                assert!(
                    replica.commit_index() >= 2,
                    "Node {} partition {} should have commit_index >= 2",
                    node_id,
                    partition
                );
                info!(
                    "Node {} partition {} commit_index: {}",
                    node_id,
                    partition,
                    replica.commit_index()
                );
            }
        }
    }

    info!("SUCCESS: Partition independence verified - failure in partition 0 didn't affect partitions 1 and 2");

    Ok(())
}

#[tokio::test]
async fn test_follower_reads_all_partitions() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    info!("TEST: Follower reads from all partitions");

    // 1. Setup cluster
    let managers = create_cluster_managers();
    let nodes: Vec<u64> = managers.iter().map(|(id, _)| *id).collect();

    let topic = "test-follower-reads";
    let assignment = setup_partition_assignment(topic, 3, 3, &nodes)?;
    create_partition_replicas(&managers, &assignment, topic)?;

    // 2. Wait for leaders
    let leaders = wait_for_all_partition_leaders(&managers, topic, 3, Duration::from_secs(10)).await?;

    // 3. Produce to each partition
    for partition in 0..3 {
        let leader_id = leaders.get(&partition).unwrap();
        let manager = managers
            .iter()
            .find(|(node_id, _)| node_id == leader_id)
            .map(|(_, mgr)| mgr)
            .unwrap();

        let data = format!("test data for partition {}", partition).into_bytes();
        let index = manager.propose(topic, partition, data).await?;

        // Wait for commit
        wait_for_partition_commit(&managers, topic, partition, index, Duration::from_secs(5)).await?;
    }

    // 4. Verify followers can serve reads (have committed data)
    for partition in 0..3 {
        let leader_id = *leaders.get(&partition).unwrap();

        // Find a follower node
        let follower_id = nodes
            .iter()
            .find(|&&id| id != leader_id)
            .copied()
            .expect(&format!("No follower found for partition {}", partition));

        let follower_manager = managers
            .iter()
            .find(|(node_id, _)| *node_id == follower_id)
            .map(|(_, mgr)| mgr)
            .unwrap();

        if let Some(follower_replica) = follower_manager.get_replica(topic, partition) {
            assert!(
                follower_replica.commit_index() >= 1,
                "Follower {} for partition {} should have committed data",
                follower_id,
                partition
            );

            info!(
                "Follower {} can serve reads for partition {} (commit_index: {})",
                follower_id,
                partition,
                follower_replica.commit_index()
            );
        }
    }

    info!("SUCCESS: Followers can serve reads from all partitions");

    Ok(())
}

#[tokio::test]
async fn test_concurrent_partition_operations() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    info!("TEST: Concurrent operations on multiple partitions");

    // 1. Setup cluster with 3 partitions
    let managers = create_cluster_managers();
    let nodes: Vec<u64> = managers.iter().map(|(id, _)| *id).collect();

    let topic = "test-concurrent";
    let assignment = setup_partition_assignment(topic, 3, 3, &nodes)?;
    create_partition_replicas(&managers, &assignment, topic)?;

    // 2. Wait for leaders
    let leaders = wait_for_all_partition_leaders(&managers, topic, 3, Duration::from_secs(10)).await?;

    // 3. Concurrently propose multiple messages to each partition
    let message_count_per_partition = 5;
    let mut all_indices = HashMap::new();

    for partition in 0..3 {
        let leader_id = leaders.get(&partition).unwrap();
        let manager = managers
            .iter()
            .find(|(node_id, _)| node_id == leader_id)
            .map(|(_, mgr)| mgr)
            .unwrap();

        let mut indices = Vec::new();
        for i in 0..message_count_per_partition {
            let data = format!("partition {} message {}", partition, i).into_bytes();
            let index = manager.propose(topic, partition, data).await?;
            indices.push(index);
        }

        all_indices.insert(partition, indices);
    }

    info!("Proposed {} messages to each of 3 partitions", message_count_per_partition);

    // 4. Wait for all partitions to commit all messages
    for (partition, indices) in &all_indices {
        let last_index = *indices.last().unwrap();
        wait_for_partition_commit(&managers, topic, *partition, last_index, Duration::from_secs(10)).await?;
    }

    // 5. Verify all nodes have all messages on all partitions
    for (node_id, manager) in &managers {
        for partition in 0..3 {
            if let Some(replica) = manager.get_replica(topic, partition) {
                let expected_index = all_indices.get(&partition).unwrap().last().unwrap();
                assert!(
                    replica.commit_index() >= *expected_index,
                    "Node {} partition {} should have all {} messages",
                    node_id,
                    partition,
                    message_count_per_partition
                );
            }
        }
    }

    info!(
        "SUCCESS: All {} partitions processed {} messages concurrently",
        3, message_count_per_partition
    );

    Ok(())
}

#[tokio::test]
async fn test_partition_leader_distribution() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    info!("TEST: Leader distribution across nodes");

    // 1. Setup cluster with 9 partitions (each node should lead 3)
    let managers = create_cluster_managers();
    let nodes: Vec<u64> = managers.iter().map(|(id, _)| *id).collect();

    let topic = "test-distribution";
    let assignment = setup_partition_assignment(topic, 9, 3, &nodes)?;
    create_partition_replicas(&managers, &assignment, topic)?;

    // 2. Wait for all leaders
    let leaders = wait_for_all_partition_leaders(&managers, topic, 9, Duration::from_secs(15)).await?;

    // 3. Count leadership distribution
    let mut leadership_counts = HashMap::new();
    for leader_id in leaders.values() {
        *leadership_counts.entry(*leader_id).or_insert(0) += 1;
    }

    info!("Leadership distribution: {:?}", leadership_counts);

    // 4. Verify balanced distribution (each node should lead 3 partitions)
    for node_id in &nodes {
        let count = leadership_counts.get(node_id).unwrap_or(&0);
        assert_eq!(
            *count, 3,
            "Node {} should lead 3 partitions, but leads {}",
            node_id, count
        );
    }

    info!("SUCCESS: Leadership is evenly distributed (each node leads 3/9 partitions)");

    Ok(())
}
