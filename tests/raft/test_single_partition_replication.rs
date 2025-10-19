/// Integration test: Single partition replication across Raft cluster
///
/// Tests that messages produced to the leader are replicated to all followers
/// and can be consumed from any node in the cluster.

mod common;

use anyhow::Result;
use common::*;
use std::time::Duration;
use tokio::time::sleep;

#[tokio::test]
async fn test_basic_replication_3_nodes() -> Result<()> {
    tracing_subscriber::fmt::init();

    // Spawn 3-node cluster
    let mut cluster = spawn_test_cluster(3).await?;

    // Wait for leader election
    let leader_id = cluster.wait_for_leader(Duration::from_secs(10)).await?;
    println!("Leader elected: node {}", leader_id);

    // Produce message to leader
    let leader_node = cluster.get_node(leader_id).unwrap();
    let producer = create_kafka_producer(leader_node.kafka_addr)?;

    let topic = "test-replication";
    let messages = vec![b"message1", b"message2", b"message3"];

    for msg in &messages {
        producer.send(topic, 0, *msg).await?;
    }

    println!("Produced {} messages to leader", messages.len());

    // Wait for replication
    sleep(Duration::from_secs(2)).await;

    // Verify all nodes have the messages
    for node in &cluster.nodes {
        let consumer = create_kafka_consumer(node.kafka_addr)?;
        let records = consumer.fetch(topic, 0, 0).await?;

        assert_eq!(
            records.len(),
            messages.len(),
            "Node {} should have {} messages",
            node.node_id,
            messages.len()
        );

        for (i, record) in records.iter().enumerate() {
            assert_eq!(
                record.value, messages[i],
                "Node {} message {} mismatch",
                node.node_id, i
            );
        }

        println!("Node {} verified: {} messages", node.node_id, records.len());
    }

    cluster.cleanup().await;
    Ok(())
}

#[tokio::test]
async fn test_replication_after_follower_restart() -> Result<()> {
    tracing_subscriber::fmt::init();

    let mut cluster = spawn_test_cluster(3).await?;
    let leader_id = cluster.wait_for_leader(Duration::from_secs(10)).await?;

    // Pick a follower to restart
    let follower_id = cluster.nodes.iter()
        .find(|n| n.node_id != leader_id)
        .unwrap()
        .node_id;

    println!("Leader: {}, Follower to restart: {}", leader_id, follower_id);

    // Produce initial batch
    let leader_node = cluster.get_node(leader_id).unwrap();
    let producer = create_kafka_producer(leader_node.kafka_addr)?;

    let topic = "test-restart";
    producer.send(topic, 0, b"before-restart").await?;

    sleep(Duration::from_secs(1)).await;

    // Restart follower
    cluster.restart_node(follower_id).await?;
    println!("Follower {} restarted", follower_id);

    sleep(Duration::from_secs(2)).await;

    // Produce after restart
    producer.send(topic, 0, b"after-restart").await?;

    sleep(Duration::from_secs(2)).await;

    // Verify follower has both messages
    let follower_node = cluster.get_node(follower_id).unwrap();
    let consumer = create_kafka_consumer(follower_node.kafka_addr)?;
    let records = consumer.fetch(topic, 0, 0).await?;

    assert_eq!(records.len(), 2, "Should have 2 messages after restart");
    assert_eq!(records[0].value, b"before-restart");
    assert_eq!(records[1].value, b"after-restart");

    println!("Follower {} successfully caught up", follower_id);

    cluster.cleanup().await;
    Ok(())
}

#[tokio::test]
async fn test_replication_with_lag() -> Result<()> {
    tracing_subscriber::fmt::init();

    let mut cluster = spawn_test_cluster(3).await?;
    let leader_id = cluster.wait_for_leader(Duration::from_secs(10)).await?;

    let follower_id = cluster.nodes.iter()
        .find(|n| n.node_id != leader_id)
        .unwrap()
        .node_id;

    // Kill follower to create lag
    cluster.kill_node(follower_id).await?;
    println!("Follower {} killed", follower_id);

    // Produce messages while follower is down
    let leader_node = cluster.get_node(leader_id).unwrap();
    let producer = create_kafka_producer(leader_node.kafka_addr)?;

    let topic = "test-lag";
    for i in 0..10 {
        producer.send(topic, 0, format!("message-{}", i).as_bytes()).await?;
    }

    sleep(Duration::from_secs(1)).await;

    // Restart follower - should catch up
    cluster.restart_node(follower_id).await?;
    println!("Follower {} restarted, catching up...", follower_id);

    // Wait for catch-up
    sleep(Duration::from_secs(3)).await;

    // Verify follower has all messages
    let follower_node = cluster.get_node(follower_id).unwrap();
    let consumer = create_kafka_consumer(follower_node.kafka_addr)?;
    let records = consumer.fetch(topic, 0, 0).await?;

    assert_eq!(records.len(), 10, "Follower should catch up all 10 messages");

    for i in 0..10 {
        let expected = format!("message-{}", i);
        assert_eq!(
            records[i].value,
            expected.as_bytes(),
            "Message {} mismatch",
            i
        );
    }

    println!("Follower {} caught up successfully", follower_id);

    cluster.cleanup().await;
    Ok(())
}

