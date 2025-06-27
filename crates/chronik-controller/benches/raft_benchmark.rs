//! Benchmarks for Raft consensus implementation.

use chronik_controller::raft::{RaftConfig, init_raft_node, Proposal};
use chronik_controller::raft::state_machine::TopicConfig;
use criterion::{black_box, criterion_group, criterion_main, Criterion, BenchmarkId};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::Runtime;

/// Benchmark single proposal throughput
fn bench_single_proposal(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    
    c.bench_function("single_proposal", |b| {
        // Setup
        let _ = std::fs::remove_dir_all("/tmp/chronik-bench-single");
        let mut config = RaftConfig::new(1);
        config.data_dir = PathBuf::from("/tmp/chronik-bench-single");
        
        let (node, handle) = rt.block_on(init_raft_node(config)).unwrap();
        let handle = Arc::new(handle);
        
        rt.spawn(async move {
            let _ = node.run().await;
        });
        
        // Wait for node to be ready
        std::thread::sleep(Duration::from_millis(500));
        
        let mut counter = 0;
        
        b.iter(|| {
            let topic_config = TopicConfig {
                name: format!("bench-topic-{}", counter),
                partition_count: 3,
                replication_factor: 1,
                configs: HashMap::new(),
                created_at: counter,
            };
            counter += 1;
            
            rt.block_on(handle.propose(Proposal::CreateTopic(topic_config))).unwrap();
        });
    });
}

/// Benchmark batch proposal throughput
fn bench_batch_proposals(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    
    let mut group = c.benchmark_group("batch_proposals");
    for batch_size in [10, 50, 100, 500].iter() {
        group.bench_with_input(
            BenchmarkId::from_parameter(batch_size),
            batch_size,
            |b, &batch_size| {
                // Setup
                let _ = std::fs::remove_dir_all("/tmp/chronik-bench-batch");
                let mut config = RaftConfig::new(1);
                config.data_dir = PathBuf::from("/tmp/chronik-bench-batch");
                
                let (node, handle) = rt.block_on(init_raft_node(config)).unwrap();
                let handle = Arc::new(handle);
                
                rt.spawn(async move {
                    let _ = node.run().await;
                });
                
                // Wait for node to be ready
                std::thread::sleep(Duration::from_millis(500));
                
                let mut counter = 0;
                
                b.iter(|| {
                    let proposals: Vec<_> = (0..batch_size)
                        .map(|i| {
                            let topic_config = TopicConfig {
                                name: format!("batch-topic-{}-{}", counter, i),
                                partition_count: 3,
                                replication_factor: 1,
                                configs: HashMap::new(),
                                created_at: counter * batch_size + i,
                            };
                            Proposal::CreateTopic(topic_config)
                        })
                        .collect();
                    counter += 1;
                    
                    for proposal in proposals {
                        rt.block_on(handle.propose(proposal)).unwrap();
                    }
                });
            },
        );
    }
    group.finish();
}

/// Benchmark multi-node consensus
fn bench_multi_node_consensus(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    
    let mut group = c.benchmark_group("multi_node_consensus");
    group.sample_size(10); // Reduce sample size for slower tests
    
    for num_nodes in [3, 5, 7].iter() {
        group.bench_with_input(
            BenchmarkId::from_parameter(num_nodes),
            num_nodes,
            |b, &num_nodes| {
                // Setup cluster
                let _ = std::fs::remove_dir_all("/tmp/chronik-bench-cluster");
                let base_port = 30000;
                
                let mut all_peers: HashMap<u64, std::net::SocketAddr> = HashMap::new();
                for i in 1..=num_nodes {
                    let addr = format!("127.0.0.1:{}", base_port + i).parse().unwrap();
                    all_peers.insert(i as u64, addr);
                }
                
                let mut handles = Vec::new();
                
                for i in 1..=num_nodes {
                    let mut config = RaftConfig::new(i as u64);
                    config.listen_addr = all_peers[&(i as u64)];
                    config.data_dir = PathBuf::from(format!("/tmp/chronik-bench-cluster/node-{}", i));
                    
                    for (j, addr) in &all_peers {
                        if *j != i as u64 {
                            config.peers.insert(*j, *addr);
                        }
                    }
                    
                    let (node, handle) = rt.block_on(init_raft_node(config)).unwrap();
                    handles.push(Arc::new(handle));
                    
                    rt.spawn(async move {
                        let _ = node.run().await;
                    });
                }
                
                // Wait for cluster formation
                std::thread::sleep(Duration::from_secs(2));
                
                // Find leader
                let mut leader_idx = None;
                for (i, handle) in handles.iter().enumerate() {
                    if let Ok(Some(leader_id)) = rt.block_on(handle.get_leader()) {
                        if leader_id == (i + 1) as u64 {
                            leader_idx = Some(i);
                            break;
                        }
                    }
                }
                
                let leader = &handles[leader_idx.unwrap()];
                let mut counter = 0;
                
                b.iter(|| {
                    let topic_config = TopicConfig {
                        name: format!("cluster-topic-{}", counter),
                        partition_count: 3,
                        replication_factor: num_nodes as i32,
                        configs: HashMap::new(),
                        created_at: counter,
                    };
                    counter += 1;
                    
                    rt.block_on(leader.propose(Proposal::CreateTopic(topic_config))).unwrap();
                });
            },
        );
    }
    group.finish();
}

