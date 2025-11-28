//! Metadata response building and encoding
//!
//! Extracted from `handle_metadata()` and `encode_metadata_response()` to reduce complexity.
//! Handles version-specific response encoding for Kafka Metadata API v0-v13.

use bytes::BytesMut;
use chronik_common::Result;
use crate::types::{MetadataResponse, MetadataBroker};
use crate::parser::Encoder;

/// Metadata response builder
///
/// Builds and encodes Kafka Metadata API responses with version-specific logic.
pub struct MetadataResponseBuilder;

impl MetadataResponseBuilder {
    /// Build metadata response
    ///
    /// Complexity: < 10 (simple struct creation with logging)
    pub fn build(
        brokers: Vec<MetadataBroker>,
        topics: Vec<crate::types::MetadataTopic>,
        broker_id: i32,
        api_version: i16,
    ) -> MetadataResponse {
        let response = MetadataResponse {
            throttle_time_ms: 0,
            brokers,
            cluster_id: Some("chronik-stream".to_string()),
            controller_id: broker_id,
            topics,
            cluster_authorized_operations: if api_version >= 8 { Some(-2147483648) } else { None },
        };

        tracing::info!("Metadata response has {} topics and {} brokers",
            response.topics.len(), response.brokers.len());

        // Debug: Log broker details
        for (i, broker) in response.brokers.iter().enumerate() {
            tracing::debug!("  Broker {}: id={}, host={}, port={}",
                i, broker.node_id, broker.host, broker.port);
        }

        response
    }

    /// Encode metadata response with version-specific logic
    ///
    /// Complexity: < 25 (version-specific encoding with clear phases)
    pub fn encode(
        buf: &mut BytesMut,
        response: &MetadataResponse,
        version: i16,
    ) -> Result<()> {
        let mut encoder = Encoder::new(buf);

        // Validate and log metadata response structure
        tracing::info!("Encoding metadata response v{} with {} topics, {} brokers, throttle_time: {}",
            version, response.topics.len(), response.brokers.len(), response.throttle_time_ms);

        Self::log_response_details(response);
        Self::validate_response(response, version);

        // Check if this is a flexible/compact version (v9+)
        let flexible = version >= 9;

        // Phase 1: Encode throttle time (v3+)
        Self::encode_throttle_time(&mut encoder, response.throttle_time_ms, version, flexible)?;

        // Phase 2: Encode brokers array
        Self::encode_brokers(&mut encoder, &response.brokers, version, flexible)?;

        // Phase 3: Encode cluster metadata (cluster_id, controller_id)
        Self::encode_cluster_metadata(&mut encoder, response, version, flexible)?;

        // Phase 4: Encode topics array
        Self::encode_topics(&mut encoder, &response.topics, version, flexible)?;

        // Phase 5: Encode cluster authorized operations (v8-v10 only)
        Self::encode_cluster_ops(&mut encoder, response.cluster_authorized_operations, version, flexible)?;

        // Phase 6: Encode top-level tagged fields (flexible versions)
        if flexible {
            encoder.write_tagged_fields();
        }

        // Log final encoded size for debugging
        let final_buffer = encoder.debug_buffer();
        let final_size = final_buffer.len();
        tracing::info!("Metadata response v{} encoded successfully: {} bytes", version, final_size);
        tracing::debug!("Final metadata response body: first 32 bytes: {:02x?}",
            &final_buffer[..final_buffer.len().min(32)]);

        Ok(())
    }

    /// Log response details for debugging
    ///
    /// Complexity: < 5 (logging only)
    fn log_response_details(response: &MetadataResponse) {
        // Log broker details
        for (i, broker) in response.brokers.iter().enumerate() {
            tracing::debug!("  Broker[{}]: node_id={}, host={}, port={}, rack={:?}",
                i, broker.node_id, broker.host, broker.port, broker.rack);
        }

        // Log topic details
        for (i, topic) in response.topics.iter().enumerate() {
            tracing::debug!("  Topic[{}]: name={}, error_code={}, partitions={}, is_internal={}",
                i, &topic.name,
                topic.error_code, topic.partitions.len(), topic.is_internal);

            // Sample first partition if exists
            if let Some(partition) = topic.partitions.first() {
                tracing::debug!("    Partition[0]: index={}, leader={}, replicas={:?}, isr={:?}, error_code={}",
                    partition.partition_index, partition.leader_id,
                    partition.replica_nodes, partition.isr_nodes, partition.error_code);
            }
        }
    }

    /// Validate response has required fields
    ///
    /// Complexity: < 5 (validation checks with warnings)
    fn validate_response(response: &MetadataResponse, version: i16) {
        if response.brokers.is_empty() {
            tracing::warn!("Metadata response has no brokers - this may cause client issues");
        }
        if response.cluster_id.is_none() && version >= 2 {
            tracing::warn!("Metadata response missing cluster_id for v{} - clients may reject", version);
        }
    }

