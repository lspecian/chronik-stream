//! Example showing how to use the Sled-based metadata store.

use chronik_common::metadata::{MetadataStore, SledMetadataStore, TopicConfig, BrokerMetadata, BrokerStatus};
use std::collections::HashMap;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize the Sled metadata store
    let metadata_store = SledMetadataStore::new("./data/metadata")?;
    
    // Initialize system state (creates internal topics)
    println!("Initializing system state...");
    metadata_store.init_system_state().await?;
    
    // Create a user topic
    println!("\nCreating user topic...");
    let topic_config = TopicConfig {
        partition_count: 6,
        replication_factor: 3,
        retention_ms: Some(7 * 24 * 60 * 60 * 1000), // 7 days
        segment_bytes: 1024 * 1024 * 1024, // 1GB
        config: {
            let mut config = HashMap::new();
            config.insert("compression.type".to_string(), "snappy".to_string());
            config.insert("min.insync.replicas".to_string(), "2".to_string());
            config
        },
    };
    
    let topic = metadata_store.create_topic("events", topic_config).await?;
    println!("Created topic: {} with ID: {}", topic.name, topic.id);
    
    // Register brokers
    println!("\nRegistering brokers...");
    for i in 1..=3 {
        let broker = BrokerMetadata {
            broker_id: i,
            host: format!("broker{}.chronik.local", i),
            port: 9092,
            rack: Some(format!("rack-{}", (i - 1) % 2 + 1)),
            status: BrokerStatus::Online,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        
        metadata_store.register_broker(broker).await?;
        println!("Registered broker {}", i);
    }
    
    // List all topics
    println!("\nListing all topics:");
    let topics = metadata_store.list_topics().await?;
    for topic in topics {
        println!("- {} (partitions: {}, replication: {})", 
            topic.name, 
            topic.config.partition_count,
            topic.config.replication_factor
        );
    }
    
    // List all brokers
    println!("\nListing all brokers:");
    let brokers = metadata_store.list_brokers().await?;
    for broker in brokers {
        println!("- Broker {}: {}:{} (rack: {:?}, status: {:?})", 
            broker.broker_id, 
            broker.host, 
            broker.port,
            broker.rack,
            broker.status
        );
    }
    
    println!("\nMetadata store initialized successfully!");
    
    Ok(())
}