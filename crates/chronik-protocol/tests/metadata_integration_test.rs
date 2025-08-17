//! Integration test for Metadata API with topic creation

use chronik_protocol::handler::ProtocolHandler;
use chronik_protocol::parser::Encoder;
use chronik_protocol::ApiKey;
use chronik_common::metadata::{MetadataStore, BrokerStatus, InMemoryMetadataStore};
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

/// Helper to encode Metadata request for version 0
fn encode_metadata_request_v0() -> Bytes {
    let mut buf = BytesMut::new();
    let mut encoder = Encoder::new(&mut buf);
    
    // Write request header
    encoder.write_i16(ApiKey::Metadata as i16);
    encoder.write_i16(0); // version 0
    encoder.write_i32(456); // correlation_id (different from create topics)
    encoder.write_string(Some("test-client")); // client_id
    
    // For version 0, no request body (gets all topics)
    
    buf.freeze()
}

#[tokio::test]
async fn test_metadata_after_topic_creation() {
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
    let create_response = handler.handle_request(&create_request).await.unwrap();
    
    // Verify CreateTopics succeeded
    assert_eq!(create_response.header.correlation_id, 123);
    
    // Now request metadata
    let metadata_request = encode_metadata_request_v0();
    let metadata_response = handler.handle_request(&metadata_request).await.unwrap();
    
    assert_eq!(metadata_response.header.correlation_id, 456);
    
    // Parse metadata response (version 0)
    let mut response_bytes = metadata_response.body;
    let mut decoder = chronik_protocol::parser::Decoder::new(&mut response_bytes);
    
    // Version 0: no throttle_time_ms
    
    // brokers array
    let broker_count = decoder.read_i32().unwrap();
    println!("Broker count: {}", broker_count);
    assert_eq!(broker_count, 1, "Should have one broker");
    
    // Broker details
    let broker_id = decoder.read_i32().unwrap();
    let host = decoder.read_string().unwrap().unwrap();
    let port = decoder.read_i32().unwrap();
    println!("Broker: id={}, host={}, port={}", broker_id, host, port);
    
    // Version 0: no cluster_id or controller_id
    
    // topics array
    let topic_count = decoder.read_i32().unwrap();
    println!("Topic count: {}", topic_count);
    assert_eq!(topic_count, 1, "Should have one topic");
    
    // Topic details
    let error_code = decoder.read_i16().unwrap();
    assert_eq!(error_code, 0, "Topic should have no error");
    
    let topic_name = decoder.read_string().unwrap().unwrap();
    assert_eq!(topic_name, "test-topic");
    println!("Topic name: {}", topic_name);
    
    // Version 0: no is_internal field
    
    // Partitions array
    let partition_count = decoder.read_i32().unwrap();
    println!("Partition count: {}", partition_count);
    assert_eq!(partition_count, 3, "Should have 3 partitions");
    
    for partition_id in 0..3 {
        let error_code = decoder.read_i16().unwrap();
        assert_eq!(error_code, 0);
        
        let partition_index = decoder.read_i32().unwrap();
        assert_eq!(partition_index, partition_id);
        
        let leader_id = decoder.read_i32().unwrap();
        assert_eq!(leader_id, 1, "Leader should be broker 1");
        
        // Version 0: no leader_epoch
        
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
        
        // Version 0: no offline_replicas
        
        println!("Partition {} - leader: {}, replicas: [{}], isr: [{}]", 
                partition_index, leader_id, replica_id, isr_id);
    }
    
    println!("Metadata test completed successfully!");
}