    /// Encode throttle time (v3+)
    ///
    /// Complexity: < 5 (simple version check)
    fn encode_throttle_time(
        encoder: &mut Encoder,
        throttle_time_ms: i32,
        version: i16,
        _flexible: bool,
    ) -> Result<()> {
        if version >= 3 {
            tracing::debug!("Writing throttle_time_ms={} for v{}", throttle_time_ms, version);
            encoder.write_i32(throttle_time_ms);
        } else {
            tracing::debug!("NOT writing throttle_time for v{} (only for v3+)", version);
        }
        Ok(())
    }

    /// Encode brokers array
    ///
    /// Complexity: < 15 (array encoding with version-specific fields)
    fn encode_brokers(
        encoder: &mut Encoder,
        brokers: &[MetadataBroker],
        version: i16,
        flexible: bool,
    ) -> Result<()> {
        tracing::debug!("About to encode {} brokers", brokers.len());

        // Get buffer position before writing
        let buffer_before = encoder.debug_buffer().len();

        // Write array length
        if flexible {
            encoder.write_compact_array_len(brokers.len());
        } else {
            encoder.write_i32(brokers.len() as i32);
        }

        let buffer_after = encoder.debug_buffer().len();
        tracing::debug!("Metadata response buffer: before={}, after={}, size={}",
            buffer_before, buffer_after, buffer_after - buffer_before);
        let debug_buf = encoder.debug_buffer();
        tracing::debug!("Metadata response bytes: {:02x?}",
            &debug_buf[buffer_before..buffer_after]);

        // Encode each broker
        for broker in brokers {
            Self::encode_broker(encoder, broker, version, flexible)?;
        }

        Ok(())
    }

    /// Encode individual broker
    ///
    /// Complexity: < 10 (broker field encoding with version checks)
    fn encode_broker(
        encoder: &mut Encoder,
        broker: &MetadataBroker,
        version: i16,
        flexible: bool,
    ) -> Result<()> {
        encoder.write_i32(broker.node_id);

        if flexible {
            encoder.write_compact_string(Some(&broker.host));
        } else {
            encoder.write_string(Some(&broker.host));
        }

        encoder.write_i32(broker.port);

        if version >= 1 {
            if flexible {
                encoder.write_compact_string(broker.rack.as_deref());
            } else {
                encoder.write_string(broker.rack.as_deref());
            }
        }

        if flexible {
            encoder.write_tagged_fields();
        }

        Ok(())
    }

    /// Encode cluster metadata (cluster_id, controller_id)
    ///
    /// Complexity: < 10 (version-specific field encoding)
    fn encode_cluster_metadata(
        encoder: &mut Encoder,
        response: &MetadataResponse,
        version: i16,
        flexible: bool,
    ) -> Result<()> {
        // Cluster ID comes after brokers for v2+
        if version >= 2 {
            tracing::debug!("Writing cluster_id {:?} at position {}", response.cluster_id, encoder.position());
            if flexible {
                encoder.write_compact_string(response.cluster_id.as_deref());
            } else {
                encoder.write_string(response.cluster_id.as_deref());
            }
        }

        // Controller ID comes after cluster_id for v2+, or directly after brokers for v1
        if version >= 1 {
            tracing::debug!("Writing controller_id {} at position {}", response.controller_id, encoder.position());
            encoder.write_i32(response.controller_id);
        }

        Ok(())
    }

    /// Encode topics array
    ///
    /// Complexity: < 20 (array encoding with partitions)
    fn encode_topics(
        encoder: &mut Encoder,
        topics: &[crate::types::MetadataTopic],
        version: i16,
        flexible: bool,
    ) -> Result<()> {
        // Write topics array length
        if flexible {
            encoder.write_compact_array_len(topics.len());
        } else {
            encoder.write_i32(topics.len() as i32);
        }

        // Encode each topic
        for topic in topics {
            Self::encode_topic(encoder, topic, version, flexible)?;
        }

        Ok(())
    }

    /// Encode individual topic
    ///
    /// Complexity: < 15 (topic field encoding with partitions)
    fn encode_topic(
        encoder: &mut Encoder,
        topic: &crate::types::MetadataTopic,
        version: i16,
        flexible: bool,
    ) -> Result<()> {
        encoder.write_i16(topic.error_code);

        if flexible {
            encoder.write_compact_string(Some(&topic.name));
        } else {
            encoder.write_string(Some(&topic.name));
        }

        // Topic ID (v10+) - UUID (16 bytes)
        if version >= 10 {
            // For now, use a null UUID (all zeros)
            encoder.write_raw_bytes(&[0u8; 16]);
        }

        if version >= 1 {
            encoder.write_bool(topic.is_internal);
        }

        // Encode partitions array
        Self::encode_partitions(encoder, &topic.partitions, version, flexible)?;

        // Topic authorized operations (v8+)
        // Note: This was NOT removed in v11 (only cluster_authorized_operations was removed)
        if version >= 8 {
            encoder.write_i32(-2147483648); // INT32_MIN means "null"
        }

        if flexible {
            encoder.write_tagged_fields();
        }

        Ok(())
    }

