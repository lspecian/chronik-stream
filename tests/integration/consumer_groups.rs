//! Consumer group coordination tests

use super::common::*;
use chronik_common::Result;
use rdkafka::{
    ClientConfig,
    Message,
    admin::{AdminClient, AdminOptions, NewTopic, TopicReplication},
    consumer::{Consumer, StreamConsumer, CommitMode},
    producer::{FutureProducer, FutureRecord},
};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::{sleep, timeout};

#[tokio::test]
async fn test_consumer_group_rebalance() -> Result<()> {
    super::test_setup::init();
    
    let cluster = TestCluster::start(TestClusterConfig::default()).await?;
    let bootstrap_servers = cluster.bootstrap_servers();
    
    // Create topic with multiple partitions
    let admin: AdminClient<_> = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .create()
        .expect("Failed to create admin client");
    
    let topic = NewTopic::new("test-rebalance", 6, TopicReplication::Fixed(1));
    admin
        .create_topics(&[topic], &AdminOptions::new())
        .await
        .expect("Failed to create topics")[0]
        .expect("Failed to create topic");
    
    // Produce test data
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .create()
        .expect("Failed to create producer");
    
    for i in 0..60 {
        producer
            .send(
                FutureRecord::to("test-rebalance")
                    .key(&format!("key-{}", i))
                    .payload(&format!("value-{}", i))
                    .partition(i % 6),
                Duration::from_secs(5),
            )
            .await
            .expect("Failed to produce");
    }
    
    // Track which partitions are assigned to which consumer
    let partition_assignments = Arc::new(Mutex::new(HashMap::new()));
    
    // Start first consumer
    let consumer1: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("group.id", "rebalance-test-group")
        .set("client.id", "consumer-1")
        .set("enable.auto.commit", "false")
        .set("auto.offset.reset", "earliest")
        .create()
        .expect("Failed to create consumer");
    
    consumer1
        .subscribe(&["test-rebalance"])
        .expect("Failed to subscribe");
    
    // Let first consumer stabilize
    sleep(Duration::from_secs(2)).await;
    
    // Verify first consumer gets all partitions
    let mut consumer1_partitions = HashSet::new();
    let start = std::time::Instant::now();
    
    while consumer1_partitions.len() < 6 && start.elapsed() < Duration::from_secs(5) {
        match timeout(Duration::from_millis(100), consumer1.recv()).await {
            Ok(Ok(message)) => {
                consumer1_partitions.insert(message.partition());
            }
            _ => continue,
        }
    }
    
    assert_eq!(consumer1_partitions.len(), 6, "First consumer should get all partitions");
    
    // Start second consumer - should trigger rebalance
    let consumer2: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("group.id", "rebalance-test-group")
        .set("client.id", "consumer-2")
        .set("enable.auto.commit", "false")
        .set("auto.offset.reset", "earliest")
        .create()
        .expect("Failed to create consumer");
    
    consumer2
        .subscribe(&["test-rebalance"])
        .expect("Failed to subscribe");
    
    // Wait for rebalance
    sleep(Duration::from_secs(3)).await;
    
    // Verify partitions are distributed
    let mut assignments = partition_assignments.lock().await;
    assignments.clear();
    
    // Collect partition assignments
    let consumers = vec![
        ("consumer-1", &consumer1),
        ("consumer-2", &consumer2),
    ];
    
    for (name, consumer) in &consumers {
        let mut consumer_partitions = HashSet::new();
        let collect_start = std::time::Instant::now();
        
        while collect_start.elapsed() < Duration::from_secs(2) {
            match timeout(Duration::from_millis(100), consumer.recv()).await {
                Ok(Ok(message)) => {
                    consumer_partitions.insert(message.partition());
                }
                _ => continue,
            }
        }
        
        assignments.insert(name.to_string(), consumer_partitions);
    }
    
    // Verify each consumer has some partitions
    assert!(!assignments["consumer-1"].is_empty());
    assert!(!assignments["consumer-2"].is_empty());
    
    // Verify no partition overlap
    let c1_parts = &assignments["consumer-1"];
    let c2_parts = &assignments["consumer-2"];
    assert!(c1_parts.is_disjoint(c2_parts));
    
    // Verify all partitions are covered
    let all_assigned: HashSet<_> = c1_parts.union(c2_parts).copied().collect();
    assert_eq!(all_assigned.len(), 6);
    
    // Start third consumer
    let consumer3: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("group.id", "rebalance-test-group")
        .set("client.id", "consumer-3")
        .set("enable.auto.commit", "false")
        .set("auto.offset.reset", "earliest")
        .create()
        .expect("Failed to create consumer");
    
    consumer3
        .subscribe(&["test-rebalance"])
        .expect("Failed to subscribe");
    
    // Wait for rebalance
    sleep(Duration::from_secs(3)).await;
    
    // Verify partitions are re-distributed among three consumers
    assignments.clear();
    
    let consumers = vec![
        ("consumer-1", &consumer1),
        ("consumer-2", &consumer2),
        ("consumer-3", &consumer3),
    ];
    
    for (name, consumer) in &consumers {
        let mut consumer_partitions = HashSet::new();
        let collect_start = std::time::Instant::now();
        
        while collect_start.elapsed() < Duration::from_secs(2) {
            match timeout(Duration::from_millis(100), consumer.recv()).await {
                Ok(Ok(message)) => {
                    consumer_partitions.insert(message.partition());
                }
                _ => continue,
            }
        }
        
        assignments.insert(name.to_string(), consumer_partitions);
    }
    
    // Each consumer should have ~2 partitions
    for (name, partitions) in assignments.iter() {
        assert!(partitions.len() >= 1 && partitions.len() <= 3,
            "{} has {} partitions", name, partitions.len());
    }
    
    Ok(())
}

