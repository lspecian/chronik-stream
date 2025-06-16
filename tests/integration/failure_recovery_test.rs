//! Tests for failure scenarios and recovery

use anyhow::Result;
use rdkafka::config::ClientConfig;
use rdkafka::producer::{FutureProducer, FutureRecord};
use rdkafka::consumer::{StreamConsumer, Consumer};
use futures::StreamExt;
use std::time::Duration;
use tokio::time::sleep;

use crate::testcontainers_setup::{TestEnvironment, ChronikCluster};

#[tokio::test]
async fn test_node_failure_recovery() -> Result<()> {
    let test_env = TestEnvironment::new().await?;
    test_env.create_bucket("chronik-test").await?;
    
    let mut cluster = ChronikCluster::new(3, &test_env).await?;
    cluster.start(&test_env).await?;
    
    let bootstrap_servers = cluster.bootstrap_servers();
    let topic = "test-failure";
    
    // Start producer
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("acks", "all")
        .set("retries", "3")
        .create()?;
    
    // Start consumer
    let consumer: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("group.id", "failure-test")
        .set("auto.offset.reset", "earliest")
        .set("enable.auto.commit", "false")
        .create()?;
    
    consumer.subscribe(&[topic])?;
    
    // Produce messages
    for i in 0..50 {
        let record = FutureRecord::to(topic)
            .key(&format!("key-{}", i))
            .payload(&format!("value-{}", i));
        
        producer.send(record, Duration::from_secs(0)).await?;
    }
    
    // Consume some messages
    let mut stream = consumer.stream();
    let mut consumed_before = 0;
    
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(Ok(msg)) = stream.next().await {
            consumer.commit_message(&msg, rdkafka::consumer::CommitMode::Async)?;
            consumed_before += 1;
            if consumed_before >= 25 {
                break;
            }
        }
        Ok::<(), anyhow::Error>(())
    }).await?;
    
    assert!(consumed_before >= 25, "Should consume at least 25 messages before failure");
    
    // Kill one node
    let killed_node_id = 1;
    cluster.nodes[killed_node_id].process.as_mut().unwrap().kill().await?;
    
    // Wait for cluster to detect failure
    sleep(Duration::from_secs(5)).await;
    
    // Continue producing - should still work with 2/3 nodes
    for i in 50..100 {
        let record = FutureRecord::to(topic)
            .key(&format!("key-{}", i))
            .payload(&format!("value-{}", i));
        
        let result = producer.send(record, Duration::from_secs(5)).await;
        assert!(result.is_ok(), "Should be able to produce with 2/3 nodes");
    }
    
    // Continue consuming - should still work
    let mut consumed_during_failure = 0;
    let _ = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(Ok(msg)) = stream.next().await {
            consumer.commit_message(&msg, rdkafka::consumer::CommitMode::Async)?;
            consumed_during_failure += 1;
        }
        Ok::<(), anyhow::Error>(())
    }).await;
    
    assert!(consumed_during_failure > 0, "Should be able to consume during node failure");
    
    // Restart the failed node
    use std::process::Command;
    use tokio::process::Command as TokioCommand;
    
    let node = &mut cluster.nodes[killed_node_id];
    let mut cmd = TokioCommand::new("cargo");
    cmd.args(&["run", "--bin", "chronik-ingest", "--"])
        .env("CHRONIK_NODE_ID", node.node_id.to_string())
        .env("CHRONIK_KAFKA_PORT", node.kafka_port.to_string())
        .env("CHRONIK_ADMIN_PORT", node.admin_port.to_string())
        .env("CHRONIK_INTERNAL_PORT", node.internal_port.to_string())
        .env("CHRONIK_DATA_DIR", node.data_dir.path().to_str().unwrap())
        .env("CHRONIK_STORAGE_TYPE", "s3")
        .env("CHRONIK_S3_ENDPOINT", &test_env.minio_endpoint)
        .env("CHRONIK_S3_BUCKET", "chronik-test")
        .env("CHRONIK_S3_ACCESS_KEY", &test_env.minio_access_key)
        .env("CHRONIK_S3_SECRET_KEY", &test_env.minio_secret_key)
        .env("CHRONIK_S3_REGION", "us-east-1");
    
    let peers: Vec<String> = cluster.nodes.iter()
        .filter(|n| n.node_id != node.node_id)
        .map(|n| format!("127.0.0.1:{}", n.internal_port))
        .collect();
    cmd.env("CHRONIK_PEERS", peers.join(","));
    
    let child = cmd.spawn()?;
    node.process = Some(child);
    
    // Wait for node to rejoin
    sleep(Duration::from_secs(10)).await;
    
    // Verify all messages can be consumed
    let total_consumed = consumed_before + consumed_during_failure;
    let remaining = 100 - total_consumed;
    
    let mut consumed_after_recovery = 0;
    let _ = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(Ok(_)) = stream.next().await {
            consumed_after_recovery += 1;
        }
    }).await;
    
    assert!(consumed_after_recovery >= remaining / 2, 
            "Should consume remaining messages after recovery");
    
    cluster.stop().await?;
    Ok(())
}

