//! Test for CreateTopics API functionality

use chronik_protocol::handler::ProtocolHandler;
use chronik_protocol::parser::Encoder;
use chronik_protocol::ApiKey;
use chronik_common::metadata::{MetadataStore, BrokerStatus};
use chronik_common::metadata::InMemoryMetadataStore;
use std::sync::Arc;
use bytes::{Bytes, BytesMut};
use std::collections::HashMap;

/// Helper to encode CreateTopics request
fn encode_create_topics_request(
    topics: Vec<(&str, i32, i16, HashMap<String, String>)>,
    timeout_ms: i32,
    validate_only: bool,
    api_version: i16,
) -> Bytes {
    let mut buf = BytesMut::new();
    let mut encoder = Encoder::new(&mut buf);
    
    // Write request header (no size prefix for handle_request)
    encoder.write_i16(ApiKey::CreateTopics as i16);
    encoder.write_i16(api_version);
    encoder.write_i32(123); // correlation_id
    encoder.write_string(Some("test-client")); // client_id
    
    // Request body
    encoder.write_i32(topics.len() as i32); // topic count
    
    for (name, partitions, replication_factor, configs) in &topics {
        encoder.write_string(Some(name));
        encoder.write_i32(*partitions);
        encoder.write_i16(*replication_factor);
        encoder.write_i32(-1); // no replica assignments
        encoder.write_i32(configs.len() as i32); // config count
        
        for (key, value) in configs {
            encoder.write_string(Some(key));
            encoder.write_string(Some(value));
        }
    }
    
    encoder.write_i32(timeout_ms);
    
    if api_version >= 1 {
        encoder.write_bool(validate_only);
    }
    
    buf.freeze()
}

#[tokio::test]
async fn test_create_topic_success() {
    let metadata_store = Arc::new(InMemoryMetadataStore::new());
    let handler = ProtocolHandler::with_metadata_and_broker(metadata_store.clone(), 1);
    
    // Register broker first
    let broker = chronik_common::metadata::BrokerMetadata {
        broker_id: 1,
        host: "localhost".to_string(),
        port: 9092,
        rack: None,
        status: BrokerStatus::Online,
        created_at: chronik_common::Utc::now(),
        updated_at: chronik_common::Utc::now(),
    };
    metadata_store.register_broker(broker).await.unwrap();
    
    // Create topic request
    let request = encode_create_topics_request(
        vec![("test-topic", 3, 1, HashMap::new())],
        5000,
        false,
        0,
    );
    
    let response = handler.handle_request(&request).await.unwrap();
    
    // Parse response
    let mut response_bytes = response.body;
    let mut decoder = chronik_protocol::parser::Decoder::new(&mut response_bytes);
    
    // For version 0, no throttle time
    
    // Topic count
    let topic_count = decoder.read_i32().unwrap();
    assert_eq!(topic_count, 1);
    
    // Topic response
    let topic_name = decoder.read_string().unwrap().unwrap();
    assert_eq!(topic_name, "test-topic");
    
    let error_code = decoder.read_i16().unwrap();
    assert_eq!(error_code, 0); // Success
    
    // For version 0, no error message or other fields
    
    // Verify topic was created in metadata store
    let topic_meta = metadata_store.get_topic("test-topic").await.unwrap();
    assert!(topic_meta.is_some());
    
    let topic_meta = topic_meta.unwrap();
    assert_eq!(topic_meta.name, "test-topic");
    assert_eq!(topic_meta.config.partition_count, 3);
    assert_eq!(topic_meta.config.replication_factor, 1);
    
    // Verify partition assignments were created
    let mut assignments = metadata_store.get_partition_assignments("test-topic").await.unwrap();
    assert_eq!(assignments.len(), 3);
    
    // Sort by partition number for consistent testing
    assignments.sort_by_key(|a| a.partition);
    
    for (i, assignment) in assignments.iter().enumerate() {
        assert_eq!(assignment.topic, "test-topic");
        assert_eq!(assignment.partition, i as u32);
        assert_eq!(assignment.broker_id, 1);
        assert!(assignment.is_leader);
    }
    
    // Verify partition offsets were initialized
    for partition in 0..3 {
        let offset = metadata_store.get_partition_offset("test-topic", partition).await.unwrap();
        assert!(offset.is_some());
        let (high_watermark, log_start_offset) = offset.unwrap();
        assert_eq!(high_watermark, 0);
        assert_eq!(log_start_offset, 0);
    }
}

