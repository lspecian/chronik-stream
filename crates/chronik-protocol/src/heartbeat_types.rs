//! Heartbeat API types

use serde::{Deserialize, Serialize};
use crate::parser::{Decoder, Encoder, KafkaDecodable, KafkaEncodable};
use chronik_common::{Result, Error};

/// Heartbeat request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatRequest {
    /// The group ID
    pub group_id: String,
    /// The generation ID
    pub generation_id: i32,
    /// The member ID
    pub member_id: String,
    /// Group instance ID (v3+)
    pub group_instance_id: Option<String>,
}

impl KafkaDecodable for HeartbeatRequest {
    fn decode(decoder: &mut Decoder, version: i16) -> Result<Self> {
        let group_id = decoder.read_string()?
            .ok_or_else(|| Error::Protocol("group_id cannot be null".into()))?;
        let generation_id = decoder.read_i32()?;
        let member_id = decoder.read_string()?
            .ok_or_else(|| Error::Protocol("member_id cannot be null".into()))?;

        let group_instance_id = if version >= 3 {
            decoder.read_string()?
        } else {
            None
        };

        Ok(HeartbeatRequest {
            group_id,
            generation_id,
            member_id,
            group_instance_id,
        })
    }
}

impl KafkaEncodable for HeartbeatRequest {
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()> {
        encoder.write_string(Some(&self.group_id));
        encoder.write_i32(self.generation_id);
        encoder.write_string(Some(&self.member_id));

        if version >= 3 {
            encoder.write_string(self.group_instance_id.as_deref());
        }

        Ok(())
    }
}

/// Heartbeat response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatResponse {
    /// Throttle time in milliseconds (v1+)
    pub throttle_time_ms: i32,
    /// Error code
    pub error_code: i16,
}

impl KafkaEncodable for HeartbeatResponse {
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()> {
        if version >= 1 {
            encoder.write_i32(self.throttle_time_ms);
        }
        encoder.write_i16(self.error_code);
        Ok(())
    }
}

/// Error codes for Heartbeat
pub mod error_codes {
    pub const NONE: i16 = 0;
    pub const COORDINATOR_NOT_AVAILABLE: i16 = 15;
    pub const UNKNOWN_MEMBER_ID: i16 = 25;
    pub const ILLEGAL_GENERATION: i16 = 22;
    pub const REBALANCE_IN_PROGRESS: i16 = 27;
    pub const GROUP_AUTHORIZATION_FAILED: i16 = 30;
}