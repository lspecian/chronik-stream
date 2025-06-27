//! Multi-node cluster tests

use super::common::*;
use chronik_common::Result;
use rdkafka::{
    ClientConfig,
    admin::{AdminClient, AdminOptions, NewTopic, TopicReplication},
    producer::{FutureProducer, FutureRecord},
    consumer::{Consumer, StreamConsumer},
};
use serde_json::json;
use std::collections::HashMap;
use std::time::Duration;
use tokio::time::{sleep, timeout};

#[tokio::test]
async fn test_multi_controller_consensus() -> Result<()> {
    super::test_setup::init();
    
    // Start cluster with 3 controllers
    let config = TestClusterConfig {
        num_controllers: 3,
        num_ingest_nodes: 2,
        num_search_nodes: 2,
        ..Default::default()
    };
    
    let cluster = TestCluster::start(config).await?;
    
    // Create topic through first controller
    let admin: AdminClient<_> = ClientConfig::new()
        .set("bootstrap.servers", &cluster.bootstrap_servers())
        .create()
        .expect("Failed to create admin client");
    
    let topic = NewTopic::new("test-consensus", 3, TopicReplication::Fixed(1));
    admin
        .create_topics(&[topic], &AdminOptions::new())
        .await
        .expect("Failed to create topics")[0]
        .expect("Failed to create topic");
    
    // Verify topic is visible from all ingest nodes
    for addr in cluster.ingest_addrs() {
        let admin: AdminClient<_> = ClientConfig::new()
            .set("bootstrap.servers", &addr.to_string())
            .create()
            .expect("Failed to create admin client");
        
        let metadata = admin
            .inner()
            .fetch_metadata(Some("test-consensus"), Duration::from_secs(5))
            .expect("Failed to fetch metadata");
        
        assert_eq!(metadata.topics().len(), 1);
        assert_eq!(metadata.topics()[0].name(), "test-consensus");
        assert_eq!(metadata.topics()[0].partitions().len(), 3);
    }
    
    // Test controller failover by simulating leader failure
    // This would require additional test infrastructure to kill/restart processes
    
    Ok(())
}

#[tokio::test]
async fn test_multi_node_data_distribution() -> Result<()> {
    super::test_setup::init();
    
    // Start cluster with multiple nodes
    let config = TestClusterConfig {
        num_controllers: 1,
        num_ingest_nodes: 3,
        num_search_nodes: 2,
        ..Default::default()
    };
    
    let cluster = TestCluster::start(config).await?;
    let search_endpoint = cluster.search_endpoint();
    
    // Create topic with partitions distributed across nodes
    let admin: AdminClient<_> = ClientConfig::new()
        .set("bootstrap.servers", &cluster.bootstrap_servers())
        .create()
        .expect("Failed to create admin client");
    
    let topic = NewTopic::new("test-distribution", 6, TopicReplication::Fixed(1));
    admin
        .create_topics(&[topic], &AdminOptions::new())
        .await
        .expect("Failed to create topics")[0]
        .expect("Failed to create topic");
    
    // Produce data to all partitions
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &cluster.bootstrap_servers())
        .create()
        .expect("Failed to create producer");
    
    let mut partition_counts = HashMap::new();
    
    for i in 0..600 {
        let partition = i % 6;
        let doc = json!({
            "id": format!("doc-{}", i),
            "partition": partition,
            "content": format!("Data for partition {}", partition),
            "timestamp": format!("2024-01-01T00:{}:00Z", i % 60)
        });
        
        producer
            .send(
                FutureRecord::to("test-distribution")
                    .key(&format!("key-{}", i))
                    .payload(&serde_json::to_string(&doc).unwrap())
                    .partition(partition as i32),
                Duration::from_secs(5),
            )
            .await
            .expect("Failed to produce");
        
        *partition_counts.entry(partition).or_insert(0) += 1;
    }
    
    // Wait for indexing
    sleep(Duration::from_secs(5)).await;
    
    // Verify data is searchable from all search nodes
    let client = reqwest::Client::new();
    
    for addr in cluster.search_addrs() {
        let response = client
            .post(&format!("http://{}/test-distribution/_search", addr))
            .json(&json!({
                "query": { "match_all": {} },
                "size": 0,
                "aggs": {
                    "partitions": {
                        "terms": {
                            "field": "partition",
                            "size": 10
                        }
                    }
                }
            }))
            .send()
            .await?;
        
        let result: serde_json::Value = response.json().await?;
        
        // All documents should be searchable
        assert_eq!(result["hits"]["total"]["value"], 600);
        
        // Verify partition distribution in search results
        let buckets = result["aggregations"]["partitions"]["buckets"].as_array().unwrap();
        assert_eq!(buckets.len(), 6);
        
        for bucket in buckets {
            let partition = bucket["key"].as_i64().unwrap();
            let count = bucket["doc_count"].as_i64().unwrap();
            assert_eq!(count, 100);
        }
    }
    
    Ok(())
}