#[tokio::test]
async fn test_create_topic_with_configs() {
    let metadata_store = Arc::new(InMemoryMetadataStore::new());
    let handler = ProtocolHandler::with_metadata_and_broker(metadata_store.clone(), 1);
    
    // Register a broker
    let broker = chronik_common::metadata::BrokerMetadata {
        broker_id: 1,
        host: "localhost".to_string(),
        port: 9092,
        rack: None,
        status: BrokerStatus::Online,
        created_at: chronik_common::Utc::now(),
        updated_at: chronik_common::Utc::now(),
    };
    metadata_store.register_broker(broker).await.unwrap();
    
    // Create topic with custom configs
    let mut configs = HashMap::new();
    configs.insert("retention.ms".to_string(), "86400000".to_string()); // 1 day
    configs.insert("segment.bytes".to_string(), "536870912".to_string()); // 512MB
    
    let request = encode_create_topics_request(
        vec![("configured-topic", 1, 1, configs)],
        5000,
        false,
        0,
    );
    
    let _response = handler.handle_request(&request).await.unwrap();
    
    // Verify topic was created with correct configs
    let topic_meta = metadata_store.get_topic("configured-topic").await.unwrap().unwrap();
    assert_eq!(topic_meta.config.retention_ms, Some(86400000));
    assert_eq!(topic_meta.config.segment_bytes, 536870912);
}

#[tokio::test]
async fn test_create_topic_invalid_name() {
    let metadata_store = Arc::new(InMemoryMetadataStore::new());
    let handler = ProtocolHandler::with_metadata_and_broker(metadata_store.clone(), 1);
    
    // Invalid topic names
    let long_name = "a".repeat(250);
    let invalid_names = vec![
        ("", "empty name"),
        (".", "dot"),
        ("..", "double dot"),
        ("topic with spaces", "contains spaces"),
        ("topic@#$", "invalid characters"),
        (long_name.as_str(), "too long"),
    ];
    
    for (name, reason) in invalid_names {
        let request = encode_create_topics_request(
            vec![(name, 1, 1, HashMap::new())],
            5000,
            false,
            0,
        );
        
        let response = handler.handle_request(&request).await.unwrap();
        
        // Parse response
        let mut response_bytes = response.body;
        let mut decoder = chronik_protocol::parser::Decoder::new(&mut response_bytes);
        
        // For version 0, no throttle_time
        decoder.read_i32().unwrap(); // topic_count
        decoder.read_string().unwrap(); // topic_name
        
        let error_code = decoder.read_i16().unwrap();
        assert_ne!(error_code, 0, "Should fail for invalid name: {} ({})", name, reason);
        
        // Verify topic was not created
        let topic_meta = metadata_store.get_topic(name).await.unwrap();
        assert!(topic_meta.is_none(), "Topic should not be created for: {} ({})", name, reason);
    }
}

