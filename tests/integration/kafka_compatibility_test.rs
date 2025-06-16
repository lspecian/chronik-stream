//! Kafka compatibility tests using real Kafka clients

use anyhow::Result;
use rdkafka::config::ClientConfig;
use rdkafka::producer::{FutureProducer, FutureRecord};
use rdkafka::consumer::{StreamConsumer, Consumer};
use rdkafka::Message;
use rdkafka::message::Headers;
use futures::StreamExt;
use std::time::Duration;

use crate::testcontainers_setup::{TestEnvironment, ChronikCluster};

#[tokio::test]
async fn test_basic_produce_consume() -> Result<()> {
    let test_env = TestEnvironment::new().await?;
    test_env.create_bucket("chronik-test").await?;
    
    let mut cluster = ChronikCluster::new(1, &test_env).await?;
    cluster.start(&test_env).await?;
    
    let bootstrap_servers = cluster.bootstrap_servers();
    let topic = "test-topic";
    
    // Create producer
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("message.timeout.ms", "5000")
        .create()?;
    
    // Produce messages
    let mut produced_messages = Vec::new();
    for i in 0..100 {
        let key = format!("key-{}", i);
        let value = format!("value-{}", i);
        produced_messages.push((key.clone(), value.clone()));
        
        let record = FutureRecord::to(topic)
            .key(&key)
            .payload(&value);
        
        producer.send(record, Duration::from_secs(0)).await?;
    }
    
    // Create consumer
    let consumer: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("group.id", "test-group")
        .set("auto.offset.reset", "earliest")
        .set("enable.auto.commit", "false")
        .create()?;
    
    consumer.subscribe(&[topic])?;
    
    // Consume messages
    let mut consumed_messages = Vec::new();
    let mut stream = consumer.stream();
    
    let timeout = tokio::time::timeout(Duration::from_secs(10), async {
        while consumed_messages.len() < produced_messages.len() {
            if let Some(Ok(message)) = stream.next().await {
                let key = message.key().map(|k| String::from_utf8_lossy(k).to_string());
                let value = String::from_utf8_lossy(message.payload().unwrap()).to_string();
                consumed_messages.push((key.unwrap(), value));
            }
        }
    }).await;
    
    assert!(timeout.is_ok(), "Timeout waiting for messages");
    
    // Verify all messages were consumed
    assert_eq!(consumed_messages.len(), produced_messages.len());
    
    // Sort and compare
    produced_messages.sort();
    consumed_messages.sort();
    assert_eq!(produced_messages, consumed_messages);
    
    cluster.stop().await?;
    Ok(())
}

#[tokio::test]
async fn test_consumer_groups() -> Result<()> {
    let test_env = TestEnvironment::new().await?;
    test_env.create_bucket("chronik-test").await?;
    
    let mut cluster = ChronikCluster::new(3, &test_env).await?;
    cluster.start(&test_env).await?;
    
    let bootstrap_servers = cluster.bootstrap_servers();
    let topic = "test-consumer-groups";
    let num_partitions = 6;
    
    // Create topic with multiple partitions (would need admin API)
    // For now, let's assume auto-create with default partitions
    
    // Produce messages
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .create()?;
    
    for i in 0..1000 {
        let record = FutureRecord::to(topic)
            .key(&format!("key-{}", i))
            .payload(&format!("value-{}", i))
            .partition(i % num_partitions);
        
        producer.send(record, Duration::from_secs(0)).await?;
    }
    
    // Create multiple consumers in the same group
    let group_id = "test-consumer-group";
    let mut consumers = Vec::new();
    
    for i in 0..3 {
        let consumer: StreamConsumer = ClientConfig::new()
            .set("bootstrap.servers", &bootstrap_servers)
            .set("group.id", group_id)
            .set("client.id", &format!("consumer-{}", i))
            .set("auto.offset.reset", "earliest")
            .set("enable.auto.commit", "true")
            .set("auto.commit.interval.ms", "1000")
            .create()?;
        
        consumer.subscribe(&[topic])?;
        consumers.push(consumer);
    }
    
    // Each consumer should get roughly 1/3 of the messages
    let mut consumed_per_consumer = vec![0; 3];
    let mut total_consumed = 0;
    
    let timeout = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            for (idx, consumer) in consumers.iter().enumerate() {
                let mut stream = consumer.stream();
                while let Some(Ok(_message)) = tokio::time::timeout(
                    Duration::from_millis(100),
                    stream.next()
                ).await.ok().flatten() {
                    consumed_per_consumer[idx] += 1;
                    total_consumed += 1;
                }
            }
            
            if total_consumed >= 1000 {
                break;
            }
        }
    }).await;
    
    assert!(timeout.is_ok(), "Timeout waiting for all messages");
    
    // Verify distribution
    for count in &consumed_per_consumer {
        assert!(*count > 200, "Consumer should have processed at least 200 messages, got {}", count);
        assert!(*count < 500, "Consumer should have processed less than 500 messages, got {}", count);
    }
    
    cluster.stop().await?;
    Ok(())
}