#[tokio::test]
async fn test_node_failure_recovery() -> Result<()> {
    super::test_setup::init();
    
    // Start cluster with redundancy
    let config = TestClusterConfig {
        num_controllers: 1,
        num_ingest_nodes: 3,
        num_search_nodes: 2,
        ..Default::default()
    };
    
    let cluster = TestCluster::start(config).await?;
    let bootstrap_servers = cluster.bootstrap_servers();
    
    // Create topic
    let admin: AdminClient<_> = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .create()
        .expect("Failed to create admin client");
    
    let topic = NewTopic::new("test-recovery", 4, TopicReplication::Fixed(1));
    admin
        .create_topics(&[topic], &AdminOptions::new())
        .await
        .expect("Failed to create topics")[0]
        .expect("Failed to create topic");
    
    // Start continuous producer
    let producer_handle = tokio::spawn({
        let bootstrap_servers = bootstrap_servers.clone();
        async move {
            let producer: FutureProducer = ClientConfig::new()
                .set("bootstrap.servers", &bootstrap_servers)
                .create()
                .expect("Failed to create producer");
            
            let mut i = 0;
            loop {
                let _ = producer
                    .send(
                        FutureRecord::to("test-recovery")
                            .key(&format!("key-{}", i))
                            .payload(&format!("value-{}", i)),
                        Duration::from_secs(1),
                    )
                    .await;
                
                i += 1;
                sleep(Duration::from_millis(100)).await;
                
                if i >= 100 {
                    break;
                }
            }
        }
    });
    
    // Start consumer
    let consumer: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("group.id", "recovery-test-group")
        .set("enable.auto.commit", "true")
        .set("auto.offset.reset", "earliest")
        .create()
        .expect("Failed to create consumer");
    
    consumer
        .subscribe(&["test-recovery"])
        .expect("Failed to subscribe");
    
    // Consume messages and track progress
    let mut consumed_before = 0;
    let start = std::time::Instant::now();
    
    while consumed_before < 50 && start.elapsed() < Duration::from_secs(10) {
        match timeout(Duration::from_millis(100), consumer.recv()).await {
            Ok(Ok(_)) => consumed_before += 1,
            _ => continue,
        }
    }
    
    assert!(consumed_before >= 30, "Should consume messages before failure");
    
    // Simulate node failure by creating new consumer
    // In real test, we would kill one ingest node
    drop(consumer);
    
    // Wait for recovery
    sleep(Duration::from_secs(2)).await;
    
    // Create new consumer - should continue from last committed offset
    let consumer2: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("group.id", "recovery-test-group")
        .set("enable.auto.commit", "true")
        .set("auto.offset.reset", "earliest")
        .create()
        .expect("Failed to create consumer");
    
    consumer2
        .subscribe(&["test-recovery"])
        .expect("Failed to subscribe");
    
    // Continue consuming
    let mut consumed_after = 0;
    let start = std::time::Instant::now();
    
    while start.elapsed() < Duration::from_secs(10) {
        match timeout(Duration::from_millis(100), consumer2.recv()).await {
            Ok(Ok(_)) => consumed_after += 1,
            _ => continue,
        }
    }
    
    producer_handle.await.unwrap();
    
    // Should have consumed remaining messages
    assert!(consumed_after > 0, "Should continue consuming after recovery");
    assert!(consumed_before + consumed_after >= 80, "Should consume most messages");
    
    Ok(())
}

