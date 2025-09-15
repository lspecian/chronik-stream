//! DescribeGroups API types

use serde::{Deserialize, Serialize};
use crate::parser::{Decoder, Encoder, KafkaDecodable, KafkaEncodable};
use chronik_common::{Result, Error};

/// DescribeGroups request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DescribeGroupsRequest {
    /// Consumer group IDs to describe
    pub group_ids: Vec<String>,
    /// Whether to include authorized operations (v3+)
    pub include_authorized_operations: bool,
}

impl KafkaDecodable for DescribeGroupsRequest {
    fn decode(decoder: &mut Decoder, version: i16) -> Result<Self> {
        // Read group IDs array
        let group_count = decoder.read_i32()?;
        if group_count < 0 {
            return Err(Error::Protocol("Invalid group count".into()));
        }

        let mut group_ids = Vec::with_capacity(group_count as usize);
        for _ in 0..group_count {
            let group_id = decoder.read_string()?
                .ok_or_else(|| Error::Protocol("Group ID cannot be null".into()))?;
            group_ids.push(group_id);
        }

        // include_authorized_operations is only in v3+
        let include_authorized_operations = if version >= 3 {
            decoder.read_i8()? != 0
        } else {
            false
        };

        Ok(DescribeGroupsRequest {
            group_ids,
            include_authorized_operations,
        })
    }
}

impl KafkaEncodable for DescribeGroupsRequest {
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()> {
        // Write group IDs array
        encoder.write_i32(self.group_ids.len() as i32);
        for group_id in &self.group_ids {
            encoder.write_string(Some(group_id));
        }

        if version >= 3 {
            encoder.write_i8(if self.include_authorized_operations { 1 } else { 0 });
        }

        Ok(())
    }
}

/// DescribeGroups response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DescribeGroupsResponse {
    /// Throttle time in milliseconds (v1+)
    pub throttle_time_ms: i32,
    /// Groups
    pub groups: Vec<DescribedGroup>,
}

impl KafkaEncodable for DescribeGroupsResponse {
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()> {
        // throttle_time_ms is only in v1+
        if version >= 1 {
            encoder.write_i32(self.throttle_time_ms);
        }

        // Write groups array
        encoder.write_i32(self.groups.len() as i32);
        for group in &self.groups {
            group.encode(encoder, version)?;
        }

        Ok(())
    }
}

/// Described group
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DescribedGroup {
    /// Error code
    pub error_code: i16,
    /// Group ID
    pub group_id: String,
    /// Group state
    pub state: String,
    /// Protocol type
    pub protocol_type: String,
    /// Protocol data
    pub protocol_data: String,
    /// Members
    pub members: Vec<GroupMember>,
    /// Authorized operations (v3+)
    pub authorized_operations: Option<i32>,
}

impl KafkaEncodable for DescribedGroup {
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()> {
        encoder.write_i16(self.error_code);
        encoder.write_string(Some(&self.group_id));
        encoder.write_string(Some(&self.state));
        encoder.write_string(Some(&self.protocol_type));
        encoder.write_string(Some(&self.protocol_data));

        // Write members array
        encoder.write_i32(self.members.len() as i32);
        for member in &self.members {
            member.encode(encoder, version)?;
        }

        // authorized_operations is only in v3+
        if version >= 3 {
            if let Some(ops) = self.authorized_operations {
                encoder.write_i32(ops);
            } else {
                encoder.write_i32(-2147483648); // INT32_MIN indicates null
            }
        }

        Ok(())
    }
}

/// Group member
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupMember {
    /// Member ID
    pub member_id: String,
    /// Group instance ID (v4+)
    pub group_instance_id: Option<String>,
    /// Client ID
    pub client_id: String,
    /// Client host
    pub client_host: String,
    /// Member metadata
    pub member_metadata: Vec<u8>,
    /// Member assignment
    pub member_assignment: Vec<u8>,
}

impl KafkaEncodable for GroupMember {
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()> {
        encoder.write_string(Some(&self.member_id));

        // group_instance_id is only in v4+
        if version >= 4 {
            encoder.write_string(self.group_instance_id.as_deref());
        }

        encoder.write_string(Some(&self.client_id));
        encoder.write_string(Some(&self.client_host));
        encoder.write_bytes(Some(&self.member_metadata));
        encoder.write_bytes(Some(&self.member_assignment));

        Ok(())
    }
}

/// Error codes for DescribeGroups
pub mod error_codes {
    pub const NONE: i16 = 0;
    pub const GROUP_AUTHORIZATION_FAILED: i16 = 30;
    pub const INVALID_GROUP_ID: i16 = 24;
    pub const COORDINATOR_NOT_AVAILABLE: i16 = 15;
    pub const NOT_COORDINATOR: i16 = 16;
    pub const GROUP_ID_NOT_FOUND: i16 = 69;
}