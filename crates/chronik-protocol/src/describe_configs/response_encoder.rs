//! DescribeConfigs Response Encoding
//!
//! Handles encoding of DescribeConfigs response including results array,
//! configs, synonyms, and version-specific fields.
//! Complexity: < 25 per function

use crate::parser::Encoder;
use crate::types::{ConfigEntry, DescribeConfigsResult};
use chronik_common::Result;
use bytes::BytesMut;

/// Encoder for DescribeConfigs response
pub struct ResponseEncoder;

impl ResponseEncoder {
    /// Encode complete DescribeConfigs response
    ///
    /// Complexity: < 15 (orchestrates encoding phases)
    pub fn encode(
        encoder: &mut Encoder,
        throttle_time_ms: i32,
        results: Vec<DescribeConfigsResult>,
        api_version: i16,
    ) -> Result<()> {
        let use_compact = api_version >= 4;

        // Phase 1: Encode throttle time
        Self::encode_throttle_time(encoder, throttle_time_ms)?;

        // Phase 2: Encode results array
        Self::encode_results_array(encoder, results, api_version, use_compact)?;

        // Phase 3: Encode response-level tagged fields (v4+)
        if use_compact {
            Self::encode_tagged_fields(encoder)?;
        }

        Ok(())
    }

    /// Encode throttle time
    ///
    /// Complexity: < 5
    fn encode_throttle_time(encoder: &mut Encoder, throttle_time_ms: i32) -> Result<()> {
        encoder.write_i32(throttle_time_ms);
        Ok(())
    }

    /// Encode results array
    ///
    /// Complexity: < 15 (array header + loop delegation)
    fn encode_results_array(
        encoder: &mut Encoder,
        results: Vec<DescribeConfigsResult>,
        api_version: i16,
        use_compact: bool,
    ) -> Result<()> {
        // Write array length
        if use_compact {
            encoder.write_unsigned_varint((results.len() + 1) as u32);
        } else {
            encoder.write_i32(results.len() as i32);
        }

        // Write each result
        for result in results {
            Self::encode_result(encoder, result, api_version, use_compact)?;
        }

        Ok(())
    }

    /// Encode single result
    ///
    /// Complexity: < 20 (result fields + configs array)
    fn encode_result(
        encoder: &mut Encoder,
        result: DescribeConfigsResult,
        api_version: i16,
        use_compact: bool,
    ) -> Result<()> {
        // Phase 1: Encode error fields
        encoder.write_i16(result.error_code);

        if use_compact {
            encoder.write_compact_string(result.error_message.as_deref());
        } else {
            encoder.write_string(result.error_message.as_deref());
        }

        // Phase 2: Encode resource fields
        encoder.write_i8(result.resource_type);

        if use_compact {
            encoder.write_compact_string(Some(&result.resource_name));
        } else {
            encoder.write_string(Some(&result.resource_name));
        }

        // Phase 3: Encode configs array
        Self::encode_configs_array(encoder, result.configs, api_version, use_compact)?;

        // Phase 4: Encode result-level tagged fields (v4+)
        if use_compact {
            Self::encode_tagged_fields(encoder)?;
        }

        Ok(())
    }

    /// Encode configs array
    ///
    /// Complexity: < 15 (array header + loop delegation)
    fn encode_configs_array(
        encoder: &mut Encoder,
        configs: Vec<ConfigEntry>,
        api_version: i16,
        use_compact: bool,
    ) -> Result<()> {
        // Write array length
        if use_compact {
            encoder.write_unsigned_varint((configs.len() + 1) as u32);
        } else {
            encoder.write_i32(configs.len() as i32);
        }

        // Write each config
        for config in configs {
            Self::encode_config(encoder, config, api_version, use_compact)?;
        }

        Ok(())
    }

    /// Encode single config entry
    ///
    /// Complexity: < 25 (config fields + synonyms)
    fn encode_config(
        encoder: &mut Encoder,
        config: ConfigEntry,
        api_version: i16,
        use_compact: bool,
    ) -> Result<()> {
        // Phase 1: Encode name and value
        if use_compact {
            encoder.write_compact_string(Some(&config.name));
            encoder.write_compact_string(config.value.as_deref());
        } else {
            encoder.write_string(Some(&config.name));
            encoder.write_string(config.value.as_deref());
        }

        // Phase 2: Encode read_only
        encoder.write_bool(config.read_only);

        // Phase 3: Encode config_source or is_default (v1+)
        if api_version >= 1 {
            encoder.write_i8(config.config_source);
        } else {
            encoder.write_bool(config.is_default);
        }

        // Phase 4: Encode is_sensitive
        encoder.write_bool(config.is_sensitive);

        // Phase 5: Encode synonyms (v1+)
        if api_version >= 1 {
            Self::encode_synonyms(encoder, config.synonyms, use_compact)?;
        }

        // Phase 6: Encode config_type and documentation (v3+)
        if api_version >= 3 {
            // Config type
            match config.config_type {
                Some(config_type) => encoder.write_i8(config_type),
                None => encoder.write_i8(0), // UNKNOWN type
            }

            // Documentation
            if use_compact {
                encoder.write_compact_string(config.documentation.as_deref());
            } else {
                encoder.write_string(config.documentation.as_deref());
            }
        }

        // Phase 7: Encode config-level tagged fields (v4+)
        if use_compact {
            Self::encode_tagged_fields(encoder)?;
        }

        Ok(())
    }