#[tokio::test]
async fn test_consumer_group_offset_commit() -> Result<()> {
    super::test_setup::init();
    
    let cluster = TestCluster::start(TestClusterConfig::default()).await?;
    let bootstrap_servers = cluster.bootstrap_servers();
    
    // Create topic
    let admin: AdminClient<_> = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .create()
        .expect("Failed to create admin client");
    
    let topic = NewTopic::new("test-offset-commit", 3, TopicReplication::Fixed(1));
    admin
        .create_topics(&[topic], &AdminOptions::new())
        .await
        .expect("Failed to create topics")[0]
        .expect("Failed to create topic");
    
    // Produce messages
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .create()
        .expect("Failed to create producer");
    
    for i in 0..30 {
        producer
            .send(
                FutureRecord::to("test-offset-commit")
                    .key(&format!("key-{}", i))
                    .payload(&format!("value-{}", i))
                    .partition(i % 3),
                Duration::from_secs(5),
            )
            .await
            .expect("Failed to produce");
    }
    
    // First consumer - process and commit offsets
    let consumer1: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("group.id", "offset-test-group")
        .set("enable.auto.commit", "false")
        .set("auto.offset.reset", "earliest")
        .create()
        .expect("Failed to create consumer");
    
    consumer1
        .subscribe(&["test-offset-commit"])
        .expect("Failed to subscribe");
    
    // Process first 15 messages
    let mut processed = 0;
    let mut offsets_to_commit = HashMap::new();
    
    while processed < 15 {
        match timeout(Duration::from_secs(1), consumer1.recv()).await {
            Ok(Ok(message)) => {
                let partition = message.partition();
                let offset = message.offset();
                
                // Track highest offset per partition
                offsets_to_commit.insert(partition, offset + 1);
                processed += 1;
                
                // Store offset for commit
                consumer1.store_offset_from_message(&message)
                    .expect("Failed to store offset");
            }
            _ => continue,
        }
    }
    
    // Commit offsets
    consumer1.commit_consumer_state(CommitMode::Sync)
        .expect("Failed to commit offsets");
    
    drop(consumer1);
    
    // Second consumer - should start from committed offsets
    let consumer2: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("group.id", "offset-test-group")
        .set("enable.auto.commit", "false")
        .set("auto.offset.reset", "earliest")
        .create()
        .expect("Failed to create consumer");
    
    consumer2
        .subscribe(&["test-offset-commit"])
        .expect("Failed to subscribe");
    
    // Should receive messages 15-29
    let mut received = Vec::new();
    let start = std::time::Instant::now();
    
    while received.len() < 15 && start.elapsed() < Duration::from_secs(5) {
        match timeout(Duration::from_secs(1), consumer2.recv()).await {
            Ok(Ok(message)) => {
                let key = message.key_view::<str>().unwrap().unwrap();
                received.push(key.to_string());
            }
            _ => continue,
        }
    }
    
    assert_eq!(received.len(), 15);
    
    // Verify we got messages 15-29
    let mut expected_keys: HashSet<_> = (15..30).map(|i| format!("key-{}", i)).collect();
    for key in received {
        assert!(expected_keys.remove(&key), "Unexpected key: {}", key);
    }
    assert!(expected_keys.is_empty(), "Missing keys: {:?}", expected_keys);
    
    Ok(())
}