#[tokio::test]
async fn test_create_topic_already_exists() {
    let metadata_store = Arc::new(InMemoryMetadataStore::new());
    let handler = ProtocolHandler::with_metadata_and_broker(metadata_store.clone(), 1);
    
    // Register a broker
    let broker = chronik_common::metadata::BrokerMetadata {
        broker_id: 1,
        host: "localhost".to_string(),
        port: 9092,
        rack: None,
        status: BrokerStatus::Online,
        created_at: chronik_common::Utc::now(),
        updated_at: chronik_common::Utc::now(),
    };
    metadata_store.register_broker(broker).await.unwrap();
    
    // Create topic first time
    let request = encode_create_topics_request(
        vec![("duplicate-topic", 1, 1, HashMap::new())],
        5000,
        false,
        0,
    );
    
    let response = handler.handle_request(&request).await.unwrap();
    
    // Parse first response
    let mut response_bytes = response.body;
    let mut decoder = chronik_protocol::parser::Decoder::new(&mut response_bytes);
    // For version 0, no throttle_time
    decoder.read_i32().unwrap(); // topic_count
    decoder.read_string().unwrap(); // topic_name
    
    let error_code = decoder.read_i16().unwrap();
    assert_eq!(error_code, 0); // Success
    
    // Try to create same topic again
    let request2 = encode_create_topics_request(
        vec![("duplicate-topic", 1, 1, HashMap::new())],
        5000,
        false,
        0,
    );
    
    let response2 = handler.handle_request(&request2).await.unwrap();
    
    // Parse second response
    let mut response_bytes2 = response2.body;
    let mut decoder2 = chronik_protocol::parser::Decoder::new(&mut response_bytes2);
    // For version 0, no throttle_time
    decoder2.read_i32().unwrap(); // topic_count
    decoder2.read_string().unwrap(); // topic_name
    
    let error_code2 = decoder2.read_i16().unwrap();
    assert_eq!(error_code2, 36); // TOPIC_ALREADY_EXISTS
}

#[tokio::test]
async fn test_create_topic_validate_only() {
    let metadata_store = Arc::new(InMemoryMetadataStore::new());
    let handler = ProtocolHandler::with_metadata_and_broker(metadata_store.clone(), 1);
    
    // Register a broker
    let broker = chronik_common::metadata::BrokerMetadata {
        broker_id: 1,
        host: "localhost".to_string(),
        port: 9092,
        rack: None,
        status: BrokerStatus::Online,
        created_at: chronik_common::Utc::now(),
        updated_at: chronik_common::Utc::now(),
    };
    metadata_store.register_broker(broker).await.unwrap();
    
    // Create topic with validate_only=true
    let request = encode_create_topics_request(
        vec![("validate-topic", 1, 1, HashMap::new())],
        5000,
        true, // validate_only
        1, // api_version must be >= 1 for validate_only
    );
    
    let response = handler.handle_request(&request).await.unwrap();
    
    // Parse response
    let mut response_bytes = response.body;
    let mut decoder = chronik_protocol::parser::Decoder::new(&mut response_bytes);
    
    // For version 1, still no throttle_time (that's version 2+)
    decoder.read_i32().unwrap(); // topic_count
    decoder.read_string().unwrap(); // topic_name
    
    let error_code = decoder.read_i16().unwrap();
    assert_eq!(error_code, 0); // Success
    
    // Version 1 has error_message
    let error_message = decoder.read_string().unwrap();
    assert!(error_message.is_none());
    
    // Verify topic was NOT created (validate_only)
    let topic_meta = metadata_store.get_topic("validate-topic").await.unwrap();
    assert!(topic_meta.is_none());
}

#[tokio::test]
async fn test_create_multiple_topics() {
    let metadata_store = Arc::new(InMemoryMetadataStore::new());
    let handler = ProtocolHandler::with_metadata_and_broker(metadata_store.clone(), 1);
    
    // Register multiple brokers
    for i in 1..=3 {
        let broker = chronik_common::metadata::BrokerMetadata {
            broker_id: i,
            host: format!("broker{}", i),
            port: 9092,
            rack: None,
            status: BrokerStatus::Online,
            created_at: chronik_common::Utc::now(),
            updated_at: chronik_common::Utc::now(),
        };
        metadata_store.register_broker(broker).await.unwrap();
    }
    
    // Create multiple topics in one request
    let topics = vec![
        ("topic1", 6, 1, HashMap::new()),
        ("topic2", 3, 1, HashMap::new()),
        ("topic3", 9, 1, HashMap::new()),
    ];
    
    let request = encode_create_topics_request(topics, 5000, false, 0);
    
    let response = handler.handle_request(&request).await.unwrap();
    
    // Parse response
    let mut response_bytes = response.body;
    let mut decoder = chronik_protocol::parser::Decoder::new(&mut response_bytes);
    
    // For version 0, no throttle_time
    let topic_count = decoder.read_i32().unwrap();
    assert_eq!(topic_count, 3);
    
    // Verify all topics were created
    for i in 1..=3 {
        let topic_name = format!("topic{}", i);
        let topic_meta = metadata_store.get_topic(&topic_name).await.unwrap();
        assert!(topic_meta.is_some());
        
        // Verify partition assignments are distributed across brokers
        let assignments = metadata_store.get_partition_assignments(&topic_name).await.unwrap();
        let broker_ids: std::collections::HashSet<_> = assignments.iter()
            .map(|a| a.broker_id)
            .collect();
        
        // With round-robin assignment, partitions should be distributed
        if assignments.len() >= 3 {
            assert!(broker_ids.len() > 1, "Partitions should be distributed across brokers");
        }
    }
}

