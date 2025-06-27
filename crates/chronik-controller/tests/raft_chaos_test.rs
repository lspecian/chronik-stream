//! Chaos tests for Raft implementation.

use chronik_controller::raft::{RaftConfig, init_raft_node, Proposal};
use chronik_controller::raft::state_machine::{TopicConfig, BrokerInfo, TopicPartition};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{sleep, Duration, timeout};
use rand::Rng;

/// Test harness for chaos testing
struct ChaosTestHarness {
    nodes: Vec<Arc<chronik_controller::raft::RaftHandle>>,
    shutdown_txs: Vec<tokio::sync::oneshot::Sender<()>>,
    node_configs: Vec<RaftConfig>,
    running: Arc<Mutex<Vec<bool>>>,
}

impl ChaosTestHarness {
    /// Create a new test harness
    async fn new(num_nodes: usize, base_path: &str) -> Self {
        let _ = std::fs::remove_dir_all(base_path);
        
        let base_port = 20000;
        let mut node_configs = Vec::new();
        let mut handles = Vec::new();
        let mut shutdown_txs = Vec::new();
        let running = Arc::new(Mutex::new(vec![true; num_nodes]));
        
        // Create peer mapping
        let mut all_peers: HashMap<u64, SocketAddr> = HashMap::new();
        for i in 1..=num_nodes {
            let addr = format!("127.0.0.1:{}", base_port + i).parse().unwrap();
            all_peers.insert(i as u64, addr);
        }
        
        // Start all nodes
        for i in 1..=num_nodes {
            let mut config = RaftConfig::new(i as u64);
            config.listen_addr = all_peers[&(i as u64)];
            config.data_dir = PathBuf::from(format!("{}/node-{}", base_path, i));
            
            // Add other nodes as peers
            for (j, addr) in &all_peers {
                if *j != i as u64 {
                    config.peers.insert(*j, *addr);
                }
            }
            
            node_configs.push(config.clone());
            
            let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel();
            shutdown_txs.push(shutdown_tx);
            
            let (node, handle) = init_raft_node(config).await.unwrap();
            handles.push(Arc::new(handle));
            
            let node_id = i;
            let running = running.clone();
            tokio::spawn(async move {
                tokio::select! {
                    result = node.run() => {
                        if let Err(e) = result {
                            eprintln!("Node {} error: {}", node_id, e);
                        }
                    }
                    _ = &mut shutdown_rx => {
                        println!("Node {} shutting down", node_id);
                        let mut running = running.lock().await;
                        running[node_id - 1] = false;
                    }
                }
            });
        }
        
        Self {
            nodes: handles,
            shutdown_txs,
            node_configs,
            running,
        }
    }
    
    /// Kill a random node
    async fn kill_random_node(&mut self) -> Option<usize> {
        let running = self.running.lock().await;
        let running_nodes: Vec<usize> = running.iter()
            .enumerate()
            .filter(|(_, &r)| r)
            .map(|(i, _)| i)
            .collect();
        drop(running);
        
        if running_nodes.is_empty() {
            return None;
        }
        
        let mut rng = rand::thread_rng();
        let idx = running_nodes[rng.gen_range(0..running_nodes.len())];
        
        if let Some(tx) = self.shutdown_txs.get_mut(idx) {
            if let Some(tx) = std::mem::replace(tx, tokio::sync::oneshot::channel().0) {
                let _ = tx.send(());
                println!("Killed node {}", idx + 1);
                return Some(idx);
            }
        }
        
        None
    }
    
    /// Restart a node
    async fn restart_node(&mut self, idx: usize) {
        let running = self.running.lock().await;
        if running[idx] {
            println!("Node {} is already running", idx + 1);
            return;
        }
        drop(running);
        
        let config = self.node_configs[idx].clone();
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel();
        self.shutdown_txs[idx] = shutdown_tx;
        
        let (node, handle) = init_raft_node(config).await.unwrap();
        self.nodes[idx] = Arc::new(handle);
        
        let node_id = idx + 1;
        let running = self.running.clone();
        tokio::spawn(async move {
            tokio::select! {
                result = node.run() => {
                    if let Err(e) = result {
                        eprintln!("Node {} error: {}", node_id, e);
                    }
                }
                _ = &mut shutdown_rx => {
                    println!("Node {} shutting down", node_id);
                    let mut running = running.lock().await;
                    running[node_id - 1] = false;
                }
            }
        });
        
        let mut running = self.running.lock().await;
        running[idx] = true;
        
        println!("Restarted node {}", idx + 1);
    }
    
