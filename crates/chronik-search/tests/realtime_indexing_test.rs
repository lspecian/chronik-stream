//! Integration tests for the real-time indexing pipeline

use chronik_search::{
    RealtimeIndexer, RealtimeIndexerConfig, JsonDocument,
    FieldIndexingPolicy, JsonPipeline, JsonPipelineBuilder,
    TantivyIndexer, IndexerConfig,
};
use chronik_storage::{RecordBatch, Record, chronik_segment::ChronikSegmentBuilder};
use serde_json::json;
use std::collections::HashMap;
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::mpsc;

#[tokio::test]
async fn test_realtime_json_indexing() {
    let temp_dir = TempDir::new().unwrap();
    
    // Configure the indexer
    let config = RealtimeIndexerConfig {
        index_base_path: temp_dir.path().to_path_buf(),
        batch_size: 5,
        batch_timeout: Duration::from_millis(100),
        ..Default::default()
    };
    
    let indexer = RealtimeIndexer::new(config).unwrap();
    let (sender, receiver) = mpsc::channel(100);
    
    // Start the indexer
    let handles = indexer.start(receiver).await.unwrap();
    
    // Send various JSON documents
    let docs = vec![
        json!({
            "type": "user",
            "id": 1,
            "name": "Alice",
            "email": "alice@example.com",
            "age": 30,
            "active": true,
            "tags": ["admin", "developer"],
            "profile": {
                "bio": "Software engineer with 10 years of experience",
                "location": "San Francisco",
                "skills": ["Rust", "Python", "JavaScript"]
            }
        }),
        json!({
            "type": "product",
            "id": 101,
            "name": "Laptop Pro",
            "price": 1299.99,
            "in_stock": true,
            "categories": ["Electronics", "Computers"],
            "specs": {
                "cpu": "Intel i7",
                "ram": "16GB",
                "storage": "512GB SSD"
            }
        }),
        json!({
            "type": "order",
            "order_id": "ORD-2024-001",
            "customer_id": 1,
            "total": 1299.99,
            "items": [
                {"product_id": 101, "quantity": 1, "price": 1299.99}
            ],
            "status": "pending",
            "created_at": "2024-01-15T10:30:00Z"
        }),
    ];
    
    // Index documents
    for (i, content) in docs.into_iter().enumerate() {
        let doc = JsonDocument {
            id: format!("doc-{}", i),
            topic: "test-collection".to_string(),
            partition: 0,
            offset: i as i64,
            timestamp: chrono::Utc::now().timestamp_millis(),
            content,
            metadata: None,
        };
        sender.send(doc).await.unwrap();
    }
    
    // Wait for indexing
    tokio::time::sleep(Duration::from_millis(200)).await;
    
    // Check metrics
    let metrics = indexer.metrics();
    assert_eq!(metrics.documents_indexed, 3);
    assert_eq!(metrics.documents_failed, 0);
    assert!(metrics.bytes_indexed > 0);
    
    // Cleanup
    indexer.stop().await.unwrap();
    drop(sender);
    
    for handle in handles {
        handle.await.unwrap();
    }
}

