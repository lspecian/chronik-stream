// AlterClientQuotas API types for Kafka protocol
// API key 49, version 0-1

use bytes::{Buf, BufMut, BytesMut};
use chronik_common::Result;
use serde::{Deserialize, Serialize};

use crate::describe_client_quotas_types::EntityData;

// AlterClientQuotas Request Types

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlterClientQuotasRequest {
    pub entries: Vec<AlterQuotaEntry>,
    pub validate_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlterQuotaEntry {
    pub entity: Vec<EntityData>,
    pub ops: Vec<AlterOp>,
}


#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlterOp {
    pub key: String,
    pub value: Option<f64>,  // null to remove
    pub remove: bool,
}

// AlterClientQuotas Response Types

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlterClientQuotasResponse {
    pub throttle_time_ms: i32,
    pub entries: Vec<AlterQuotaEntryResponse>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlterQuotaEntryResponse {
    pub error_code: i16,
    pub error_message: Option<String>,
    pub entity: Vec<EntityData>,
}

// Decoder implementation

impl AlterClientQuotasRequest {
    pub fn decode(decoder: &mut crate::parser::Decoder, version: i16) -> Result<Self> {
        // Entries array
        let entries = if version >= 1 {
            // Compact array in v1+
            let count = decoder.read_unsigned_varint()?;
            let mut entries = Vec::with_capacity((count - 1) as usize);
            for _ in 0..(count - 1) {
                // Entity array
                let entity_count = decoder.read_unsigned_varint()?;
                let mut entity = Vec::with_capacity((entity_count - 1) as usize);
                for _ in 0..(entity_count - 1) {
                    let entity_type = decoder.read_compact_string()?.unwrap_or_default();
                    let entity_name = decoder.read_compact_nullable_string()?;

                    entity.push(EntityData {
                        entity_type,
                        entity_name,
                    });

                    // Tagged fields
                    let _tag_count = decoder.read_unsigned_varint()?;
                }

                // Ops array
                let ops_count = decoder.read_unsigned_varint()?;
                let mut ops = Vec::with_capacity((ops_count - 1) as usize);
                for _ in 0..(ops_count - 1) {
                    let key = decoder.read_compact_string()?.unwrap_or_default();
                    let value = decoder.read_f64()?;
                    let remove = decoder.read_bool()?;

                    ops.push(AlterOp {
                        key,
                        value: if remove { None } else { Some(value) },
                        remove,
                    });

                    // Tagged fields
                    let _tag_count = decoder.read_unsigned_varint()?;
                }

                entries.push(AlterQuotaEntry {
                    entity,
                    ops,
                });

                // Tagged fields
                let _tag_count = decoder.read_unsigned_varint()?;
            }
            entries
        } else {
            // Regular array in v0
            let count = decoder.read_i32()?;
            let mut entries = Vec::with_capacity(count as usize);
            for _ in 0..count {
                // Entity array
                let entity_count = decoder.read_i32()?;
                let mut entity = Vec::with_capacity(entity_count as usize);
                for _ in 0..entity_count {
                    let entity_type = decoder.read_string()?.unwrap_or_default();
                    let entity_name = decoder.read_nullable_string()?;

                    entity.push(EntityData {
                        entity_type,
                        entity_name,
                    });
                }

                // Ops array
                let ops_count = decoder.read_i32()?;
                let mut ops = Vec::with_capacity(ops_count as usize);
                for _ in 0..ops_count {
                    let key = decoder.read_string()?.unwrap_or_default();
                    let value = decoder.read_f64()?;
                    let remove = decoder.read_bool()?;

                    ops.push(AlterOp {
                        key,
                        value: if remove { None } else { Some(value) },
                        remove,
                    });
                }

                entries.push(AlterQuotaEntry {
                    entity,
                    ops,
                });
            }
            entries
        };

        // Validate only flag
        let validate_only = decoder.read_bool()?;

        // Tagged fields in v1+
        if version >= 1 {
            let _tag_count = decoder.read_unsigned_varint()?;
        }

        Ok(AlterClientQuotasRequest {
            entries,
            validate_only,
        })
    }
}

// Encoder implementation

impl AlterClientQuotasResponse {
    pub fn encode_to_bytes(&self, version: i16) -> Result<BytesMut> {
        let mut buffer = BytesMut::new();

        // Throttle time
        buffer.put_i32(self.throttle_time_ms);

        // Entries array
        if version >= 1 {
            // Compact array
            buffer.put_u8((self.entries.len() + 1) as u8);
        } else {
            buffer.put_i32(self.entries.len() as i32);
        }

        for entry in &self.entries {
            // Error code
            buffer.put_i16(entry.error_code);

            // Error message
            if version >= 1 {
                // Compact nullable string
                if let Some(ref msg) = entry.error_message {
                    let bytes = msg.as_bytes();
                    buffer.put_u8((bytes.len() + 1) as u8);
                    buffer.put_slice(bytes);
                } else {
                    buffer.put_u8(0);  // null
                }
            } else {
                // Regular nullable string
                if let Some(ref msg) = entry.error_message {
                    let bytes = msg.as_bytes();
                    buffer.put_i16(bytes.len() as i16);
                    buffer.put_slice(bytes);
                } else {
                    buffer.put_i16(-1);  // null
                }
            }

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

// Error codes specific to AlterClientQuotas
pub mod error_codes {
    pub const NONE: i16 = 0;
    pub const INVALID_REQUEST: i16 = 42;
    pub const INVALID_CLIENT_QUOTA_CONFIGURATION: i16 = 89;
    pub const UNSUPPORTED_VERSION: i16 = 35;
    pub const CLUSTER_AUTHORIZATION_FAILED: i16 = 31;
}