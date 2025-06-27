//! End-to-end data flow tests

use super::common::*;
use chronik_common::Result;
use rdkafka::{
    ClientConfig,
    admin::{AdminClient, AdminOptions, NewTopic, TopicReplication},
    producer::{FutureProducer, FutureRecord},
};
use serde_json::json;
use std::time::Duration;
use tokio::time::{sleep, timeout};

#[tokio::test]
async fn test_produce_to_search() -> Result<()> {
    super::test_setup::init();
    
    let cluster = TestCluster::start(TestClusterConfig::default()).await?;
    let bootstrap_servers = cluster.bootstrap_servers();
    let search_endpoint = cluster.search_endpoint();
    
    // Create topic
    let admin: AdminClient<_> = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .create()
        .expect("Failed to create admin client");
    
    let topic = NewTopic::new("test-search", 1, TopicReplication::Fixed(1));
    admin
        .create_topics(&[topic], &AdminOptions::new())
        .await
        .expect("Failed to create topics")[0]
        .expect("Failed to create topic");
    
    // Produce JSON documents
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .create()
        .expect("Failed to create producer");
    
    let documents = vec![
        json!({
            "id": "doc1",
            "title": "Introduction to Chronik Stream",
            "content": "Chronik Stream is a distributed log storage system with search capabilities",
            "timestamp": "2024-01-01T00:00:00Z"
        }),
        json!({
            "id": "doc2", 
            "title": "Kafka Protocol Compatibility",
            "content": "Chronik Stream supports the full Kafka protocol for producing and consuming",
            "timestamp": "2024-01-02T00:00:00Z"
        }),
        json!({
            "id": "doc3",
            "title": "Search Features",
            "content": "Full-text search is powered by Tantivy with Elasticsearch-compatible API",
            "timestamp": "2024-01-03T00:00:00Z"
        }),
    ];
    
    for doc in &documents {
        let key = doc["id"].as_str().unwrap();
        let value = serde_json::to_string(&doc).unwrap();
        
        producer
            .send(
                FutureRecord::to("test-search")
                    .key(key)
                    .payload(&value),
                Duration::from_secs(5),
            )
            .await
            .expect("Failed to produce");
    }
    
    // Wait for indexing
    sleep(Duration::from_secs(3)).await;
    
    // Search for documents
    let client = reqwest::Client::new();
    
    // Test 1: Match all query
    let response = client
        .post(&format!("{}/test-search/_search", search_endpoint))
        .json(&json!({
            "query": {
                "match_all": {}
            }
        }))
        .send()
        .await?;
    
    assert!(response.status().is_success());
    let result: serde_json::Value = response.json().await?;
    
    assert_eq!(result["hits"]["total"]["value"], 3);
    assert_eq!(result["hits"]["hits"].as_array().unwrap().len(), 3);
    
    // Test 2: Term query
    let response = client
        .post(&format!("{}/test-search/_search", search_endpoint))
        .json(&json!({
            "query": {
                "term": {
                    "title": "kafka"
                }
            }
        }))
        .send()
        .await?;
    
    let result: serde_json::Value = response.json().await?;
    assert_eq!(result["hits"]["total"]["value"], 1);
    assert_eq!(result["hits"]["hits"][0]["_source"]["id"], "doc2");
    
    // Test 3: Full-text search
    let response = client
        .post(&format!("{}/test-search/_search", search_endpoint))
        .json(&json!({
            "query": {
                "match": {
                    "content": "distributed search"
                }
            }
        }))
        .send()
        .await?;
    
    let result: serde_json::Value = response.json().await?;
    assert!(result["hits"]["total"]["value"].as_u64().unwrap() >= 2);
    
    Ok(())
}

#[tokio::test]
async fn test_streaming_updates() -> Result<()> {
    super::test_setup::init();
    
    let cluster = TestCluster::start(TestClusterConfig::default()).await?;
    let bootstrap_servers = cluster.bootstrap_servers();
    let search_endpoint = cluster.search_endpoint();
    
    // Create topic
    let admin: AdminClient<_> = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .create()
        .expect("Failed to create admin client");
    
    let topic = NewTopic::new("test-streaming", 1, TopicReplication::Fixed(1));
    admin
        .create_topics(&[topic], &AdminOptions::new())
        .await
        .expect("Failed to create topics")[0]
        .expect("Failed to create topic");
    
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .create()
        .expect("Failed to create producer");
    
    let client = reqwest::Client::new();
    
    // Produce initial document
    let doc = json!({
        "id": "stream1",
        "counter": 1,
        "status": "active"
    });
    
    producer
        .send(
            FutureRecord::to("test-streaming")
                .key("stream1")
                .payload(&serde_json::to_string(&doc).unwrap()),
            Duration::from_secs(5),
        )
        .await
        .expect("Failed to produce");
    
    // Wait for indexing
    sleep(Duration::from_secs(2)).await;
    
    // Verify initial state
    let response = client
        .post(&format!("{}/test-streaming/_search", search_endpoint))
        .json(&json!({
            "query": {
                "term": { "id": "stream1" }
            }
        }))
        .send()
        .await?;
    
    let result: serde_json::Value = response.json().await?;
    assert_eq!(result["hits"]["hits"][0]["_source"]["counter"], 1);
    
    // Update the document multiple times
    for i in 2..=5 {
        let doc = json!({
            "id": "stream1",
            "counter": i,
            "status": "active",
            "updated_at": format!("2024-01-01T00:0{}:00Z", i)
        });
        
        producer
            .send(
                FutureRecord::to("test-streaming")
                    .key("stream1")
                    .payload(&serde_json::to_string(&doc).unwrap()),
                Duration::from_secs(5),
            )
            .await
            .expect("Failed to produce");
        
        // Wait briefly between updates
        sleep(Duration::from_millis(500)).await;
    }
    
    // Wait for final indexing
    sleep(Duration::from_secs(3)).await;
    
    // Verify final state (should have latest value)
    let response = client
        .post(&format!("{}/test-streaming/_search", search_endpoint))
        .json(&json!({
            "query": {
                "term": { "id": "stream1" }
            }
        }))
        .send()
        .await?;
    
    let result: serde_json::Value = response.json().await?;
    assert_eq!(result["hits"]["hits"][0]["_source"]["counter"], 5);
    assert!(result["hits"]["hits"][0]["_source"]["updated_at"].as_str().unwrap().contains("05:00"));
    
    Ok(())
}