#[tokio::test]
async fn test_consumer_group_failure_handling() -> Result<()> {
    super::test_setup::init();
    
    let cluster = TestCluster::start(TestClusterConfig::default()).await?;
    let bootstrap_servers = cluster.bootstrap_servers();
    
    // Create topic
    let admin: AdminClient<_> = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .create()
        .expect("Failed to create admin client");
    
    let topic = NewTopic::new("test-failure", 4, TopicReplication::Fixed(1));
    admin
        .create_topics(&[topic], &AdminOptions::new())
        .await
        .expect("Failed to create topics")[0]
        .expect("Failed to create topic");
    
    // Produce messages continuously
    let producer_handle = tokio::spawn(async move {
        let producer: FutureProducer = ClientConfig::new()
            .set("bootstrap.servers", &bootstrap_servers)
            .create()
            .expect("Failed to create producer");
        
        let mut i = 0;
        loop {
            let _ = producer
                .send(
                    FutureRecord::to("test-failure")
                        .key(&format!("key-{}", i))
                        .payload(&format!("value-{}", i)),
                    Duration::from_secs(1),
                )
                .await;
            
            i += 1;
            sleep(Duration::from_millis(100)).await;
        }
    });
    
    // Start two consumers
    let consumer1: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", cluster.bootstrap_servers())
        .set("group.id", "failure-test-group")
        .set("client.id", "consumer-1")
        .set("session.timeout.ms", "10000")
        .set("heartbeat.interval.ms", "3000")
        .set("enable.auto.commit", "true")
        .set("auto.commit.interval.ms", "1000")
        .set("auto.offset.reset", "latest")
        .create()
        .expect("Failed to create consumer");
    
    consumer1
        .subscribe(&["test-failure"])
        .expect("Failed to subscribe");
    
    let consumer2: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", cluster.bootstrap_servers())
        .set("group.id", "failure-test-group")
        .set("client.id", "consumer-2")
        .set("session.timeout.ms", "10000")
        .set("heartbeat.interval.ms", "3000")
        .set("enable.auto.commit", "true")
        .set("auto.commit.interval.ms", "1000")
        .set("auto.offset.reset", "latest")
        .create()
        .expect("Failed to create consumer");
    
    consumer2
        .subscribe(&["test-failure"])
        .expect("Failed to subscribe");
    
    // Let them stabilize
    sleep(Duration::from_secs(2)).await;
    
    // Track partition ownership
    let partitions_c1 = Arc::new(Mutex::new(HashSet::new()));
    let partitions_c2 = Arc::new(Mutex::new(HashSet::new()));
    
    // Consumer 1 processing
    let p1 = partitions_c1.clone();
    let c1_handle = tokio::spawn(async move {
        for _ in 0..50 {
            match timeout(Duration::from_millis(100), consumer1.recv()).await {
                Ok(Ok(message)) => {
                    p1.lock().await.insert(message.partition());
                }
                _ => continue,
            }
        }
        // Simulate failure by dropping consumer
        drop(consumer1);
    });
    
    // Consumer 2 processing
    let p2 = partitions_c2.clone();
    let c2_handle = tokio::spawn(async move {
        let mut message_count = 0;
        let start = std::time::Instant::now();
        
        while start.elapsed() < Duration::from_secs(10) {
            match timeout(Duration::from_millis(100), consumer2.recv()).await {
                Ok(Ok(message)) => {
                    p2.lock().await.insert(message.partition());
                    message_count += 1;
                }
                _ => continue,
            }
        }
        
        message_count
    });
    
    // Wait for consumer 1 to "fail"
    c1_handle.await.unwrap();
    
    // Wait for rebalance (consumer 2 should take over)
    sleep(Duration::from_secs(5)).await;
    
    // Consumer 2 should now have all partitions
    let final_count = c2_handle.await.unwrap();
    let c2_partitions = partitions_c2.lock().await;
    
    assert_eq!(c2_partitions.len(), 4, "Consumer 2 should have all 4 partitions after rebalance");
    assert!(final_count > 50, "Consumer 2 should continue processing after rebalance");
    
    producer_handle.abort();
    
    Ok(())
}