#[tokio::test]
async fn test_batch_topic_creation() {
    let metadata_store = Arc::new(InMemoryMetadataStore::new());
    let handler = ProtocolHandler::with_metadata_and_broker(metadata_store.clone(), 1);
    
    // Register multiple brokers
    for i in 1..=3 {
        let broker = chronik_common::metadata::BrokerMetadata {
            broker_id: i,
            host: format!("broker{}", i),
            port: 9092,
            rack: None,
            status: BrokerStatus::Online,
            created_at: chronik_common::Utc::now(),
            updated_at: chronik_common::Utc::now(),
        };
        metadata_store.register_broker(broker).await.unwrap();
    }
    
    // Test batch creation with mixed success and failure
    let topics = vec![
        ("valid-topic-1", 3, 1, HashMap::new()),
        ("valid-topic-2", 5, 2, HashMap::new()),
        ("invalid-partition-count", 0, 1, HashMap::new()), // Should fail
        ("valid-topic-3", 2, 1, HashMap::new()),
    ];
    
    let request = encode_create_topics_request(topics, 5000, false, 0);
    let response = handler.handle_request(&request).await.unwrap();
    
    // Parse response
    let mut response_bytes = response.body;
    let mut decoder = chronik_protocol::parser::Decoder::new(&mut response_bytes);
    
    let topic_count = decoder.read_i32().unwrap();
    assert_eq!(topic_count, 4);
    
    // Check each topic response
    let expected_results = [
        ("valid-topic-1", 0), // Success
        ("valid-topic-2", 0), // Success  
        ("invalid-partition-count", 37), // INVALID_PARTITIONS
        ("valid-topic-3", 0), // Success
    ];
    
    for (expected_name, expected_error) in &expected_results {
        let topic_name = decoder.read_string().unwrap().unwrap();
        let error_code = decoder.read_i16().unwrap();
        
        assert_eq!(topic_name, *expected_name);
        assert_eq!(error_code, *expected_error);
    }
    
    // Verify successful topics were created with atomic operations
    let valid_topics = ["valid-topic-1", "valid-topic-2", "valid-topic-3"];
    for topic_name in &valid_topics {
        let topic_meta = metadata_store.get_topic(topic_name).await.unwrap();
        assert!(topic_meta.is_some(), "Topic {} should have been created", topic_name);
        
        // Verify partition assignments exist
        let assignments = metadata_store.get_partition_assignments(topic_name).await.unwrap();
        assert!(!assignments.is_empty(), "Topic {} should have partition assignments", topic_name);
        
        // Verify partition offsets were initialized
        for assignment in &assignments {
            let offset = metadata_store.get_partition_offset(topic_name, assignment.partition).await.unwrap();
            assert!(offset.is_some(), "Partition offset should be initialized for {}", topic_name);
            let (high_watermark, log_start_offset) = offset.unwrap();
            assert_eq!(high_watermark, 0);
            assert_eq!(log_start_offset, 0);
        }
    }
    
    // Verify failed topic was not created
    let failed_topic = metadata_store.get_topic("invalid-partition-count").await.unwrap();
    assert!(failed_topic.is_none(), "Failed topic should not have been created");
}