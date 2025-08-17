//! Test for CreateTopics API validation and configuration

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

fn parse_error_code(response: &chronik_protocol::handler::Response) -> i16 {
    let mut response_bytes = response.body.clone();
    let mut decoder = chronik_protocol::parser::Decoder::new(&mut response_bytes);
    
    // For version 0, no throttle_time
    decoder.read_i32().unwrap(); // topic_count
    decoder.read_string().unwrap(); // topic_name
    decoder.read_i16().unwrap() // error_code
}

#[tokio::test]
async fn test_partition_count_validation() {
    let metadata_store = Arc::new(InMemoryMetadataStore::new());
    let handler = ProtocolHandler::with_metadata_and_broker(metadata_store.clone(), 1);
    
    // Test maximum partition count
    let request = encode_create_topics_request(
        vec![("too-many-partitions", 10001, 1, HashMap::new())],
        5000,
        false,
        0,
    );
    
    let response = handler.handle_request(&request).await.unwrap();
    let error_code = parse_error_code(&response);
    assert_eq!(error_code, 37); // INVALID_REQUEST
    
    // Verify topic was not created
    let topic_meta = metadata_store.get_topic("too-many-partitions").await.unwrap();
    assert!(topic_meta.is_none());
}

#[tokio::test]
async fn test_replication_factor_validation() {
    let metadata_store = Arc::new(InMemoryMetadataStore::new());
    let handler = ProtocolHandler::with_metadata_and_broker(metadata_store.clone(), 1);
    
    // Register only 2 brokers
    for i in 1..=2 {
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
    
    // Try to create topic with replication factor > available brokers
    let request = encode_create_topics_request(
        vec![("over-replicated", 3, 3, HashMap::new())],
        5000,
        false,
        0,
    );
    
    let response = handler.handle_request(&request).await.unwrap();
    let error_code = parse_error_code(&response);
    assert_eq!(error_code, 38); // INVALID_REPLICATION_FACTOR
    
    // Verify topic was not created
    let topic_meta = metadata_store.get_topic("over-replicated").await.unwrap();
    assert!(topic_meta.is_none());
}

#[tokio::test]
async fn test_config_validation_retention_ms() {
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
    
    // Test invalid retention.ms
    let mut configs = HashMap::new();
    configs.insert("retention.ms".to_string(), "-2".to_string()); // Invalid: < -1
    
    let request = encode_create_topics_request(
        vec![("bad-retention", 1, 1, configs)],
        5000,
        false,
        0,
    );
    
    let response = handler.handle_request(&request).await.unwrap();
    let error_code = parse_error_code(&response);
    assert_eq!(error_code, 40); // INVALID_CONFIG
    
    // Test valid retention.ms values
    let mut configs = HashMap::new();
    configs.insert("retention.ms".to_string(), "-1".to_string()); // Infinite retention
    
    let request = encode_create_topics_request(
        vec![("infinite-retention", 1, 1, configs)],
        5000,
        false,
        0,
    );
    
    let response = handler.handle_request(&request).await.unwrap();
    let error_code = parse_error_code(&response);
    assert_eq!(error_code, 0); // Success
    
    // Verify topic was created with correct config
    let topic_meta = metadata_store.get_topic("infinite-retention").await.unwrap().unwrap();
    assert_eq!(topic_meta.config.retention_ms, Some(-1));
}

#[tokio::test]
async fn test_config_validation_segment_bytes() {
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
    
    // Test invalid segment.bytes
    let mut configs = HashMap::new();
    configs.insert("segment.bytes".to_string(), "10".to_string()); // Invalid: < 14
    
    let request = encode_create_topics_request(
        vec![("small-segments", 1, 1, configs)],
        5000,
        false,
        0,
    );
    
    let response = handler.handle_request(&request).await.unwrap();
    let error_code = parse_error_code(&response);
    assert_eq!(error_code, 40); // INVALID_CONFIG
    
    // Test valid segment.bytes
    let mut configs = HashMap::new();
    configs.insert("segment.bytes".to_string(), "1048576".to_string()); // 1MB
    
    let request = encode_create_topics_request(
        vec![("custom-segments", 1, 1, configs)],
        5000,
        false,
        0,
    );
    
    let response = handler.handle_request(&request).await.unwrap();
    let error_code = parse_error_code(&response);
    assert_eq!(error_code, 0); // Success
    
    // Verify topic was created with correct config
    let topic_meta = metadata_store.get_topic("custom-segments").await.unwrap().unwrap();
    assert_eq!(topic_meta.config.segment_bytes, 1048576);
}

#[tokio::test]
async fn test_config_validation_compression_type() {
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
    
    // Test invalid compression type
    let mut configs = HashMap::new();
    configs.insert("compression.type".to_string(), "brotli".to_string()); // Invalid
    
    let request = encode_create_topics_request(
        vec![("bad-compression", 1, 1, configs)],
        5000,
        false,
        0,
    );
    
    let response = handler.handle_request(&request).await.unwrap();
    let error_code = parse_error_code(&response);
    assert_eq!(error_code, 40); // INVALID_CONFIG
    
    // Test all valid compression types
    for compression in &["none", "gzip", "snappy", "lz4", "zstd"] {
        let mut configs = HashMap::new();
        configs.insert("compression.type".to_string(), compression.to_string());
        
        let topic_name = format!("{}-compression", compression);
        let request = encode_create_topics_request(
            vec![(&topic_name, 1, 1, configs)],
            5000,
            false,
            0,
        );
        
        let response = handler.handle_request(&request).await.unwrap();
        let error_code = parse_error_code(&response);
        assert_eq!(error_code, 0, "Failed for compression type: {}", compression);
        
        // Verify config was stored
        let topic_meta = metadata_store.get_topic(&topic_name).await.unwrap().unwrap();
        assert_eq!(
            topic_meta.config.config.get("compression.type").unwrap(),
            compression
        );
    }
}

#[tokio::test]
async fn test_config_validation_cleanup_policy() {
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
    
    // Test invalid cleanup policy
    let mut configs = HashMap::new();
    configs.insert("cleanup.policy".to_string(), "archive".to_string()); // Invalid
    
    let request = encode_create_topics_request(
        vec![("bad-cleanup", 1, 1, configs)],
        5000,
        false,
        0,
    );
    
    let response = handler.handle_request(&request).await.unwrap();
    let error_code = parse_error_code(&response);
    assert_eq!(error_code, 40); // INVALID_CONFIG
    
    // Test valid cleanup policies
    for policy in &["delete", "compact", "delete,compact", "compact,delete"] {
        let mut configs = HashMap::new();
        configs.insert("cleanup.policy".to_string(), policy.to_string());
        
        let topic_name = format!("{}-policy", policy.replace(",", "-"));
        let request = encode_create_topics_request(
            vec![(&topic_name, 1, 1, configs)],
            5000,
            false,
            0,
        );
        
        let response = handler.handle_request(&request).await.unwrap();
        let error_code = parse_error_code(&response);
        assert_eq!(error_code, 0, "Failed for cleanup policy: {}", policy);
    }
}

#[tokio::test]
async fn test_config_validation_min_insync_replicas() {
    let metadata_store = Arc::new(InMemoryMetadataStore::new());
    let handler = ProtocolHandler::with_metadata_and_broker(metadata_store.clone(), 1);
    
    // Register 3 brokers
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
    
    // Test min.insync.replicas > replication_factor
    let mut configs = HashMap::new();
    configs.insert("min.insync.replicas".to_string(), "3".to_string());
    
    let request = encode_create_topics_request(
        vec![("over-insync", 3, 2, configs)], // RF=2 but min.insync=3
        5000,
        false,
        0,
    );
    
    let response = handler.handle_request(&request).await.unwrap();
    let error_code = parse_error_code(&response);
    assert_eq!(error_code, 40); // INVALID_CONFIG
    
    // Test valid min.insync.replicas
    let mut configs = HashMap::new();
    configs.insert("min.insync.replicas".to_string(), "2".to_string());
    
    let request = encode_create_topics_request(
        vec![("proper-insync", 3, 3, configs)], // RF=3, min.insync=2
        5000,
        false,
        0,
    );
    
    let response = handler.handle_request(&request).await.unwrap();
    let error_code = parse_error_code(&response);
    assert_eq!(error_code, 0); // Success
}

#[tokio::test]
async fn test_multiple_config_validation() {
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
    
    // Test multiple valid configs together
    let mut configs = HashMap::new();
    configs.insert("retention.ms".to_string(), "604800000".to_string()); // 7 days
    configs.insert("segment.bytes".to_string(), "536870912".to_string()); // 512MB
    configs.insert("compression.type".to_string(), "lz4".to_string());
    configs.insert("cleanup.policy".to_string(), "delete".to_string());
    configs.insert("max.message.bytes".to_string(), "1048576".to_string()); // 1MB
    configs.insert("flush.messages".to_string(), "10000".to_string());
    configs.insert("segment.ms".to_string(), "3600000".to_string()); // 1 hour
    
    let request = encode_create_topics_request(
        vec![("multi-config-topic", 4, 1, configs.clone())],
        5000,
        false,
        0,
    );
    
    let response = handler.handle_request(&request).await.unwrap();
    let error_code = parse_error_code(&response);
    assert_eq!(error_code, 0); // Success
    
    // Verify all configs were stored
    let topic_meta = metadata_store.get_topic("multi-config-topic").await.unwrap().unwrap();
    assert_eq!(topic_meta.config.retention_ms, Some(604800000));
    assert_eq!(topic_meta.config.segment_bytes, 536870912);
    
    // Check all other configs
    for (key, expected_value) in &configs {
        if key == "retention.ms" || key == "segment.bytes" {
            continue; // These are stored separately
        }
        assert_eq!(
            topic_meta.config.config.get(key).unwrap(),
            expected_value,
            "Config {} not stored correctly", key
        );
    }
}

#[tokio::test]
async fn test_unknown_config_warning() {
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
    
    // Test with unknown config (should succeed but log warning)
    let mut configs = HashMap::new();
    configs.insert("custom.config".to_string(), "some-value".to_string());
    configs.insert("retention.ms".to_string(), "86400000".to_string());
    
    let request = encode_create_topics_request(
        vec![("custom-config-topic", 1, 1, configs)],
        5000,
        false,
        0,
    );
    
    let response = handler.handle_request(&request).await.unwrap();
    let error_code = parse_error_code(&response);
    assert_eq!(error_code, 0); // Should succeed
    
    // Verify unknown config was still stored
    let topic_meta = metadata_store.get_topic("custom-config-topic").await.unwrap().unwrap();
    assert_eq!(
        topic_meta.config.config.get("custom.config").unwrap(),
        "some-value"
    );
}