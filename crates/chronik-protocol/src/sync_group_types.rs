//! SyncGroup API types

use serde::{Deserialize, Serialize};
use bytes::Bytes;
use crate::parser::{Decoder, Encoder, KafkaDecodable, KafkaEncodable};
use chronik_common::{Result, Error};

/// SyncGroup request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncGroupRequest {
    /// The group ID
    pub group_id: String,
    /// The generation ID
    pub generation_id: i32,
    /// The member ID
    pub member_id: String,
    /// Group instance ID (v3+)
    pub group_instance_id: Option<String>,
    /// Protocol type (v5+)
    pub protocol_type: Option<String>,
    /// Protocol name (v5+)
    pub protocol_name: Option<String>,
    /// Group assignments
    pub assignments: Vec<SyncGroupRequestAssignment>,
}

impl KafkaDecodable for SyncGroupRequest {
    fn decode(decoder: &mut Decoder, version: i16) -> Result<Self> {
        // v4+ uses flexible/compact format
        let flexible = version >= 4;

        let group_id = if flexible {
            decoder.read_compact_string()?
        } else {
            decoder.read_string()?
        }.ok_or_else(|| Error::Protocol("Group ID cannot be null".into()))?;

        let generation_id = decoder.read_i32()?;

        let member_id = if flexible {
            decoder.read_compact_string()?
        } else {
            decoder.read_string()?
        }.ok_or_else(|| Error::Protocol("Member ID cannot be null".into()))?;

        let group_instance_id = if version >= 3 {
            if flexible {
                decoder.read_compact_string()?
            } else {
                decoder.read_string()?
            }
        } else {
            None
        };

        let (protocol_type, protocol_name) = if version >= 5 {
            (
                if flexible { decoder.read_compact_string()? } else { decoder.read_string()? },
                if flexible { decoder.read_compact_string()? } else { decoder.read_string()? }
            )
        } else {
            (None, None)
        };

        // Read assignments array
        let assignment_count = if flexible {
            let count = decoder.read_unsigned_varint()? as i32 - 1;
            if count < 0 { 0 } else { count as usize }
        } else {
            decoder.read_i32()? as usize
        };

        let mut assignments = Vec::with_capacity(assignment_count);

        for _ in 0..assignment_count {
            let member_id = if flexible {
                decoder.read_compact_string()?
            } else {
                decoder.read_string()?
            }.ok_or_else(|| Error::Protocol("Member ID in assignment cannot be null".into()))?;

            let assignment = if flexible {
                decoder.read_compact_bytes()?
            } else {
                decoder.read_bytes()?
            }.ok_or_else(|| Error::Protocol("Assignment cannot be null".into()))?;

            // Skip tagged fields for flexible versions (assignment level)
            if flexible {
                let tag_count = decoder.read_unsigned_varint()?;
                for _ in 0..tag_count {
                    let _tag_id = decoder.read_unsigned_varint()?;
                    let tag_size = decoder.read_unsigned_varint()? as usize;
                    decoder.advance(tag_size)?;
                }
            }

            assignments.push(SyncGroupRequestAssignment { member_id, assignment });
        }

        // Skip tagged fields for flexible versions (request level)
        if flexible {
            let tag_count = decoder.read_unsigned_varint()?;
            for _ in 0..tag_count {
                let _tag_id = decoder.read_unsigned_varint()?;
                let tag_size = decoder.read_unsigned_varint()? as usize;
                decoder.advance(tag_size)?;
            }
        }

        Ok(SyncGroupRequest {
            group_id,
            generation_id,
            member_id,
            group_instance_id,
            protocol_type,
            protocol_name,
            assignments,
        })
    }
}

/// Assignment in SyncGroup request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncGroupRequestAssignment {
    /// Member ID
    pub member_id: String,
    /// Assignment data
    pub assignment: Bytes,
}

/// SyncGroup response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncGroupResponse {
    /// Throttle time in milliseconds (v1+)
    pub throttle_time_ms: i32,
    /// Error code
    pub error_code: i16,
    /// Protocol type (v5+)
    pub protocol_type: Option<String>,
    /// Protocol name (v5+)
    pub protocol_name: Option<String>,
    /// Member assignment
    pub assignment: Bytes,
}

impl KafkaEncodable for SyncGroupResponse {
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()> {
        // v4+ uses flexible/compact format
        let flexible = version >= 4;

        if version >= 1 {
            encoder.write_i32(self.throttle_time_ms);
        }

        encoder.write_i16(self.error_code);

        if version >= 5 {
            // v5+ requires protocol_type and protocol_name to be non-nullable
            // If None, write empty string instead of null
            let protocol_type_str = self.protocol_type.as_deref().unwrap_or("");
            let protocol_name_str = self.protocol_name.as_deref().unwrap_or("");

            if flexible {
                encoder.write_compact_string(Some(protocol_type_str));
                encoder.write_compact_string(Some(protocol_name_str));
            } else {
                encoder.write_string(Some(protocol_type_str));
                encoder.write_string(Some(protocol_name_str));
            }
        }

        // Write assignment
        if flexible {
            encoder.write_compact_bytes(Some(&self.assignment));
        } else {
            encoder.write_bytes(Some(&self.assignment));
        }

        // Tagged fields for flexible versions
        if flexible {
            encoder.write_tagged_fields();
        }

        Ok(())
    }
}

/// Error codes for SyncGroup
pub mod error_codes {
    pub const NONE: i16 = 0;
    pub const COORDINATOR_NOT_AVAILABLE: i16 = 15;
    pub const UNKNOWN_MEMBER_ID: i16 = 25;
    pub const ILLEGAL_GENERATION: i16 = 22;
    pub const REBALANCE_IN_PROGRESS: i16 = 27;
    pub const GROUP_AUTHORIZATION_FAILED: i16 = 30;
}