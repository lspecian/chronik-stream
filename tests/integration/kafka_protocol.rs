//! Kafka protocol compatibility tests

use super::common::*;
use chronik_common::Result;
use rdkafka::{
    ClientConfig,
    Message,
    admin::{AdminClient, AdminOptions, NewTopic, TopicReplication},
    consumer::{Consumer, StreamConsumer},
    producer::{FutureProducer, FutureRecord},
    metadata::Metadata,
};
use std::time::Duration;
use tokio::time::timeout;

#[tokio::test]
async fn test_kafka_metadata_api() -> Result<()> {
    super::test_setup::init();
    
    let cluster = TestCluster::start(TestClusterConfig::default()).await?;
    let bootstrap_servers = cluster.bootstrap_servers();
    
    // Create admin client
    let admin: AdminClient<_> = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .create()
        .expect("Failed to create admin client");
    
    // Fetch metadata
    let metadata = timeout(Duration::from_secs(5), async {
        admin.inner().fetch_metadata(None, Duration::from_secs(5))
    })
    .await
    .expect("Timeout fetching metadata")
    .expect("Failed to fetch metadata");
    
    // Verify broker information
    assert!(!metadata.brokers().is_empty());
    assert_eq!(metadata.brokers().len(), cluster.ingest_addrs().len());
    
    for broker in metadata.brokers() {
        assert!(broker.id() >= 0);
        assert!(!broker.host().is_empty());
        assert!(broker.port() > 0);
    }
    
    Ok(())
}

#[tokio::test]
async fn test_kafka_topic_management() -> Result<()> {
    super::test_setup::init();
    
    let cluster = TestCluster::start(TestClusterConfig::default()).await?;
    let bootstrap_servers = cluster.bootstrap_servers();
    
    // Create admin client
    let admin: AdminClient<_> = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .create()
        .expect("Failed to create admin client");
    
    // Create topics
    let topics = vec![
        NewTopic::new("test-topic-1", 3, TopicReplication::Fixed(1)),
        NewTopic::new("test-topic-2", 1, TopicReplication::Fixed(1)),
    ];
    
    let results = admin
        .create_topics(&topics, &AdminOptions::new())
        .await
        .expect("Failed to create topics");
    
    // Verify all topics created successfully
    for result in results {
        result.expect("Failed to create topic");
    }
    
    // List topics
    let metadata = admin
        .inner()
        .fetch_metadata(None, Duration::from_secs(5))
        .expect("Failed to fetch metadata");
    
    let topic_names: Vec<&str> = metadata.topics()
        .iter()
        .map(|t| t.name())
        .collect();
    
    assert!(topic_names.contains(&"test-topic-1"));
    assert!(topic_names.contains(&"test-topic-2"));
    
    // Verify partition counts
    let topic1 = metadata.topics()
        .iter()
        .find(|t| t.name() == "test-topic-1")
        .unwrap();
    assert_eq!(topic1.partitions().len(), 3);
    
    let topic2 = metadata.topics()
        .iter()
        .find(|t| t.name() == "test-topic-2")
        .unwrap();
    assert_eq!(topic2.partitions().len(), 1);
    
    // Delete topic
    let topics_to_delete = &["test-topic-2"];
    let results = admin
        .delete_topics(topics_to_delete, &AdminOptions::new())
        .await
        .expect("Failed to delete topics");
    
    for result in results {
        result.expect("Failed to delete topic");
    }
    
    Ok(())
}

#[tokio::test]
async fn test_kafka_produce_consume() -> Result<()> {
    super::test_setup::init();
    
    let cluster = TestCluster::start(TestClusterConfig::default()).await?;
    let bootstrap_servers = cluster.bootstrap_servers();
    
    // Create topic first
    let admin: AdminClient<_> = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .create()
        .expect("Failed to create admin client");
    
    let topic = NewTopic::new("test-produce-consume", 2, TopicReplication::Fixed(1));
    admin
        .create_topics(&[topic], &AdminOptions::new())
        .await
        .expect("Failed to create topics")[0]
        .expect("Failed to create topic");
    
    // Create producer
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("message.timeout.ms", "5000")
        .create()
        .expect("Failed to create producer");
    
    // Produce messages
    let mut delivery_futures = Vec::new();
    for i in 0..10 {
        let key = format!("key-{}", i);
        let value = format!("value-{}", i);
        
        let future = producer.send(
            FutureRecord::to("test-produce-consume")
                .key(&key)
                .payload(&value)
                .partition(i % 2), // Alternate between partitions
            Duration::from_secs(5),
        );
        
        delivery_futures.push(future);
    }
    
    // Wait for all deliveries
    for (i, future) in delivery_futures.into_iter().enumerate() {
        let (partition, offset) = future
            .await
            .expect("Delivery failed")
            .expect("Delivery error");
        
        assert_eq!(partition, (i % 2) as i32);
        assert!(offset >= 0);
    }
    
    // Create consumer
    let consumer: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("group.id", "test-consumer-group")
        .set("enable.auto.commit", "false")
        .set("auto.offset.reset", "earliest")
        .create()
        .expect("Failed to create consumer");
    
    // Subscribe to topic
    consumer
        .subscribe(&["test-produce-consume"])
        .expect("Failed to subscribe");
    
    // Consume messages
    let mut consumed_count = 0;
    let consume_timeout = Duration::from_secs(10);
    let start = std::time::Instant::now();
    
    while consumed_count < 10 && start.elapsed() < consume_timeout {
        match timeout(Duration::from_secs(1), consumer.recv()).await {
            Ok(Ok(message)) => {
                let key = message.key_view::<str>().unwrap().unwrap();
                let value = message.payload_view::<str>().unwrap().unwrap();
                
                assert!(key.starts_with("key-"));
                assert!(value.starts_with("value-"));
                assert_eq!(message.topic(), "test-produce-consume");
                
                consumed_count += 1;
            }
            Ok(Err(e)) => panic!("Error consuming message: {:?}", e),
            Err(_) => continue, // Timeout, keep trying
        }
    }
    
    assert_eq!(consumed_count, 10);
    
    Ok(())
}

