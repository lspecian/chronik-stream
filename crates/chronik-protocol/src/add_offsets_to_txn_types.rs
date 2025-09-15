//! AddOffsetsToTxn API types

use serde::{Deserialize, Serialize};
use crate::parser::{Decoder, Encoder, KafkaDecodable, KafkaEncodable};
use chronik_common::{Result, Error};

/// AddOffsetsToTxn request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddOffsetsToTxnRequest {
    /// Transactional ID
    pub transactional_id: String,
    /// Producer ID
    pub producer_id: i64,
    /// Producer epoch
    pub producer_epoch: i16,
    /// Consumer group ID
    pub consumer_group_id: String,
}

impl KafkaDecodable for AddOffsetsToTxnRequest {
    fn decode(decoder: &mut Decoder, _version: i16) -> Result<Self> {
        let transactional_id = decoder.read_string()?
            .ok_or_else(|| Error::Protocol("Transactional ID cannot be null".into()))?;
        let producer_id = decoder.read_i64()?;
        let producer_epoch = decoder.read_i16()?;
        let consumer_group_id = decoder.read_string()?
            .ok_or_else(|| Error::Protocol("Consumer group ID cannot be null".into()))?;

        Ok(AddOffsetsToTxnRequest {
            transactional_id,
            producer_id,
            producer_epoch,
            consumer_group_id,
        })
    }
}

impl KafkaEncodable for AddOffsetsToTxnRequest {
    fn encode(&self, encoder: &mut Encoder, _version: i16) -> Result<()> {
        encoder.write_string(Some(&self.transactional_id));
        encoder.write_i64(self.producer_id);
        encoder.write_i16(self.producer_epoch);
        encoder.write_string(Some(&self.consumer_group_id));
        Ok(())
    }
}

/// AddOffsetsToTxn response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddOffsetsToTxnResponse {
    /// Throttle time in milliseconds
    pub throttle_time_ms: i32,
    /// Error code
    pub error_code: i16,
}

impl KafkaEncodable for AddOffsetsToTxnResponse {
    fn encode(&self, encoder: &mut Encoder, _version: i16) -> Result<()> {
        encoder.write_i32(self.throttle_time_ms);
        encoder.write_i16(self.error_code);
        Ok(())
    }
}

/// Error codes for AddOffsetsToTxn
pub mod error_codes {
    pub const NONE: i16 = 0;
    pub const NOT_COORDINATOR: i16 = 16;
    pub const COORDINATOR_NOT_AVAILABLE: i16 = 15;
    pub const COORDINATOR_LOAD_IN_PROGRESS: i16 = 14;
    pub const INVALID_PRODUCER_ID_MAPPING: i16 = 49;
    pub const INVALID_PRODUCER_EPOCH: i16 = 47;
    pub const INVALID_TXN_STATE: i16 = 48;
    pub const GROUP_AUTHORIZATION_FAILED: i16 = 30;
    pub const TRANSACTIONAL_ID_AUTHORIZATION_FAILED: i16 = 53;
    pub const UNKNOWN: i16 = -1;
}