#[tokio::test]
async fn test_consumer_group_incremental_rebalance() -> Result<()> {
    super::test_setup::init();
    
    let cluster = TestCluster::start(TestClusterConfig::default()).await?;
    let bootstrap_servers = cluster.bootstrap_servers();
    
    // Create topic
    let admin: AdminClient<_> = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .create()
        .expect("Failed to create admin client");
    
    let topic = NewTopic::new("test-incremental", 8, TopicReplication::Fixed(1));
    admin
        .create_topics(&[topic], &AdminOptions::new())
        .await
        .expect("Failed to create topics")[0]
        .expect("Failed to create topic");
    
    // Produce initial data
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .create()
        .expect("Failed to create producer");
    
    for i in 0..80 {
        producer
            .send(
                FutureRecord::to("test-incremental")
                    .key(&format!("key-{}", i))
                    .payload(&format!("value-{}", i)),
                Duration::from_secs(5),
            )
            .await
            .expect("Failed to produce");
    }
    
    // Create consumers with cooperative-sticky assignment
    let create_consumer = |client_id: &str| -> StreamConsumer {
        ClientConfig::new()
            .set("bootstrap.servers", &bootstrap_servers)
            .set("group.id", "incremental-test-group")
            .set("client.id", client_id)
            .set("partition.assignment.strategy", "cooperative-sticky")
            .set("enable.auto.commit", "false")
            .set("auto.offset.reset", "earliest")
            .create()
            .expect("Failed to create consumer")
    };
    
    // Start consumers gradually
    let mut consumers = Vec::new();
    let mut partition_history = Vec::new();
    
    for i in 0..4 {
        let consumer = create_consumer(&format!("consumer-{}", i));
        consumer
            .subscribe(&["test-incremental"])
            .expect("Failed to subscribe");
        
        consumers.push(consumer);
        
        // Wait for rebalance
        sleep(Duration::from_secs(2)).await;
        
        // Record partition assignments
        let mut current_assignments = HashMap::new();
        
        for (j, consumer) in consumers.iter().enumerate() {
            let mut partitions = HashSet::new();
            let start = std::time::Instant::now();
            
            while start.elapsed() < Duration::from_secs(1) {
                match timeout(Duration::from_millis(50), consumer.recv()).await {
                    Ok(Ok(message)) => {
                        partitions.insert(message.partition());
                    }
                    _ => continue,
                }
            }
            
            if !partitions.is_empty() {
                current_assignments.insert(format!("consumer-{}", j), partitions);
            }
        }
        
        partition_history.push(current_assignments);
    }
    
    // Verify incremental behavior
    // Each consumer should keep most of its partitions when new ones join
    for i in 1..partition_history.len() {
        let prev = &partition_history[i - 1];
        let curr = &partition_history[i];
        
        for (consumer_id, prev_partitions) in prev {
            if let Some(curr_partitions) = curr.get(consumer_id) {
                // Calculate retention rate
                let retained: HashSet<_> = prev_partitions.intersection(curr_partitions).collect();
                let retention_rate = retained.len() as f64 / prev_partitions.len() as f64;
                
                // With cooperative rebalancing, consumers should retain most partitions
                assert!(retention_rate >= 0.5,
                    "Consumer {} only retained {}% of partitions",
                    consumer_id, retention_rate * 100.0);
            }
        }
    }
    
    Ok(())
}