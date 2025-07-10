//! Example demonstrating the Produce Request Handler functionality

use chronik_ingest::produce_handler::{ProduceHandler, ProduceHandlerConfig};
use chronik_storage::object_store::backends::local::LocalObjectStore;
use chronik_common::metadata::TiKVMetadataStore;
use chronik_protocol::{
    ProduceRequest, ProduceRequestTopic, ProduceRequestPartition,
    records::RecordBatch as KafkaRecordBatch,
};
use std::sync::Arc;
use bytes::Bytes;
use tokio;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging
    tracing_subscriber::fmt::init();
    
    // Create storage backend
    let storage = Arc::new(LocalObjectStore::new("/tmp/chronik-produce-example")?);
    
    // Create metadata store
    let pd_endpoints = vec!["localhost:2379".to_string()];
    let metadata_store = Arc::new(TiKVMetadataStore::new(pd_endpoints).await?);
    
    // Create topic if not exists
    if metadata_store.get_topic("events").await?.is_none() {
        metadata_store.create_topic("events", 3, 1).await?;
        for i in 0..3 {
            metadata_store.update_partition_leader("events", i, 0).await?;
        }
    }
    
    // Configure produce handler
    let config = ProduceHandlerConfig {
        node_id: 0,
        enable_indexing: true,
        enable_idempotence: true,
        enable_transactions: false,
        compression_type: chronik_protocol::compression::CompressionType::Gzip,
        ..Default::default()
    };
    
    // Create produce handler
    let handler = ProduceHandler::new(config, storage, metadata_store).await?;
    
    // Create a sample record batch
    let mut batch = KafkaRecordBatch::new(0, 1234, 0); // base_offset=0, producer_id=1234, epoch=0
    batch.add_record(
        Some(Bytes::from("user-123")),
        Some(Bytes::from(r#"{"event": "login", "timestamp": "2024-01-15T10:00:00Z"}"#)),
        vec![],
    );
    batch.add_record(
        Some(Bytes::from("user-456")),
        Some(Bytes::from(r#"{"event": "purchase", "amount": 99.99, "timestamp": "2024-01-15T10:05:00Z"}"#)),
        vec![],
    );
    
    let records_data = batch.encode()?.to_vec();
    
    // Create produce request
    let request = ProduceRequest {
        transactional_id: None,
        acks: 1, // Wait for leader acknowledgment
        timeout_ms: 30000,
        topics: vec![
            ProduceRequestTopic {
                name: "events".to_string(),
                partitions: vec![
                    ProduceRequestPartition {
                        index: 0,
                        records: records_data,
                    },
                ],
            },
        ],
    };
    
    // Send produce request
    println!("Sending produce request with 2 records...");
    let response = handler.handle_produce(request, 1).await?;
    
    // Check response
    for topic in response.topics {
        println!("Topic: {}", topic.name);
        for partition in topic.partitions {
            if partition.error_code == 0 {
                println!(
                    "  Partition {}: Success! Base offset: {}",
                    partition.index, partition.base_offset
                );
            } else {
                println!(
                    "  Partition {}: Error code: {}",
                    partition.index, partition.error_code
                );
            }
        }
    }
    
    // Display metrics
    let metrics = handler.metrics();
    println!("\nMetrics:");
    println!("  Records produced: {}", metrics.records_produced.load(std::sync::atomic::Ordering::Relaxed));
    println!("  Bytes produced: {}", metrics.bytes_produced.load(std::sync::atomic::Ordering::Relaxed));
    println!("  Errors: {}", metrics.produce_errors.load(std::sync::atomic::Ordering::Relaxed));
    println!("  Duplicate records: {}", metrics.duplicate_records.load(std::sync::atomic::Ordering::Relaxed));
    
    // Test idempotence by sending the same request again
    println!("\nTesting idempotence - sending same request again...");
    let response2 = handler.handle_produce(request.clone(), 2).await?;
    
    for topic in response2.topics {
        for partition in topic.partitions {
            if partition.error_code != 0 {
                println!(
                    "  Partition {}: Expected duplicate error, got code: {}",
                    partition.index, partition.error_code
                );
            }
        }
    }
    
    println!("  Duplicate records detected: {}", 
        metrics.duplicate_records.load(std::sync::atomic::Ordering::Relaxed));
    
    // Test different acknowledgment modes
    println!("\nTesting different acknowledgment modes...");
    
    // acks=0 (fire and forget)
    let mut request_acks0 = request.clone();
    request_acks0.acks = 0;
    let start = std::time::Instant::now();
    let _ = handler.handle_produce(request_acks0, 3).await?;
    println!("  acks=0 completed in: {:?}", start.elapsed());
    
    // acks=-1 (all replicas)
    let mut request_acks_all = request.clone();
    request_acks_all.acks = -1;
    let start = std::time::Instant::now();
    let _ = handler.handle_produce(request_acks_all, 4).await?;
    println!("  acks=-1 completed in: {:?}", start.elapsed());
    
    // Shutdown handler
    handler.shutdown().await?;
    
    println!("\nExample completed successfully!");
    
    Ok(())
}