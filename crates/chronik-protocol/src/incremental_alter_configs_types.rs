use chronik_common::{Result, Error};
use crate::parser::{Decoder, Encoder, KafkaDecodable, KafkaEncodable};
use bytes::Bytes;

#[derive(Debug, Clone)]
pub struct IncrementalAlterConfigsRequest {
    pub resources: Vec<AlterableConfigResource>,
    pub validate_only: bool,
}

#[derive(Debug, Clone)]
pub struct AlterableConfigResource {
    pub resource_type: i8,
    pub resource_name: String,
    pub configs: Vec<AlterableConfig>,
}

#[derive(Debug, Clone)]
pub struct AlterableConfig {
    pub name: String,
    pub config_operation: i8,
    pub value: Option<String>,
}

#[derive(Debug, Clone)]
pub struct IncrementalAlterConfigsResponse {
    pub throttle_time_ms: i32,
    pub resources: Vec<AlterConfigsResourceResponse>,
}

#[derive(Debug, Clone)]
pub struct AlterConfigsResourceResponse {
    pub error_code: i16,
    pub error_message: Option<String>,
    pub resource_type: i8,
    pub resource_name: String,
}

impl KafkaDecodable for IncrementalAlterConfigsRequest {
    fn decode(decoder: &mut Decoder, api_version: i16) -> Result<Self> {
        // For v0, use regular arrays and strings
        // For v1+, use compact arrays and strings (simplified for stub)

        // Read resources array
        let resource_count = decoder.read_i32()?;
        if resource_count < 0 {
            return Err(Error::Protocol("Invalid resource count".into()));
        }

        let mut resources = Vec::with_capacity(resource_count as usize);
        for _ in 0..resource_count {
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
                let name = decoder.read_string()?
                    .ok_or_else(|| Error::Protocol("Config name cannot be null".into()))?;
                let config_operation = decoder.read_i8()?;
                let value = decoder.read_string()?;

                configs.push(AlterableConfig {
                    name,
                    config_operation,
                    value,
                });
            }

            resources.push(AlterableConfigResource {
                resource_type,
                resource_name,
                configs,
            });
        }

        let validate_only = decoder.read_i8()? != 0;

        Ok(IncrementalAlterConfigsRequest {
            resources,
            validate_only,
        })
    }
}

impl KafkaEncodable for IncrementalAlterConfigsResponse {
    fn encode(&self, encoder: &mut Encoder, _api_version: i16) -> Result<()> {
        encoder.write_i32(self.throttle_time_ms);

        // Write resources array
        encoder.write_i32(self.resources.len() as i32);
        for resource in &self.resources {
            encoder.write_i16(resource.error_code);
            encoder.write_string(resource.error_message.as_ref().map(|s| s.as_str()));
            encoder.write_i8(resource.resource_type);
            encoder.write_string(Some(&resource.resource_name));
        }

        Ok(())
    }
}

pub mod error_codes {
    pub const NONE: i16 = 0;
    pub const UNKNOWN_TOPIC_OR_PARTITION: i16 = 3;
    pub const UNSUPPORTED_VERSION: i16 = 35;
    pub const INVALID_REQUEST: i16 = 42;
    pub const UNKNOWN_SERVER_ERROR: i16 = -1;
}

pub mod config_operation {
    pub const SET: i8 = 0;
    pub const DELETE: i8 = 1;
    pub const APPEND: i8 = 2;
    pub const SUBTRACT: i8 = 3;
}

pub mod resource_type {
    pub const UNKNOWN: i8 = 0;
    pub const ANY: i8 = 1;
    pub const TOPIC: i8 = 2;
    pub const GROUP: i8 = 3;
    pub const CLUSTER: i8 = 4;
    pub const BROKER: i8 = 5;
    pub const USER: i8 = 6;
    pub const CLIENT: i8 = 7;
    pub const BROKER_LOGGER: i8 = 8;
}