#[tokio::test]
async fn test_replication_consistency() -> Result<()> {
    tracing_subscriber::fmt::init();

    let mut cluster = spawn_test_cluster(5).await?;
    let leader_id = cluster.wait_for_leader(Duration::from_secs(10)).await?;

    // Produce large batch
    let leader_node = cluster.get_node(leader_id).unwrap();
    let producer = create_kafka_producer(leader_node.kafka_addr)?;

    let topic = "test-consistency";
    let message_count = 100;

    for i in 0..message_count {
        producer.send(topic, 0, format!("msg-{:04}", i).as_bytes()).await?;
    }

    println!("Produced {} messages", message_count);

    // Wait for replication
    sleep(Duration::from_secs(3)).await;

    // Verify all nodes have identical state
    let mut checksums = Vec::new();

    for node in &cluster.nodes {
        let consumer = create_kafka_consumer(node.kafka_addr)?;
        let records = consumer.fetch(topic, 0, 0).await?;

        assert_eq!(
            records.len(),
            message_count,
            "Node {} should have {} messages",
            node.node_id,
            message_count
        );

        // Calculate checksum of all messages
        let checksum = calculate_checksum(&records);
        checksums.push((node.node_id, checksum));

        println!("Node {} checksum: {:x}", node.node_id, checksum);
    }

    // All checksums should match
    let first_checksum = checksums[0].1;
    for (node_id, checksum) in &checksums {
        assert_eq!(
            *checksum, first_checksum,
            "Node {} has different checksum",
            node_id
        );
    }

    println!("All nodes have consistent state");

    cluster.cleanup().await;
    Ok(())
}

// Helper functions

fn create_kafka_producer(addr: std::net::SocketAddr) -> Result<KafkaProducer> {
    // TODO: Implement using kafka-python or rdkafka
    // For now, mock implementation
    Ok(KafkaProducer { addr })
}

fn create_kafka_consumer(addr: std::net::SocketAddr) -> Result<KafkaConsumer> {
    Ok(KafkaConsumer { addr })
}

fn calculate_checksum(records: &[KafkaRecord]) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    for record in records {
        record.value.hash(&mut hasher);
    }
    hasher.finish()
}

// Mock Kafka client (replace with real implementation)

struct KafkaProducer {
    addr: std::net::SocketAddr,
}

impl KafkaProducer {
    async fn send(&self, topic: &str, partition: i32, value: &[u8]) -> Result<()> {
        // TODO: Use rdkafka or kafka-python
        // For now, use HTTP API as placeholder
        let url = format!(
            "http://{}/api/v1/produce?topic={}&partition={}",
            self.addr, topic, partition
        );

        reqwest::Client::new()
            .post(&url)
            .body(value.to_vec())
            .send()
            .await?
            .error_for_status()?;

        Ok(())
    }
}

struct KafkaConsumer {
    addr: std::net::SocketAddr,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct KafkaRecord {
    offset: i64,
    value: Vec<u8>,
}

impl KafkaConsumer {
    async fn fetch(&self, topic: &str, partition: i32, offset: i64) -> Result<Vec<KafkaRecord>> {
        // TODO: Use rdkafka or kafka-python
        // For now, use HTTP API as placeholder
        let url = format!(
            "http://{}/api/v1/fetch?topic={}&partition={}&offset={}",
            self.addr, topic, partition, offset
        );

        let resp = reqwest::Client::new()
            .get(&url)
            .send()
            .await?
            .error_for_status()?;

        let records: Vec<KafkaRecord> = resp.json().await?;
        Ok(records)
    }
}

// Property-based testing with proptest

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(10))]

        #[test]
        fn test_arbitrary_message_replication(
            messages in prop::collection::vec(
                prop::collection::vec(any::<u8>(), 1..1024),
                1..50
            )
        ) {
            let runtime = tokio::runtime::Runtime::new().unwrap();
            runtime.block_on(async move {
                let mut cluster = spawn_test_cluster(3).await.unwrap();
                let leader_id = cluster.wait_for_leader(Duration::from_secs(10)).await.unwrap();

                let leader_node = cluster.get_node(leader_id).unwrap();
                let producer = create_kafka_producer(leader_node.kafka_addr).unwrap();

                let topic = "proptest-topic";

                // Produce all messages
                for msg in &messages {
                    producer.send(topic, 0, msg).await.unwrap();
                }

                // Wait for replication
                sleep(Duration::from_secs(2)).await;

                // Verify all nodes have same messages
                for node in &cluster.nodes {
                    let consumer = create_kafka_consumer(node.kafka_addr).unwrap();
                    let records = consumer.fetch(topic, 0, 0).await.unwrap();

                    assert_eq!(records.len(), messages.len());

                    for (i, record) in records.iter().enumerate() {
                        assert_eq!(&record.value, &messages[i]);
                    }
                }

                cluster.cleanup().await;
            });
        }
    }
}
