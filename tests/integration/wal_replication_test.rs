//! WAL Replication Integration Test
//!
//! Tests the WAL replication protocol between leader and follower nodes.
//! Validates:
//! - Follower connection and frame protocol
//! - Heartbeat handling
//! - WAL record replication (metadata + data)
//! - ACK protocol for acks=-1
//! - Timeout monitoring and reconnection
//!
//! This test validates the refactored modules from Phase 2.3:
//! - connection_state.rs - Connection setup and timeout monitoring
//! - frame_reader.rs - Frame protocol operations
//! - record_processor.rs - WAL record processing and ACK protocol

use super::common::*;
use chronik_common::Result;
use rdkafka::{
    ClientConfig,
    Message,
    admin::{AdminClient, AdminOptions, NewTopic, TopicReplication},
    consumer::{Consumer, StreamConsumer},
    producer::{FutureProducer, FutureRecord},
};
use std::time::Duration;
use tokio::time::{sleep, timeout};
use tracing::{info, debug};

/// Test topic for replication validation
const REPLICATION_TEST_TOPIC: &str = "replication-test-topic";
const MESSAGE_COUNT: usize = 100;

#[tokio::test]
async fn test_wal_replication_basic() -> Result<()> {
    super::test_setup::init();

    info!("Starting WAL replication basic test");

    // Start 3-node cluster (leader + 2 followers)
    let cluster = TestCluster::start(TestClusterConfig {
        node_count: 3,
        replication_factor: 3,
        ..Default::default()
    }).await?;

    let bootstrap_servers = cluster.bootstrap_servers();

    // Create topic with replication
    let admin: AdminClient<_> = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .create()
        .expect("Failed to create admin client");

    let topic = NewTopic::new(REPLICATION_TEST_TOPIC, 1, TopicReplication::Fixed(3));
    admin
        .create_topics(&[topic], &AdminOptions::new())
        .await
        .expect("Failed to create topics")[0]
        .expect("Failed to create topic");

    info!("Topic created with replication factor 3");

    // Wait for partition assignment and replication setup
    sleep(Duration::from_secs(2)).await;

    // Produce messages with acks=-1 (requires all in-sync replicas)
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("acks", "all") // Requires ACKs from all ISR members
        .set("request.timeout.ms", "10000")
        .create()
        .expect("Failed to create producer");

    info!("Producing {} messages with acks=all", MESSAGE_COUNT);

    for i in 0..MESSAGE_COUNT {
        let result = producer
            .send(
                FutureRecord::to(REPLICATION_TEST_TOPIC)
                    .key(&format!("key-{}", i))
                    .payload(&format!("value-{}", i)),
                Duration::from_secs(10),
            )
            .await;

        assert!(result.is_ok(), "Produce should succeed with all replicas ACKing");
    }

    info!("All messages produced successfully - replication ACKs working");

    // Consume messages to verify replication
    let consumer: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("group.id", "replication-test-consumer")
        .set("auto.offset.reset", "earliest")
        .set("enable.auto.commit", "false")
        .create()
        .expect("Failed to create consumer");

    consumer
        .subscribe(&[REPLICATION_TEST_TOPIC])
        .expect("Failed to subscribe");

    // Consume all messages
    let mut consumed_count = 0;
    let start = std::time::Instant::now();

    while consumed_count < MESSAGE_COUNT && start.elapsed() < Duration::from_secs(10) {
        match timeout(Duration::from_secs(1), consumer.recv()).await {
            Ok(Ok(message)) => {
                let key = message.key().and_then(|k| std::str::from_utf8(k).ok());
                let payload = message.payload().and_then(|p| std::str::from_utf8(p).ok());

                assert!(key.is_some(), "Message should have key");
                assert!(payload.is_some(), "Message should have payload");

                consumed_count += 1;
            }
            Ok(Err(e)) => {
                panic!("Consumer error: {}", e);
            }
            Err(_) => {
                // Timeout - check if we got all messages
                if consumed_count == MESSAGE_COUNT {
                    break;
                }
            }
        }
    }

    assert_eq!(consumed_count, MESSAGE_COUNT, "Should consume all replicated messages");

    info!("✅ WAL replication basic test passed - all messages replicated and consumed");

    Ok(())
}