    /// Find the current leader
    async fn find_leader(&self) -> Option<(usize, u64)> {
        for (i, node) in self.nodes.iter().enumerate() {
            let running = self.running.lock().await;
            if !running[i] {
                continue;
            }
            drop(running);
            
            if let Ok(Some(leader_id)) = node.get_leader().await {
                if leader_id == (i + 1) as u64 {
                    return Some((i, leader_id));
                }
            }
        }
        None
    }
    
    /// Get a running node handle
    async fn get_running_node(&self) -> Option<&Arc<chronik_controller::raft::RaftHandle>> {
        let running = self.running.lock().await;
        for (i, node) in self.nodes.iter().enumerate() {
            if running[i] {
                return Some(node);
            }
        }
        None
    }
}

#[tokio::test]
async fn test_random_node_failures() {
    let mut harness = ChaosTestHarness::new(5, "/tmp/chronik-chaos-test").await;
    
    // Wait for initial cluster formation
    sleep(Duration::from_secs(3)).await;
    
    // Verify initial leader election
    let initial_leader = harness.find_leader().await;
    assert!(initial_leader.is_some(), "No initial leader elected");
    println!("Initial leader: node {}", initial_leader.unwrap().1);
    
    // Create initial data
    if let Some((leader_idx, _)) = initial_leader {
        let leader = &harness.nodes[leader_idx];
        for i in 0..10 {
            let topic_config = TopicConfig {
                name: format!("chaos-topic-{}", i),
                partition_count: 3,
                replication_factor: 3,
                configs: HashMap::new(),
                created_at: i,
            };
            leader.propose(Proposal::CreateTopic(topic_config)).await.unwrap();
        }
    }
    
    // Run chaos test: randomly kill and restart nodes
    let mut rng = rand::thread_rng();
    for round in 0..10 {
        println!("\n=== Chaos round {} ===", round + 1);
        
        // Random action: kill, restart, or do nothing
        let action = rng.gen_range(0..3);
        
        match action {
            0 => {
                // Kill a random node
                if let Some(killed_idx) = harness.kill_random_node().await {
                    sleep(Duration::from_secs(2)).await;
                    
                    // Verify cluster still functions
                    if let Some(node) = harness.get_running_node().await {
                        let result = timeout(
                            Duration::from_secs(5),
                            node.get_state()
                        ).await;
                        
                        assert!(result.is_ok(), "Cluster not responding after killing node");
                    }
                }
            }
            1 => {
                // Restart a dead node
                let running = harness.running.lock().await;
                let dead_nodes: Vec<usize> = running.iter()
                    .enumerate()
                    .filter(|(_, &r)| !r)
                    .map(|(i, _)| i)
                    .collect();
                drop(running);
                
                if !dead_nodes.is_empty() {
                    let idx = dead_nodes[rng.gen_range(0..dead_nodes.len())];
                    harness.restart_node(idx).await;
                    sleep(Duration::from_secs(2)).await;
                }
            }
            _ => {
                // Do nothing, just verify state
                println!("Verifying cluster state...");
            }
        }
        
        // Try to make progress
        if let Some(leader) = harness.find_leader().await {
            let leader_node = &harness.nodes[leader.0];
            let topic_config = TopicConfig {
                name: format!("chaos-topic-round-{}", round),
                partition_count: 1,
                replication_factor: 1,
                configs: HashMap::new(),
                created_at: round as u64,
            };
            
            let result = timeout(
                Duration::from_secs(3),
                leader_node.propose(Proposal::CreateTopic(topic_config))
            ).await;
            
            if result.is_ok() {
                println!("Successfully created topic in round {}", round);
            } else {
                println!("Failed to create topic in round {} (expected during chaos)", round);
            }
        }
        
        sleep(Duration::from_millis(500)).await;
    }
    
    // Restart all dead nodes
    for i in 0..harness.nodes.len() {
        let running = harness.running.lock().await;
        if !running[i] {
            drop(running);
            harness.restart_node(i).await;
        }
    }
    
    // Wait for cluster to stabilize
    sleep(Duration::from_secs(5)).await;
    
    // Verify final state consistency
    let mut states = Vec::new();
    for node in &harness.nodes {
        if let Ok(state) = node.get_state().await {
            states.push(state);
        }
    }
    
    assert!(!states.is_empty(), "No nodes responded with state");
    
    // All nodes should have the same number of topics
    let first_topic_count = states[0].topics.len();
    for state in &states[1..] {
        assert_eq!(
            state.topics.len(), 
            first_topic_count, 
            "Inconsistent topic count across nodes"
        );
    }
    
    println!("\nFinal topic count: {}", first_topic_count);
}

