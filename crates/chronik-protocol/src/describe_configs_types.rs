//! DescribeConfigs API types

use serde::{Deserialize, Serialize};
use crate::parser::{Decoder, Encoder, KafkaDecodable, KafkaEncodable};
use chronik_common::{Result, Error};

/// Configuration resource to describe
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DescribeConfigsResource {
    /// Resource type (2 = Topic, 4 = Broker)
    pub resource_type: i8,
    /// Resource name
    pub resource_name: String,
    /// Configuration names to describe (null = all)
    pub configuration_names: Option<Vec<String>>,
}

impl KafkaDecodable for DescribeConfigsResource {
    fn decode(decoder: &mut Decoder, _version: i16) -> Result<Self> {
        let resource_type = decoder.read_i8()?;
        let resource_name = decoder.read_string()?
            .ok_or_else(|| Error::Protocol("Resource name cannot be null".into()))?;

        // Read configuration names array (nullable)
        let config_count = decoder.read_i32()?;
        let configuration_names = if config_count < 0 {
            None
        } else {
            let mut names = Vec::with_capacity(config_count as usize);
            for _ in 0..config_count {
                let name = decoder.read_string()?
                    .ok_or_else(|| Error::Protocol("Config name cannot be null".into()))?;
                names.push(name);
            }
            Some(names)
        };

        Ok(DescribeConfigsResource {
            resource_type,
            resource_name,
            configuration_names,
        })
    }
}

impl KafkaEncodable for DescribeConfigsResource {
    fn encode(&self, encoder: &mut Encoder, _version: i16) -> Result<()> {
        encoder.write_i8(self.resource_type);
        encoder.write_string(Some(&self.resource_name));

        // Write configuration names array (nullable)
        if let Some(ref names) = self.configuration_names {
            encoder.write_i32(names.len() as i32);
            for name in names {
                encoder.write_string(Some(name));
            }
        } else {
            encoder.write_i32(-1); // null array
        }

        Ok(())
    }
}

/// DescribeConfigs request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DescribeConfigsRequest {
    /// Resources to describe configurations for
    pub resources: Vec<DescribeConfigsResource>,
    /// Whether to include synonyms
    pub include_synonyms: bool,
}

impl KafkaDecodable for DescribeConfigsRequest {
    fn decode(decoder: &mut Decoder, version: i16) -> Result<Self> {
        // Read resources array
        let resource_count = decoder.read_i32()?;
        if resource_count < 0 {
            return Err(Error::Protocol("Invalid resource count".into()));
        }

        let mut resources = Vec::with_capacity(resource_count as usize);
        for _ in 0..resource_count {
            resources.push(DescribeConfigsResource::decode(decoder, version)?);
        }

        // include_synonyms is only in v1+
        let include_synonyms = if version >= 1 {
            decoder.read_i8()? != 0
        } else {
            false
        };

        Ok(DescribeConfigsRequest {
            resources,
            include_synonyms,
        })
    }
}

impl KafkaEncodable for DescribeConfigsRequest {
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()> {
        // Write resources array
        encoder.write_i32(self.resources.len() as i32);
        for resource in &self.resources {
            resource.encode(encoder, version)?;
        }

        if version >= 1 {
            encoder.write_i8(if self.include_synonyms { 1 } else { 0 });
        }

        Ok(())
    }
}

/// DescribeConfigs response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DescribeConfigsResponse {
    /// Throttle time in milliseconds
    pub throttle_time_ms: i32,
    /// Results for each resource
    pub results: Vec<DescribeConfigsResult>,
}

impl KafkaEncodable for DescribeConfigsResponse {
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()> {
        encoder.write_i32(self.throttle_time_ms);

        // Write results array
        encoder.write_i32(self.results.len() as i32);
        for result in &self.results {
            result.encode(encoder, version)?;
        }

        Ok(())
    }
}

/// Result for a single resource
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DescribeConfigsResult {
    /// Error code
    pub error_code: i16,
    /// Error message
    pub error_message: Option<String>,
    /// Resource type
    pub resource_type: i8,
    /// Resource name
    pub resource_name: String,
    /// Configuration entries
    pub config_entries: Vec<ConfigEntry>,
}

impl KafkaEncodable for DescribeConfigsResult {
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()> {
        encoder.write_i16(self.error_code);
        encoder.write_string(self.error_message.as_deref());
        encoder.write_i8(self.resource_type);
        encoder.write_string(Some(&self.resource_name));

        // Write config entries array
        encoder.write_i32(self.config_entries.len() as i32);
        for entry in &self.config_entries {
            entry.encode(encoder, version)?;
        }

        Ok(())
    }
}

/// Configuration entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigEntry {
    /// Configuration name
    pub config_name: String,
    /// Configuration value
    pub config_value: Option<String>,
    /// Whether this configuration is read-only
    pub read_only: bool,
    /// Whether this configuration is a default
    pub is_default: bool,
    /// Whether this configuration is sensitive
    pub is_sensitive: bool,
}

impl KafkaEncodable for ConfigEntry {
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()> {
        encoder.write_string(Some(&self.config_name));
        encoder.write_string(self.config_value.as_deref());
        encoder.write_i8(if self.read_only { 1 } else { 0 });

        // is_default is only in v1+
        if version >= 1 {
            encoder.write_i8(if self.is_default { 1 } else { 0 });
        }

        encoder.write_i8(if self.is_sensitive { 1 } else { 0 });

        // Skip synonyms for now (v1+)
        if version >= 1 {
            encoder.write_i32(0); // empty synonyms array
        }

        Ok(())
    }
}

/// Error codes for DescribeConfigs
pub mod error_codes {
    pub const NONE: i16 = 0;
    pub const INVALID_REQUEST: i16 = 42;
    pub const CONFIG_NOT_FOUND: i16 = 76;
    pub const CLUSTER_AUTHORIZATION_FAILED: i16 = 31;
    pub const TOPIC_AUTHORIZATION_FAILED: i16 = 29;
    pub const UNKNOWN_TOPIC_OR_PARTITION: i16 = 3;
    pub const INVALID_CONFIG: i16 = 40;
}