#[tokio::test]
async fn test_consumer_group_rebalance() -> Result<()> {
    let test_env = TestEnvironment::new().await?;
    test_env.create_bucket("chronik-test").await?;
    
    let mut cluster = ChronikCluster::new(3, &test_env).await?;
    cluster.start(&test_env).await?;
    
    let bootstrap_servers = cluster.bootstrap_servers();
    let topic = "test-rebalance";
    let group_id = "rebalance-group";
    
    // Start with one consumer
    let consumer1: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("group.id", group_id)
        .set("client.id", "consumer-1")
        .set("auto.offset.reset", "earliest")
        .create()?;
    
    consumer1.subscribe(&[topic])?;
    
    // Produce some messages
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .create()?;
    
    for i in 0..100 {
        let record = FutureRecord::to(topic)
            .key(&format!("key-{}", i))
            .payload(&format!("value-{}", i));
        
        producer.send(record, Duration::from_secs(0)).await?;
    }
    
    // Let consumer1 process some messages
    let mut stream1 = consumer1.stream();
    let mut count1 = 0;
    
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(Ok(_)) = stream1.next().await {
            count1 += 1;
            if count1 >= 50 {
                break;
            }
        }
    }).await;
    
    assert!(count1 >= 50, "Consumer1 should have processed at least 50 messages");
    
    // Add second consumer - should trigger rebalance
    let consumer2: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("group.id", group_id)
        .set("client.id", "consumer-2")
        .set("auto.offset.reset", "earliest")
        .create()?;
    
    consumer2.subscribe(&[topic])?;
    
    // Wait for rebalance
    tokio::time::sleep(Duration::from_secs(3)).await;
    
    // Produce more messages
    for i in 100..200 {
        let record = FutureRecord::to(topic)
            .key(&format!("key-{}", i))
            .payload(&format!("value-{}", i));
        
        producer.send(record, Duration::from_secs(0)).await?;
    }
    
    // Both consumers should now process messages
    let mut stream2 = consumer2.stream();
    let mut count2 = 0;
    let mut new_count1 = 0;
    
    let _ = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            tokio::select! {
                Some(Ok(_)) = stream1.next() => {
                    new_count1 += 1;
                }
                Some(Ok(_)) = stream2.next() => {
                    count2 += 1;
                }
            }
            
            if new_count1 + count2 >= 50 {
                break;
            }
        }
    }).await;
    
    // Both consumers should have processed messages after rebalance
    assert!(new_count1 > 0, "Consumer1 should still process messages after rebalance");
    assert!(count2 > 0, "Consumer2 should process messages after joining");
    
    cluster.stop().await?;
    Ok(())
}

