//! Integration test for Phase 1: Single partition Raft replication
//!
//! Tests that messages proposed to a partition leader are correctly replicated
//! across all Raft followers and can be consumed after commit.
//!
//! Test scenarios:
//! 1. Basic replication: Message proposed to leader replicates to all nodes
//! 2. Leader election: Cluster elects leader within timeout
//! 3. Message commit: Messages commit after quorum acknowledgment
//! 4. Follower rejection: Followers reject propose attempts
//! 5. Leader failover: New leader elected after old leader fails
//! 6. No message loss: All committed messages survive failover

use anyhow::{Context, Result};
use chronik_raft::{PartitionReplica, RaftConfig, MemoryLogStorage, RaftEntry, StateMachine, SnapshotData};
use bytes::Bytes;
use raft::prelude::Message;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::RwLock as TokioRwLock;
use tokio::time::sleep;
use tracing::{debug, info, warn};

/// No-op state machine for testing
struct NoOpStateMachine {
    last_applied: AtomicU64,
}

impl NoOpStateMachine {
    fn new() -> Self {
        Self {
            last_applied: AtomicU64::new(0),
        }
    }
}

#[async_trait::async_trait]
impl StateMachine for NoOpStateMachine {
    async fn apply(&mut self, entry: &RaftEntry) -> chronik_raft::Result<Bytes> {
        self.last_applied.store(entry.index, Ordering::SeqCst);
        Ok(Bytes::from("ok"))
    }

    async fn snapshot(&self, last_index: u64, last_term: u64) -> chronik_raft::Result<SnapshotData> {
        Ok(SnapshotData {
            last_index,
            last_term,
            conf_state: vec![],
            data: vec![],
        })
    }

    async fn restore(&mut self, snapshot: &SnapshotData) -> chronik_raft::Result<()> {
        self.last_applied.store(snapshot.last_index, Ordering::SeqCst);
        Ok(())
    }

    fn last_applied(&self) -> u64 {
        self.last_applied.load(Ordering::SeqCst)
    }
}

/// Helper to process Raft events for a cluster
async fn process_cluster_tick(replicas: &HashMap<u64, Arc<PartitionReplica>>) -> Result<Vec<Message>> {
    let mut all_messages = Vec::new();

    // Tick all replicas
    for replica in replicas.values() {
        let _ = replica.tick();
    }

    // Process ready states and collect messages
    for replica in replicas.values() {
        if let Ok((messages, _committed)) = replica.ready().await {
            all_messages.extend(messages);
        }
    }

    Ok(all_messages)
}

/// Route messages to their destination replicas
async fn route_messages(
    replicas: &HashMap<u64, Arc<PartitionReplica>>,
    messages: Vec<Message>,
) -> Result<()> {
    for msg in messages {
        let to = msg.to;
        if let Some(replica) = replicas.get(&to) {
            replica.step(msg).await?;
        }
    }
    Ok(())
}

/// Create a 3-node Raft cluster for testing
fn create_test_cluster(topic: &str, partition: i32) -> Result<HashMap<u64, Arc<PartitionReplica>>> {
    let mut replicas = HashMap::new();
    let node_ids = vec![1, 2, 3];

    for &node_id in &node_ids {
        let config = RaftConfig {
            node_id,
            listen_addr: format!("127.0.0.1:{}", 5000 + node_id),
            election_timeout_ms: 1000,
            heartbeat_interval_ms: 100,
            max_entries_per_batch: 100,
            snapshot_threshold: 10_000,
        };

        let storage = Arc::new(MemoryLogStorage::new());
        let state_machine = Arc::new(TokioRwLock::new(NoOpStateMachine::new()));

        // Peers are all other nodes
        let peers: Vec<u64> = node_ids.iter()
            .filter(|&&id| id != node_id)
            .copied()
            .collect();

        let replica = PartitionReplica::new(
            topic.to_string(),
            partition,
            config,
            storage,
            state_machine,
            peers,
        )?;

        replicas.insert(node_id, Arc::new(replica));
    }

    Ok(replicas)
}

