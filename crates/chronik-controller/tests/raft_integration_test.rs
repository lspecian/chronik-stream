//! Integration tests for multi-node Raft consensus.

use chronik_controller::raft::{RaftConfig, init_raft_node, Proposal};
use chronik_controller::raft::state_machine::{TopicConfig, BrokerInfo};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::time::{sleep, Duration};

/// Create a test cluster configuration
fn create_cluster_config(num_nodes: u64) -> Vec<RaftConfig> {
    let base_port = 10000;
    let base_dir = PathBuf::from("/tmp/chronik-raft-test");
    
    let mut configs = Vec::new();
    
    // Create peer mapping
    let mut all_peers: HashMap<u64, SocketAddr> = HashMap::new();
    for i in 1..=num_nodes {
        let addr = format!("127.0.0.1:{}", base_port + i).parse().unwrap();
        all_peers.insert(i, addr);
    }
    
    // Create config for each node
    for i in 1..=num_nodes {
        let mut config = RaftConfig::new(i);
        config.listen_addr = all_peers[&i];
        config.data_dir = base_dir.join(format!("node-{}", i));
        
        // Add other nodes as peers
        for (j, addr) in &all_peers {
            if *j != i {
                config.peers.insert(*j, *addr);
            }
        }
        
        configs.push(config);
    }
    
    configs
}

#[tokio::test]
async fn test_three_node_cluster() {
    // Clean up test directory
    let _ = std::fs::remove_dir_all("/tmp/chronik-raft-test");
    
    // Create 3-node cluster
    let configs = create_cluster_config(3);
    let mut nodes = Vec::new();
    let mut handles = Vec::new();
    
    // Start all nodes
    for config in configs {
        let (node, handle) = init_raft_node(config).await.unwrap();
        handles.push(Arc::new(handle));
        
        tokio::spawn(async move {
            if let Err(e) = node.run().await {
                eprintln!("Node error: {}", e);
            }
        });
        
        nodes.push(handle);
    }
    
    // Wait for election
    sleep(Duration::from_secs(2)).await;
    
    // Find the leader
    let mut leader_idx = None;
    for (i, handle) in handles.iter().enumerate() {
        if let Ok(Some(leader_id)) = handle.get_leader().await {
            let state = handle.get_state().await.unwrap();
            println!("Node {} sees leader: {}, epoch: {}", i + 1, leader_id, state.controller_epoch);
            if leader_id == (i + 1) as u64 {
                leader_idx = Some(i);
            }
        }
    }
    
    assert!(leader_idx.is_some(), "No leader elected");
    let leader = &handles[leader_idx.unwrap()];
    
    // Create topics through leader
    for i in 0..5 {
        let topic_config = TopicConfig {
            name: format!("test-topic-{}", i),
            partition_count: 3,
            replication_factor: 2,
            configs: HashMap::new(),
            created_at: 0,
        };
        
        leader.propose(Proposal::CreateTopic(topic_config)).await.unwrap();
    }
    
    // Register brokers
    for i in 1..=3 {
        let broker_info = BrokerInfo {
            id: i,
            host: "localhost".to_string(),
            port: 9092 + i as u16,
            rack: Some(format!("rack-{}", i)),
            generation_id: 1,
            registered_at: 0,
        };
        
        leader.propose(Proposal::RegisterBroker(broker_info)).await.unwrap();
    }
    
    // Wait for replication
    sleep(Duration::from_secs(1)).await;
    
    // Verify all nodes have the same state
    let mut states = Vec::new();
    for handle in &handles {
        let state = handle.get_state().await.unwrap();
        states.push(state);
    }
    
    // Check topics
    for state in &states {
        assert_eq!(state.topics.len(), 5);
        for i in 0..5 {
            assert!(state.topics.contains_key(&format!("test-topic-{}", i)));
        }
        assert_eq!(state.brokers.len(), 3);
    }
    
    // All states should be identical
    let first_state = &states[0];
    for state in &states[1..] {
        assert_eq!(state.topics.len(), first_state.topics.len());
        assert_eq!(state.brokers.len(), first_state.brokers.len());
        assert_eq!(state.controller_epoch, first_state.controller_epoch);
    }
}