/// Benchmark state machine operations
fn bench_state_machine(c: &mut Criterion) {
    use chronik_controller::raft::state_machine::{
        ControllerStateMachine, BrokerInfo, TopicPartition,
    };
    
    let mut group = c.benchmark_group("state_machine");
    
    // Benchmark topic creation
    group.bench_function("create_topic", |b| {
        let state_machine = ControllerStateMachine::new();
        let mut counter = 0;
        
        b.iter(|| {
            let topic_config = TopicConfig {
                name: format!("sm-topic-{}", counter),
                partition_count: 10,
                replication_factor: 3,
                configs: HashMap::new(),
                created_at: counter,
            };
            counter += 1;
            
            state_machine.apply(Proposal::CreateTopic(topic_config)).unwrap();
        });
    });
    
    // Benchmark partition leader updates
    group.bench_function("update_partition_leader", |b| {
        let state_machine = ControllerStateMachine::new();
        
        // Create topics first
        for i in 0..100 {
            let topic_config = TopicConfig {
                name: format!("leader-topic-{}", i),
                partition_count: 10,
                replication_factor: 3,
                configs: HashMap::new(),
                created_at: i,
            };
            state_machine.apply(Proposal::CreateTopic(topic_config)).unwrap();
        }
        
        let mut counter = 0;
        
        b.iter(|| {
            let topic_id = counter % 100;
            let partition = (counter / 100) % 10;
            let leader = (counter % 10) + 1;
            
            let tp = TopicPartition::new(
                format!("leader-topic-{}", topic_id),
                partition as i32,
            );
            
            state_machine.apply(Proposal::UpdatePartitionLeader {
                topic_partition: tp,
                leader: leader as i32,
                leader_epoch: 1,
            }).unwrap();
            
            counter += 1;
        });
    });
    
    // Benchmark snapshot creation
    group.bench_function("snapshot", |b| {
        let state_machine = ControllerStateMachine::new();
        
        // Create substantial state
        for i in 0..1000 {
            let topic_config = TopicConfig {
                name: format!("snap-topic-{}", i),
                partition_count: 10,
                replication_factor: 3,
                configs: vec![
                    ("retention.ms".to_string(), "86400000".to_string()),
                    ("segment.bytes".to_string(), "1073741824".to_string()),
                ].into_iter().collect(),
                created_at: i,
            };
            state_machine.apply(Proposal::CreateTopic(topic_config)).unwrap();
        }
        
        for i in 1..=100 {
            let broker_info = BrokerInfo {
                id: i,
                host: format!("broker-{}.example.com", i),
                port: 9092,
                rack: Some(format!("rack-{}", (i - 1) / 10 + 1)),
                generation_id: 1,
                registered_at: i as u64,
            };
            state_machine.apply(Proposal::RegisterBroker(broker_info)).unwrap();
        }
        
        b.iter(|| {
            black_box(state_machine.snapshot());
        });
    });
    
    group.finish();
}

criterion_group!(
    benches,
    bench_single_proposal,
    bench_batch_proposals,
    bench_multi_node_consensus,
    bench_state_machine
);
criterion_main!(benches);