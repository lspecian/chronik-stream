//! Test for Metadata API functionality

use chronik_protocol::handler::ProtocolHandler;
use chronik_protocol::parser::Encoder;
use chronik_protocol::ApiKey;
use chronik_common::metadata::{MetadataStore, BrokerStatus};
use chronik_common::metadata::InMemoryMetadataStore;
use std::sync::Arc;
use bytes::{Bytes, BytesMut};
use std::collections::HashMap;

/// Helper to encode Metadata request
fn encode_metadata_request(
    topics: Option<Vec<&str>>,
    api_version: i16,
) -> Bytes {
    let mut buf = BytesMut::new();
    let mut encoder = Encoder::new(&mut buf);
    
    // Write request header (no size prefix for handle_request)
    encoder.write_i16(ApiKey::Metadata as i16);
    encoder.write_i16(api_version);
    encoder.write_i32(123); // correlation_id
    encoder.write_string(Some("test-client")); // client_id
    
    // Request body based on version
    if api_version >= 1 {
        match topics {
            Some(topic_list) => {
                encoder.write_i32(topic_list.len() as i32);
                for topic in topic_list {
                    encoder.write_string(Some(topic));
                }
            }
            None => {
                encoder.write_i32(-1); // All topics
            }
        }
    }
    
    if api_version >= 4 {
        encoder.write_bool(false); // allow_auto_topic_creation
    }
    
    if api_version >= 8 {
        encoder.write_bool(false); // include_cluster_authorized_operations
        encoder.write_bool(false); // include_topic_authorized_operations
    }
    
    buf.freeze()
}

#[tokio::test]
async fn test_metadata_empty_when_no_topics() {
    let metadata_store = Arc::new(InMemoryMetadataStore::new());
    let handler = ProtocolHandler::with_metadata_store(metadata_store.clone());
    
    // Request metadata when no topics exist
    let request = encode_metadata_request(None, 1);
    let response = handler.handle_request(&request).await.unwrap();
    
    // Parse response
    let mut response_bytes = response.body;
    let mut decoder = chronik_protocol::parser::Decoder::new(&mut response_bytes);
    
    // throttle_time_ms (version 1+)
    let _throttle_time = decoder.read_i32().unwrap();
    
    // brokers array
    let broker_count = decoder.read_i32().unwrap();
    assert!(broker_count > 0, "Should have at least one broker");
    
    // Skip broker details for now
    for _ in 0..broker_count {
        decoder.read_i32().unwrap(); // node_id
        decoder.read_string().unwrap(); // host
        decoder.read_i32().unwrap(); // port
        decoder.read_string().unwrap(); // rack (nullable)
    }
    
    // cluster_id
    let _cluster_id = decoder.read_string().unwrap();
    
    // controller_id
    let _controller_id = decoder.read_i32().unwrap();
    
    // topics array
    let topic_count = decoder.read_i32().unwrap();
    assert_eq!(topic_count, 0, "Should have no topics when none created");
}

#[tokio::test]
async fn test_metadata_returns_created_topics() {
    let metadata_store = Arc::new(InMemoryMetadataStore::new());
    let handler = ProtocolHandler::with_metadata_store(metadata_store.clone());
    
    // Register a broker first
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
    
    // Create a topic first
    let create_request = encode_create_topics_request(
        vec![("test-topic", 3, 1, HashMap::new())],
        5000,
        false,
        0,
    );
    let _create_response = handler.handle_request(&create_request).await.unwrap();
    
    // Now request metadata
    let metadata_request = encode_metadata_request(None, 1);
    let response = handler.handle_request(&metadata_request).await.unwrap();
    
    // Parse response
    let mut response_bytes = response.body;
    let mut decoder = chronik_protocol::parser::Decoder::new(&mut response_bytes);
    
    // throttle_time_ms
    let _throttle_time = decoder.read_i32().unwrap();
    
    // brokers array
    let broker_count = decoder.read_i32().unwrap();
    assert_eq!(broker_count, 1, "Should have one broker");
    
    // Broker details
    let broker_id = decoder.read_i32().unwrap();
    assert_eq!(broker_id, 1);
    let host = decoder.read_string().unwrap().unwrap();
    assert_eq!(host, "localhost");
    let port = decoder.read_i32().unwrap();
    assert_eq!(port, 9092);
    let _rack = decoder.read_string().unwrap(); // nullable
    
    // cluster_id
    let cluster_id = decoder.read_string().unwrap().unwrap();
    assert_eq!(cluster_id, "chronik-stream");
    
    // controller_id
    let controller_id = decoder.read_i32().unwrap();
    assert_eq!(controller_id, 1);
    
    // topics array
    let topic_count = decoder.read_i32().unwrap();
    assert_eq!(topic_count, 1, "Should have one topic");
    
    // Topic details
    let error_code = decoder.read_i16().unwrap();
    assert_eq!(error_code, 0, "Topic should have no error");
    
    let topic_name = decoder.read_string().unwrap().unwrap();
    assert_eq!(topic_name, "test-topic");
    
    let is_internal = decoder.read_bool().unwrap();
    assert!(!is_internal, "Topic should not be internal");
    
    // Partitions array
    let partition_count = decoder.read_i32().unwrap();
    assert_eq!(partition_count, 3, "Should have 3 partitions");
    
    for partition_id in 0..3 {
        let error_code = decoder.read_i16().unwrap();
        assert_eq!(error_code, 0);
        
        let partition_index = decoder.read_i32().unwrap();
        assert_eq!(partition_index, partition_id);
        
        let leader_id = decoder.read_i32().unwrap();
        assert_eq!(leader_id, 1, "Leader should be broker 1");
        
        let leader_epoch = decoder.read_i32().unwrap();
        assert_eq!(leader_epoch, 0);
        
        // replica_nodes array
        let replica_count = decoder.read_i32().unwrap();
        assert_eq!(replica_count, 1);
        let replica_id = decoder.read_i32().unwrap();
        assert_eq!(replica_id, 1);
        
        // isr_nodes array
        let isr_count = decoder.read_i32().unwrap();
        assert_eq!(isr_count, 1);
        let isr_id = decoder.read_i32().unwrap();
        assert_eq!(isr_id, 1);
        
        // offline_replicas array
        let offline_count = decoder.read_i32().unwrap();
        assert_eq!(offline_count, 0);
    }
}