    /// Encode partitions array
    ///
    /// Complexity: < 15 (array encoding with partition metadata)
    fn encode_partitions(
        encoder: &mut Encoder,
        partitions: &[crate::types::MetadataPartition],
        version: i16,
        flexible: bool,
    ) -> Result<()> {
        // Write partitions array length
        if flexible {
            encoder.write_compact_array_len(partitions.len());
        } else {
            encoder.write_i32(partitions.len() as i32);
        }

        // Encode each partition
        for partition in partitions {
            Self::encode_partition(encoder, partition, version, flexible)?;
        }

        Ok(())
    }

    /// Encode individual partition
    ///
    /// Complexity: < 15 (partition field encoding with arrays)
    fn encode_partition(
        encoder: &mut Encoder,
        partition: &crate::types::MetadataPartition,
        version: i16,
        flexible: bool,
    ) -> Result<()> {
        encoder.write_i16(partition.error_code);
        encoder.write_i32(partition.partition_index);
        encoder.write_i32(partition.leader_id);

        if version >= 7 {
            encoder.write_i32(partition.leader_epoch);
        }

        // Replica nodes
        if flexible {
            encoder.write_compact_array_len(partition.replica_nodes.len());
        } else {
            encoder.write_i32(partition.replica_nodes.len() as i32);
        }
        for replica in &partition.replica_nodes {
            encoder.write_i32(*replica);
        }

        // ISR nodes
        if flexible {
            encoder.write_compact_array_len(partition.isr_nodes.len());
        } else {
            encoder.write_i32(partition.isr_nodes.len() as i32);
        }
        for isr in &partition.isr_nodes {
            encoder.write_i32(*isr);
        }

        // Offline replicas (v5+)
        if version >= 5 {
            if flexible {
                encoder.write_compact_array_len(partition.offline_replicas.len());
            } else {
                encoder.write_i32(partition.offline_replicas.len() as i32);
            }
            for offline in &partition.offline_replicas {
                encoder.write_i32(*offline);
            }
        }

        if flexible {
            encoder.write_tagged_fields();
        }

        Ok(())
    }

    /// Encode cluster authorized operations (v8-v10 only)
    ///
    /// Complexity: < 5 (version range check)
    fn encode_cluster_ops(
        encoder: &mut Encoder,
        cluster_authorized_operations: Option<i32>,
        version: i16,
        _flexible: bool,
    ) -> Result<()> {
        // Cluster authorized operations (v8-v10 only, removed in v11+)
        // KIP-430: cluster_authorized_operations was removed starting from v11
        if version >= 8 && version <= 10 {
            if let Some(ops) = cluster_authorized_operations {
                encoder.write_i32(ops);
            } else {
                encoder.write_i32(-2147483648); // INT32_MIN means "null"
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::MetadataTopic;

    #[test]
    fn test_build_metadata_response() {
        let brokers = vec![MetadataBroker {
            node_id: 1,
            host: "localhost".to_string(),
            port: 9092,
            rack: None,
        }];

        let topics = vec![MetadataTopic {
            error_code: 0,
            name: "test-topic".to_string(),
            is_internal: false,
            partitions: vec![],
        }];

        let response = MetadataResponseBuilder::build(brokers.clone(), topics.clone(), 1, 9);

        assert_eq!(response.brokers.len(), 1);
        assert_eq!(response.topics.len(), 1);
        assert_eq!(response.controller_id, 1);
        assert_eq!(response.cluster_id, Some("chronik-stream".to_string()));
    }

    #[test]
    fn test_encode_metadata_response_v0() {
        let brokers = vec![MetadataBroker {
            node_id: 1,
            host: "localhost".to_string(),
            port: 9092,
            rack: None,
        }];

        let topics = vec![];

        let response = MetadataResponseBuilder::build(brokers, topics, 1, 0);
        let mut buf = BytesMut::new();

        let result = MetadataResponseBuilder::encode(&mut buf, &response, 0);
        assert!(result.is_ok());
        assert!(buf.len() > 0);
    }

    #[test]
    fn test_encode_metadata_response_v9_flexible() {
        let brokers = vec![MetadataBroker {
            node_id: 1,
            host: "localhost".to_string(),
            port: 9092,
            rack: None,
        }];

        let topics = vec![];

        let response = MetadataResponseBuilder::build(brokers, topics, 1, 9);
        let mut buf = BytesMut::new();

        let result = MetadataResponseBuilder::encode(&mut buf, &response, 9);
        assert!(result.is_ok());
        assert!(buf.len() > 0);
    }
}
