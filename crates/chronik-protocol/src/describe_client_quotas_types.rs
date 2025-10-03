// DescribeClientQuotas API types for Kafka protocol
// API key 48, version 0-1

use bytes::{Buf, BufMut, BytesMut};
use chronik_common::Result;
use serde::{Deserialize, Serialize};

// DescribeClientQuotas Request Types

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DescribeClientQuotasRequest {
    pub components: Vec<ComponentFilter>,
    pub strict: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComponentFilter {
    pub entity_type: String,
    pub match_type: i8,  // 0=EXACT, 1=DEFAULT, 2=ANY
    pub match_value: Option<String>,  // null for DEFAULT or ANY
}

// DescribeClientQuotas Response Types

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DescribeClientQuotasResponse {
    pub throttle_time_ms: i32,
    pub error_code: i16,
    pub error_message: Option<String>,
    pub entries: Vec<QuotaEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotaEntry {
    pub entity: Vec<EntityData>,
    pub values: Vec<QuotaValue>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityData {
    pub entity_type: String,
    pub entity_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotaValue {
    pub key: String,
    pub value: f64,
}

// Match types
pub mod match_type {
    pub const EXACT: i8 = 0;
    pub const DEFAULT: i8 = 1;
    pub const ANY: i8 = 2;
}

// Common quota keys
pub mod quota_keys {
    pub const PRODUCER_BYTE_RATE: &str = "producer_byte_rate";
    pub const CONSUMER_BYTE_RATE: &str = "consumer_byte_rate";
    pub const REQUEST_PERCENTAGE: &str = "request_percentage";
    pub const CONTROLLER_MUTATION_RATE: &str = "controller_mutation_rate";
}

// Decoder implementation

impl DescribeClientQuotasRequest {
    pub fn decode(decoder: &mut crate::parser::Decoder, version: i16) -> Result<Self> {
        // Components array
        let components = if version >= 1 {
            // Compact array in v1+
            let count = decoder.read_unsigned_varint()?;
            let mut components = Vec::with_capacity((count - 1) as usize);
            for _ in 0..(count - 1) {
                let entity_type = decoder.read_compact_string()?.unwrap_or_default();
                let match_type = decoder.read_i8()?;
                let match_value = decoder.read_compact_nullable_string()?;

                components.push(ComponentFilter {
                    entity_type,
                    match_type,
                    match_value,
                });

                // Tagged fields
                let _tag_count = decoder.read_unsigned_varint()?;
            }
            components
        } else {
            // Regular array in v0
            let count = decoder.read_i32()?;
            let mut components = Vec::with_capacity(count as usize);
            for _ in 0..count {
                let entity_type = decoder.read_string()?.unwrap_or_default();
                let match_type = decoder.read_i8()?;
                let match_value = decoder.read_nullable_string()?;

                components.push(ComponentFilter {
                    entity_type,
                    match_type,
                    match_value,
                });
            }
            components
        };

        // Strict flag
        let strict = decoder.read_bool()?;

        // Tagged fields in v1+
        if version >= 1 {
            let _tag_count = decoder.read_unsigned_varint()?;
        }

        Ok(DescribeClientQuotasRequest {
            components,
            strict,
        })
    }
}

// Encoder implementation

impl DescribeClientQuotasResponse {
    pub fn encode_to_bytes(&self, version: i16) -> Result<BytesMut> {
        let mut buffer = BytesMut::new();

        // Throttle time
        buffer.put_i32(self.throttle_time_ms);

        // Error code
        buffer.put_i16(self.error_code);

        // Error message
        if version >= 1 {
            // Compact nullable string
            if let Some(ref msg) = self.error_message {
                let bytes = msg.as_bytes();
                buffer.put_u8((bytes.len() + 1) as u8);
                buffer.put_slice(bytes);
            } else {
                buffer.put_u8(0);  // null
            }
        } else {
            // Regular nullable string
            if let Some(ref msg) = self.error_message {
                let bytes = msg.as_bytes();
                buffer.put_i16(bytes.len() as i16);
                buffer.put_slice(bytes);
            } else {
                buffer.put_i16(-1);  // null
            }
        }

        // Entries array
        if version >= 1 {
            // Compact array
            buffer.put_u8((self.entries.len() + 1) as u8);
        } else {
            buffer.put_i32(self.entries.len() as i32);
        }

        for entry in &self.entries {
            // Entity array
            if version >= 1 {
                buffer.put_u8((entry.entity.len() + 1) as u8);
            } else {
                buffer.put_i32(entry.entity.len() as i32);
            }

            for entity_data in &entry.entity {
                // Entity type
                if version >= 1 {
                    let bytes = entity_data.entity_type.as_bytes();
                    buffer.put_u8((bytes.len() + 1) as u8);
                    buffer.put_slice(bytes);
                } else {
                    let bytes = entity_data.entity_type.as_bytes();
                    buffer.put_i16(bytes.len() as i16);
                    buffer.put_slice(bytes);
                }

                // Entity name (nullable)
                if version >= 1 {
                    if let Some(ref name) = entity_data.entity_name {
                        let bytes = name.as_bytes();
                        buffer.put_u8((bytes.len() + 1) as u8);
                        buffer.put_slice(bytes);
                    } else {
                        buffer.put_u8(0);
                    }
                } else {
                    if let Some(ref name) = entity_data.entity_name {
                        let bytes = name.as_bytes();
                        buffer.put_i16(bytes.len() as i16);
                        buffer.put_slice(bytes);
                    } else {
                        buffer.put_i16(-1);
                    }
                }

                // Tagged fields in v1+
                if version >= 1 {
                    buffer.put_u8(0);  // No tagged fields
                }
            }

            // Values array
            if version >= 1 {
                buffer.put_u8((entry.values.len() + 1) as u8);
            } else {
                buffer.put_i32(entry.values.len() as i32);
            }

            for value in &entry.values {
                // Key
                if version >= 1 {
                    let bytes = value.key.as_bytes();
                    buffer.put_u8((bytes.len() + 1) as u8);
                    buffer.put_slice(bytes);
                } else {
                    let bytes = value.key.as_bytes();
                    buffer.put_i16(bytes.len() as i16);
                    buffer.put_slice(bytes);
                }

                // Value
                buffer.put_f64(value.value);

                // Tagged fields in v1+
                if version >= 1 {
                    buffer.put_u8(0);  // No tagged fields
                }
            }

            // Tagged fields in v1+
            if version >= 1 {
                buffer.put_u8(0);  // No tagged fields
            }
        }

        // Tagged fields in v1+
        if version >= 1 {
            buffer.put_u8(0);  // No tagged fields
        }

        Ok(buffer)
    }
}

// Error codes specific to DescribeClientQuotas
pub mod error_codes {
    pub const NONE: i16 = 0;
    pub const INVALID_REQUEST: i16 = 42;
    pub const UNSUPPORTED_VERSION: i16 = 35;
    pub const CLUSTER_AUTHORIZATION_FAILED: i16 = 31;
}