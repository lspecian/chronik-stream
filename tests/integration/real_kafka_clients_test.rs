//! Integration tests with real Kafka clients (kafkactl, Sarama, confluent-kafka-python)
//!
//! These tests verify that external Kafka clients can successfully interact with Chronik Stream.

use anyhow::{Result, Context};
use std::process::Stdio;
use tokio::process::Command;
use std::time::Duration;
use tempfile::TempDir;
use std::path::PathBuf;

use crate::testcontainers_setup::{TestEnvironment, ChronikCluster};

/// Helper to run a command and capture output
async fn run_command(cmd: &str, args: &[&str]) -> Result<(String, String, bool)> {
    let output = Command::new(cmd)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context(format!("Failed to execute {}", cmd))?;
    
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let success = output.status.success();
    
    Ok((stdout, stderr, success))
}

/// Test kafkactl client operations
#[tokio::test]
#[ignore] // Requires kafkactl to be installed
async fn test_kafkactl_operations() -> Result<()> {
    // Check if kafkactl is available
    if run_command("which", &["kafkactl"]).await.is_err() {
        eprintln!("Skipping test: kafkactl not found in PATH");
        return Ok(());
    }
    
    let test_env = TestEnvironment::new().await?;
    test_env.create_bucket("chronik-test").await?;
    
    let mut cluster = ChronikCluster::new(3, &test_env).await?;
    cluster.start(&test_env).await?;
    
    let bootstrap_servers = cluster.bootstrap_servers();
    let topic = "kafkactl-test-topic";
    
    // Test 1: Get broker information
    let (stdout, stderr, success) = run_command("kafkactl", &[
        "get", "brokers",
        "--brokers", &bootstrap_servers,
    ]).await?;
    
    assert!(success, "Failed to get brokers: {}", stderr);
    assert!(stdout.contains("ID"), "Output should contain broker IDs");
    assert!(stdout.contains(&bootstrap_servers), "Output should contain bootstrap servers");
    
    // Test 2: Create topic
    let (stdout, stderr, success) = run_command("kafkactl", &[
        "create", "topic", topic,
        "--partitions", "3",
        "--replication-factor", "2",
        "--brokers", &bootstrap_servers,
    ]).await?;
    
    assert!(success, "Failed to create topic: {}", stderr);
    assert!(stdout.contains("created") || stderr.contains("created"), 
            "Should confirm topic creation");
    
    // Test 3: List topics
    let (stdout, stderr, success) = run_command("kafkactl", &[
        "get", "topics",
        "--brokers", &bootstrap_servers,
    ]).await?;
    
    assert!(success, "Failed to list topics: {}", stderr);
    assert!(stdout.contains(topic), "Topic list should contain our topic");
    
    // Test 4: Produce message
    let test_message = "Hello from kafkactl!";
    let (stdout, stderr, success) = run_command("kafkactl", &[
        "produce", topic,
        "--value", test_message,
        "--brokers", &bootstrap_servers,
    ]).await?;
    
    assert!(success, "Failed to produce message: {}", stderr);
    
    // Test 5: Consume message
    let (stdout, stderr, success) = run_command("kafkactl", &[
        "consume", topic,
        "--from-beginning",
        "--max-messages", "1",
        "--print-values",
        "--brokers", &bootstrap_servers,
    ]).await?;
    
    assert!(success, "Failed to consume message: {}", stderr);
    assert!(stdout.contains(test_message), "Should consume the produced message");
    
    // Test 6: Describe topic
    let (stdout, stderr, success) = run_command("kafkactl", &[
        "describe", "topic", topic,
        "--brokers", &bootstrap_servers,
    ]).await?;
    
    assert!(success, "Failed to describe topic: {}", stderr);
    assert!(stdout.contains("Partitions: 3"), "Should show correct partition count");
    assert!(stdout.contains("ReplicationFactor: 2"), "Should show correct replication factor");
    
    // Test 7: Consumer groups
    let group_id = "kafkactl-test-group";
    let (stdout, stderr, success) = run_command("kafkactl", &[
        "consume", topic,
        "--group", group_id,
        "--from-beginning",
        "--max-messages", "1",
        "--brokers", &bootstrap_servers,
    ]).await?;
    
    assert!(success, "Failed to consume with group: {}", stderr);
    
    // List consumer groups
    let (stdout, stderr, success) = run_command("kafkactl", &[
        "get", "consumer-groups",
        "--brokers", &bootstrap_servers,
    ]).await?;
    
    assert!(success, "Failed to list consumer groups: {}", stderr);
    assert!(stdout.contains(group_id), "Should list our consumer group");
    
    cluster.stop().await?;
    Ok(())
}