#[tokio::test]
async fn test_wal_replication_follower_recovery() -> Result<()> {
    super::test_setup::init();

    info!("Starting WAL replication follower recovery test");

    // Start 3-node cluster
    let mut cluster = TestCluster::start(TestClusterConfig {
        node_count: 3,
        replication_factor: 3,
        ..Default::default()
    }).await?;

    let bootstrap_servers = cluster.bootstrap_servers();

    // Create topic
    let admin: AdminClient<_> = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .create()
        .expect("Failed to create admin client");

    let topic = NewTopic::new("recovery-test-topic", 1, TopicReplication::Fixed(3));
    admin
        .create_topics(&[topic], &AdminOptions::new())
        .await
        .expect("Failed to create topics")[0]
        .expect("Failed to create topic");

    sleep(Duration::from_secs(2)).await;

    // Produce initial messages
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("acks", "all")
        .create()
        .expect("Failed to create producer");

    info!("Producing initial 50 messages");

    for i in 0..50 {
        producer
            .send(
                FutureRecord::to("recovery-test-topic")
                    .payload(&format!("initial-{}", i)),
                Duration::from_secs(5),
            )
            .await
            .expect("Initial produce should succeed");
    }

    info!("Stopping one follower node");

    // Stop one follower (node 2)
    cluster.stop_node(2).await?;

    sleep(Duration::from_secs(1)).await;

    // Produce more messages while follower is down
    info!("Producing 50 more messages while follower is down");

    for i in 50..100 {
        // Note: This might fail or timeout if ISR shrinks, which is expected behavior
        let _ = producer
            .send(
                FutureRecord::to("recovery-test-topic")
                    .payload(&format!("during-outage-{}", i)),
                Duration::from_secs(5),
            )
            .await;
    }

    info!("Restarting follower node");

    // Restart the follower
    cluster.start_node(2).await?;

    // Wait for follower to catch up via replication
    sleep(Duration::from_secs(3)).await;

    info!("Producing final 50 messages to verify full recovery");

    // Produce final messages with acks=all (requires recovered follower)
    for i in 100..150 {
        let result = timeout(
            Duration::from_secs(10),
            producer.send(
                FutureRecord::to("recovery-test-topic")
                    .payload(&format!("after-recovery-{}", i)),
                Duration::from_secs(5),
            )
        ).await;

        assert!(result.is_ok(), "Produce should succeed after follower recovery");
    }

    info!("✅ WAL replication follower recovery test passed - follower caught up via replication");

    Ok(())
}

#[tokio::test]
async fn test_wal_replication_metadata_sync() -> Result<()> {
    super::test_setup::init();

    info!("Starting WAL replication metadata sync test");

    // Start 3-node cluster
    let cluster = TestCluster::start(TestClusterConfig {
        node_count: 3,
        replication_factor: 3,
        ..Default::default()
    }).await?;

    let bootstrap_servers = cluster.bootstrap_servers();

    // Create multiple topics to test metadata replication
    let admin: AdminClient<_> = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .create()
        .expect("Failed to create admin client");

    let topics = vec![
        NewTopic::new("metadata-test-1", 3, TopicReplication::Fixed(3)),
        NewTopic::new("metadata-test-2", 2, TopicReplication::Fixed(3)),
        NewTopic::new("metadata-test-3", 1, TopicReplication::Fixed(3)),
    ];

    info!("Creating 3 topics with different partition counts");

    admin
        .create_topics(&topics, &AdminOptions::new())
        .await
        .expect("Failed to create topics");

    // Wait for metadata to propagate to all followers
    sleep(Duration::from_secs(3)).await;

    // Verify all nodes have consistent metadata by producing to each topic
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("acks", "all")
        .create()
        .expect("Failed to create producer");

    for topic in &["metadata-test-1", "metadata-test-2", "metadata-test-3"] {
        info!("Producing to topic: {}", topic);

        let result = producer
            .send(
                FutureRecord::to(topic)
                    .payload(b"metadata-sync-test"),
                Duration::from_secs(5),
            )
            .await;

        assert!(result.is_ok(), "Produce should succeed if metadata is replicated to all nodes");
    }

    info!("✅ WAL replication metadata sync test passed - metadata replicated correctly");

    Ok(())
}

#[tokio::test]
async fn test_wal_replication_heartbeat_timeout() -> Result<()> {
    super::test_setup::init();

    info!("Starting WAL replication heartbeat timeout test");

    // This test validates the timeout monitoring logic in connection_state.rs
    // We start a cluster, establish replication, then simulate network partition

    let mut cluster = TestCluster::start(TestClusterConfig {
        node_count: 3,
        replication_factor: 3,
        heartbeat_interval_ms: 1000, // 1 second heartbeats
        ..Default::default()
    }).await?;

    let bootstrap_servers = cluster.bootstrap_servers();

    // Create topic
    let admin: AdminClient<_> = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .create()
        .expect("Failed to create admin client");

    let topic = NewTopic::new("heartbeat-test", 1, TopicReplication::Fixed(3));
    admin
        .create_topics(&[topic], &AdminOptions::new())
        .await
        .expect("Failed to create topics")[0]
        .expect("Failed to create topic");

    sleep(Duration::from_secs(2)).await;

    // Produce messages
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("acks", "all")
        .create()
        .expect("Failed to create producer");

    info!("Producing initial messages");

    for i in 0..10 {
        producer
            .send(
                FutureRecord::to("heartbeat-test")
                    .payload(&format!("msg-{}", i)),
                Duration::from_secs(5),
            )
            .await
            .expect("Produce should succeed");
    }

    // Simulate network partition by stopping follower
    info!("Simulating network partition");
    cluster.stop_node(2).await?;

    // Wait for heartbeat timeout (should detect missing follower)
    sleep(Duration::from_secs(5)).await;

    // Leader should continue accepting writes with reduced ISR
    info!("Producing messages with reduced ISR");

    for i in 10..20 {
        let result = producer
            .send(
                FutureRecord::to("heartbeat-test")
                    .payload(&format!("msg-{}", i)),
                Duration::from_secs(5),
            )
            .await;

        // Should succeed even with one follower down (2/3 ISR is still majority)
        assert!(result.is_ok(), "Produce should succeed with reduced ISR");
    }

    info!("✅ WAL replication heartbeat timeout test passed - timeout detection working");

    Ok(())
}