/// Wait for a leader to be elected
async fn wait_for_leader(
    replicas: &HashMap<u64, Arc<PartitionReplica>>,
    timeout: Duration,
) -> Result<u64> {
    let start = Instant::now();
    let mut iter = 0;

    loop {
        if start.elapsed() > timeout {
            // Log final state before failing
            for (&node_id, replica) in replicas {
                warn!(
                    "Node {} final state: role={:?}, leader={}, term={}, commit={}",
                    node_id,
                    replica.role(),
                    replica.leader_id(),
                    replica.term(),
                    replica.commit_index()
                );
            }
            anyhow::bail!("Timeout waiting for leader election");
        }

        // Process cluster events
        let messages = process_cluster_tick(replicas).await?;
        debug!("Tick {}: generated {} messages", iter, messages.len());
        route_messages(replicas, messages).await?;

        // Log state every 20 iterations
        if iter % 20 == 0 {
            for (&node_id, replica) in replicas {
                debug!(
                    "Node {}: role={:?}, leader={}, term={}",
                    node_id,
                    replica.role(),
                    replica.leader_id(),
                    replica.term()
                );
            }
        }

        // Check if any replica is the leader
        for (&node_id, replica) in replicas {
            if replica.is_leader() {
                info!("Leader elected: node {}", node_id);
                return Ok(node_id);
            }
        }

        iter += 1;
        sleep(Duration::from_millis(50)).await;
    }
}

/// Wait for all nodes to commit up to a certain index
async fn wait_for_commit(
    replicas: &HashMap<u64, Arc<PartitionReplica>>,
    index: u64,
    timeout: Duration,
) -> Result<()> {
    let start = Instant::now();

    loop {
        if start.elapsed() > timeout {
            anyhow::bail!("Timeout waiting for commit index {}", index);
        }

        // Process cluster events
        let messages = process_cluster_tick(replicas).await?;
        route_messages(replicas, messages).await?;

        // Check if all nodes have committed
        let mut all_committed = true;
        for replica in replicas.values() {
            if replica.commit_index() < index {
                all_committed = false;
                break;
            }
        }

        if all_committed {
            info!("All nodes committed up to index {}", index);
            return Ok(());
        }

        sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn test_leader_election_timeout() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    info!("TEST: Leader election completes within 2 seconds");

    let replicas = create_test_cluster("test-election", 0)?;

    let start = Instant::now();
    let leader_id = wait_for_leader(&replicas, Duration::from_secs(2)).await?;
    let elapsed = start.elapsed();

    info!("Leader {} elected in {:?}", leader_id, elapsed);

    assert!(
        elapsed < Duration::from_secs(2),
        "Leader election took {:?}, should be < 2s",
        elapsed
    );

    Ok(())
}

#[tokio::test]
async fn test_single_partition_replication() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    info!("TEST: Single partition replication");

    // 1. Setup: Create 3-node cluster
    let replicas = create_test_cluster("test-topic", 0)?;

    // 2. Wait for leader election
    let leader_id = wait_for_leader(&replicas, Duration::from_secs(5))
        .await
        .context("Failed to elect leader")?;

    info!("Leader elected: node {}", leader_id);

    // 3. Propose message to leader
    let leader = replicas.get(&leader_id)
        .context("Leader replica not found")?;

    let test_data = b"test message for replication".to_vec();
    let index = leader.propose(test_data.clone()).await?;

    info!("Proposed message at index {}", index);

    // 4. Wait for message to replicate to all nodes (commit)
    wait_for_commit(&replicas, index, Duration::from_secs(5)).await?;

    // 5. Verify all replicas have committed the entry
    for (&node_id, replica) in &replicas {
        let commit_idx = replica.commit_index();
        assert!(
            commit_idx >= index,
            "Node {} commit_index {} should be >= proposed index {}",
            node_id,
            commit_idx,
            index
        );
        info!("Node {} committed index {}", node_id, commit_idx);
    }

    info!("SUCCESS: Message replicated to all nodes");

    Ok(())
}

#[tokio::test]
async fn test_follower_cannot_propose() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    info!("TEST: Followers cannot propose entries");

    let replicas = create_test_cluster("test-follower", 0)?;

    // Wait for leader election
    let leader_id = wait_for_leader(&replicas, Duration::from_secs(5)).await?;

    // Find a follower
    let follower_id = replicas.keys()
        .find(|&&id| id != leader_id)
        .copied()
        .context("No follower found")?;

    info!("Leader: {}, Follower: {}", leader_id, follower_id);

    // Try to propose on follower (should fail)
    let follower = replicas.get(&follower_id).unwrap();
    let test_data = b"this should fail".to_vec();

    let result = follower.propose(test_data).await;

    assert!(
        result.is_err(),
        "Follower should reject propose, but got: {:?}",
        result
    );

    info!("SUCCESS: Follower correctly rejected propose");

    Ok(())
}

