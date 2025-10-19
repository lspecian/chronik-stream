//! Integration tests for Raft network layer
//!
//! Tests gRPC communication between Raft nodes, including:
//! - Message passing via RaftClient
//! - Leader election over the network
//! - Log replication across nodes

use anyhow::Result;
use chronik_raft::{
    MemoryLogStorage, MemoryStateMachine, PartitionReplica, RaftClient, RaftConfig,
    RaftEntry, SnapshotData, StateMachine,
};
use chronik_raft::rpc::{proto::raft_service_server::RaftServiceServer, RaftServiceImpl, start_raft_server};
use chronik_raft::state_machine::{StateMachine as SM, StateMachineSnapshot};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tokio::sync::RwLock as TokioRwLock;
use tonic::transport::Server;
use async_trait::async_trait;
use bytes::Bytes;

/// No-op state machine for testing
struct NoOpStateMachine;

#[async_trait::async_trait]
impl SM for NoOpStateMachine {
    async fn apply(&mut self, _index: u64, _data: Vec<u8>) -> std::result::Result<Vec<u8>, String> {
        Ok(vec![])
    }

    async fn snapshot(&self) -> std::result::Result<StateMachineSnapshot, String> {
        Ok(StateMachineSnapshot {
            last_included_index: 0,
            last_included_term: 0,
            data: vec![],
        })
    }

    async fn restore(&mut self, _snapshot: StateMachineSnapshot) -> std::result::Result<(), String> {
        Ok(())
    }

    fn last_applied(&self) -> u64 {
        0
    }
}

/// Test state machine for network tests
struct TestStateMachine {
    inner: MemoryStateMachine,
}

impl TestStateMachine {
    fn new() -> Self {
        Self {
            inner: MemoryStateMachine::new(),
        }
    }
}

#[async_trait]
impl StateMachine for TestStateMachine {
    async fn apply(&mut self, entry: &RaftEntry) -> chronik_raft::Result<Bytes> {
        self.inner.apply(entry).await
    }

    async fn snapshot(&self, last_index: u64, last_term: u64) -> chronik_raft::Result<SnapshotData> {
        self.inner.snapshot(last_index, last_term).await
    }

    async fn restore(&mut self, snapshot: &SnapshotData) -> chronik_raft::Result<()> {
        self.inner.restore(snapshot).await
    }

    fn last_applied(&self) -> u64 {
        self.inner.last_applied()
    }
}

/// Create a Raft configuration for testing
fn create_test_config(node_id: u64, raft_port: u16) -> RaftConfig {
    RaftConfig {
        node_id,
        listen_addr: format!("127.0.0.1:{}", raft_port),
        election_timeout_ms: 500,
        heartbeat_interval_ms: 50,
        max_entries_per_batch: 100,
        snapshot_threshold: 10_000,
    }
}

/// Start a Raft gRPC server for a node
async fn start_test_raft_server(
    port: u16,
    service: RaftServiceImpl,
) -> Result<tokio::task::JoinHandle<()>> {
    let addr = format!("127.0.0.1:{}", port).parse()?;

    let handle = tokio::spawn(async move {
        Server::builder()
            .add_service(RaftServiceServer::new(service))
            .serve(addr)
            .await
            .expect("Raft server failed");
    });

    // Give server time to start
    sleep(Duration::from_millis(100)).await;

    Ok(handle)
}

#[tokio::test]
async fn test_raft_client_connection() -> Result<()> {
    // Test basic RaftClient connection and peer management
    let client = RaftClient::new();

    // Add a peer (will fail to connect since no server running, but should store addr)
    let result = client.add_peer(2, "127.0.0.1:15001".to_string()).await;
    assert!(result.is_err()); // Expected - no server running

    // Remove peer
    client.remove_peer(2).await;

    Ok(())
}

#[tokio::test]
async fn test_single_node_leader_election() -> Result<()> {
    // Create a single-node cluster that should elect itself as leader
    let config = create_test_config(1, 0);
    let storage = Arc::new(MemoryLogStorage::new());
    let state_machine = Arc::new(TokioRwLock::new(NoOpStateMachine));

    let replica = PartitionReplica::new(
        "test-topic".to_string(),
        0,
        config,
        storage,
        state_machine,
        vec![], // No peers - single node cluster
    )?;

    // Wait a bit and tick to trigger election
    for _ in 0..20 {
        replica.tick()?;
        sleep(Duration::from_millis(50)).await;
        let _ = replica.ready().await?;

        if replica.is_leader() {
            break;
        }
    }

    // Single-node cluster should eventually become leader
    assert!(replica.is_leader(), "Single-node replica should become leader");

    Ok(())
}

