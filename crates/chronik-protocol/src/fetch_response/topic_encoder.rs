//! Topic Encoding
//!
//! Handles encoding of topic-level Fetch response data including partition arrays.
//! Complexity: < 20 per function

use crate::parser::Encoder;
use crate::types::FetchResponseTopic;
use super::{PartitionEncoder, TaggedFieldsEncoder};
use tracing::{trace, debug};

/// Encoder for topic-level Fetch response data
pub struct TopicEncoder;

impl TopicEncoder {
    /// Encode topics array with flexible/compact encoding support
    ///
    /// Complexity: < 15 (array length encoding with flexible support)
    pub fn encode_topics_array_header(
        encoder: &mut Encoder,
        topics_count: usize,
        flexible: bool,
    ) {
        if flexible {
            encoder.write_unsigned_varint((topics_count + 1) as u32);
        } else {
            encoder.write_i32(topics_count as i32);
        }
        debug!("  Topics count: {}", topics_count);
    }

    /// Encode a single topic response
    ///
    /// Complexity: < 20 (topic name + partitions loop + tagged fields)
    pub fn encode_topic(
        encoder: &mut Encoder,
        topic: &FetchResponseTopic,
        version: i16,
        flexible: bool,
    ) {
        // Topic name
        if flexible {
            encoder.write_compact_string(Some(&topic.name));
        } else {
            encoder.write_string(Some(&topic.name));
        }
        trace!("    Topic: {}", topic.name);

        // Partitions array
        Self::encode_partitions_array(encoder, topic, version, flexible);

        // Tagged fields for topic (v12+)
        TaggedFieldsEncoder::encode_topic_tagged_fields(encoder, flexible);
    }

    /// Encode partitions array for a topic
    ///
    /// Complexity: < 15 (array header + partition loop)
    fn encode_partitions_array(
        encoder: &mut Encoder,
        topic: &FetchResponseTopic,
        version: i16,
        flexible: bool,
    ) {
        // Partitions array length
        if flexible {
            encoder.write_unsigned_varint((topic.partitions.len() + 1) as u32);
        } else {
            encoder.write_i32(topic.partitions.len() as i32);
        }
        trace!("    Partitions count: {}", topic.partitions.len());

        // Encode each partition
        for partition in &topic.partitions {
            PartitionEncoder::encode_partition(encoder, partition, version, flexible);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::BytesMut;
    use crate::types::FetchResponsePartition;

    fn create_test_topic() -> FetchResponseTopic {
        FetchResponseTopic {
            name: "test-topic".to_string(),
            partitions: vec![FetchResponsePartition {
                partition: 0,
                error_code: 0,
                high_watermark: 100,
                last_stable_offset: 95,
                log_start_offset: 0,
                preferred_read_replica: -1,
                records: vec![],
                aborted: None,
            }],
        }
    }

    #[test]
    fn test_encode_topics_array_header_non_flexible() {
        let mut buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut buf);

        TopicEncoder::encode_topics_array_header(&mut encoder, 3, false);

        // Non-flexible: i32 count = 4 bytes
        assert_eq!(buf.len(), 4);
    }

    #[test]
    fn test_encode_topics_array_header_flexible() {
        let mut buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut buf);

        TopicEncoder::encode_topics_array_header(&mut encoder, 3, true);

        // Flexible: varint (3+1=4) = 1 byte
        assert_eq!(buf.len(), 1);
        assert_eq!(buf[0], 4);
    }

    #[test]
    fn test_encode_topic_v0() {
        let mut buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut buf);
        let topic = create_test_topic();

        TopicEncoder::encode_topic(&mut encoder, &topic, 0, false);

        // Topic name + partitions array
        assert!(buf.len() > 0);
    }

    #[test]
    fn test_encode_topic_v12_flexible() {
        let mut buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut buf);
        let topic = create_test_topic();

        TopicEncoder::encode_topic(&mut encoder, &topic, 12, true);

        // Flexible encoding with compact strings and tagged fields
        assert!(buf.len() > 0);
    }
}
