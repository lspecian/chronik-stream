//! DeleteGroups API types

use serde::{Deserialize, Serialize};
use crate::parser::{Decoder, Encoder, KafkaDecodable, KafkaEncodable};
use chronik_common::{Result, Error};

/// DeleteGroups request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteGroupsRequest {
    /// The group IDs to delete
    pub groups_names: Vec<String>,
}

impl KafkaDecodable for DeleteGroupsRequest {
    fn decode(decoder: &mut Decoder, _version: i16) -> Result<Self> {
        // Read array of group names
        let group_count = decoder.read_i32()?;
        if group_count < 0 {
            return Err(Error::Protocol("Invalid group count".into()));
        }

        let mut groups_names = Vec::with_capacity(group_count as usize);
        for _ in 0..group_count {
            let group_name = decoder.read_string()?
                .ok_or_else(|| Error::Protocol("Group name cannot be null".into()))?;
            groups_names.push(group_name);
        }

        Ok(DeleteGroupsRequest {
            groups_names,
        })
    }
}

impl KafkaEncodable for DeleteGroupsRequest {
    fn encode(&self, encoder: &mut Encoder, _version: i16) -> Result<()> {
        // Write array of group names
        encoder.write_i32(self.groups_names.len() as i32);
        for group_name in &self.groups_names {
            encoder.write_string(Some(group_name));
        }

        Ok(())
    }
}

/// DeleteGroups response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteGroupsResponse {
    /// The duration in milliseconds for which the request was throttled
    pub throttle_time_ms: i32,
    /// Results for each group deletion request
    pub results: Vec<DeletableGroupResult>,
}

impl KafkaEncodable for DeleteGroupsResponse {
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()> {
        encoder.write_i32(self.throttle_time_ms);

        // Write array of results
        encoder.write_i32(self.results.len() as i32);
        for result in &self.results {
            result.encode(encoder, version)?;
        }

        Ok(())
    }
}

/// Result for a single group deletion
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeletableGroupResult {
    /// The group ID
    pub group_id: String,
    /// The error code
    pub error_code: i16,
}

impl KafkaEncodable for DeletableGroupResult {
    fn encode(&self, encoder: &mut Encoder, _version: i16) -> Result<()> {
        encoder.write_string(Some(&self.group_id));
        encoder.write_i16(self.error_code);
        Ok(())
    }
}

/// Error codes for DeleteGroups
pub mod error_codes {
    pub const NONE: i16 = 0;
    pub const GROUP_ID_NOT_FOUND: i16 = 69;
    pub const GROUP_NOT_EMPTY: i16 = 68;
    pub const INVALID_GROUP_ID: i16 = 24;
    pub const GROUP_AUTHORIZATION_FAILED: i16 = 30;
    pub const NOT_COORDINATOR: i16 = 16;
    pub const COORDINATOR_NOT_AVAILABLE: i16 = 15;
    pub const REQUEST_TIMED_OUT: i16 = 7;
}