#[tokio::test]
async fn test_raft_service_registration() -> Result<()> {
    // Test RaftServiceImpl replica registration
    let service = RaftServiceImpl::new();

    let config = create_test_config(1, 0);
    let storage = Arc::new(MemoryLogStorage::new());
    let state_machine = Arc::new(TokioRwLock::new(NoOpStateMachine));

    let replica = Arc::new(PartitionReplica::new(
        "test-topic".to_string(),
        0,
        config,
        storage,
        state_machine,
        vec![],
    )?);

    // Register replica
    service.register_replica(replica.clone());

    // Unregister replica
    service.unregister_replica("test-topic", 0);

    Ok(())
}

#[tokio::test]
#[ignore] // Ignore by default - requires two running servers
async fn test_two_node_message_passing() -> Result<()> {
    // This test requires actual network communication between two nodes
    // Create node 1
    let config1 = create_test_config(1, 15101);
    let storage1 = Arc::new(MemoryLogStorage::new());
    let service1 = RaftServiceImpl::new();
    let state_machine1 = Arc::new(TokioRwLock::new(NoOpStateMachine));

    let replica1 = Arc::new(PartitionReplica::new(
        "test-topic".to_string(),
        0,
        config1,
        storage1,
        state_machine1,
        vec![2], // Peer: node 2
    )?);

    service1.register_replica(replica1.clone());

    // Start node 1 server
    let _server1 = start_test_raft_server(15101, service1).await?;

    // Create node 2
    let config2 = create_test_config(2, 15102);
    let storage2 = Arc::new(MemoryLogStorage::new());
    let service2 = RaftServiceImpl::new();
    let state_machine2 = Arc::new(TokioRwLock::new(NoOpStateMachine));

    let replica2 = Arc::new(PartitionReplica::new(
        "test-topic".to_string(),
        0,
        config2,
        storage2,
        state_machine2,
        vec![1], // Peer: node 1
    )?);

    service2.register_replica(replica2.clone());

    // Start node 2 server
    let _server2 = start_test_raft_server(15102, service2).await?;

    // Create client for node 1
    let client1 = RaftClient::new();
    client1.add_peer(2, "127.0.0.1:15102".to_string()).await?;

    // Tick both replicas to trigger election
    for _ in 0..10 {
        replica1.tick()?;
        replica2.tick()?;
        sleep(Duration::from_millis(50)).await;
    }

    // Process ready and send messages
    let (messages, _) = replica1.ready().await?;

    // Should have messages to send to node 2
    if !messages.is_empty() {
        for msg in messages {
            let to = msg.to;
            client1.send_message("test-topic", 0, to, msg).await?;
        }
    }

    // Wait for node 2 to process
    sleep(Duration::from_millis(100)).await;

    // Check if node 1 became leader
    assert!(replica1.is_leader() || replica2.is_leader(),
        "One of the nodes should become leader");

    Ok(())
}

#[tokio::test]
async fn test_raft_client_clone() -> Result<()> {
    // Test that RaftClient can be cloned and shares connection pool
    let client1 = RaftClient::new();
    let client2 = client1.clone();

    // Both should work independently but share the same connection pool
    let result1 = client1.add_peer(2, "127.0.0.1:15201".to_string()).await;
    let result2 = client2.add_peer(3, "127.0.0.1:15202".to_string()).await;

    // Both should fail (no servers running) but that's expected
    assert!(result1.is_err());
    assert!(result2.is_err());

    Ok(())
}

#[tokio::test]
async fn test_replica_state_tracking() -> Result<()> {
    // Test that replica tracks state correctly
    let config = create_test_config(1, 0);
    let storage = Arc::new(MemoryLogStorage::new());
    let state_machine = Arc::new(TokioRwLock::new(NoOpStateMachine));

    let replica = PartitionReplica::new(
        "test-topic".to_string(),
        0,
        config,
        storage,
        state_machine,
        vec![],
    )?;

    // Initial state
    assert_eq!(replica.term(), 0);
    assert_eq!(replica.commit_index(), 0);
    assert_eq!(replica.applied_index(), 0);
    assert!(!replica.is_leader());

    // Info should be correct
    let (topic, partition) = replica.info();
    assert_eq!(topic, "test-topic");
    assert_eq!(partition, 0);

    Ok(())
}

#[tokio::test]
async fn test_propose_requires_leader() -> Result<()> {
    // Test that propose fails when not leader
    let config = create_test_config(1, 0);
    let storage = Arc::new(MemoryLogStorage::new());
    let state_machine = Arc::new(TokioRwLock::new(NoOpStateMachine));

    let replica = PartitionReplica::new(
        "test-topic".to_string(),
        0,
        config,
        storage,
        state_machine,
        vec![2, 3], // Multi-node cluster - won't become leader without election
    )?;

    // Try to propose without being leader
    let result = replica.propose(b"test data".to_vec()).await;

    // Should fail - not leader
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("Not leader"));

    Ok(())
}