#[tokio::test]
async fn test_cross_node_search() -> Result<()> {
    super::test_setup::init();
    
    // Start cluster with data distributed across nodes
    let config = TestClusterConfig {
        num_controllers: 1,
        num_ingest_nodes: 2,
        num_search_nodes: 3,
        ..Default::default()
    };
    
    let cluster = TestCluster::start(config).await?;
    
    // Create topics on different nodes
    let topics = vec!["products", "users", "orders"];
    let admin: AdminClient<_> = ClientConfig::new()
        .set("bootstrap.servers", &cluster.bootstrap_servers())
        .create()
        .expect("Failed to create admin client");
    
    for topic in &topics {
        let new_topic = NewTopic::new(topic, 2, TopicReplication::Fixed(1));
        admin
            .create_topics(&[new_topic], &AdminOptions::new())
            .await
            .expect("Failed to create topics")[0]
            .expect("Failed to create topic");
    }
    
    // Produce data to each topic
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &cluster.bootstrap_servers())
        .create()
        .expect("Failed to create producer");
    
    // Products
    for i in 0..50 {
        let doc = json!({
            "id": format!("prod-{}", i),
            "name": format!("Product {}", i),
            "price": 10.0 + (i as f64),
            "category": if i % 2 == 0 { "electronics" } else { "books" }
        });
        
        producer
            .send(
                FutureRecord::to("products")
                    .key(&format!("prod-{}", i))
                    .payload(&serde_json::to_string(&doc).unwrap()),
                Duration::from_secs(5),
            )
            .await
            .expect("Failed to produce");
    }
    
    // Users
    for i in 0..30 {
        let doc = json!({
            "id": format!("user-{}", i),
            "name": format!("User {}", i),
            "email": format!("user{}@example.com", i),
            "active": i % 3 != 0
        });
        
        producer
            .send(
                FutureRecord::to("users")
                    .key(&format!("user-{}", i))
                    .payload(&serde_json::to_string(&doc).unwrap()),
                Duration::from_secs(5),
            )
            .await
            .expect("Failed to produce");
    }
    
    // Orders
    for i in 0..100 {
        let doc = json!({
            "id": format!("order-{}", i),
            "user_id": format!("user-{}", i % 30),
            "product_id": format!("prod-{}", i % 50),
            "quantity": (i % 5) + 1,
            "total": ((i % 5) + 1) as f64 * (10.0 + (i % 50) as f64)
        });
        
        producer
            .send(
                FutureRecord::to("orders")
                    .key(&format!("order-{}", i))
                    .payload(&serde_json::to_string(&doc).unwrap()),
                Duration::from_secs(5),
            )
            .await
            .expect("Failed to produce");
    }
    
    // Wait for indexing
    sleep(Duration::from_secs(5)).await;
    
    let client = reqwest::Client::new();
    
    // Cross-topic search from any search node
    for addr in cluster.search_addrs() {
        // Search across all indices
        let response = client
            .post(&format!("http://{}/_search", addr))
            .json(&json!({
                "query": { "match_all": {} },
                "size": 0,
                "aggs": {
                    "indices": {
                        "terms": {
                            "field": "_index",
                            "size": 10
                        }
                    }
                }
            }))
            .send()
            .await?;
        
        let result: serde_json::Value = response.json().await?;
        
        // Should see all three indices
        let buckets = result["aggregations"]["indices"]["buckets"].as_array().unwrap();
        assert_eq!(buckets.len(), 3);
        
        let index_counts: HashMap<String, i64> = buckets.iter()
            .map(|b| (b["key"].as_str().unwrap().to_string(), b["doc_count"].as_i64().unwrap()))
            .collect();
        
        assert_eq!(index_counts.get("products"), Some(&50));
        assert_eq!(index_counts.get("users"), Some(&30));
        assert_eq!(index_counts.get("orders"), Some(&100));
        
        // Multi-index query
        let response = client
            .post(&format!("http://{}/products,users/_search", addr))
            .json(&json!({
                "query": { "match_all": {} },
                "size": 1
            }))
            .send()
            .await?;
        
        let result: serde_json::Value = response.json().await?;
        assert_eq!(result["hits"]["total"]["value"], 80); // 50 products + 30 users
    }
    
    Ok(())
}

#[tokio::test]
async fn test_load_balancing() -> Result<()> {
    super::test_setup::init();
    
    // Start cluster with multiple nodes
    let config = TestClusterConfig {
        num_controllers: 1,
        num_ingest_nodes: 3,
        num_search_nodes: 3,
        ..Default::default()
    };
    
    let cluster = TestCluster::start(config).await?;
    
    // Create topic with many partitions
    let admin: AdminClient<_> = ClientConfig::new()
        .set("bootstrap.servers", &cluster.bootstrap_servers())
        .create()
        .expect("Failed to create admin client");
    
    let topic = NewTopic::new("test-load-balance", 12, TopicReplication::Fixed(1));
    admin
        .create_topics(&[topic], &AdminOptions::new())
        .await
        .expect("Failed to create topics")[0]
        .expect("Failed to create topic");
    
    // Track which node handles which partition
    let mut partition_to_node = HashMap::new();
    
    // Produce to specific partitions and track responses
    for node_idx in 0..cluster.ingest_addrs().len() {
        let producer: FutureProducer = ClientConfig::new()
            .set("bootstrap.servers", &cluster.ingest_addrs()[node_idx].to_string())
            .create()
            .expect("Failed to create producer");
        
        for partition in 0..12 {
            match producer
                .send(
                    FutureRecord::to("test-load-balance")
                        .key(&format!("test-{}", partition))
                        .payload(&format!("node-{}", node_idx))
                        .partition(partition),
                    Duration::from_secs(1),
                )
                .await
            {
                Ok(_) => {
                    partition_to_node.insert(partition, node_idx);
                }
                Err(_) => {
                    // This node doesn't handle this partition
                }
            }
        }
    }
    
    // Verify partitions are distributed across nodes
    let mut node_partition_counts = HashMap::new();
    for (_, node) in partition_to_node {
        *node_partition_counts.entry(node).or_insert(0) += 1;
    }
    
    // Each node should handle roughly equal partitions
    for (node, count) in node_partition_counts {
        assert!(count >= 3 && count <= 5,
            "Node {} handles {} partitions, expected 3-5", node, count);
    }
    
    Ok(())
}