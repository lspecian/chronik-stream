//! CreateTopics Response Encoding
//!
//! Handles encoding of CreateTopics response with version-specific fields.
//! Complexity: < 25 per function

use crate::parser::Encoder;
use crate::create_topics_types::CreateTopicsResponse;
use chronik_common::Result;
use bytes::BytesMut;
use tracing::debug;

/// Encoder for CreateTopics response
pub struct ResponseEncoder;

impl ResponseEncoder {
    /// Encode CreateTopics response
    ///
    /// Complexity: < 20 (orchestrates version-specific encoding)
    pub fn encode(
        buf: &mut BytesMut,
        response: &CreateTopicsResponse,
        version: i16,
    ) -> Result<()> {
        let mut encoder = Encoder::new(buf);
        let use_compact = version >= 5;

        debug!("Encoding CreateTopics response v{}, use_compact={}, topics_len={}",
            version, use_compact, response.topics.len());

        // Phase 1: Encode throttle time (v2+)
        Self::encode_throttle_time(&mut encoder, response.throttle_time_ms, version);

        // Phase 2: Encode topics array header
        Self::encode_topics_array_header(&mut encoder, response.topics.len(), use_compact);

        // Phase 3: Encode each topic
        for topic in &response.topics {
            Self::encode_topic(&mut encoder, topic, version, use_compact)?;
        }

        // Phase 4: Encode response-level tagged fields (v5+)
        if use_compact {
            encoder.write_unsigned_varint(0); // Empty tagged fields
        }

        Ok(())
    }

    /// Encode throttle time (v2+)
    ///
    /// Complexity: < 5 (version check + write)
    fn encode_throttle_time(encoder: &mut Encoder, throttle_time_ms: i32, version: i16) {
        if version >= 2 {
            encoder.write_i32(throttle_time_ms);
        }
    }

    /// Encode topics array header
    ///
    /// Complexity: < 5 (flexible vs non-flexible)
    fn encode_topics_array_header(encoder: &mut Encoder, topics_len: usize, use_compact: bool) {
        if use_compact {
            encoder.write_compact_array_len(topics_len);
        } else {
            encoder.write_i32(topics_len as i32);
        }
    }

    /// Encode a single topic response
    ///
    /// Complexity: < 25 (orchestrates topic field encoding)
    fn encode_topic(
        encoder: &mut Encoder,
        topic: &crate::create_topics_types::CreateTopicResponse,
        version: i16,
        use_compact: bool,
    ) -> Result<()> {
        // Topic name
        Self::encode_topic_name(encoder, &topic.name, use_compact);

        // Topic ID (v7+)
        if version >= 7 {
            Self::encode_topic_id(encoder);
        }

        // Error code
        encoder.write_i16(topic.error_code);

        // Error message (v1+)
        if version >= 1 {
            Self::encode_error_message(encoder, topic.error_message.as_deref(), use_compact);
        }

        // Detailed topic info (v5+)
        if version >= 5 {
            Self::encode_topic_details(encoder, topic, use_compact)?;
        }

        Ok(())
    }

    /// Encode topic name
    ///
    /// Complexity: < 5
    fn encode_topic_name(encoder: &mut Encoder, name: &str, use_compact: bool) {
        if use_compact {
            encoder.write_compact_string(Some(name));
        } else {
            encoder.write_string(Some(name));
        }
    }

    /// Encode topic ID (v7+)
    ///
    /// Complexity: < 5
    fn encode_topic_id(encoder: &mut Encoder) {
        // Placeholder UUID (16 bytes of zeros)
        // Real implementation would store actual topic IDs
        let uuid_bytes = [0u8; 16];
        encoder.write_raw_bytes(&uuid_bytes);
    }

    /// Encode error message
    ///
    /// Complexity: < 5
    fn encode_error_message(
        encoder: &mut Encoder,
        error_message: Option<&str>,
        use_compact: bool,
    ) {
        if use_compact {
            encoder.write_compact_string(error_message);
        } else {
            encoder.write_string(error_message);
        }
    }

