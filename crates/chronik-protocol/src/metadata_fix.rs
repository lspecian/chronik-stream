//! Metadata response encoding fix
//!
//! This module provides a version-aware metadata response encoder that ensures
//! throttle_time is only included for v3+ as per the Kafka protocol specification.

use bytes::BytesMut;
use crate::parser::Encoder;
use crate::types::MetadataResponse;

/// Encode a metadata response with proper version handling
pub fn encode_metadata_response_versioned(
    response: &MetadataResponse,
    version: i16,
) -> Vec<u8> {
    let mut buf = BytesMut::new();
    let mut encoder = Encoder::new(&mut buf);
    
    // Correlation ID is always first
    encoder.write_i32(response.correlation_id);
    
    // Throttle time only for v3+
    if version >= 3 {
        encoder.write_i32(response.throttle_time_ms);
    }
    
    // Brokers array
    encoder.write_i32(response.brokers.len() as i32);
    for broker in &response.brokers {
        encoder.write_i32(broker.node_id);
        encoder.write_string(Some(&broker.host));
        encoder.write_i32(broker.port);
        
        // Rack (v1+)
        if version >= 1 {
            encoder.write_string(broker.rack.as_deref());
        }
    }
    
    // Cluster ID (v2+)
    if version >= 2 {
        encoder.write_string(response.cluster_id.as_deref());
    }
    
    // Controller ID (v1+)
    if version >= 1 {
        encoder.write_i32(response.controller_id);
    }
    
    // Topics array
    encoder.write_i32(response.topics.len() as i32);
    for topic in &response.topics {
        encoder.write_i16(topic.error_code);
        encoder.write_string(Some(&topic.name));
        
        // Is internal (v1+)
        if version >= 1 {
            encoder.write_bool(topic.is_internal);
        }
        
        // Partitions array
        encoder.write_i32(topic.partitions.len() as i32);
        for partition in &topic.partitions {
            encoder.write_i16(partition.error_code);
            encoder.write_i32(partition.partition_index);
            encoder.write_i32(partition.leader_id);
            
            // Leader epoch (v7+)
            if version >= 7 {
                encoder.write_i32(partition.leader_epoch);
            }
            
            // Replica nodes
            encoder.write_i32(partition.replica_nodes.len() as i32);
            for replica in &partition.replica_nodes {
                encoder.write_i32(*replica);
            }
            
            // ISR nodes
            encoder.write_i32(partition.isr_nodes.len() as i32);
            for isr in &partition.isr_nodes {
                encoder.write_i32(*isr);
            }
            
            // Offline replicas (v5+)
            if version >= 5 {
                encoder.write_i32(partition.offline_replicas.len() as i32);
                for offline in &partition.offline_replicas {
                    encoder.write_i32(*offline);
                }
            }
        }
        
        // Topic authorized operations (v8+)
        if version >= 8 {
            encoder.write_i32(-2147483648); // INT32_MIN indicates null
        }
    }
    
    // Cluster authorized operations (v8+)
    if version >= 8 {
        encoder.write_i32(-2147483648); // INT32_MIN indicates null
    }
    
    buf.to_vec()
}