    /// Encode synonyms array
    ///
    /// Complexity: < 20 (array header + loop with synonym fields)
    fn encode_synonyms(
        encoder: &mut Encoder,
        synonyms: Vec<crate::types::ConfigSynonym>,
        use_compact: bool,
    ) -> Result<()> {
        // Write array length
        if use_compact {
            encoder.write_unsigned_varint((synonyms.len() + 1) as u32);
        } else {
            encoder.write_i32(synonyms.len() as i32);
        }

        // Write each synonym
        for synonym in synonyms {
            // Name
            if use_compact {
                encoder.write_compact_string(Some(&synonym.name));
            } else {
                encoder.write_string(Some(&synonym.name));
            }

            // Value
            if use_compact {
                encoder.write_compact_string(synonym.value.as_deref());
            } else {
                encoder.write_string(synonym.value.as_deref());
            }

            // Source
            encoder.write_i8(synonym.source);

            // Synonym-level tagged fields (v4+)
            if use_compact {
                Self::encode_tagged_fields(encoder)?;
            }
        }

        Ok(())
    }

    /// Encode empty tagged fields
    ///
    /// Complexity: < 5
    fn encode_tagged_fields(encoder: &mut Encoder) -> Result<()> {
        encoder.write_unsigned_varint(0); // No tagged fields
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ConfigSynonym;
    use crate::describe_configs_types::error_codes;

    #[test]
    fn test_encode_throttle_time() {
        let mut buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut buf);

        ResponseEncoder::encode_throttle_time(&mut encoder, 100).unwrap();

        let bytes = buf.freeze();
        assert_eq!(bytes.len(), 4);
    }

    #[test]
    fn test_encode_results_array_non_compact() {
        let mut buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut buf);

        let results = vec![
            DescribeConfigsResult {
                error_code: error_codes::NONE,
                error_message: None,
                resource_type: 2, // TOPIC
                resource_name: "test-topic".to_string(),
                configs: vec![],
            },
        ];

        ResponseEncoder::encode_results_array(&mut encoder, results, 0, false).unwrap();

        let bytes = buf.freeze();
        // Should start with array length (4 bytes)
        assert!(bytes.len() >= 4);
    }

    #[test]
    fn test_encode_results_array_compact() {
        let mut buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut buf);

        let results = vec![
            DescribeConfigsResult {
                error_code: error_codes::NONE,
                error_message: None,
                resource_type: 2, // TOPIC
                resource_name: "test-topic".to_string(),
                configs: vec![],
            },
        ];

        ResponseEncoder::encode_results_array(&mut encoder, results, 4, true).unwrap();

        let bytes = buf.freeze();
        // Compact array starts with varint
        assert!(bytes.len() >= 1);
    }

    #[test]
    fn test_encode_config_v0() {
        let mut buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut buf);

        let config = ConfigEntry {
            name: "retention.ms".to_string(),
            value: Some("604800000".to_string()),
            read_only: false,
            is_default: true,
            config_source: 5,
            is_sensitive: false,
            synonyms: vec![],
            config_type: None,
            documentation: None,
        };

        ResponseEncoder::encode_config(&mut encoder, config, 0, false).unwrap();

        let bytes = buf.freeze();
        // Should have name, value, read_only, is_default, is_sensitive
        assert!(bytes.len() > 10);
    }

    #[test]
    fn test_encode_config_v3_with_documentation() {
        let mut buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut buf);

        let config = ConfigEntry {
            name: "retention.ms".to_string(),
            value: Some("604800000".to_string()),
            read_only: false,
            is_default: true,
            config_source: 5,
            is_sensitive: false,
            synonyms: vec![],
            config_type: Some(3), // LONG
            documentation: Some("Retention period".to_string()),
        };

        ResponseEncoder::encode_config(&mut encoder, config, 3, false).unwrap();

        let bytes = buf.freeze();
        // Should include config_type and documentation
        assert!(bytes.len() > 20);
    }

    #[test]
    fn test_encode_synonyms() {
        let mut buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut buf);

        let synonyms = vec![
            ConfigSynonym {
                name: "retention.ms".to_string(),
                value: Some("604800000".to_string()),
                source: 5, // DEFAULT_CONFIG
            },
        ];

        ResponseEncoder::encode_synonyms(&mut encoder, synonyms, false).unwrap();

        let bytes = buf.freeze();
        // Should have array length + synonym fields
        assert!(bytes.len() > 10);
    }

    #[test]
    fn test_encode_tagged_fields() {
        let mut buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut buf);

        ResponseEncoder::encode_tagged_fields(&mut encoder).unwrap();

        let bytes = buf.freeze();
        // Should write varint 0
        assert_eq!(bytes.len(), 1);
        assert_eq!(bytes[0], 0);
    }
}