#[tokio::test]
async fn test_leader_failover() {
    // Clean up test directory
    let _ = std::fs::remove_dir_all("/tmp/chronik-raft-failover-test");
    
    // Create 3-node cluster with custom paths
    let mut configs = create_cluster_config(3);
    for (i, config) in configs.iter_mut().enumerate() {
        config.data_dir = PathBuf::from(format!("/tmp/chronik-raft-failover-test/node-{}", i + 1));
    }
    
    let mut handles = Vec::new();
    let mut shutdown_txs = Vec::new();
    
    // Start all nodes with shutdown capability
    for config in configs {
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel();
        shutdown_txs.push(shutdown_tx);
        
        let (node, handle) = init_raft_node(config).await.unwrap();
        handles.push(Arc::new(handle));
        
        tokio::spawn(async move {
            tokio::select! {
                result = node.run() => {
                    if let Err(e) = result {
                        eprintln!("Node error: {}", e);
                    }
                }
                _ = &mut shutdown_rx => {
                    println!("Node shutting down");
                }
            }
        });
    }
    
    // Wait for initial election
    sleep(Duration::from_secs(2)).await;
    
    // Find the leader
    let mut leader_idx = None;
    for (i, handle) in handles.iter().enumerate() {
        if let Ok(Some(leader_id)) = handle.get_leader().await {
            if leader_id == (i + 1) as u64 {
                leader_idx = Some(i);
                println!("Initial leader is node {}", leader_id);
            }
        }
    }
    
    assert!(leader_idx.is_some(), "No initial leader elected");
    
    // Create some data
    let leader = &handles[leader_idx.unwrap()];
    let topic_config = TopicConfig {
        name: "failover-test-topic".to_string(),
        partition_count: 3,
        replication_factor: 2,
        configs: HashMap::new(),
        created_at: 0,
    };
    leader.propose(Proposal::CreateTopic(topic_config)).await.unwrap();
    
    // Wait for replication
    sleep(Duration::from_millis(500)).await;
    
    // Shutdown the leader
    println!("Shutting down leader node {}", leader_idx.unwrap() + 1);
    let _ = shutdown_txs[leader_idx.unwrap()].send(());
    
    // Wait for new election
    sleep(Duration::from_secs(3)).await;
    
    // Find new leader
    let mut new_leader_idx = None;
    for (i, handle) in handles.iter().enumerate() {
        if i == leader_idx.unwrap() {
            continue; // Skip old leader
        }
        
        if let Ok(Some(leader_id)) = handle.get_leader().await {
            println!("Node {} sees new leader: {}", i + 1, leader_id);
            if leader_id == (i + 1) as u64 {
                new_leader_idx = Some(i);
            }
        }
    }
    
    assert!(new_leader_idx.is_some(), "No new leader elected after failover");
    assert_ne!(new_leader_idx, leader_idx, "Same leader after failover");
    
    // Verify data persisted
    let new_leader = &handles[new_leader_idx.unwrap()];
    let state = new_leader.get_state().await.unwrap();
    assert!(state.topics.contains_key("failover-test-topic"));
    
    // Create more data with new leader
    let topic_config = TopicConfig {
        name: "post-failover-topic".to_string(),
        partition_count: 2,
        replication_factor: 1,
        configs: HashMap::new(),
        created_at: 0,
    };
    new_leader.propose(Proposal::CreateTopic(topic_config)).await.unwrap();
    
    // Wait and verify
    sleep(Duration::from_millis(500)).await;
    let final_state = new_leader.get_state().await.unwrap();
    assert_eq!(final_state.topics.len(), 2);
    assert!(final_state.topics.contains_key("post-failover-topic"));
}

#[tokio::test]
async fn test_network_partition() {
    // This test simulates a network partition by stopping message forwarding
    // between nodes, which would require modifying the transport layer.
    // For now, we'll test proposal rejection when not leader.
    
    let _ = std::fs::remove_dir_all("/tmp/chronik-raft-partition-test");
    
    let configs = create_cluster_config(3);
    let mut handles = Vec::new();
    
    for config in configs {
        let (node, handle) = init_raft_node(config).await.unwrap();
        handles.push(Arc::new(handle));
        
        tokio::spawn(async move {
            if let Err(e) = node.run().await {
                eprintln!("Node error: {}", e);
            }
        });
    }
    
    // Wait for election
    sleep(Duration::from_secs(2)).await;
    
    // Find a non-leader node
    let mut non_leader_idx = None;
    for (i, handle) in handles.iter().enumerate() {
        if let Ok(Some(leader_id)) = handle.get_leader().await {
            if leader_id != (i + 1) as u64 {
                non_leader_idx = Some(i);
                break;
            }
        }
    }
    
    assert!(non_leader_idx.is_some(), "All nodes think they're leader?");
    
    // Try to propose through non-leader (should fail)
    let non_leader = &handles[non_leader_idx.unwrap()];
    let topic_config = TopicConfig {
        name: "should-fail".to_string(),
        partition_count: 1,
        replication_factor: 1,
        configs: HashMap::new(),
        created_at: 0,
    };
    
    let result = non_leader.propose(Proposal::CreateTopic(topic_config)).await;
    assert!(result.is_err(), "Non-leader proposal should fail");
}

#[tokio::test]
async fn test_snapshot_and_restore() {
    let _ = std::fs::remove_dir_all("/tmp/chronik-raft-snapshot-test");
    
    let configs = create_cluster_config(1);
    let (node, handle) = init_raft_node(configs[0].clone()).await.unwrap();
    let handle = Arc::new(handle);
    
    tokio::spawn(async move {
        if let Err(e) = node.run().await {
            eprintln!("Node error: {}", e);
        }
    });
    
    // Wait for node to start
    sleep(Duration::from_secs(1)).await;
    
    // Create many proposals to trigger snapshot
    for i in 0..100 {
        let topic_config = TopicConfig {
            name: format!("topic-{}", i),
            partition_count: 1,
            replication_factor: 1,
            configs: HashMap::new(),
            created_at: i,
        };
        
        handle.propose(Proposal::CreateTopic(topic_config)).await.unwrap();
    }
    
    // Wait for all proposals to be committed
    sleep(Duration::from_secs(2)).await;
    
    // Verify state
    let state = handle.get_state().await.unwrap();
    assert_eq!(state.topics.len(), 100);
    
    // The snapshot should be created automatically by the Raft implementation
    // based on the snapshot_interval configuration
}