    /// Encode detailed topic info (v5+)
    ///
    /// Complexity: < 15 (partitions, replication, configs array)
    fn encode_topic_details(
        encoder: &mut Encoder,
        topic: &crate::create_topics_types::CreateTopicResponse,
        use_compact: bool,
    ) -> Result<()> {
        // Num partitions and replication factor
        encoder.write_i32(topic.num_partitions);
        encoder.write_i16(topic.replication_factor);

        // Configs array
        Self::encode_configs_array(encoder, &topic.configs, use_compact)?;

        // Topic-level tagged fields (v5+)
        if use_compact {
            encoder.write_unsigned_varint(0); // Empty tagged fields
        }

        Ok(())
    }

    /// Encode configs array
    ///
    /// Complexity: < 15 (array header + config loop)
    fn encode_configs_array(
        encoder: &mut Encoder,
        configs: &[crate::create_topics_types::CreateTopicConfigEntry],
        use_compact: bool,
    ) -> Result<()> {
        // Configs array header
        if use_compact {
            encoder.write_compact_array_len(configs.len());
        } else {
            encoder.write_i32(configs.len() as i32);
        }

        // Encode each config
        for config in configs {
            Self::encode_config(encoder, config, use_compact);
        }

        Ok(())
    }

    /// Encode a single config
    ///
    /// Complexity: < 10 (config fields + tagged fields)
    fn encode_config(
        encoder: &mut Encoder,
        config: &crate::create_topics_types::CreateTopicConfigEntry,
        use_compact: bool,
    ) {
        // Config name and value
        if use_compact {
            encoder.write_compact_string(Some(&config.name));
            encoder.write_compact_string(config.value.as_deref());
        } else {
            encoder.write_string(Some(&config.name));
            encoder.write_string(config.value.as_deref());
        }

        // Config metadata
        encoder.write_bool(config.read_only);
        encoder.write_i8(config.config_source);
        encoder.write_bool(config.is_sensitive);

        // Config-level tagged fields (v5+)
        if use_compact {
            encoder.write_unsigned_varint(0); // Empty tagged fields
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::create_topics_types::{CreateTopicResponse, error_codes};

    #[test]
    fn test_encode_response_v0_empty() {
        let response = CreateTopicsResponse {
            throttle_time_ms: 0,
            topics: Vec::new(),
        };

        let mut buf = BytesMut::new();
        ResponseEncoder::encode(&mut buf, &response, 0).unwrap();

        // v0: no throttle time, topics array length (4 bytes) = 4 bytes total
        assert_eq!(buf.len(), 4);
    }

    #[test]
    fn test_encode_response_v2_with_throttle() {
        let response = CreateTopicsResponse {
            throttle_time_ms: 100,
            topics: Vec::new(),
        };

        let mut buf = BytesMut::new();
        ResponseEncoder::encode(&mut buf, &response, 2).unwrap();

        // v2: throttle_time (4 bytes) + topics array length (4 bytes) = 8 bytes
        assert_eq!(buf.len(), 8);
    }

    #[test]
    fn test_encode_response_v5_compact() {
        let response = CreateTopicsResponse {
            throttle_time_ms: 0,
            topics: vec![CreateTopicResponse {
                name: "test".to_string(),
                error_code: error_codes::NONE,
                error_message: None,
                num_partitions: 3,
                replication_factor: 1,
                configs: Vec::new(),
            }],
        };

        let mut buf = BytesMut::new();
        ResponseEncoder::encode(&mut buf, &response, 5).unwrap();

        // v5: compact encoding, should be smaller
        assert!(buf.len() > 0);
    }

    #[test]
    fn test_encode_topic_name_non_compact() {
        let mut buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut buf);

        ResponseEncoder::encode_topic_name(&mut encoder, "test-topic", false);

        // String length (2 bytes) + "test-topic" (10 bytes) = 12 bytes
        assert_eq!(buf.len(), 12);
    }

    #[test]
    fn test_encode_topic_name_compact() {
        let mut buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut buf);

        ResponseEncoder::encode_topic_name(&mut encoder, "test-topic", true);

        // Compact string: varint length + bytes (smaller than non-compact)
        assert_eq!(buf.len(), 11); // varint(11) + 10 bytes
    }
}
