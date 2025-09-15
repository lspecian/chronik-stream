//! AlterConfigs API types

use serde::{Deserialize, Serialize};
use crate::parser::{Decoder, Encoder, KafkaDecodable, KafkaEncodable};
use chronik_common::{Result, Error};

/// Configuration entry to alter
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlterConfigsResource {
    /// Resource type (2 = Topic, 4 = Broker)
    pub resource_type: i8,
    /// Resource name
    pub resource_name: String,
    /// Configurations to alter
    pub configs: Vec<AlterableConfig>,
}

impl KafkaDecodable for AlterConfigsResource {
    fn decode(decoder: &mut Decoder, _version: i16) -> Result<Self> {
        let resource_type = decoder.read_i8()?;
        let resource_name = decoder.read_string()?
            .ok_or_else(|| Error::Protocol("Resource name cannot be null".into()))?;

        // Read configs array
        let config_count = decoder.read_i32()?;
        if config_count < 0 {
            return Err(Error::Protocol("Invalid config count".into()));
        }

        let mut configs = Vec::with_capacity(config_count as usize);
        for _ in 0..config_count {
            configs.push(AlterableConfig::decode(decoder, _version)?);
        }

        Ok(AlterConfigsResource {
            resource_type,
            resource_name,
            configs,
        })
    }
}

impl KafkaEncodable for AlterConfigsResource {
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()> {
        encoder.write_i8(self.resource_type);
        encoder.write_string(Some(&self.resource_name));

        // Write configs array
        encoder.write_i32(self.configs.len() as i32);
        for config in &self.configs {
            config.encode(encoder, version)?;
        }

        Ok(())
    }
}

/// Configuration to alter
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlterableConfig {
    /// Configuration key
    pub name: String,
    /// Configuration value
    pub value: Option<String>,
}

impl KafkaDecodable for AlterableConfig {
    fn decode(decoder: &mut Decoder, _version: i16) -> Result<Self> {
        let name = decoder.read_string()?
            .ok_or_else(|| Error::Protocol("Config name cannot be null".into()))?;
        let value = decoder.read_string()?;

        Ok(AlterableConfig {
            name,
            value,
        })
    }
}

impl KafkaEncodable for AlterableConfig {
    fn encode(&self, encoder: &mut Encoder, _version: i16) -> Result<()> {
        encoder.write_string(Some(&self.name));
        encoder.write_string(self.value.as_deref());
        Ok(())
    }
}

/// AlterConfigs request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlterConfigsRequest {
    /// Resources to alter configurations for
    pub resources: Vec<AlterConfigsResource>,
    /// Whether to validate only without altering
    pub validate_only: bool,
}

impl KafkaDecodable for AlterConfigsRequest {
    fn decode(decoder: &mut Decoder, version: i16) -> Result<Self> {
        // Read resources array
        let resource_count = decoder.read_i32()?;
        if resource_count < 0 {
            return Err(Error::Protocol("Invalid resource count".into()));
        }

        let mut resources = Vec::with_capacity(resource_count as usize);
        for _ in 0..resource_count {
            resources.push(AlterConfigsResource::decode(decoder, version)?);
        }

        let validate_only = decoder.read_i8()? != 0;

        Ok(AlterConfigsRequest {
            resources,
            validate_only,
        })
    }
}

impl KafkaEncodable for AlterConfigsRequest {
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()> {
        // Write resources array
        encoder.write_i32(self.resources.len() as i32);
        for resource in &self.resources {
            resource.encode(encoder, version)?;
        }

        encoder.write_i8(if self.validate_only { 1 } else { 0 });
        Ok(())
    }
}

/// AlterConfigs response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlterConfigsResponse {
    /// Throttle time in milliseconds
    pub throttle_time_ms: i32,
    /// Results for each resource
    pub resources: Vec<AlterConfigsResourceResponse>,
}

impl KafkaEncodable for AlterConfigsResponse {
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()> {
        encoder.write_i32(self.throttle_time_ms);

        // Write resources array
        encoder.write_i32(self.resources.len() as i32);
        for resource in &self.resources {
            resource.encode(encoder, version)?;
        }

        Ok(())
    }
}

/// Response for a single resource
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlterConfigsResourceResponse {
    /// Error code
    pub error_code: i16,
    /// Error message
    pub error_message: Option<String>,
    /// Resource type
    pub resource_type: i8,
    /// Resource name
    pub resource_name: String,
}

impl KafkaEncodable for AlterConfigsResourceResponse {
    fn encode(&self, encoder: &mut Encoder, _version: i16) -> Result<()> {
        encoder.write_i16(self.error_code);
        encoder.write_string(self.error_message.as_deref());
        encoder.write_i8(self.resource_type);
        encoder.write_string(Some(&self.resource_name));
        Ok(())
    }
}

/// Error codes for AlterConfigs
pub mod error_codes {
    pub const NONE: i16 = 0;
    pub const INVALID_REQUEST: i16 = 42;
    pub const INVALID_CONFIG: i16 = 40;
    pub const CONFIG_NOT_FOUND: i16 = 76;
    pub const CLUSTER_AUTHORIZATION_FAILED: i16 = 31;
    pub const TOPIC_AUTHORIZATION_FAILED: i16 = 29;
    pub const UNKNOWN_TOPIC_OR_PARTITION: i16 = 3;
}