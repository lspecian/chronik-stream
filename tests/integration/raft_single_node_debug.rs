//! Debugging test for single-node Raft commit issue

use chronik_raft::state_machine::{StateMachine, StateMachineSnapshot};
use chronik_raft::{MemoryLogStorage, PartitionReplica, RaftConfig};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock as TokioRwLock;
use tokio::time::sleep;
use tracing::{debug, info, warn};

/// No-op state machine for testing
struct NoOpStateMachine;

#[async_trait::async_trait]
impl StateMachine for NoOpStateMachine {
    async fn apply(&mut self, _index: u64, _data: Vec<u8>) -> Result<Vec<u8>, String> {
        Ok(vec![])
    }

    async fn snapshot(&self) -> Result<StateMachineSnapshot, String> {
        Ok(StateMachineSnapshot {
            last_included_index: 0,
            last_included_term: 0,
            data: vec![],
        })
    }

    async fn restore(&mut self, _snapshot: StateMachineSnapshot) -> Result<(), String> {
        Ok(())
    }

    fn last_applied(&self) -> u64 {
        0
    }
}

#[tokio::test]
async fn test_single_node_propose_and_commit() {
    // Initialize tracing
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .try_init();

    info!("=== Single Node Raft Commit Debug Test ===");

    // Create a single-node Raft replica
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

    info!("Creating single-node replica (no peers)");
    let replica = match PartitionReplica::new(
        "test-topic".to_string(),
        0,
        config,
        storage,
        state_machine,
        vec![], // Empty peers = single node
    ) {
        Ok(r) => Arc::new(r),
        Err(e) => {
            panic!("Failed to create replica: {}", e);
        }
    };

    info!("Replica created");

    // Give it a moment to stabilize
    sleep(Duration::from_millis(100)).await;

    // Check leadership
    info!(
        "Initial state: is_leader={}, role={:?}, leader_id={}, term={}",
        replica.is_leader(),
        replica.role(),
        replica.leader_id(),
        replica.term()
    );

    if !replica.is_leader() {
        warn!("Node is not leader yet, waiting for election...");
        sleep(Duration::from_millis(500)).await;

        info!(
            "After wait: is_leader={}, role={:?}, leader_id={}, term={}",
            replica.is_leader(),
            replica.role(),
            replica.leader_id(),
            replica.term()
        );

        if !replica.is_leader() {
            panic!("Single-node cluster did not become leader!");
        }
    }

    info!("✓ Node is leader");

    // Start background tick/ready loop
    let replica_clone = replica.clone();
    let tick_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(10));
        loop {
            interval.tick().await;
            let _ = replica_clone.tick();
            match replica_clone.ready().await {
                Ok((messages, committed)) => {
                    if !committed.is_empty() {
                        info!("Background loop: committed {} entries", committed.len());
                    }
                }
                Err(e) => {
                    debug!("Ready error: {}", e);
                }
            }
        }
    });

    // Propose a test entry
    let test_data = b"Hello from single-node Raft!".to_vec();
    info!("Proposing entry with {} bytes", test_data.len());

    let propose_result = replica.propose_and_wait(test_data).await;

    match propose_result {
        Ok(index) => {
            info!("✓ Entry committed at index {}", index);
            info!("✓ SUCCESS: Single-node commit works!");
        }
        Err(e) => {
            warn!("✗ Proposal failed: {}", e);
            panic!("Single-node commit failed: {}", e);
        }
    }

    // Clean up
    tick_handle.abort();

    info!("=== Test Complete ===");
}
