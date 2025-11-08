//! Basic tests for storage layer functionality
//!
//! v2.2.7: WalMetadataAdapter deleted (replaced by Raft-backed metadata)
//! Metadata tests now use InMemoryMetadataStore

use chronik_common::metadata::{MetadataStore, TopicConfig, InMemoryMetadataStore};
use std::sync::Arc;

#[tokio::test]
async fn test_metadata_store_basic_operations() {
    let metadata_store = Arc::new(InMemoryMetadataStore::new());

    // Test topic creation
    let topic_metadata = metadata_store.create_topic("test-topic", TopicConfig {
        partition_count: 3,
        replication_factor: 1,
        retention_ms: None,
        segment_bytes: 1024 * 1024,
        config: Default::default(),
    }).await.unwrap();

    assert_eq!(topic_metadata.name, "test-topic");
    assert_eq!(topic_metadata.config.partition_count, 3);

    // Test topic retrieval
    let retrieved = metadata_store.get_topic("test-topic").await.unwrap();
    assert!(retrieved.is_some());
    assert_eq!(retrieved.unwrap().name, "test-topic");

    // Test list topics
    let topics = metadata_store.list_topics().await.unwrap();
    assert_eq!(topics.len(), 1);
    assert_eq!(topics[0].name, "test-topic");

    println!("✓ Metadata store operations work correctly");
}