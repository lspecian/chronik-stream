//! LeaveGroup API types

use serde::{Deserialize, Serialize};
use crate::parser::{Decoder, Encoder, KafkaDecodable, KafkaEncodable};
use chronik_common::{Result, Error};

/// Member identity for leave group request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemberIdentity {
    /// The member ID
    pub member_id: String,
    /// The group instance ID (v3+)
    pub group_instance_id: Option<String>,
}

impl KafkaDecodable for MemberIdentity {
    fn decode(decoder: &mut Decoder, version: i16) -> Result<Self> {
        let member_id = decoder.read_string()?
            .ok_or_else(|| Error::Protocol("member_id cannot be null".into()))?;

        let group_instance_id = if version >= 3 {
            decoder.read_string()?
        } else {
            None
        };

        Ok(MemberIdentity {
            member_id,
            group_instance_id,
        })
    }
}

impl KafkaEncodable for MemberIdentity {
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()> {
        encoder.write_string(Some(&self.member_id));

        if version >= 3 {
            encoder.write_string(self.group_instance_id.as_deref());
        }

        Ok(())
    }
}

/// LeaveGroup request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaveGroupRequest {
    /// The group ID to leave
    pub group_id: String,
    /// Members to remove (v3+: array, v0-2: single member extracted into array)
    pub members: Vec<MemberIdentity>,
}

impl KafkaDecodable for LeaveGroupRequest {
    fn decode(decoder: &mut Decoder, version: i16) -> Result<Self> {
        let group_id = decoder.read_string()?
            .ok_or_else(|| Error::Protocol("group_id cannot be null".into()))?;

        let members = if version >= 3 {
            // Version 3+: array of members
            let member_count = decoder.read_i32()? as usize;
            let mut members = Vec::with_capacity(member_count);
            for _ in 0..member_count {
                members.push(MemberIdentity::decode(decoder, version)?);
            }
            members
        } else {
            // Version 0-2: single member
            let member_id = decoder.read_string()?
                .ok_or_else(|| Error::Protocol("member_id cannot be null".into()))?;
            vec![MemberIdentity {
                member_id,
                group_instance_id: None,
            }]
        };

        Ok(LeaveGroupRequest {
            group_id,
            members,
        })
    }
}

impl KafkaEncodable for LeaveGroupRequest {
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()> {
        encoder.write_string(Some(&self.group_id));

        if version >= 3 {
            // Version 3+: array of members
            encoder.write_i32(self.members.len() as i32);
            for member in &self.members {
                member.encode(encoder, version)?;
            }
        } else {
            // Version 0-2: single member
            if let Some(member) = self.members.first() {
                encoder.write_string(Some(&member.member_id));
            } else {
                encoder.write_string(None);
            }
        }

        Ok(())
    }
}

/// LeaveGroup response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaveGroupResponse {
    /// Throttle time in milliseconds (v1+)
    pub throttle_time_ms: i32,
    /// Error code (v0-2)
    pub error_code: i16,
    /// Members responses (v3+)
    pub members: Vec<MemberResponse>,
}

impl KafkaEncodable for LeaveGroupResponse {
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()> {
        if version >= 1 {
            encoder.write_i32(self.throttle_time_ms);
        }

        if version <= 2 {
            // Version 0-2: single error code
            encoder.write_i16(self.error_code);
        } else {
            // Version 3+: per-member responses
            encoder.write_i32(self.members.len() as i32);
            for member in &self.members {
                member.encode(encoder, version)?;
            }
        }

        Ok(())
    }
}

/// Per-member response for leave group
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemberResponse {
    /// The member ID
    pub member_id: String,
    /// The group instance ID
    pub group_instance_id: Option<String>,
    /// Error code for this member
    pub error_code: i16,
}

impl KafkaEncodable for MemberResponse {
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()> {
        encoder.write_string(Some(&self.member_id));

        if version >= 3 {
            encoder.write_string(self.group_instance_id.as_deref());
        }

        encoder.write_i16(self.error_code);

        Ok(())
    }
}

/// Error codes for LeaveGroup
pub mod error_codes {
    pub const NONE: i16 = 0;
    pub const COORDINATOR_NOT_AVAILABLE: i16 = 15;
    pub const NOT_COORDINATOR: i16 = 16;
    pub const INVALID_GROUP_ID: i16 = 24;
    pub const UNKNOWN_MEMBER_ID: i16 = 25;
    pub const GROUP_AUTHORIZATION_FAILED: i16 = 30;
    pub const GROUP_ID_NOT_FOUND: i16 = 69;
}