#[tokio::test]
async fn test_transactional_producer() -> Result<()> {
    let test_env = TestEnvironment::new().await?;
    test_env.create_bucket("chronik-test").await?;
    
    let mut cluster = ChronikCluster::new(3, &test_env).await?;
    cluster.start(&test_env).await?;
    
    let bootstrap_servers = cluster.bootstrap_servers();
    let topic = "test-transactions";
    
    // Create transactional producer
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("transactional.id", "test-tx-producer")
        .set("enable.idempotence", "true")
        .create()?;
    
    // Initialize transactions
    producer.init_transactions(Duration::from_secs(30))?;
    
    // Begin transaction
    producer.begin_transaction()?;
    
    // Send messages in transaction
    for i in 0..10 {
        let record = FutureRecord::to(topic)
            .key(&format!("tx-key-{}", i))
            .payload(&format!("tx-value-{}", i));
        
        producer.send(record, Duration::from_secs(0)).await?;
    }
    
    // Commit transaction
    producer.commit_transaction(Duration::from_secs(30))?;
    
    // Create consumer to read committed messages
    let consumer: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("group.id", "tx-consumer")
        .set("isolation.level", "read_committed")
        .set("auto.offset.reset", "earliest")
        .create()?;
    
    consumer.subscribe(&[topic])?;
    
    // Should see all 10 messages
    let mut consumed = 0;
    let mut stream = consumer.stream();
    
    let _ = tokio::time::timeout(Duration::from_secs(10), async {
        while consumed < 10 {
            if let Some(Ok(_)) = stream.next().await {
                consumed += 1;
            }
        }
    }).await;
    
    assert_eq!(consumed, 10, "Should consume all committed messages");
    
    // Test aborted transaction
    producer.begin_transaction()?;
    
    for i in 10..20 {
        let record = FutureRecord::to(topic)
            .key(&format!("abort-key-{}", i))
            .payload(&format!("abort-value-{}", i));
        
        producer.send(record, Duration::from_secs(0)).await?;
    }
    
    // Abort this transaction
    producer.abort_transaction(Duration::from_secs(30))?;
    
    // Consumer should not see aborted messages
    let mut extra_consumed = 0;
    let _ = tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(Ok(_)) = stream.next().await {
            extra_consumed += 1;
        }
    }).await;
    
    assert_eq!(extra_consumed, 0, "Should not see aborted messages");
    
    cluster.stop().await?;
    Ok(())
}

#[tokio::test]
async fn test_message_headers() -> Result<()> {
    let test_env = TestEnvironment::new().await?;
    test_env.create_bucket("chronik-test").await?;
    
    let mut cluster = ChronikCluster::new(1, &test_env).await?;
    cluster.start(&test_env).await?;
    
    let bootstrap_servers = cluster.bootstrap_servers();
    let topic = "test-headers";
    
    // Producer
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .create()?;
    
    // Create message with headers
    let mut headers = rdkafka::message::OwnedHeaders::new();
    headers = headers.insert(rdkafka::message::Header {
        key: "header1",
        value: Some("value1".as_bytes()),
    });
    headers = headers.insert(rdkafka::message::Header {
        key: "header2",
        value: Some("value2".as_bytes()),
    });
    
    let record = FutureRecord::to(topic)
        .key("test-key")
        .payload("test-value")
        .headers(headers);
    
    producer.send(record, Duration::from_secs(0)).await?;
    
    // Consumer
    let consumer: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("group.id", "header-test")
        .set("auto.offset.reset", "earliest")
        .create()?;
    
    consumer.subscribe(&[topic])?;
    
    let mut stream = consumer.stream();
    
    let message = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await?
        .unwrap()?;
    
    // Verify headers
    let headers = message.headers().expect("Should have headers");
    assert_eq!(headers.count(), 2);
    
    let header1 = headers.get(0);
    assert_eq!(header1.key, "header1");
    assert_eq!(header1.value, Some("value1".as_bytes()));
    
    let header2 = headers.get(1);
    assert_eq!(header2.key, "header2");
    assert_eq!(header2.value, Some("value2".as_bytes()));
    
    cluster.stop().await?;
    Ok(())
}