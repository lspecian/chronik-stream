//! Metadata request parsing
//!
//! Extracted from `handle_metadata()` to reduce complexity.
//! Handles version-specific parsing for Kafka Metadata API v0-v13.

use bytes::Bytes;
use chronik_common::{Error, Result};
use crate::types::MetadataRequest;
use crate::parser::Decoder;
use crate::RequestHeader;

/// Metadata request parser
///
/// Handles version-specific parsing of Kafka Metadata API requests.
/// Supports both flexible (v9+) and non-flexible protocols.
pub struct MetadataRequestParser;

impl MetadataRequestParser {
    /// Parse metadata request from bytes
    ///
    /// Complexity: < 20 (version-specific parsing with clear branches)
    pub fn parse(header: &RequestHeader, body: &mut Bytes) -> Result<MetadataRequest> {
        tracing::debug!("Parsing metadata request v{}", header.api_version);

        let mut decoder = Decoder::new(body);
        let flexible = header.api_version >= 9;

        tracing::debug!("Parsing metadata request, flexible={}", flexible);

        // Parse topics list (version-specific)
        let topics = Self::parse_topics(&mut decoder, header.api_version, flexible)?;

        // Parse request flags
        let allow_auto_topic_creation = Self::parse_allow_auto_topic_creation(&mut decoder, header.api_version)?;
        let include_cluster_authorized_operations = Self::parse_include_cluster_ops(&mut decoder, header.api_version)?;
        let include_topic_authorized_operations = Self::parse_include_topic_ops(&mut decoder, header.api_version)?;

        // Parse tagged fields (flexible versions only)
        if flexible {
            Self::parse_tagged_fields(&mut decoder)?;
        }

        Ok(MetadataRequest {
            topics,
            allow_auto_topic_creation,
            include_cluster_authorized_operations,
            include_topic_authorized_operations,
        })
    }

    /// Parse topics list with version-specific logic
    ///
    /// Complexity: < 15 (array parsing with UUID support for v10+)
    fn parse_topics(
        decoder: &mut Decoder,
        api_version: i16,
        flexible: bool,
    ) -> Result<Option<Vec<String>>> {
        if api_version < 1 {
            // v0 gets all topics
            return Ok(None);
        }

        // Read topic count (compact array for flexible versions)
        let topic_count = if flexible {
            let count = decoder.read_unsigned_varint()? as i32;
            tracing::debug!("Topic count varint read: {}", count);
            if count == 0 {
                -1 // Null array
            } else {
                count - 1 // Compact arrays use +1 encoding
            }
        } else {
            decoder.read_i32()?
        };

        tracing::debug!("Topic count decoded: {}", topic_count);

        if topic_count < 0 {
            return Ok(None); // All topics
        }

        // Parse individual topics
        let mut topic_names = Vec::with_capacity(topic_count as usize);
        for i in 0..topic_count {
            tracing::trace!("Reading topic {}/{}", i + 1, topic_count);

            // v10+ includes topic_id (UUID) before name
            if api_version >= 10 {
                Self::skip_topic_uuid(decoder)?;
            }

            // Read topic name
            let name = if flexible {
                decoder.read_compact_string()?
            } else {
                decoder.read_string()?
            };

            if let Some(name) = name {
                topic_names.push(name);
            }

            // Skip tagged fields for each topic (flexible versions)
            if flexible {
                let _tagged_count = decoder.read_unsigned_varint()?;
            }
        }

        Ok(Some(topic_names))
    }

    /// Skip topic UUID (16 bytes) for v10+
    ///
    /// Complexity: < 5 (simple loop)
    fn skip_topic_uuid(decoder: &mut Decoder) -> Result<()> {
        let mut topic_id = [0u8; 16];
        for j in 0..16 {
            topic_id[j] = decoder.read_i8()? as u8;
        }
        tracing::trace!("Topic ID: {:02x?}", &topic_id);
        Ok(())
    }