#[tokio::test]
async fn test_multi_partition_ordering() -> Result<()> {
    super::test_setup::init();
    
    let cluster = TestCluster::start(TestClusterConfig::default()).await?;
    let bootstrap_servers = cluster.bootstrap_servers();
    let search_endpoint = cluster.search_endpoint();
    
    // Create multi-partition topic
    let admin: AdminClient<_> = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .create()
        .expect("Failed to create admin client");
    
    let topic = NewTopic::new("test-partitions", 4, TopicReplication::Fixed(1));
    admin
        .create_topics(&[topic], &AdminOptions::new())
        .await
        .expect("Failed to create topics")[0]
        .expect("Failed to create topic");
    
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .create()
        .expect("Failed to create producer");
    
    // Produce documents to different partitions based on user_id
    let users = vec!["alice", "bob", "charlie", "david"];
    
    for (batch, user) in users.iter().enumerate() {
        for seq in 0..10 {
            let doc = json!({
                "user_id": user,
                "sequence": seq,
                "batch": batch,
                "message": format!("Message {} from {}", seq, user),
                "timestamp": format!("2024-01-01T{}:{}:00Z", batch, seq)
            });
            
            producer
                .send(
                    FutureRecord::to("test-partitions")
                        .key(user) // Key determines partition
                        .payload(&serde_json::to_string(&doc).unwrap()),
                    Duration::from_secs(5),
                )
                .await
                .expect("Failed to produce");
        }
    }
    
    // Wait for indexing
    sleep(Duration::from_secs(3)).await;
    
    let client = reqwest::Client::new();
    
    // Verify each user's messages are in order
    for user in &users {
        let response = client
            .post(&format!("{}/test-partitions/_search", search_endpoint))
            .json(&json!({
                "query": {
                    "term": { "user_id": user }
                },
                "sort": [
                    { "sequence": { "order": "asc" } }
                ],
                "size": 20
            }))
            .send()
            .await?;
        
        let result: serde_json::Value = response.json().await?;
        let hits = result["hits"]["hits"].as_array().unwrap();
        
        assert_eq!(hits.len(), 10);
        
        // Verify sequence is in order
        for (i, hit) in hits.iter().enumerate() {
            assert_eq!(hit["_source"]["sequence"], i as i64);
        }
    }
    
    Ok(())
}

#[tokio::test]
async fn test_large_document_handling() -> Result<()> {
    super::test_setup::init();
    
    let cluster = TestCluster::start(TestClusterConfig::default()).await?;
    let bootstrap_servers = cluster.bootstrap_servers();
    let search_endpoint = cluster.search_endpoint();
    
    // Create topic
    let admin: AdminClient<_> = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .create()
        .expect("Failed to create admin client");
    
    let topic = NewTopic::new("test-large-docs", 1, TopicReplication::Fixed(1));
    admin
        .create_topics(&[topic], &AdminOptions::new())
        .await
        .expect("Failed to create topics")[0]
        .expect("Failed to create topic");
    
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("message.max.bytes", "10485760") // 10MB
        .create()
        .expect("Failed to create producer");
    
    // Create documents of various sizes
    let sizes = vec![
        ("small", 1024),        // 1KB
        ("medium", 102400),     // 100KB  
        ("large", 1048576),     // 1MB
        ("xlarge", 5242880),    // 5MB
    ];
    
    for (size_name, size) in sizes {
        let content = "x".repeat(size);
        let doc = json!({
            "id": format!("doc-{}", size_name),
            "size_category": size_name,
            "content": content,
            "size_bytes": size
        });
        
        producer
            .send(
                FutureRecord::to("test-large-docs")
                    .key(&format!("doc-{}", size_name))
                    .payload(&serde_json::to_string(&doc).unwrap()),
                Duration::from_secs(30),
            )
            .await
            .expect("Failed to produce");
    }
    
    // Wait for indexing
    sleep(Duration::from_secs(5)).await;
    
    let client = reqwest::Client::new();
    
    // Verify all documents are searchable
    let response = client
        .post(&format!("{}/test-large-docs/_search", search_endpoint))
        .json(&json!({
            "query": { "match_all": {} },
            "sort": [{ "size_bytes": { "order": "asc" } }]
        }))
        .send()
        .await?;
    
    let result: serde_json::Value = response.json().await?;
    assert_eq!(result["hits"]["total"]["value"], 4);
    
    // Verify we can retrieve large documents
    for hit in result["hits"]["hits"].as_array().unwrap() {
        let size_bytes = hit["_source"]["size_bytes"].as_u64().unwrap();
        let content = hit["_source"]["content"].as_str().unwrap();
        assert_eq!(content.len(), size_bytes as usize);
    }
    
    Ok(())
}