#[tokio::test]
async fn test_concurrent_proposals() {
    let harness = ChaosTestHarness::new(3, "/tmp/chronik-concurrent-test").await;
    
    // Wait for cluster formation
    sleep(Duration::from_secs(2)).await;
    
    // Find leader
    let leader = harness.find_leader().await.unwrap();
    let leader_node = &harness.nodes[leader.0];
    
    // Submit many proposals concurrently
    let mut handles = Vec::new();
    for i in 0..50 {
        let node = leader_node.clone();
        let handle = tokio::spawn(async move {
            let topic_config = TopicConfig {
                name: format!("concurrent-topic-{}", i),
                partition_count: 3,
                replication_factor: 2,
                configs: HashMap::new(),
                created_at: i,
            };
            
            node.propose(Proposal::CreateTopic(topic_config)).await
        });
        handles.push(handle);
    }
    
    // Wait for all proposals
    let mut success_count = 0;
    for handle in handles {
        if handle.await.unwrap().is_ok() {
            success_count += 1;
        }
    }
    
    println!("Successfully proposed {}/50 topics", success_count);
    assert!(success_count >= 45, "Too many failed proposals");
    
    // Verify final state
    sleep(Duration::from_secs(1)).await;
    let state = leader_node.get_state().await.unwrap();
    assert_eq!(state.topics.len(), success_count);
}

#[tokio::test]
async fn test_large_state_machine() {
    let harness = ChaosTestHarness::new(3, "/tmp/chronik-large-state-test").await;
    
    // Wait for cluster formation
    sleep(Duration::from_secs(2)).await;
    
    let leader = harness.find_leader().await.unwrap();
    let leader_node = &harness.nodes[leader.0];
    
    // Create many topics
    for batch in 0..10 {
        let mut proposals = Vec::new();
        for i in 0..100 {
            let topic_num = batch * 100 + i;
            let topic_config = TopicConfig {
                name: format!("large-state-topic-{}", topic_num),
                partition_count: 10,
                replication_factor: 3,
                configs: vec![
                    ("retention.ms".to_string(), "86400000".to_string()),
                    ("segment.bytes".to_string(), "1073741824".to_string()),
                ].into_iter().collect(),
                created_at: topic_num,
            };
            proposals.push(Proposal::CreateTopic(topic_config));
        }
        
        // Submit batch
        for proposal in proposals {
            let _ = leader_node.propose(proposal).await;
        }
        
        println!("Created batch {} (100 topics)", batch + 1);
        sleep(Duration::from_millis(500)).await;
    }
    
    // Register many brokers
    for i in 1..=100 {
        let broker_info = BrokerInfo {
            id: i,
            host: format!("broker-{}.example.com", i),
            port: 9092,
            rack: Some(format!("rack-{}", (i - 1) / 10 + 1)),
            generation_id: 1,
            registered_at: i as u64,
        };
        let _ = leader_node.propose(Proposal::RegisterBroker(broker_info)).await;
    }
    
    // Create partition assignments
    for topic_id in 0..100 {
        for partition in 0..10 {
            let tp = TopicPartition::new(
                format!("large-state-topic-{}", topic_id),
                partition,
            );
            let leader_id = (topic_id * 10 + partition) % 100 + 1;
            let _ = leader_node.propose(Proposal::UpdatePartitionLeader {
                topic_partition: tp.clone(),
                leader: leader_id,
                leader_epoch: 1,
            }).await;
            
            // Create ISR
            let isr = vec![leader_id, (leader_id % 100) + 1, ((leader_id + 1) % 100) + 1];
            let _ = leader_node.propose(Proposal::UpdateIsr {
                topic_partition: tp,
                isr,
                leader_epoch: 1,
            }).await;
        }
    }
    
    // Wait for all operations to complete
    sleep(Duration::from_secs(5)).await;
    
    // Verify large state
    let state = leader_node.get_state().await.unwrap();
    assert_eq!(state.topics.len(), 1000, "Should have 1000 topics");
    assert_eq!(state.brokers.len(), 100, "Should have 100 brokers");
    assert!(!state.partition_leaders.is_empty(), "Should have partition leaders");
    assert!(!state.isr_lists.is_empty(), "Should have ISR lists");
    
    println!("Large state test completed:");
    println!("- Topics: {}", state.topics.len());
    println!("- Brokers: {}", state.brokers.len());
    println!("- Partition leaders: {}", state.partition_leaders.len());
    println!("- ISR lists: {}", state.isr_lists.len());
}