#[tokio::test]
async fn test_message_commit_after_quorum() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    info!("TEST: Message commits after quorum acknowledgment");

    let replicas = create_test_cluster("test-quorum", 0)?;

    // Wait for leader
    let leader_id = wait_for_leader(&replicas, Duration::from_secs(5)).await?;
    let leader = replicas.get(&leader_id).unwrap();

    // Propose multiple messages
    let messages = vec![
        b"message 1".to_vec(),
        b"message 2".to_vec(),
        b"message 3".to_vec(),
    ];

    let mut proposed_indices = Vec::new();
    for msg in &messages {
        let index = leader.propose(msg.clone()).await?;
        proposed_indices.push(index);
        info!("Proposed message at index {}", index);
    }

    // Wait for all messages to commit
    let last_index = *proposed_indices.last().unwrap();
    wait_for_commit(&replicas, last_index, Duration::from_secs(5)).await?;

    // Verify all nodes have committed all messages
    for (&node_id, replica) in &replicas {
        assert!(
            replica.commit_index() >= last_index,
            "Node {} should have commit_index >= {}",
            node_id,
            last_index
        );
    }

    info!("SUCCESS: All messages committed after quorum");

    Ok(())
}

#[tokio::test]
async fn test_leader_failover_and_recovery() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    info!("TEST: Leader failover and no message loss");

    // 1. Setup cluster
    let mut replicas = create_test_cluster("test-failover", 0)?;

    // 2. Wait for initial leader
    let leader_id = wait_for_leader(&replicas, Duration::from_secs(5)).await?;
    info!("Initial leader: {}", leader_id);

    // 3. Propose first message
    let leader = replicas.get(&leader_id).unwrap();
    let msg1 = b"message before failover".to_vec();
    let index1 = leader.propose(msg1.clone()).await?;
    info!("Proposed message 1 at index {}", index1);

    // Wait for commit
    wait_for_commit(&replicas, index1, Duration::from_secs(5)).await?;

    // 4. Kill the leader
    replicas.remove(&leader_id);
    info!("Killed leader node {}", leader_id);

    // 5. Wait for new leader election
    sleep(Duration::from_secs(2)).await; // Give time for election timeout

    let new_leader_id = wait_for_leader(&replicas, Duration::from_secs(5)).await?;
    assert_ne!(
        new_leader_id, leader_id,
        "New leader should be different from old leader"
    );
    info!("New leader elected: {}", new_leader_id);

    // 6. Propose second message to new leader
    let new_leader = replicas.get(&new_leader_id).unwrap();
    let msg2 = b"message after failover".to_vec();
    let index2 = new_leader.propose(msg2.clone()).await?;
    info!("Proposed message 2 at index {}", index2);

    // Wait for commit
    wait_for_commit(&replicas, index2, Duration::from_secs(5)).await?;

    // 7. Verify no message loss - both messages should be committed on surviving nodes
    for (&node_id, replica) in &replicas {
        let commit_idx = replica.commit_index();
        assert!(
            commit_idx >= index2,
            "Node {} should have both messages (commit_index >= {})",
            node_id,
            index2
        );
        info!("Node {} final commit_index: {}", node_id, commit_idx);
    }

    info!("SUCCESS: No message loss during failover");

    Ok(())
}

#[tokio::test]
async fn test_consume_from_follower_after_commit() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    info!("TEST: Consume from follower after commit");

    let replicas = create_test_cluster("test-consume", 0)?;

    // Wait for leader
    let leader_id = wait_for_leader(&replicas, Duration::from_secs(5)).await?;
    let leader = replicas.get(&leader_id).unwrap();

    // Propose message
    let test_data = b"test message".to_vec();
    let index = leader.propose(test_data.clone()).await?;

    // Wait for commit
    wait_for_commit(&replicas, index, Duration::from_secs(5)).await?;

    // Find a follower
    let follower_id = replicas.keys()
        .find(|&&id| id != leader_id)
        .copied()
        .unwrap();

    let follower = replicas.get(&follower_id).unwrap();

    // Verify follower has committed the entry
    assert!(
        follower.commit_index() >= index,
        "Follower should have committed index {}",
        index
    );

    info!("SUCCESS: Follower can serve committed data");

    Ok(())
}

#[tokio::test]
async fn test_multiple_messages_replication() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    info!("TEST: Multiple messages replication");

    let replicas = create_test_cluster("test-multi", 0)?;

    let leader_id = wait_for_leader(&replicas, Duration::from_secs(5)).await?;
    let leader = replicas.get(&leader_id).unwrap();

    // Propose 10 messages rapidly
    let message_count = 10;
    let mut indices = Vec::new();

    for i in 0..message_count {
        let data = format!("message {}", i).into_bytes();
        let index = leader.propose(data).await?;
        indices.push(index);
    }

    info!("Proposed {} messages", message_count);

    // Wait for all to commit
    let last_index = *indices.last().unwrap();
    wait_for_commit(&replicas, last_index, Duration::from_secs(10)).await?;

    // Verify all nodes have all messages
    for (&node_id, replica) in &replicas {
        assert!(
            replica.commit_index() >= last_index,
            "Node {} should have commit_index >= {}",
            node_id,
            last_index
        );
    }

    info!("SUCCESS: All {} messages replicated", message_count);

    Ok(())
}