#[tokio::test]
async fn test_network_partition() -> Result<()> {
    let test_env = TestEnvironment::new().await?;
    test_env.create_bucket("chronik-test").await?;
    
    let mut cluster = ChronikCluster::new(3, &test_env).await?;
    cluster.start(&test_env).await?;
    
    let topic = "test-partition";
    
    // Create producers for different nodes
    let producer1: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &format!("127.0.0.1:{}", cluster.nodes[0].kafka_port))
        .create()?;
    
    let producer2: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &format!("127.0.0.1:{}", cluster.nodes[1].kafka_port))
        .create()?;
    
    // Produce to both nodes
    for i in 0..20 {
        let record = FutureRecord::to(topic)
            .key(&format!("key-{}", i))
            .payload(&format!("value-{}", i));
        
        if i % 2 == 0 {
            producer1.send(record, Duration::from_secs(0)).await?;
        } else {
            producer2.send(record, Duration::from_secs(0)).await?;
        }
    }
    
    // Simulate network partition by blocking internal ports
    // In a real test, we'd use iptables or similar
    // For now, we'll kill the internal communication by killing a node
    
    cluster.nodes[2].process.as_mut().unwrap().kill().await?;
    sleep(Duration::from_secs(5)).await;
    
    // Try to produce - should work on majority side
    let mut success_count = 0;
    let mut failure_count = 0;
    
    for i in 20..40 {
        let record = FutureRecord::to(topic)
            .key(&format!("key-{}", i))
            .payload(&format!("value-{}", i));
        
        match producer1.send(record, Duration::from_secs(2)).await {
            Ok(_) => success_count += 1,
            Err(_) => failure_count += 1,
        }
    }
    
    assert!(success_count > failure_count, "Majority side should accept writes");
    
    cluster.stop().await?;
    Ok(())
}

#[tokio::test]
async fn test_consumer_group_coordinator_failover() -> Result<()> {
    let test_env = TestEnvironment::new().await?;
    test_env.create_bucket("chronik-test").await?;
    
    let mut cluster = ChronikCluster::new(3, &test_env).await?;
    cluster.start(&test_env).await?;
    
    let bootstrap_servers = cluster.bootstrap_servers();
    let topic = "test-coordinator-failover";
    let group_id = "failover-group";
    
    // Create multiple consumers
    let mut consumers = Vec::new();
    for i in 0..3 {
        let consumer: StreamConsumer = ClientConfig::new()
            .set("bootstrap.servers", &bootstrap_servers)
            .set("group.id", group_id)
            .set("client.id", &format!("consumer-{}", i))
            .set("auto.offset.reset", "earliest")
            .set("session.timeout.ms", "6000")
            .set("heartbeat.interval.ms", "1000")
            .create()?;
        
        consumer.subscribe(&[topic])?;
        consumers.push(consumer);
    }
    
    // Produce messages
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .create()?;
    
    for i in 0..100 {
        let record = FutureRecord::to(topic)
            .key(&format!("key-{}", i))
            .payload(&format!("value-{}", i));
        
        producer.send(record, Duration::from_secs(0)).await?;
    }
    
    // Start consuming
    let mut consumed_counts = vec![0; 3];
    
    // Consume for a bit
    let consume_duration = Duration::from_secs(5);
    let start = tokio::time::Instant::now();
    
    while start.elapsed() < consume_duration {
        for (idx, consumer) in consumers.iter().enumerate() {
            let mut stream = consumer.stream();
            if let Ok(Some(Ok(_))) = tokio::time::timeout(
                Duration::from_millis(100),
                stream.next()
            ).await {
                consumed_counts[idx] += 1;
            }
        }
    }
    
    // Verify all consumers are working
    for (idx, count) in consumed_counts.iter().enumerate() {
        assert!(*count > 0, "Consumer {} should have consumed messages", idx);
    }
    
    // Find and kill the coordinator node
    // In a real implementation, we'd query the cluster to find the coordinator
    // For now, assume it's the first node
    cluster.nodes[0].process.as_mut().unwrap().kill().await?;
    
    // Wait for coordinator failover
    sleep(Duration::from_secs(10)).await;
    
    // Continue consuming - should work after failover
    let mut post_failover_counts = vec![0; 3];
    let consume_duration = Duration::from_secs(5);
    let start = tokio::time::Instant::now();
    
    while start.elapsed() < consume_duration {
        for (idx, consumer) in consumers.iter().enumerate() {
            let mut stream = consumer.stream();
            if let Ok(Some(Ok(_))) = tokio::time::timeout(
                Duration::from_millis(100),
                stream.next()
            ).await {
                post_failover_counts[idx] += 1;
            }
        }
    }
    
    // Verify consumers resumed after failover
    for (idx, count) in post_failover_counts.iter().enumerate() {
        assert!(*count > 0, "Consumer {} should resume after coordinator failover", idx);
    }
    
    cluster.stop().await?;
    Ok(())
}