/// Test Sarama (Go) client
#[tokio::test]
#[ignore] // Requires Go to be installed
async fn test_sarama_client() -> Result<()> {
    // Check if Go is available
    if run_command("which", &["go"]).await.is_err() {
        eprintln!("Skipping test: go not found in PATH");
        return Ok(());
    }
    
    let test_env = TestEnvironment::new().await?;
    test_env.create_bucket("chronik-test").await?;
    
    let mut cluster = ChronikCluster::new(3, &test_env).await?;
    cluster.start(&test_env).await?;
    
    let bootstrap_servers = cluster.bootstrap_servers();
    let topic = "sarama-test-topic";
    
    // Create a temporary directory for Go test
    let temp_dir = TempDir::new()?;
    let go_test_path = temp_dir.path().join("sarama_test.go");
    
    // Write Go test file
    let go_test_content = format!(r#"
package main

import (
    "fmt"
    "log"
    "os"
    "time"
    "github.com/Shopify/sarama"
)

func main() {{
    // Configuration
    config := sarama.NewConfig()
    config.Version = sarama.V2_6_0_0
    config.Producer.Return.Successes = true
    config.Consumer.Return.Errors = true
    
    brokers := []string{{"{}"}}
    topic := "{}"
    
    // Create client
    client, err := sarama.NewClient(brokers, config)
    if err != nil {{
        log.Fatalf("Failed to create client: %v", err)
    }}
    defer client.Close()
    
    // Get metadata
    topics, err := client.Topics()
    if err != nil {{
        log.Fatalf("Failed to get topics: %v", err)
    }}
    fmt.Printf("Available topics: %v\n", topics)
    
    // Create producer
    producer, err := sarama.NewSyncProducerFromClient(client)
    if err != nil {{
        log.Fatalf("Failed to create producer: %v", err)
    }}
    defer producer.Close()
    
    // Send message
    message := &sarama.ProducerMessage{{
        Topic: topic,
        Key:   sarama.StringEncoder("test-key"),
        Value: sarama.StringEncoder("Hello from Sarama!"),
    }}
    
    partition, offset, err := producer.SendMessage(message)
    if err != nil {{
        log.Fatalf("Failed to send message: %v", err)
    }}
    fmt.Printf("Message sent to partition %d at offset %d\n", partition, offset)
    
    // Create consumer
    consumer, err := sarama.NewConsumerFromClient(client)
    if err != nil {{
        log.Fatalf("Failed to create consumer: %v", err)
    }}
    defer consumer.Close()
    
    // Consume from partition
    partitionConsumer, err := consumer.ConsumePartition(topic, partition, offset)
    if err != nil {{
        log.Fatalf("Failed to create partition consumer: %v", err)
    }}
    defer partitionConsumer.Close()
    
    // Read message
    select {{
    case msg := <-partitionConsumer.Messages():
        fmt.Printf("Consumed message: key=%s, value=%s, offset=%d\n", 
            string(msg.Key), string(msg.Value), msg.Offset)
        if string(msg.Value) != "Hello from Sarama!" {{
            log.Fatal("Unexpected message content")
        }}
    case err := <-partitionConsumer.Errors():
        log.Fatalf("Consumer error: %v", err)
    case <-time.After(5 * time.Second):
        log.Fatal("Timeout waiting for message")
    }}
    
    fmt.Println("Sarama test completed successfully!")
}}
"#, bootstrap_servers, topic);
    
    std::fs::write(&go_test_path, go_test_content)?;
    
    // Initialize Go module
    let (_, stderr, success) = run_command("go", &[
        "mod", "init", "sarama_test",
    ]).await?;
    
    if !success {
        eprintln!("Failed to initialize Go module: {}", stderr);
    }
    
    // Get dependencies
    let (_, stderr, success) = run_command("go", &[
        "get", "github.com/Shopify/sarama@latest",
    ]).await?;
    
    if !success {
        eprintln!("Failed to get Sarama dependency: {}", stderr);
    }
    
    // Change to temp directory and run test
    std::env::set_current_dir(&temp_dir)?;
    
    let (stdout, stderr, success) = run_command("go", &[
        "run", "sarama_test.go",
    ]).await?;
    
    assert!(success, "Sarama test failed: {}\n{}", stdout, stderr);
    assert!(stdout.contains("Sarama test completed successfully!"), 
            "Test should complete successfully");
    
    cluster.stop().await?;
    Ok(())
}

/// Test confluent-kafka-python client
#[tokio::test]
#[ignore] // Requires Python and confluent-kafka to be installed
async fn test_confluent_kafka_python() -> Result<()> {
    // Check if Python is available
    if run_command("which", &["python3"]).await.is_err() {
        eprintln!("Skipping test: python3 not found in PATH");
        return Ok(());
    }
    
    // Check if confluent-kafka is installed
    let (_, _, has_confluent) = run_command("python3", &[
        "-c", "import confluent_kafka",
    ]).await?;
    
    if !has_confluent {
        eprintln!("Skipping test: confluent-kafka not installed");
        eprintln!("Install with: pip3 install confluent-kafka");
        return Ok(());
    }
    
    let test_env = TestEnvironment::new().await?;
    test_env.create_bucket("chronik-test").await?;
    
    let mut cluster = ChronikCluster::new(3, &test_env).await?;
    cluster.start(&test_env).await?;
    
    let bootstrap_servers = cluster.bootstrap_servers();
    let topic = "python-test-topic";
    
    // Create Python test script
    let python_test = format!(r#"
import sys
from confluent_kafka import Producer, Consumer, KafkaError, KafkaException
from confluent_kafka.admin import AdminClient, NewTopic
import time

def test_confluent_kafka():
    bootstrap_servers = '{}'
    topic = '{}'
    
    # Test 1: Admin client
    print("Testing admin client...")
    admin = AdminClient({{'bootstrap.servers': bootstrap_servers}})
    
    # Get metadata
    metadata = admin.list_topics(timeout=10)
    print(f"Cluster has {{len(metadata.brokers)}} brokers")
    
    # Create topic
    new_topic = NewTopic(topic, num_partitions=3, replication_factor=2)
    fs = admin.create_topics([new_topic])
    
    for topic_name, f in fs.items():
        try:
            f.result()  # Wait for operation to complete
            print(f"Topic {{topic_name}} created successfully")
        except Exception as e:
            if 'already exists' not in str(e):
                print(f"Failed to create topic {{topic_name}}: {{e}}")
                sys.exit(1)
    
    # Test 2: Producer
    print("Testing producer...")
    producer = Producer({{
        'bootstrap.servers': bootstrap_servers,
        'client.id': 'python-test-producer'
    }})
    
    delivered = []
    
    def delivery_report(err, msg):
        if err is not None:
            print(f'Message delivery failed: {{err}}')
            sys.exit(1)
        else:
            delivered.append(msg)
            print(f'Message delivered to {{msg.topic()}} [{{msg.partition()}}] @ {{msg.offset()}}')
    
    # Produce messages
    for i in range(10):
        producer.produce(
            topic,
            key=f'key-{{i}}'.encode('utf-8'),
            value=f'Hello from Python {{i}}'.encode('utf-8'),
            callback=delivery_report
        )
    
    # Wait for delivery
    producer.flush()
    
    if len(delivered) != 10:
        print(f"Expected 10 delivered messages, got {{len(delivered)}}")
        sys.exit(1)
    
    # Test 3: Consumer
    print("Testing consumer...")
    consumer = Consumer({{
        'bootstrap.servers': bootstrap_servers,
        'group.id': 'python-test-group',
        'auto.offset.reset': 'earliest'
    }})
    
    consumer.subscribe([topic])
    
    consumed = 0
    start_time = time.time()
    
    while consumed < 10 and time.time() - start_time < 10:
        msg = consumer.poll(timeout=1.0)
        
        if msg is None:
            continue
            
        if msg.error():
            if msg.error().code() == KafkaError._PARTITION_EOF:
                continue
            else:
                print(f'Consumer error: {{msg.error()}}')
                sys.exit(1)
        
        print(f'Consumed: key={{msg.key().decode("utf-8")}}, value={{msg.value().decode("utf-8")}}')
        consumed += 1
    
    consumer.close()
    
    if consumed != 10:
        print(f"Expected to consume 10 messages, got {{consumed}}")
        sys.exit(1)
    
    print("Python test completed successfully!")

if __name__ == '__main__':
    test_confluent_kafka()
"#, bootstrap_servers, topic);
    
    // Create temp file for Python script
    let temp_dir = TempDir::new()?;
    let python_script_path = temp_dir.path().join("test_confluent.py");
    std::fs::write(&python_script_path, python_test)?;
    
    // Run Python test
    let (stdout, stderr, success) = run_command("python3", &[
        python_script_path.to_str().unwrap(),
    ]).await?;
    
    assert!(success, "Python test failed: {}\n{}", stdout, stderr);
    assert!(stdout.contains("Python test completed successfully!"), 
            "Test should complete successfully");
    
    cluster.stop().await?;
    Ok(())
}

/// Test cross-client compatibility
#[tokio::test]
#[ignore] // Requires all clients to be installed
async fn test_cross_client_compatibility() -> Result<()> {
    // Check prerequisites
    let has_kafkactl = run_command("which", &["kafkactl"]).await.is_ok();
    let has_python = run_command("which", &["python3"]).await.is_ok();
    
    if !has_kafkactl || !has_python {
        eprintln!("Skipping test: requires both kafkactl and python3");
        return Ok(());
    }
    
    let (_, _, has_confluent) = run_command("python3", &[
        "-c", "import confluent_kafka",
    ]).await?;
    
    if !has_confluent {
        eprintln!("Skipping test: confluent-kafka not installed");
        return Ok(());
    }
    
    let test_env = TestEnvironment::new().await?;
    test_env.create_bucket("chronik-test").await?;
    
    let mut cluster = ChronikCluster::new(3, &test_env).await?;
    cluster.start(&test_env).await?;
    
    let bootstrap_servers = cluster.bootstrap_servers();
    let topic = "cross-client-test";
    
    // Create topic with kafkactl
    let (_, _, success) = run_command("kafkactl", &[
        "create", "topic", topic,
        "--partitions", "1",
        "--brokers", &bootstrap_servers,
    ]).await?;
    
    assert!(success, "Failed to create topic");
    
    // Produce with Python
    let python_produce = format!(r#"
from confluent_kafka import Producer
import sys

p = Producer({{'bootstrap.servers': '{}'}})

def delivery_report(err, msg):
    if err:
        sys.exit(1)

for i in range(5):
    p.produce('{}', key=f'py-key-{{i}}'.encode(), value=f'Message from Python {{i}}'.encode(), callback=delivery_report)

p.flush()
print("Produced 5 messages")
"#, bootstrap_servers, topic);
    
    let (stdout, _, success) = run_command("python3", &[
        "-c", &python_produce,
    ]).await?;
    
    assert!(success, "Failed to produce with Python");
    assert!(stdout.contains("Produced 5 messages"));
    
    // Produce with kafkactl
    for i in 0..5 {
        let (_, _, success) = run_command("kafkactl", &[
            "produce", topic,
            "--key", &format!("kafkactl-key-{}", i),
            "--value", &format!("Message from kafkactl {}", i),
            "--brokers", &bootstrap_servers,
        ]).await?;
        
        assert!(success, "Failed to produce with kafkactl");
    }
    
    // Consume all messages with kafkactl and verify
    let (stdout, _, success) = run_command("kafkactl", &[
        "consume", topic,
        "--from-beginning",
        "--max-messages", "10",
        "--print-keys",
        "--print-values",
        "--brokers", &bootstrap_servers,
    ]).await?;
    
    assert!(success, "Failed to consume messages");
    
    // Verify we got messages from both producers
    let python_count = stdout.matches("Message from Python").count();
    let kafkactl_count = stdout.matches("Message from kafkactl").count();
    
    assert_eq!(python_count, 5, "Should have 5 messages from Python");
    assert_eq!(kafkactl_count, 5, "Should have 5 messages from kafkactl");
    
    cluster.stop().await?;
    Ok(())
}

/// Test client error handling
#[tokio::test]
#[ignore] // Requires kafkactl to be installed
async fn test_client_error_handling() -> Result<()> {
    if run_command("which", &["kafkactl"]).await.is_err() {
        eprintln!("Skipping test: kafkactl not found in PATH");
        return Ok(());
    }
    
    let test_env = TestEnvironment::new().await?;
    test_env.create_bucket("chronik-test").await?;
    
    let mut cluster = ChronikCluster::new(1, &test_env).await?;
    cluster.start(&test_env).await?;
    
    let bootstrap_servers = cluster.bootstrap_servers();
    
    // Test 1: Invalid topic name
    let (stdout, stderr, success) = run_command("kafkactl", &[
        "create", "topic", "invalid..topic",
        "--brokers", &bootstrap_servers,
    ]).await?;
    
    assert!(!success, "Should fail with invalid topic name");
    assert!(stdout.contains("invalid") || stderr.contains("invalid"), 
            "Should report invalid topic name");
    
    // Test 2: Non-existent topic
    let (stdout, stderr, success) = run_command("kafkactl", &[
        "consume", "non-existent-topic",
        "--brokers", &bootstrap_servers,
        "--max-messages", "1",
    ]).await?;
    
    // This might succeed with no messages or fail - both are acceptable
    if !success {
        assert!(stderr.contains("topic") || stderr.contains("not found"),
                "Should indicate topic issue");
    } else {
        // If it succeeds, should not output any messages
        assert!(stdout.is_empty() || stdout.trim().is_empty(),
                "Should not have messages from non-existent topic");
    }
    
    // Test 3: Invalid broker address
    let (_, stderr, success) = run_command("kafkactl", &[
        "get", "brokers",
        "--brokers", "invalid-broker:9999",
    ]).await?;
    
    assert!(!success, "Should fail with invalid broker");
    assert!(stderr.contains("connect") || stderr.contains("resolve") || 
            stderr.contains("broker"), "Should indicate connection issue");
    
    cluster.stop().await?;
    Ok(())
}

/// Helper function to check if Chronik Stream properly handles Kafka protocol versions
#[tokio::test]
async fn test_protocol_version_negotiation() -> Result<()> {
    let test_env = TestEnvironment::new().await?;
    test_env.create_bucket("chronik-test").await?;
    
    let mut cluster = ChronikCluster::new(1, &test_env).await?;
    cluster.start(&test_env).await?;
    
    // Use rdkafka to test version negotiation
    use rdkafka::config::ClientConfig;
    use rdkafka::admin::{AdminClient, AdminOptions};
    
    let bootstrap_servers = cluster.bootstrap_servers();
    
    // Test with different API version settings
    let versions = vec![
        "0.10.0.0",
        "0.11.0.0",
        "1.0.0",
        "2.0.0",
        "2.6.0",
    ];
    
    for version in versions {
        println!("Testing with broker.version.fallback = {}", version);
        
        let admin: AdminClient<_> = ClientConfig::new()
            .set("bootstrap.servers", &bootstrap_servers)
            .set("broker.version.fallback", version)
            .set("api.version.request", "true")
            .create()
            .context(format!("Failed to create admin client for version {}", version))?;
        
        // Try to get metadata - should work regardless of version
        let metadata = admin.inner().fetch_metadata(None, Duration::from_secs(5))
            .context(format!("Failed to fetch metadata for version {}", version))?;
        
        assert!(!metadata.brokers().is_empty(), 
                "Should have brokers for version {}", version);
        
        println!("Successfully negotiated protocol for version {}", version);
    }
    
    cluster.stop().await?;
    Ok(())
}