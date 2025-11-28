//! DescribeConfigs Request Parsing
//!
//! Handles parsing of DescribeConfigs request body including resources array,
//! include_synonyms, and include_documentation flags.
//! Complexity: < 20 per function

use crate::parser::Decoder;
use crate::types::ConfigResource;
use chronik_common::Result;
use tracing::debug;

/// DescribeConfigs request structure
pub struct DescribeConfigsRequest {
    pub resources: Vec<ConfigResource>,
    pub include_synonyms: bool,
    pub include_documentation: bool,
}

/// Parser for DescribeConfigs request
pub struct RequestParser;

impl RequestParser {
    /// Parse complete DescribeConfigs request
    ///
    /// Complexity: < 20 (orchestrates parsing of all fields)
    pub fn parse(
        decoder: &mut Decoder,
        api_version: i16,
    ) -> Result<DescribeConfigsRequest> {
        let use_compact = api_version >= 4;

        // Phase 1: Parse resources array
        let resources = Self::parse_resources_array(decoder, use_compact, api_version)?;

        // Phase 2: Parse include flags
        let include_synonyms = Self::parse_include_synonyms(decoder, api_version)?;
        let include_documentation = Self::parse_include_documentation(decoder, api_version)?;

        debug!(
            "DescribeConfigs v{}: include_synonyms={}, include_documentation={}",
            api_version, include_synonyms, include_documentation
        );

        // Phase 3: Parse request-level tagged fields (v4+)
        if use_compact {
            Self::skip_tagged_fields(decoder, api_version)?;
        }

        Ok(DescribeConfigsRequest {
            resources,
            include_synonyms,
            include_documentation,
        })
    }

    /// Parse resources array
    ///
    /// Complexity: < 15 (array loop with delegation to resource_parser)
    fn parse_resources_array(
        decoder: &mut Decoder,
        use_compact: bool,
        api_version: i16,
    ) -> Result<Vec<ConfigResource>> {
        use super::ResourceParser;

        // Parse array length
        let resource_count = if use_compact {
            let compact_len = decoder.read_unsigned_varint()?;
            if compact_len == 0 {
                0  // NULL array
            } else {
                (compact_len - 1) as usize
            }
        } else {
            decoder.read_i32()? as usize
        };

        debug!("Resource count: {}", resource_count);

        let mut resources = Vec::with_capacity(resource_count);

        for i in 0..resource_count {
            let resource = ResourceParser::parse(decoder, use_compact, api_version, i)?;
            resources.push(resource);
        }

        Ok(resources)
    }

    /// Parse include_synonyms flag (v1+)
    ///
    /// Complexity: < 5
    fn parse_include_synonyms(decoder: &mut Decoder, api_version: i16) -> Result<bool> {
        if api_version >= 1 {
            decoder.read_bool()
        } else {
            Ok(false)
        }
    }

    /// Parse include_documentation flag (v3+)
    ///
    /// Complexity: < 5
    fn parse_include_documentation(decoder: &mut Decoder, api_version: i16) -> Result<bool> {
        if api_version >= 3 {
            decoder.read_bool()
        } else {
            Ok(false)
        }
    }

    /// Skip request-level tagged fields (v4+)
    ///
    /// Complexity: < 10
    fn skip_tagged_fields(decoder: &mut Decoder, api_version: i16) -> Result<()> {
        let tag_count = decoder.read_unsigned_varint()?;
        debug!(
            "DescribeConfigs v{}: parsing {} request-level tagged fields",
            api_version, tag_count
        );

        for _ in 0..tag_count {
            let tag_id = decoder.read_unsigned_varint()?;
            let tag_size = decoder.read_unsigned_varint()? as usize;
            debug!(
                "DescribeConfigs v{}: skipping tagged field {} ({} bytes)",
                api_version, tag_id, tag_size
            );
            decoder.skip(tag_size)?;
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
    fn test_parse_include_flags_v0() {
        let buf = BytesMut::new();
        let bytes = buf.freeze();
        let mut bytes_clone = bytes.clone();
        let mut decoder = Decoder::new(&mut bytes_clone);

        // v0 has no include flags
        assert_eq!(RequestParser::parse_include_synonyms(&mut decoder, 0).unwrap(), false);
        assert_eq!(RequestParser::parse_include_documentation(&mut decoder, 0).unwrap(), false);
    }

    #[test]
    fn test_parse_include_flags_v1() {
        let mut buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut buf);

        // v1 has include_synonyms
        encoder.write_bool(true);

        let bytes = buf.freeze();
        let mut bytes_clone = bytes.clone();
        let mut decoder = Decoder::new(&mut bytes_clone);

        assert_eq!(RequestParser::parse_include_synonyms(&mut decoder, 1).unwrap(), true);
        assert_eq!(RequestParser::parse_include_documentation(&mut decoder, 1).unwrap(), false);
    }

    #[test]
    fn test_parse_include_flags_v3() {
        let mut buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut buf);

        // v3 has both flags
        encoder.write_bool(true);  // include_synonyms
        encoder.write_bool(false); // include_documentation

        let bytes = buf.freeze();
        let mut bytes_clone = bytes.clone();
        let mut decoder = Decoder::new(&mut bytes_clone);

        assert_eq!(RequestParser::parse_include_synonyms(&mut decoder, 3).unwrap(), true);
        assert_eq!(RequestParser::parse_include_documentation(&mut decoder, 3).unwrap(), false);
    }

    #[test]
    fn test_skip_tagged_fields_empty() {
        let mut buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut buf);

        // Write 0 tagged fields
        encoder.write_unsigned_varint(0);

        let bytes = buf.freeze();
        let mut bytes_clone = bytes.clone();
        let mut decoder = Decoder::new(&mut bytes_clone);

        RequestParser::skip_tagged_fields(&mut decoder, 4).unwrap();
    }
}