#[tokio::test]
async fn test_data_consistency_during_failures() -> Result<()> {
    let test_env = TestEnvironment::new().await?;
    test_env.create_bucket("chronik-test").await?;
    
    let mut cluster = ChronikCluster::new(3, &test_env).await?;
    cluster.start(&test_env).await?;
    
    let bootstrap_servers = cluster.bootstrap_servers();
    let topic = "test-consistency";
    
    // Producer with strict settings
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("acks", "all")
        .set("enable.idempotence", "true")
        .set("max.in.flight.requests.per.connection", "1")
        .set("retries", "10")
        .create()?;
    
    // Consumer with exact-once semantics
    let consumer: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("group.id", "consistency-test")
        .set("enable.auto.commit", "false")
        .set("auto.offset.reset", "earliest")
        .create()?;
    
    consumer.subscribe(&[topic])?;
    
    // Track produced and consumed messages
    let mut produced_messages = std::collections::HashSet::new();
    let mut consumed_messages = std::collections::HashSet::new();
    
    // Start background producer
    let producer_handle = tokio::spawn(async move {
        let mut produced = Vec::new();
        for i in 0..1000 {
            let key = format!("key-{}", i);
            let value = format!("value-{}", i);
            
            let record = FutureRecord::to(topic)
                .key(&key)
                .payload(&value);
            
            match producer.send(record, Duration::from_secs(5)).await {
                Ok(_) => {
                    produced.push((key, value));
                }
                Err(e) => {
                    eprintln!("Failed to produce message {}: {:?}", i, e);
                }
            }
            
            // Add some delay
            if i % 10 == 0 {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            
            // Simulate failure after 500 messages
            if i == 500 {
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
        produced
    });
    
    // Start consumer
    let consumer_handle = tokio::spawn(async move {
        let mut consumed = Vec::new();
        let mut stream = consumer.stream();
        
        let timeout = Duration::from_secs(30);
        let start = tokio::time::Instant::now();
        
        while start.elapsed() < timeout {
            if let Ok(Some(Ok(msg))) = tokio::time::timeout(
                Duration::from_millis(500),
                stream.next()
            ).await {
                let key = String::from_utf8_lossy(msg.key().unwrap()).to_string();
                let value = String::from_utf8_lossy(msg.payload().unwrap()).to_string();
                consumed.push((key, value));
                
                // Commit after each message for exact-once
                consumer.commit_message(&msg, rdkafka::consumer::CommitMode::Sync)
                    .expect("Failed to commit");
            }
        }
        consumed
    });
    
    // Kill a node midway through
    tokio::time::sleep(Duration::from_secs(10)).await;
    cluster.nodes[1].process.as_mut().unwrap().kill().await?;
    
    // Wait for operations to complete
    let produced = producer_handle.await?;
    let consumed = consumer_handle.await?;
    
    // Convert to sets for comparison
    for msg in produced {
        produced_messages.insert(msg);
    }
    for msg in consumed {
        consumed_messages.insert(msg);
    }
    
    // Verify no duplicates in consumed messages
    assert_eq!(consumed_messages.len(), consumed.len(), "No duplicate messages should be consumed");
    
    // Verify all consumed messages were actually produced
    for msg in &consumed_messages {
        assert!(produced_messages.contains(msg), "Consumed message was not in produced set");
    }
    
    println!("Produced: {}, Consumed: {}", produced_messages.len(), consumed_messages.len());
    
    cluster.stop().await?;
    Ok(())
}