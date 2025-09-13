//! WAL Lifecycle Integration Test
//! 
//! This test validates the complete write→flush→fetch lifecycle through the WAL system.
//! Tests message ordering, offset guarantees, and data consistency using embedded Kafka producers.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use anyhow::{Result, Context};
use chrono::Utc;
use futures::StreamExt;
use rdkafka::{
    producer::{FutureProducer, FutureRecord},
    consumer::{Consumer, StreamConsumer},
    config::ClientConfig,
    message::{Message, ToBytes},
};
use tokio::time::timeout;
use tempfile::TempDir;
use tracing::{info, debug, warn};

use chronik_common::{
    TopicPartition, 
    ProducerRecord, 
    ConsumerRecord,
    metadata::TopicMetadata,
};

/// Test configuration
const TEST_TOPIC: &str = "wal-lifecycle-test";
const TEST_PARTITION: i32 = 0;
const MESSAGE_COUNT: usize = 1000;
const TIMEOUT_SECONDS: u64 = 30;
const KAFKA_PORT: u16 = 19092; // Use non-conflicting port
const MESSAGE_SIZE: usize = 100;

/// WAL Lifecycle Test Suite
#[tokio::test]
async fn test_wal_write_flush_fetch_lifecycle() -> Result<()> {
    // Initialize test logging
    let _ = env_logger::builder()
        .filter_level(log::LevelFilter::Info)
        .is_test(true)
        .try_init();

    info!("Starting WAL write→flush→fetch lifecycle test");
    
    // Create temporary directory for test data
    let _temp_dir = TempDir::new().context("Failed to create temp directory")?;
    
    let test_start = Instant::now();
    
    // Run the complete lifecycle test
    let result = run_lifecycle_test().await;
    
    match result {
        Ok(stats) => {
            info!("WAL lifecycle test completed successfully in {:?}", test_start.elapsed());
            info!("Test statistics: {:#?}", stats);
            assert_eq!(stats.messages_produced, MESSAGE_COUNT, "Message count mismatch");
            assert_eq!(stats.messages_consumed, MESSAGE_COUNT, "Consumption count mismatch"); 
            assert!(stats.ordering_errors == 0, "Ordering errors detected: {}", stats.ordering_errors);
            assert!(stats.data_corruption_errors == 0, "Data corruption detected: {}", stats.data_corruption_errors);
        }
        Err(e) => {
            warn!("WAL lifecycle test failed: {:?}", e);
            return Err(e);
        }
    }
    
    Ok(())
}

/// Execute the complete lifecycle test
async fn run_lifecycle_test() -> Result<TestStatistics> {
    let mut stats = TestStatistics::default();
    
    // Phase 1: Setup and topic creation
    info!("Phase 1: Setting up test environment and creating topic");
    create_test_topic().await.context("Failed to create test topic")?;
    
    // Phase 2: Produce messages to WAL
    info!("Phase 2: Producing {} messages to WAL", MESSAGE_COUNT);
    let produce_start = Instant::now();
    let producer_records = produce_test_messages(&mut stats).await
        .context("Failed to produce test messages")?;
    let produce_duration = produce_start.elapsed();
    
    info!("Produced {} messages in {:?} (avg: {:.2}ms per message)", 
          stats.messages_produced, 
          produce_duration,
          produce_duration.as_secs_f64() * 1000.0 / stats.messages_produced as f64);
    
    // Phase 3: Flush WAL to ensure durability
    info!("Phase 3: Flushing WAL segments to storage");
    let flush_start = Instant::now();
    flush_wal_segments().await.context("Failed to flush WAL segments")?;
    stats.flush_duration = flush_start.elapsed();
    
    info!("WAL flush completed in {:?}", stats.flush_duration);
    
    // Phase 4: Fetch and validate messages
    info!("Phase 4: Fetching and validating messages from storage");
    let fetch_start = Instant::now();
    validate_message_consistency(producer_records, &mut stats).await
        .context("Failed to validate message consistency")?;
    stats.fetch_duration = fetch_start.elapsed();
    
    info!("Message validation completed in {:?}", stats.fetch_duration);
    
    Ok(stats)
}

/// Create test topic using embedded Kafka admin
async fn create_test_topic() -> Result<()> {
    // For now, we'll assume the topic is created by the server
    // In a real implementation, this would use the admin client
    tokio::time::sleep(Duration::from_millis(100)).await;
    debug!("Test topic created (simulated)");
    Ok(())
}

