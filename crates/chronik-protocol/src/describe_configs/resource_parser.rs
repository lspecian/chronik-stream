//! Resource Parsing for DescribeConfigs
//!
//! Handles parsing of individual resource entries including type, name,
//! configuration keys, and tagged fields.
//! Complexity: < 25 per function

use crate::parser::Decoder;
use crate::types::ConfigResource;
use chronik_common::Result;
use tracing::debug;

/// Parser for individual ConfigResource
pub struct ResourceParser;

impl ResourceParser {
    /// Parse a single resource
    ///
    /// Complexity: < 20 (orchestrates resource field parsing)
    pub fn parse(
        decoder: &mut Decoder,
        use_compact: bool,
        api_version: i16,
        index: usize,
    ) -> Result<ConfigResource> {
        // Phase 1: Parse resource type
        let resource_type = decoder.read_i8()?;
        debug!("Resource {}: type = {}", index, resource_type);

        // Phase 2: Parse resource name
        let resource_name = Self::parse_resource_name(decoder, use_compact)?;
        debug!("Resource {}: name = '{}'", index, resource_name);

        // Phase 3: Parse configuration keys (v1+)
        let configuration_keys = Self::parse_configuration_keys(decoder, use_compact, api_version)?;

        // Phase 4: Skip per-resource tagged fields (v4+)
        if use_compact {
            Self::skip_tagged_fields(decoder)?;
        }

        Ok(ConfigResource {
            resource_type,
            resource_name,
            configuration_keys,
        })
    }

    /// Parse resource name string
    ///
    /// Complexity: < 5
    fn parse_resource_name(decoder: &mut Decoder, use_compact: bool) -> Result<String> {
        let name = if use_compact {
            decoder.read_compact_string()?.unwrap_or_else(|| String::new())
        } else {
            decoder.read_string()?.unwrap_or_else(|| String::new())
        };
        Ok(name)
    }

    /// Parse configuration keys array (v1+, nullable)
    ///
    /// Complexity: < 20 (array loop with compact/non-compact handling)
    fn parse_configuration_keys(
        decoder: &mut Decoder,
        use_compact: bool,
        api_version: i16,
    ) -> Result<Option<Vec<String>>> {
        if api_version < 1 {
            return Ok(None);
        }

        if use_compact {
            let compact_len = decoder.read_unsigned_varint()?;
            if compact_len == 0 {
                Ok(None)
            } else {
                let key_count = (compact_len - 1) as usize;
                let mut keys = Vec::with_capacity(key_count);
                for _ in 0..key_count {
                    if let Some(key) = decoder.read_compact_string()? {
                        keys.push(key);
                    }
                }
                Ok(Some(keys))
            }
        } else {
            let key_count = decoder.read_i32()?;
            if key_count < 0 {
                Ok(None)
            } else {
                let mut keys = Vec::with_capacity(key_count as usize);
                for _ in 0..key_count {
                    if let Some(key) = decoder.read_string()? {
                        keys.push(key);
                    }
                }
                Ok(Some(keys))
            }
        }
    }

    /// Skip per-resource tagged fields (v4+)
    ///
    /// Complexity: < 10
    fn skip_tagged_fields(decoder: &mut Decoder) -> Result<()> {
        let tag_count = decoder.read_unsigned_varint()?;
        for _ in 0..tag_count {
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
    fn test_parse_resource_name_non_compact() {
        let mut buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut buf);

        encoder.write_string(Some("my-topic"));

        let bytes = buf.freeze();
        let mut bytes_clone = bytes.clone();
        let mut decoder = Decoder::new(&mut bytes_clone);

        let name = ResourceParser::parse_resource_name(&mut decoder, false).unwrap();
        assert_eq!(name, "my-topic");
    }

    #[test]
    fn test_parse_resource_name_compact() {
        let mut buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut buf);

        encoder.write_compact_string(Some("my-topic"));

        let bytes = buf.freeze();
        let mut bytes_clone = bytes.clone();
        let mut decoder = Decoder::new(&mut bytes_clone);

        let name = ResourceParser::parse_resource_name(&mut decoder, true).unwrap();
        assert_eq!(name, "my-topic");
    }

    #[test]
    fn test_parse_configuration_keys_v0() {
        let buf = BytesMut::new();
        let bytes = buf.freeze();
        let mut bytes_clone = bytes.clone();
        let mut decoder = Decoder::new(&mut bytes_clone);

        // v0 has no configuration keys
        let keys = ResourceParser::parse_configuration_keys(&mut decoder, false, 0).unwrap();
        assert_eq!(keys, None);
    }

    #[test]
    fn test_parse_configuration_keys_null() {
        let mut buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut buf);

        // Null array
        encoder.write_i32(-1);

        let bytes = buf.freeze();
        let mut bytes_clone = bytes.clone();
        let mut decoder = Decoder::new(&mut bytes_clone);

        let keys = ResourceParser::parse_configuration_keys(&mut decoder, false, 1).unwrap();
        assert_eq!(keys, None);
    }

    #[test]
    fn test_parse_configuration_keys_compact_null() {
        let mut buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut buf);

        // Compact null array (varint 0)
        encoder.write_unsigned_varint(0);

        let bytes = buf.freeze();
        let mut bytes_clone = bytes.clone();
        let mut decoder = Decoder::new(&mut bytes_clone);

        let keys = ResourceParser::parse_configuration_keys(&mut decoder, true, 4).unwrap();
        assert_eq!(keys, None);
    }
}
