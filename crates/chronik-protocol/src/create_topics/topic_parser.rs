//! Individual Topic Parsing
//!
//! Handles parsing of individual topic from CreateTopics request including
//! name, partitions, replication factor, replica assignments, and configs.
//! Complexity: < 25 per function

use crate::parser::Decoder;
use crate::create_topics_types::{CreateTopicRequest, ReplicaAssignment};
use chronik_common::{Error, Result};
use std::collections::HashMap;
use tracing::trace;

/// Parser for individual topic in CreateTopics request
pub struct TopicParser;

impl TopicParser {
    /// Parse a single topic from request
    ///
    /// Complexity: < 20 (orchestrates parsing of topic fields)
    pub fn parse(decoder: &mut Decoder, use_compact: bool) -> Result<CreateTopicRequest> {
        // Parse topic name
        let name = Self::parse_topic_name(decoder, use_compact)?;

        // Parse basic topic parameters
        let num_partitions = decoder.read_i32()?;
        let replication_factor = decoder.read_i16()?;

        trace!("Parsing topic '{}': partitions={}, replication={}",
            name, num_partitions, replication_factor);

        // Parse replica assignments
        let replica_assignments = Self::parse_replica_assignments(decoder, use_compact)?;

        // Parse configs
        let configs = Self::parse_configs(decoder, use_compact)?;

        // Skip topic-level tagged fields (v5+)
        if use_compact {
            Self::skip_tagged_fields(decoder)?;
        }

        Ok(CreateTopicRequest {
            name,
            num_partitions,
            replication_factor,
            replica_assignments,
            configs,
        })
    }

    /// Parse topic name string
    ///
    /// Complexity: < 5 (string parsing with validation)
    fn parse_topic_name(decoder: &mut Decoder, use_compact: bool) -> Result<String> {
        let name = if use_compact {
            decoder.read_compact_string()?
                .ok_or_else(|| Error::Protocol("Topic name cannot be null".into()))?
        } else {
            decoder.read_string()?
                .ok_or_else(|| Error::Protocol("Topic name cannot be null".into()))?
        };
        Ok(name)
    }

    /// Parse replica assignments array
    ///
    /// Complexity: < 20 (array loop with nested broker IDs)
    fn parse_replica_assignments(
        decoder: &mut Decoder,
        use_compact: bool,
    ) -> Result<Vec<ReplicaAssignment>> {
        // Parse assignment count
        let assignment_count = if use_compact {
            (decoder.read_unsigned_varint()? as i32 - 1) as usize
        } else {
            decoder.read_i32()? as usize
        };

        if assignment_count == 0 {
            return Ok(Vec::new());
        }

        let mut replica_assignments = Vec::with_capacity(assignment_count);

        for _ in 0..assignment_count {
            let partition_index = decoder.read_i32()?;

            // Parse broker IDs for this partition
            let broker_count = if use_compact {
                (decoder.read_unsigned_varint()? as i32 - 1) as usize
            } else {
                decoder.read_i32()? as usize
            };

            let mut broker_ids = Vec::with_capacity(broker_count);
            for _ in 0..broker_count {
                broker_ids.push(decoder.read_i32()?);
            }

            replica_assignments.push(ReplicaAssignment {
                partition_index,
                broker_ids,
            });

            // Skip tagged fields for this assignment (v5+)
            if use_compact {
                Self::skip_tagged_fields(decoder)?;
            }
        }

        Ok(replica_assignments)
    }

    /// Parse topic configs (key-value pairs)
    ///
    /// Complexity: < 15 (config array loop)
    fn parse_configs(
        decoder: &mut Decoder,
        use_compact: bool,
    ) -> Result<HashMap<String, String>> {
        // Parse config count
        let config_count = if use_compact {
            (decoder.read_unsigned_varint()? as i32 - 1) as usize
        } else {
            decoder.read_i32()? as usize
        };

        if config_count == 0 {
            return Ok(HashMap::new());
        }

        let mut configs = HashMap::with_capacity(config_count);

        for _ in 0..config_count {
            let key = if use_compact {
                decoder.read_compact_string()?
                    .ok_or_else(|| Error::Protocol("Config key cannot be null".into()))?
            } else {
                decoder.read_string()?
                    .ok_or_else(|| Error::Protocol("Config key cannot be null".into()))?
            };

            let value = if use_compact {
                decoder.read_compact_string()?
                    .ok_or_else(|| Error::Protocol("Config value cannot be null".into()))?
            } else {
                decoder.read_string()?
                    .ok_or_else(|| Error::Protocol("Config value cannot be null".into()))?
            };

            configs.insert(key, value);

            // Skip tagged fields for this config (v5+)
            if use_compact {
                Self::skip_tagged_fields(decoder)?;
            }
        }

        Ok(configs)
    }

    /// Skip tagged fields
    ///
    /// Complexity: < 5 (simple loop)
    fn skip_tagged_fields(decoder: &mut Decoder) -> Result<()> {
        let num_tagged_fields = decoder.read_unsigned_varint()?;
        for _ in 0..num_tagged_fields {
            let _tag_id = decoder.read_unsigned_varint()?;
            let tag_size = decoder.read_unsigned_varint()? as usize;
            decoder.advance(tag_size)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::BytesMut;
    use crate::parser::Encoder;

    #[test]
    fn test_parse_topic_name_non_compact() {
        let mut buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut buf);

        encoder.write_string(Some("test-topic"));

        let bytes = buf.freeze();
        let mut bytes_clone = bytes.clone();
        let mut decoder = Decoder::new(&mut bytes_clone);

        let name = TopicParser::parse_topic_name(&mut decoder, false).unwrap();
        assert_eq!(name, "test-topic");
    }

    #[test]
    fn test_parse_topic_name_compact() {
        let mut buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut buf);

        encoder.write_compact_string(Some("test-topic"));

        let bytes = buf.freeze();
        let mut bytes_clone = bytes.clone();
        let mut decoder = Decoder::new(&mut bytes_clone);

        let name = TopicParser::parse_topic_name(&mut decoder, true).unwrap();
        assert_eq!(name, "test-topic");
    }

    #[test]
    fn test_parse_empty_replica_assignments() {
        let mut buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut buf);

        // Empty array in non-compact mode
        encoder.write_i32(0);

        let bytes = buf.freeze();
        let mut bytes_clone = bytes.clone();
        let mut decoder = Decoder::new(&mut bytes_clone);

        let assignments = TopicParser::parse_replica_assignments(&mut decoder, false).unwrap();
        assert_eq!(assignments.len(), 0);
    }

    #[test]
    fn test_parse_empty_configs() {
        let mut buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut buf);

        // Empty array in compact mode (varint 1 = 0 elements)
        encoder.write_unsigned_varint(1);

        let bytes = buf.freeze();
        let mut bytes_clone = bytes.clone();
        let mut decoder = Decoder::new(&mut bytes_clone);

        let configs = TopicParser::parse_configs(&mut decoder, true).unwrap();
        assert_eq!(configs.len(), 0);
    }
}