#[tokio::test]
async fn test_metadata_specific_topics() {
    let metadata_store = Arc::new(InMemoryMetadataStore::new());
    let handler = ProtocolHandler::with_metadata_store(metadata_store.clone());
    
    // Register broker
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
    
    // Create multiple topics
    let create_request = encode_create_topics_request(
        vec![
            ("topic1", 1, 1, HashMap::new()),
            ("topic2", 2, 1, HashMap::new()),
            ("topic3", 1, 1, HashMap::new()),
        ],
        5000,
        false,
        0,
    );
    let _create_response = handler.handle_request(&create_request).await.unwrap();
    
    // Request metadata for specific topics
    let metadata_request = encode_metadata_request(Some(vec!["topic1", "topic3"]), 1);
    let response = handler.handle_request(&metadata_request).await.unwrap();
    
    // Parse response
    let mut response_bytes = response.body;
    let mut decoder = chronik_protocol::parser::Decoder::new(&mut response_bytes);
    
    // Skip to topics section
    let _throttle_time = decoder.read_i32().unwrap();
    
    // Skip brokers
    let broker_count = decoder.read_i32().unwrap();
    for _ in 0..broker_count {
        decoder.read_i32().unwrap(); // node_id
        decoder.read_string().unwrap(); // host
        decoder.read_i32().unwrap(); // port
        decoder.read_string().unwrap(); // rack
    }
    
    decoder.read_string().unwrap(); // cluster_id
    decoder.read_i32().unwrap(); // controller_id
    
    // Check topics
    let topic_count = decoder.read_i32().unwrap();
    assert_eq!(topic_count, 2, "Should return only requested topics");
    
    let mut topic_names = Vec::new();
    for _ in 0..topic_count {
        decoder.read_i16().unwrap(); // error_code
        let name = decoder.read_string().unwrap().unwrap();
        topic_names.push(name);
        decoder.read_bool().unwrap(); // is_internal
        
        // Skip partition details
        let partition_count = decoder.read_i32().unwrap();
        for _ in 0..partition_count {
            decoder.read_i16().unwrap(); // error_code
            decoder.read_i32().unwrap(); // partition_index
            decoder.read_i32().unwrap(); // leader_id
            decoder.read_i32().unwrap(); // leader_epoch
            
            let replica_count = decoder.read_i32().unwrap();
            for _ in 0..replica_count {
                decoder.read_i32().unwrap();
            }
            
            let isr_count = decoder.read_i32().unwrap();
            for _ in 0..isr_count {
                decoder.read_i32().unwrap();
            }
            
            let offline_count = decoder.read_i32().unwrap();
            for _ in 0..offline_count {
                decoder.read_i32().unwrap();
            }
        }
    }
    
    topic_names.sort();
    assert_eq!(topic_names, vec!["topic1", "topic3"]);
}

/// Helper to encode CreateTopics request (reused from create_topics_test.rs)
fn encode_create_topics_request(
    topics: Vec<(&str, i32, i16, HashMap<String, String>)>,
    timeout_ms: i32,
    validate_only: bool,
    api_version: i16,
) -> Bytes {
    let mut buf = BytesMut::new();
    let mut encoder = Encoder::new(&mut buf);
    
    // Write request header
    encoder.write_i16(ApiKey::CreateTopics as i16);
    encoder.write_i16(api_version);
    encoder.write_i32(123); // correlation_id
    encoder.write_string(Some("test-client")); // client_id
    
    // Request body
    encoder.write_i32(topics.len() as i32);
    
    for (name, partitions, replication_factor, configs) in &topics {
        encoder.write_string(Some(name));
        encoder.write_i32(*partitions);
        encoder.write_i16(*replication_factor);
        encoder.write_i32(-1); // no replica assignments
        encoder.write_i32(configs.len() as i32);
        
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