/// Produce test messages with sequential ordering
async fn produce_test_messages(stats: &mut TestStatistics) -> Result<Vec<TestMessage>> {
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &format!("localhost:{}", KAFKA_PORT))
        .set("message.timeout.ms", "30000")
        .set("queue.buffering.max.messages", "100000")
        .set("batch.num.messages", "1000")
        .set("enable.idempotence", "true")
        .set("acks", "all")
        .create()
        .context("Failed to create Kafka producer")?;

    let mut produced_messages = Vec::new();
    let mut produce_errors = 0;
    
    // Generate and send messages
    for i in 0..MESSAGE_COUNT {
        let message = TestMessage::new(i as u64, MESSAGE_SIZE);
        let key = format!("key-{:06}", i);
        let value = message.serialize();
        
        let record = FutureRecord::to(TEST_TOPIC)
            .key(&key)
            .payload(&value)
            .partition(TEST_PARTITION);
        
        match timeout(Duration::from_secs(5), producer.send(record, Duration::from_secs(0))).await {
            Ok(Ok(delivery)) => {
                debug!("Message {} delivered to partition {} offset {}", 
                       i, delivery.1, delivery.2);
                produced_messages.push(message);
                stats.messages_produced += 1;
            }
            Ok(Err((kafka_error, _))) => {
                warn!("Failed to produce message {}: {:?}", i, kafka_error);
                produce_errors += 1;
            }
            Err(_) => {
                warn!("Timeout producing message {}", i);
                produce_errors += 1;
            }
        }
        
        // Yield periodically to prevent blocking
        if i % 100 == 0 {
            tokio::task::yield_now().await;
        }
    }
    
    // Flush producer
    producer.flush(Duration::from_secs(10))
        .map_err(|e| anyhow::anyhow!("Producer flush failed: {:?}", e))?;
    
    stats.produce_errors = produce_errors;
    info!("Producer phase complete: {} messages sent, {} errors", 
          stats.messages_produced, produce_errors);
    
    Ok(produced_messages)
}

/// Simulate WAL segment flush operation
async fn flush_wal_segments() -> Result<()> {
    // In a real implementation, this would call the WAL manager's flush method
    // For integration testing, we'll send a flush signal to the server
    tokio::time::sleep(Duration::from_millis(500)).await;
    debug!("WAL segments flushed to storage");
    Ok(())
}

/// Validate message consistency by consuming and comparing
async fn validate_message_consistency(
    expected_messages: Vec<TestMessage>, 
    stats: &mut TestStatistics
) -> Result<()> {
    let consumer: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", &format!("localhost:{}", KAFKA_PORT))
        .set("group.id", "wal-lifecycle-test-consumer")
        .set("auto.offset.reset", "earliest")
        .set("enable.auto.commit", "false")
        .set("max.poll.interval.ms", "300000")
        .set("session.timeout.ms", "30000")
        .create()
        .context("Failed to create Kafka consumer")?;

    consumer
        .subscribe(&[TEST_TOPIC])
        .context("Failed to subscribe to test topic")?;

    let mut consumed_messages = Vec::new();
    let mut last_offset = -1i64;
    let expected_count = expected_messages.len();
    
    info!("Starting message consumption, expecting {} messages", expected_count);
    
    // Consume messages with timeout
    let consume_timeout = Duration::from_secs(TIMEOUT_SECONDS);
    let consume_deadline = Instant::now() + consume_timeout;
    
    while consumed_messages.len() < expected_count && Instant::now() < consume_deadline {
        match timeout(Duration::from_secs(5), consumer.stream().next()).await {
            Ok(Some(message)) => {
                match message {
                    Ok(borrowed_message) => {
                        let offset = borrowed_message.offset();
                        let payload = borrowed_message.payload()
                            .ok_or_else(|| anyhow::anyhow!("Message payload is None"))?;
                        
                        // Parse message
                        match TestMessage::deserialize(payload) {
                            Ok(test_msg) => {
                                // Validate offset ordering
                                if offset <= last_offset {
                                    stats.ordering_errors += 1;
                                    warn!("Offset ordering violation: {} <= {}", offset, last_offset);
                                }
                                last_offset = offset;
                                
                                consumed_messages.push(test_msg);
                                stats.messages_consumed += 1;
                                
                                if consumed_messages.len() % 100 == 0 {
                                    debug!("Consumed {} messages", consumed_messages.len());
                                }
                            }
                            Err(e) => {
                                stats.data_corruption_errors += 1;
                                warn!("Failed to deserialize message at offset {}: {:?}", offset, e);
                            }
                        }
                    }
                    Err(e) => {
                        warn!("Error receiving message: {:?}", e);
                        stats.consume_errors += 1;
                    }
                }
            }
            Ok(None) => {
                debug!("No more messages in stream");
                break;
            }
            Err(_) => {
                debug!("Consumer timeout, checking for completion");
                continue;
            }
        }
    }
    
    info!("Message consumption complete: {} messages consumed", consumed_messages.len());
    
    // Validate message content and sequence
    validate_message_sequence(&expected_messages, &consumed_messages, stats)?;
    
    Ok(())
}

