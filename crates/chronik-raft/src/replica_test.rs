//! Unit tests for PartitionReplica

use super::*;
use crate::state_machine::{StateMachine, StateMachineResult};
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::RwLock as TokioRwLock;

/// No-op state machine for testing
struct NoOpStateMachine;

#[async_trait]
impl StateMachine for NoOpStateMachine {
    async fn apply(&mut self, _index: u64, _data: &[u8]) -> StateMachineResult<Vec<u8>> {
        Ok(Vec::new())
    }

    async fn snapshot(&self) -> StateMachineResult<Vec<u8>> {
        Ok(Vec::new())
    }

    async fn restore(&mut self, _snapshot: &[u8]) -> StateMachineResult<()> {
        Ok(())
    }
}

#[tokio::test]
async fn test_single_node_quick_debug() {
    // Initialize tracing
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .try_init();

    println!("\n=== Quick Single Node Debug ===\n");

    // Create single-node replica
    let config = RaftConfig {
        node_id: 1,
        listen_addr: "127.0.0.1:5001".to_string(),
        election_timeout_min_ms: 150,
        election_timeout_max_ms: 300,
        heartbeat_interval_ms: 50,
        max_inflight_msgs: 256,
        ..Default::default()
    };

    let storage = Arc::new(MemoryLogStorage::new());
    let state_machine = Arc::new(TokioRwLock::new(NoOpStateMachine));

    println!("Creating replica...");
    let replica = PartitionReplica::new(
        "test".to_string(),
        0,
        config,
        storage,
        state_machine,
        vec![], // Single node
    )
    .expect("Failed to create replica");

    println!("Replica created");
    println!("is_leader: {}", replica.is_leader());
    println!("role: {:?}", replica.role());
    println!("leader_id: {}", replica.leader_id());

    if !replica.is_leader() {
        panic!("Single-node should be leader!");
    }

    // Check log state before proposal
    {
        let node = replica.raw_node.read();
        println!("\nBefore proposal:");
        println!("  last_index: {}", node.raft.raft_log.last_index());
        println!("  committed: {}", node.raft.raft_log.committed);
        println!("  has_ready: {}", node.has_ready());
    }

    // Try to propose
    println!("\nProposing entry...");
    let data = b"test".to_vec();
    let result = replica.propose(data).await;

    match &result {
        Ok(index) => {
            println!("✓ Proposal succeeded, index: {}", index);
        }
        Err(e) => {
            println!("✗ Proposal failed: {}", e);
        }
    }

    // Check log state after proposal
    {
        let node = replica.raw_node.read();
        println!("\nAfter proposal:");
        println!("  last_index: {}", node.raft.raft_log.last_index());
        println!("  committed: {}", node.raft.raft_log.committed);
        println!("  has_ready: {}", node.has_ready());
    }

    assert!(result.is_ok(), "Proposal should succeed");

    println!("\n=== Test Complete ===\n");
}