#[tokio::test]
async fn test_json_pipeline_integration() {
    let temp_dir = TempDir::new().unwrap();
    
    // Build pipeline with custom configuration
    let pipeline = JsonPipelineBuilder::new()
        .channel_buffer_size(1000)
        .batch_size(10)
        .indexing_memory_budget(64 * 1024 * 1024) // 64MB
        .num_indexing_threads(2)
        .parse_keys_as_json(true)
        .build()
        .await
        .unwrap();
    
    // Create Kafka-like record batch
    let batch = RecordBatch {
        records: vec![
            // User event with JSON key
            Record {
                offset: 100,
                timestamp: chrono::Utc::now().timestamp_millis(),
                key: Some(r#"{"user_id": 123}"#.as_bytes().to_vec()),
                value: r#"{
                    "event": "user_login",
                    "timestamp": "2024-01-15T10:00:00Z",
                    "ip": "192.168.1.100",
                    "user_agent": "Mozilla/5.0"
                }"#.as_bytes().to_vec(),
                headers: [
                    ("event_type".to_string(), b"login".to_vec()),
                    ("source".to_string(), b"web".to_vec()),
                ].into_iter().collect(),
            },
            // Product view event
            Record {
                offset: 101,
                timestamp: chrono::Utc::now().timestamp_millis(),
                key: Some(b"product-456".to_vec()),
                value: r#"{
                    "event": "product_view",
                    "product": {
                        "id": 456,
                        "name": "Wireless Mouse",
                        "category": "Electronics"
                    },
                    "user_id": 123,
                    "session_id": "sess-789"
                }"#.as_bytes().to_vec(),
                headers: HashMap::new(),
            },
            // Order event with nested data
            Record {
                offset: 102,
                timestamp: chrono::Utc::now().timestamp_millis(),
                key: None,
                value: r#"{
                    "event": "order_placed",
                    "order": {
                        "id": "ORD-2024-002",
                        "items": [
                            {"product_id": 456, "quantity": 2, "price": 29.99}
                        ],
                        "total": 59.98,
                        "shipping": {
                            "method": "express",
                            "address": {
                                "city": "New York",
                                "country": "USA"
                            }
                        }
                    }
                }"#.as_bytes().to_vec(),
                headers: HashMap::new(),
            },
        ],
    };
    
    // Process the batch
    pipeline.process_batch("events", 0, &batch).await.unwrap();
    
    // Wait for processing
    tokio::time::sleep(Duration::from_millis(300)).await;
    
    // Check pipeline stats
    let stats = pipeline.stats().await;
    assert_eq!(stats.messages_processed, 3);
    assert_eq!(stats.parse_errors, 0);
    assert!(stats.bytes_processed > 0);
    
    // Check indexer metrics
    let indexer_metrics = pipeline.indexer_metrics();
    assert_eq!(indexer_metrics.documents_indexed, 3);
    
    pipeline.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_chronik_segment_integration() {
    let temp_dir = TempDir::new().unwrap();
    
    // Create a batch of JSON records
    let records: Vec<Record> = (0..100)
        .map(|i| {
            let value = json!({
                "id": i,
                "type": if i % 2 == 0 { "even" } else { "odd" },
                "data": {
                    "value": i * 10,
                    "description": format!("Record number {}", i),
                    "tags": vec![format!("tag{}", i % 5)]
                }
            });
            
            Record {
                offset: i,
                timestamp: chrono::Utc::now().timestamp_millis() + i,
                key: Some(format!("key-{}", i).into_bytes()),
                value: serde_json::to_vec(&value).unwrap(),
                headers: HashMap::new(),
            }
        })
        .collect();
    
    let batch = RecordBatch { records };
    
    // Create Chronik segment with Tantivy index
    let segment = ChronikSegmentBuilder::new("json-topic".to_string(), 0)
        .add_batch(batch)
        .with_index(true)
        .with_bloom_filter(true)
        .build()
        .unwrap();
    
    // Verify segment metadata
    let metadata = segment.metadata();
    assert_eq!(metadata.topic, "json-topic");
    assert_eq!(metadata.partition_id, 0);
    assert_eq!(metadata.record_count, 100);
    assert_eq!(metadata.base_offset, 0);
    assert_eq!(metadata.last_offset, 99);
    
    // Write segment to file
    let segment_path = temp_dir.path().join("segment.chronik");
    let mut file = std::fs::File::create(&segment_path).unwrap();
    let mut segment_mut = segment;
    segment_mut.write_to(&mut file).unwrap();
    
    // Verify file was created
    assert!(segment_path.exists());
    let file_size = std::fs::metadata(&segment_path).unwrap().len();
    assert!(file_size > 0);
}

#[tokio::test]
async fn test_field_indexing_policies() {
    let temp_dir = TempDir::new().unwrap();
    
    // Configure custom field policies
    let mut field_policy = FieldIndexingPolicy::default();
    field_policy.max_nesting_depth = 2;
    field_policy.excluded_fields = vec!["password".to_string(), "secret".to_string()];
    
    let config = RealtimeIndexerConfig {
        index_base_path: temp_dir.path().to_path_buf(),
        field_policy,
        ..Default::default()
    };
    
    let indexer = RealtimeIndexer::new(config).unwrap();
    let (sender, receiver) = mpsc::channel(100);
    
    let handles = indexer.start(receiver).await.unwrap();
    
    // Send document with sensitive fields
    let doc = JsonDocument {
        id: "user-1".to_string(),
        topic: "users".to_string(),
        partition: 0,
        offset: 1,
        timestamp: chrono::Utc::now().timestamp_millis(),
        content: json!({
            "username": "alice",
            "email": "alice@example.com",
            "password": "this-should-not-be-indexed",
            "profile": {
                "name": "Alice Smith",
                "preferences": {
                    "theme": "dark",
                    "notifications": {
                        "email": true,
                        "sms": false,
                        "too_deep": {
                            "this_should_be_ignored": true
                        }
                    }
                }
            },
            "secret": "also-not-indexed"
        }),
        metadata: None,
    };
    
    sender.send(doc).await.unwrap();
    
    tokio::time::sleep(Duration::from_millis(100)).await;
    
    let metrics = indexer.metrics();
    assert_eq!(metrics.documents_indexed, 1);
    
    indexer.stop().await.unwrap();
    drop(sender);
    
    for handle in handles {
        handle.await.unwrap();
    }
}

