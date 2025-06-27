//! Basic usage example for Chronik Stream components

use chronik_common::Result;
use chronik_storage::{
    RecordBatch, Record,
    object_store::{
        config::{StorageBackend, ObjectStoreConfig},
        factory::ObjectStoreFactory,
    },
};
use std::collections::HashMap;
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::fmt::init();
    
    println!("=== Chronik Stream Basic Usage Example ===\n");
    
    // Example 1: Creating record batches
    println!("1. Creating a record batch:");
    let records = vec![
        Record {
            offset: 0,
            timestamp: 1234567890,
            key: Some(b"user-123".to_vec()),
            value: b"{\"action\": \"login\", \"ip\": \"192.168.1.1\"}".to_vec(),
            headers: HashMap::from([
                ("content-type".to_string(), b"application/json".to_vec()),
            ]),
        },
        Record {
            offset: 1,
            timestamp: 1234567891,
            key: Some(b"user-456".to_vec()),
            value: b"{\"action\": \"purchase\", \"amount\": 99.99}".to_vec(),
            headers: HashMap::new(),
        },
    ];
    
    let batch = RecordBatch { records: records.clone() };
    println!("  Created batch with {} records", batch.records.len());
    
    // Example 2: Object store configuration
    println!("\n2. Object store configurations:");
    
    // Local storage
    let local_config = ObjectStoreConfig {
        backend: StorageBackend::Local {
            path: "/tmp/chronik-storage".to_string(),
        },
        bucket: "chronik".to_string(),
        prefix: Some("segments".to_string()),
        ..Default::default()
    };
    println!("  Local storage: {:?}", local_config.backend);
    
    // S3 storage
    let s3_config = ObjectStoreConfig {
        backend: StorageBackend::S3,
        bucket: "my-chronik-bucket".to_string(),
        prefix: Some("prod/segments".to_string()),
        ..Default::default()
    };
    println!("  S3 storage: bucket={}, prefix={:?}", s3_config.bucket, s3_config.prefix);
    
    // Example 3: Record encoding/decoding
    println!("\n3. Record encoding:");
    let encoded = batch.encode()?;
    println!("  Encoded batch size: {} bytes", encoded.len());
    
    let decoded = RecordBatch::decode(&encoded)?;
    println!("  Decoded {} records", decoded.records.len());
    
    // Example 4: Working with metadata
    println!("\n4. Topic metadata example:");
    use chronik_common::metadata::TopicConfig;
    
    let topic_config = TopicConfig {
        name: "user-events".to_string(),
        partition_count: 6,
        replication_factor: 3,
        retention_ms: Some(7 * 24 * 60 * 60 * 1000), // 7 days
        segment_ms: Some(60 * 60 * 1000), // 1 hour
        compression_type: Some("snappy".to_string()),
        max_message_bytes: Some(1024 * 1024), // 1MB
    };
    
    println!("  Topic: {}", topic_config.name);
    println!("  Partitions: {}", topic_config.partition_count);
    println!("  Replication: {}", topic_config.replication_factor);
    println!("  Retention: {} days", topic_config.retention_ms.unwrap_or(0) / (24 * 60 * 60 * 1000));
    
    // Example 5: Consumer group metadata
    println!("\n5. Consumer group example:");
    use chronik_common::metadata::{ConsumerGroupMetadata, ConsumerOffset};
    
    let group_metadata = ConsumerGroupMetadata {
        state: "Stable".to_string(),
        protocol_type: "consumer".to_string(),
        protocol: Some("range".to_string()),
        members: vec!["consumer-1".to_string(), "consumer-2".to_string()],
    };
    
    println!("  Group state: {}", group_metadata.state);
    println!("  Members: {:?}", group_metadata.members);
    
    let offset = ConsumerOffset {
        topic: "user-events".to_string(),
        partition: 0,
        offset: 12345,
        metadata: Some("checkpoint-1".to_string()),
    };
    
    println!("  Offset: topic={}, partition={}, offset={}", 
             offset.topic, offset.partition, offset.offset);
    
    println!("\n=== Example completed successfully ===");
    
    Ok(())
}