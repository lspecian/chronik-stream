//! CreateTopics Request Parsing
//!
//! Handles parsing of CreateTopics request body including topics array, timeout, and validate_only flag.
//! Complexity: < 20 per function

use crate::parser::Decoder;
use crate::create_topics_types::CreateTopicsRequest;
use chronik_common::{Error, Result};
use tracing::debug;

/// Parser for CreateTopics request
pub struct RequestParser;

impl RequestParser {
    /// Parse complete CreateTopics request
    ///
    /// Complexity: < 20 (orchestrates topic parsing and final fields)
    pub fn parse(
        decoder: &mut Decoder,
        api_version: i16,
    ) -> Result<CreateTopicsRequest> {
        let use_compact = api_version >= 5;

        debug!("Parsing CreateTopics request v{}, use_compact={}", api_version, use_compact);

        // Parse topics array
        let topics = Self::parse_topics_array(decoder, use_compact)?;

        // Parse timeout
        let timeout_ms = decoder.read_i32()?;

        // Parse validate_only flag (v1+)
        let validate_only = if api_version >= 1 {
            decoder.read_bool()?
        } else {
            false
        };

        // Skip trailing tagged fields (v5+)
        if use_compact {
            Self::skip_tagged_fields(decoder)?;
        }

        Ok(CreateTopicsRequest {
            topics,
            timeout_ms,
            validate_only,
        })
    }

    /// Parse topics array with delegation to TopicParser
    ///
    /// Complexity: < 15 (array length parsing + loop delegation)
    fn parse_topics_array(
        decoder: &mut Decoder,
        use_compact: bool,
    ) -> Result<Vec<crate::create_topics_types::CreateTopicRequest>> {
        use super::topic_parser::TopicParser;

        // Parse topic count
        let topic_count = if use_compact {
            let compact_len = decoder.read_unsigned_varint()?;
            if compact_len == 0 {
                return Err(Error::Protocol("CreateTopics compact array cannot be null".into()));
            }
            let actual_len = compact_len - 1;
            if actual_len > 1000 {
                return Err(Error::Protocol("CreateTopics topic count too large".into()));
            }
            actual_len as usize
        } else {
            let len = decoder.read_i32()?;
            if len < 0 || len > 1000 {
                return Err(Error::Protocol("CreateTopics topic count invalid".into()));
            }
            len as usize
        };

        debug!("Parsing {} topics", topic_count);

        let mut topics = Vec::with_capacity(topic_count);
        for _ in 0..topic_count {
            let topic = TopicParser::parse(decoder, use_compact)?;
            topics.push(topic);
        }

        Ok(topics)
    }

    /// Skip tagged fields at end of request (v5+)
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
    fn test_skip_tagged_fields_empty() {
        let mut buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut buf);

        // Write 0 tagged fields
        encoder.write_unsigned_varint(0);

        let bytes = buf.freeze();
        let mut bytes_clone = bytes.clone();
        let mut decoder = Decoder::new(&mut bytes_clone);

        RequestParser::skip_tagged_fields(&mut decoder).unwrap();
    }

    #[test]
    fn test_skip_tagged_fields_with_data() {
        let mut buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut buf);

        // Write 2 tagged fields
        encoder.write_unsigned_varint(2);
        // Tag 1: id=10, size=5
        encoder.write_unsigned_varint(10);
        encoder.write_unsigned_varint(5);
        encoder.write_raw_bytes(&[1, 2, 3, 4, 5]);
        // Tag 2: id=20, size=3
        encoder.write_unsigned_varint(20);
        encoder.write_unsigned_varint(3);
        encoder.write_raw_bytes(&[6, 7, 8]);

        let bytes = buf.freeze();
        let mut bytes_clone = bytes.clone();
        let mut decoder = Decoder::new(&mut bytes_clone);

        RequestParser::skip_tagged_fields(&mut decoder).unwrap();
    }
}