#[tokio::test]
async fn test_error_handling_and_recovery() {
    let temp_dir = TempDir::new().unwrap();
    
    let pipeline = JsonPipelineBuilder::new()
        .build()
        .await
        .unwrap();
    
    // Create batch with various problematic records
    let batch = RecordBatch {
        records: vec![
            // Valid JSON
            Record {
                offset: 1,
                timestamp: chrono::Utc::now().timestamp_millis(),
                key: None,
                value: r#"{"valid": true}"#.as_bytes().to_vec(),
                headers: HashMap::new(),
            },
            // Invalid JSON
            Record {
                offset: 2,
                timestamp: chrono::Utc::now().timestamp_millis(),
                key: None,
                value: b"not json at all".to_vec(),
                headers: HashMap::new(),
            },
            // Truncated JSON
            Record {
                offset: 3,
                timestamp: chrono::Utc::now().timestamp_millis(),
                key: None,
                value: r#"{"truncated": tr"#.as_bytes().to_vec(),
                headers: HashMap::new(),
            },
            // Another valid JSON
            Record {
                offset: 4,
                timestamp: chrono::Utc::now().timestamp_millis(),
                key: None,
                value: r#"{"valid": true, "after_errors": true}"#.as_bytes().to_vec(),
                headers: HashMap::new(),
            },
        ],
    };
    
    pipeline.process_batch("test", 0, &batch).await.unwrap();
    
    tokio::time::sleep(Duration::from_millis(200)).await;
    
    let stats = pipeline.stats().await;
    assert_eq!(stats.messages_processed, 4);
    assert_eq!(stats.parse_errors, 2); // Two invalid JSON documents
    
    // All documents should be indexed (invalid ones as raw text)
    let indexer_metrics = pipeline.indexer_metrics();
    assert_eq!(indexer_metrics.documents_indexed, 4);
    
    pipeline.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_high_throughput_indexing() {
    let temp_dir = TempDir::new().unwrap();
    
    let config = RealtimeIndexerConfig {
        index_base_path: temp_dir.path().to_path_buf(),
        batch_size: 100,
        batch_timeout: Duration::from_millis(50),
        num_indexing_threads: 4,
        ..Default::default()
    };
    
    let indexer = RealtimeIndexer::new(config).unwrap();
    let (sender, receiver) = mpsc::channel(10000);
    
    let handles = indexer.start(receiver).await.unwrap();
    
    // Send many documents rapidly
    let start = std::time::Instant::now();
    
    for i in 0..1000 {
        let doc = JsonDocument {
            id: format!("doc-{}", i),
            topic: "high-volume".to_string(),
            partition: i % 4, // Distribute across partitions
            offset: i as i64,
            timestamp: chrono::Utc::now().timestamp_millis(),
            content: json!({
                "id": i,
                "type": "metric",
                "value": i * 3.14,
                "tags": vec![format!("tag-{}", i % 10)],
                "metadata": {
                    "source": "test",
                    "batch": i / 100
                }
            }),
            metadata: None,
        };
        sender.send(doc).await.unwrap();
    }
    
    // Wait for all documents to be indexed
    tokio::time::sleep(Duration::from_millis(500)).await;
    
    let elapsed = start.elapsed();
    let metrics = indexer.metrics();
    
    assert_eq!(metrics.documents_indexed, 1000);
    assert_eq!(metrics.documents_failed, 0);
    
    let docs_per_sec = 1000.0 / elapsed.as_secs_f64();
    println!("Indexed {} docs/sec", docs_per_sec);
    assert!(docs_per_sec > 100.0); // Should handle at least 100 docs/sec
    
    indexer.stop().await.unwrap();
    drop(sender);
    
    for handle in handles {
        handle.await.unwrap();
    }
}