    /// Parse allow_auto_topic_creation flag
    ///
    /// Complexity: < 5 (simple version check)
    fn parse_allow_auto_topic_creation(
        decoder: &mut Decoder,
        api_version: i16,
    ) -> Result<bool> {
        if api_version >= 4 {
            decoder.read_bool()
        } else {
            Ok(true)
        }
    }

    /// Parse include_cluster_authorized_operations flag
    ///
    /// Complexity: < 5 (version range check)
    fn parse_include_cluster_ops(
        decoder: &mut Decoder,
        api_version: i16,
    ) -> Result<bool> {
        // Removed in v11
        if api_version >= 8 && api_version <= 10 {
            decoder.read_bool()
        } else {
            Ok(false)
        }
    }

    /// Parse include_topic_authorized_operations flag
    ///
    /// Complexity: < 5 (simple version check)
    fn parse_include_topic_ops(
        decoder: &mut Decoder,
        api_version: i16,
    ) -> Result<bool> {
        if api_version >= 8 {
            decoder.read_bool()
        } else {
            Ok(false)
        }
    }

    /// Parse tagged fields (flexible versions only)
    ///
    /// Complexity: < 5 (error handling for librdkafka quirk)
    fn parse_tagged_fields(decoder: &mut Decoder) -> Result<()> {
        // librdkafka v2.11.1 quirk: First Metadata request may be missing tagged fields
        if decoder.has_remaining() {
            match decoder.read_unsigned_varint() {
                Ok(_) => {
                    // Tagged fields read and ignored
                }
                Err(e) => {
                    return Err(e);
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::BytesMut;
    use crate::parser::ApiKey;

    #[test]
    fn test_parse_metadata_v0_all_topics() {
        // v0 has no topics field, should return None (all topics)
        let header = RequestHeader {
            api_key: ApiKey::Metadata,
            api_version: 0,
            correlation_id: 1,
            client_id: Some("test".to_string()),
        };

        let mut body = Bytes::new();
        let request = MetadataRequestParser::parse(&header, &mut body).unwrap();

        assert_eq!(request.topics, None);
        assert_eq!(request.allow_auto_topic_creation, true);
    }

    #[test]
    fn test_parse_allow_auto_topic_creation_v4() {
        // v4 added allow_auto_topic_creation field
        let header = RequestHeader {
            api_key: ApiKey::Metadata,
            api_version: 4,
            correlation_id: 1,
            client_id: Some("test".to_string()),
        };

        let mut buf = BytesMut::new();
        // Topics: -1 (all topics)
        buf.extend_from_slice(&(-1i32).to_be_bytes());
        // allow_auto_topic_creation: true
        buf.extend_from_slice(&[1u8]);

        let mut body = buf.freeze();
        let request = MetadataRequestParser::parse(&header, &mut body).unwrap();

        assert_eq!(request.topics, None);
        assert_eq!(request.allow_auto_topic_creation, true);
    }

    #[test]
    fn test_parse_include_cluster_ops_v8() {
        // v8 added include_cluster_authorized_operations
        let header = RequestHeader {
            api_key: ApiKey::Metadata,
            api_version: 8,
            correlation_id: 1,
            client_id: Some("test".to_string()),
        };

        let mut buf = BytesMut::new();
        // Topics: -1 (all topics)
        buf.extend_from_slice(&(-1i32).to_be_bytes());
        // allow_auto_topic_creation: true
        buf.extend_from_slice(&[1u8]);
        // include_cluster_authorized_operations: false
        buf.extend_from_slice(&[0u8]);
        // include_topic_authorized_operations: false
        buf.extend_from_slice(&[0u8]);

        let mut body = buf.freeze();
        let request = MetadataRequestParser::parse(&header, &mut body).unwrap();

        assert_eq!(request.include_cluster_authorized_operations, false);
        assert_eq!(request.include_topic_authorized_operations, false);
    }
}