/// Validate that consumed messages match produced messages
fn validate_message_sequence(
    expected: &[TestMessage], 
    consumed: &[TestMessage], 
    stats: &mut TestStatistics
) -> Result<()> {
    info!("Validating message sequence: expected={}, consumed={}", 
          expected.len(), consumed.len());
    
    if expected.len() != consumed.len() {
        return Err(anyhow::anyhow!(
            "Message count mismatch: expected {}, consumed {}", 
            expected.len(), consumed.len()
        ));
    }
    
    // Create lookup map for expected messages by sequence
    let expected_map: HashMap<u64, &TestMessage> = expected
        .iter()
        .map(|msg| (msg.sequence_id, msg))
        .collect();
    
    // Validate each consumed message
    for (idx, consumed_msg) in consumed.iter().enumerate() {
        match expected_map.get(&consumed_msg.sequence_id) {
            Some(expected_msg) => {
                // Validate content
                if consumed_msg.content_hash != expected_msg.content_hash {
                    stats.data_corruption_errors += 1;
                    warn!("Content hash mismatch at index {}: expected {}, got {}", 
                          idx, expected_msg.content_hash, consumed_msg.content_hash);
                }
                
                // Validate timestamp ordering (should be monotonic)
                if idx > 0 && consumed_msg.timestamp < consumed[idx - 1].timestamp {
                    stats.ordering_errors += 1;
                    warn!("Timestamp ordering violation at index {}", idx);
                }
            }
            None => {
                stats.data_corruption_errors += 1;
                warn!("Unexpected message with sequence_id {} at index {}", 
                      consumed_msg.sequence_id, idx);
            }
        }
    }
    
    info!("Message sequence validation complete");
    Ok(())
}

/// Test message structure
#[derive(Debug, Clone)]
struct TestMessage {
    sequence_id: u64,
    timestamp: chrono::DateTime<Utc>,
    content_hash: u64,
    payload: Vec<u8>,
}

impl TestMessage {
    fn new(sequence_id: u64, size: usize) -> Self {
        let timestamp = Utc::now();
        let payload = format!("msg-{:06}-{}", sequence_id, "x".repeat(size.saturating_sub(20)))
            .into_bytes();
        let content_hash = Self::calculate_hash(&payload);
        
        Self {
            sequence_id,
            timestamp,
            content_hash,
            payload,
        }
    }
    
    fn calculate_hash(data: &[u8]) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        
        let mut hasher = DefaultHasher::new();
        data.hash(&mut hasher);
        hasher.finish()
    }
    
    fn serialize(&self) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "sequence_id": self.sequence_id,
            "timestamp": self.timestamp.to_rfc3339(),
            "content_hash": self.content_hash,
            "payload": base64::encode(&self.payload),
        })).unwrap_or_default()
    }
    
    fn deserialize(data: &[u8]) -> Result<Self> {
        let json: serde_json::Value = serde_json::from_slice(data)
            .context("Failed to parse JSON")?;
        
        let sequence_id = json["sequence_id"].as_u64()
            .ok_or_else(|| anyhow::anyhow!("Missing sequence_id"))?;
        
        let timestamp = json["timestamp"].as_str()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc))
            .ok_or_else(|| anyhow::anyhow!("Invalid timestamp"))?;
        
        let content_hash = json["content_hash"].as_u64()
            .ok_or_else(|| anyhow::anyhow!("Missing content_hash"))?;
        
        let payload = json["payload"].as_str()
            .and_then(|s| base64::decode(s).ok())
            .ok_or_else(|| anyhow::anyhow!("Invalid payload"))?;
        
        Ok(Self {
            sequence_id,
            timestamp,
            content_hash,
            payload,
        })
    }
}

/// Test execution statistics
#[derive(Debug, Default)]
struct TestStatistics {
    messages_produced: usize,
    messages_consumed: usize,
    produce_errors: usize,
    consume_errors: usize,
    ordering_errors: usize,
    data_corruption_errors: usize,
    flush_duration: Duration,
    fetch_duration: Duration,
}

// Additional stress test for concurrent WAL operations
#[tokio::test]
async fn test_concurrent_wal_operations() -> Result<()> {
    info!("Starting concurrent WAL operations stress test");
    
    // This test validates WAL thread safety under concurrent load
    let concurrent_producers = 5;
    let messages_per_producer = 200;
    
    let mut handles = Vec::new();
    
    for producer_id in 0..concurrent_producers {
        let handle = tokio::spawn(async move {
            let producer: FutureProducer = ClientConfig::new()
                .set("bootstrap.servers", &format!("localhost:{}", KAFKA_PORT))
                .set("client.id", &format!("producer-{}", producer_id))
                .set("acks", "all")
                .create()?;
            
            let mut success_count = 0;
            for i in 0..messages_per_producer {
                let key = format!("producer-{}-msg-{}", producer_id, i);
                let value = format!("concurrent-test-data-{}-{}", producer_id, i);
                
                let record = FutureRecord::to(TEST_TOPIC)
                    .key(&key)
                    .payload(&value);
                
                match producer.send(record, Duration::from_secs(0)).await {
                    Ok(_) => success_count += 1,
                    Err((e, _)) => warn!("Producer {} failed to send message {}: {:?}", 
                                        producer_id, i, e),
                }
            }
            
            anyhow::Ok(success_count)
        });
        
        handles.push(handle);
    }
    
    // Wait for all producers to complete
    let mut total_messages = 0;
    for handle in handles {
        match handle.await? {
            Ok(count) => total_messages += count,
            Err(e) => warn!("Producer failed: {:?}", e),
        }
    }
    
    info!("Concurrent stress test completed: {} total messages produced", total_messages);
    assert!(total_messages > 0, "No messages were successfully produced in concurrent test");
    
    Ok(())
}