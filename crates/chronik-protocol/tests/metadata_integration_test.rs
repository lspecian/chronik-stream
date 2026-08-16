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
/// RP-1.2: Metadata must report the in-sync set the leader measured, not the
/// assignment.
///
/// This was `isr_nodes: replica_nodes.clone()` with the note "for now, all
/// replicas are in-sync" — the same inversion RP-1.2 removed from
/// `/admin/status`, where a partition replicating to nobody reported a full ISR.
/// Metadata is where every Kafka client and monitoring tool reads ISR
/// (`kafka-topics --describe`, Kafka UI, Cruise Control), so the one signal that
/// says "acknowledged data is at risk" read healthy in exactly the case it
/// exists to flag.
#[tokio::test]
async fn metadata_reports_the_measured_isr_not_the_assignment() {
    use chronik_common::metadata::{PartitionAssignment, TopicConfig};

    let metadata_store = Arc::new(InMemoryMetadataStore::new());
    let handler = ProtocolHandler::with_metadata_store(metadata_store.clone());

    for broker_id in 1..=3 {
        metadata_store
            .register_broker(chronik_common::metadata::BrokerMetadata {
                broker_id,
                host: "localhost".to_string(),
                port: 9092 + broker_id,
                rack: None,
                status: BrokerStatus::Online,
                created_at: chronik_common::Utc::now(),
                updated_at: chronik_common::Utc::now(),
            })
            .await
            .unwrap();
    }

    metadata_store
        .create_topic(
            "replicated",
            TopicConfig {
                partition_count: 1,
                replication_factor: 3,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    // Three replicas assigned; node 3 has fallen out of the in-sync set.
    metadata_store
        .assign_partition(PartitionAssignment {
            topic: "replicated".to_string(),
            partition: 0,
            broker_id: 1,
            is_leader: true,
            replicas: vec![1, 2, 3],
            leader_id: 1,
            leader_epoch: 0,
            isr: vec![1, 2],
        })
        .await
        .unwrap();

    let response = handler
        .handle_request(&encode_metadata_request_v0())
        .await
        .unwrap();

    let mut body = response.body;
    let mut decoder = chronik_protocol::parser::Decoder::new(&mut body);

    // brokers
    let broker_count = decoder.read_i32().unwrap();
    for _ in 0..broker_count {
        let _id = decoder.read_i32().unwrap();
        let _host = decoder.read_string().unwrap();
        let _port = decoder.read_i32().unwrap();
    }

    let topic_count = decoder.read_i32().unwrap();
    let mut checked = false;

    for _ in 0..topic_count {
        let _err = decoder.read_i16().unwrap();
        let name = decoder.read_string().unwrap().unwrap_or_default();
        let partition_count = decoder.read_i32().unwrap();

        for _ in 0..partition_count {
            let _p_err = decoder.read_i16().unwrap();
            let _index = decoder.read_i32().unwrap();
            let _leader = decoder.read_i32().unwrap();

            let replica_count = decoder.read_i32().unwrap();
            let replicas: Vec<i32> = (0..replica_count)
                .map(|_| decoder.read_i32().unwrap())
                .collect();

            let isr_count = decoder.read_i32().unwrap();
            let isr: Vec<i32> = (0..isr_count).map(|_| decoder.read_i32().unwrap()).collect();

            if name == "replicated" {
                assert_eq!(replicas, vec![1, 2, 3], "all three are still assigned");
                assert_eq!(
                    isr,
                    vec![1, 2],
                    "Metadata reported the assignment as the in-sync set; node 3 is not in sync"
                );
                checked = true;
            }
        }
    }

    assert!(checked, "topic `replicated` missing from the Metadata response");
}

/// A partition nobody has measured yet — single-node, or never written to —
/// must still report its replicas rather than an empty ISR.
///
/// An empty in-sync set is a hard error for clients: `min.insync.replicas`
/// cannot be satisfied and producers refuse to write. "Unknown" and "nobody is
/// in sync" are different answers, and only the leader can tell them apart.
#[tokio::test]
async fn metadata_falls_back_to_replicas_when_isr_was_never_measured() {
    use chronik_common::metadata::{PartitionAssignment, TopicConfig};

    let metadata_store = Arc::new(InMemoryMetadataStore::new());
    let handler = ProtocolHandler::with_metadata_store(metadata_store.clone());

    metadata_store
        .register_broker(chronik_common::metadata::BrokerMetadata {
            broker_id: 1,
            host: "localhost".to_string(),
            port: 9092,
            rack: None,
            status: BrokerStatus::Online,
            created_at: chronik_common::Utc::now(),
            updated_at: chronik_common::Utc::now(),
        })
        .await
        .unwrap();

    metadata_store
        .create_topic(
            "fresh",
            TopicConfig {
                partition_count: 1,
                replication_factor: 1,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    metadata_store
        .assign_partition(PartitionAssignment {
            topic: "fresh".to_string(),
            partition: 0,
            broker_id: 1,
            is_leader: true,
            replicas: vec![1],
            leader_id: 1,
            leader_epoch: 0,
            isr: vec![], // nothing measured yet
        })
        .await
        .unwrap();

    let response = handler
        .handle_request(&encode_metadata_request_v0())
        .await
        .unwrap();

    let mut body = response.body;
    let mut decoder = chronik_protocol::parser::Decoder::new(&mut body);

    let broker_count = decoder.read_i32().unwrap();
    for _ in 0..broker_count {
        let _id = decoder.read_i32().unwrap();
        let _host = decoder.read_string().unwrap();
        let _port = decoder.read_i32().unwrap();
    }

    let topic_count = decoder.read_i32().unwrap();
    for _ in 0..topic_count {
        let _err = decoder.read_i16().unwrap();
        let name = decoder.read_string().unwrap().unwrap_or_default();
        let partition_count = decoder.read_i32().unwrap();

        for _ in 0..partition_count {
            let _p_err = decoder.read_i16().unwrap();
            let _index = decoder.read_i32().unwrap();
            let _leader = decoder.read_i32().unwrap();

            let replica_count = decoder.read_i32().unwrap();
            for _ in 0..replica_count {
                let _ = decoder.read_i32().unwrap();
            }

            let isr_count = decoder.read_i32().unwrap();
            let isr: Vec<i32> = (0..isr_count).map(|_| decoder.read_i32().unwrap()).collect();

            if name == "fresh" {
                assert_eq!(
                    isr,
                    vec![1],
                    "an unmeasured partition must not report an empty ISR — \
                     producers would be unable to write to it"
                );
            }
        }
    }
}