#[tokio::test]
async fn test_kafka_consumer_groups() -> Result<()> {
    super::test_setup::init();
    
    let cluster = TestCluster::start(TestClusterConfig::default()).await?;
    let bootstrap_servers = cluster.bootstrap_servers();
    
    // Create topic
    let admin: AdminClient<_> = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .create()
        .expect("Failed to create admin client");
    
    let topic = NewTopic::new("test-consumer-groups", 4, TopicReplication::Fixed(1));
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
    
    for i in 0..100 {
        producer
            .send(
                FutureRecord::to("test-consumer-groups")
                    .key(&format!("key-{}", i))
                    .payload(&format!("value-{}", i)),
                Duration::from_secs(5),
            )
            .await
            .expect("Failed to produce");
    }
    
    // Create multiple consumers in the same group
    let mut consumers = Vec::new();
    for i in 0..3 {
        let consumer: StreamConsumer = ClientConfig::new()
            .set("bootstrap.servers", &bootstrap_servers)
            .set("group.id", "test-group")
            .set("client.id", format!("consumer-{}", i))
            .set("enable.auto.commit", "false")
            .set("auto.offset.reset", "earliest")
            .create()
            .expect("Failed to create consumer");
        
        consumer
            .subscribe(&["test-consumer-groups"])
            .expect("Failed to subscribe");
        
        consumers.push(consumer);
    }
    
    // Give time for rebalance
    tokio::time::sleep(Duration::from_secs(2)).await;
    
    // Each consumer should receive messages from different partitions
    let mut total_consumed = 0;
    let mut consumer_counts = vec![0; consumers.len()];
    
    let consume_duration = Duration::from_secs(10);
    let start = std::time::Instant::now();
    
    while total_consumed < 100 && start.elapsed() < consume_duration {
        for (i, consumer) in consumers.iter().enumerate() {
            match timeout(Duration::from_millis(100), consumer.recv()).await {
                Ok(Ok(message)) => {
                    let partition = message.partition();
                    assert!(partition >= 0 && partition < 4);
                    consumer_counts[i] += 1;
                    total_consumed += 1;
                }
                _ => continue,
            }
        }
    }
    
    assert_eq!(total_consumed, 100);
    
    // Verify that work was distributed (each consumer should have processed some messages)
    for count in consumer_counts {
        assert!(count > 0, "Consumer didn't receive any messages");
    }
    
    Ok(())
}

#[tokio::test]
async fn test_kafka_offset_management() -> Result<()> {
    super::test_setup::init();
    
    let cluster = TestCluster::start(TestClusterConfig::default()).await?;
    let bootstrap_servers = cluster.bootstrap_servers();
    
    // Create topic
    let admin: AdminClient<_> = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .create()
        .expect("Failed to create admin client");
    
    let topic = NewTopic::new("test-offsets", 2, TopicReplication::Fixed(1));
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
    
    for i in 0..20 {
        producer
            .send(
                FutureRecord::to("test-offsets")
                    .key(&format!("key-{}", i))
                    .payload(&format!("value-{}", i))
                    .partition(i % 2),
                Duration::from_secs(5),
            )
            .await
            .expect("Failed to produce");
    }
    
    // Consumer with manual offset management
    let consumer: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("group.id", "test-offset-group")
        .set("enable.auto.commit", "false")
        .set("auto.offset.reset", "earliest")
        .create()
        .expect("Failed to create consumer");
    
    consumer
        .subscribe(&["test-offsets"])
        .expect("Failed to subscribe");
    
    // Consume first 10 messages and commit
    let mut consumed = 0;
    while consumed < 10 {
        match timeout(Duration::from_secs(1), consumer.recv()).await {
            Ok(Ok(message)) => {
                consumer.store_offset_from_message(&message)
                    .expect("Failed to store offset");
                consumed += 1;
            }
            _ => continue,
        }
    }
    
    // Commit stored offsets
    consumer.commit_consumer_state(rdkafka::consumer::CommitMode::Sync)
        .expect("Failed to commit offsets");
    
    // Create new consumer with same group - should start from committed offset
    let consumer2: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("group.id", "test-offset-group")
        .set("enable.auto.commit", "false")
        .set("auto.offset.reset", "earliest")
        .create()
        .expect("Failed to create consumer");
    
    consumer2
        .subscribe(&["test-offsets"])
        .expect("Failed to subscribe");
    
    // Should receive messages 10-19
    let mut next_expected = 10;
    let consume_timeout = Duration::from_secs(5);
    let start = std::time::Instant::now();
    
    while next_expected < 20 && start.elapsed() < consume_timeout {
        match timeout(Duration::from_secs(1), consumer2.recv()).await {
            Ok(Ok(message)) => {
                let key = message.key_view::<str>().unwrap().unwrap();
                let expected_key = format!("key-{}", next_expected);
                assert_eq!(key, expected_key);
                next_expected += 1;
            }
            _ => continue,
        }
    }
    
    assert_eq!(next_expected, 20);
    
    Ok(())
}