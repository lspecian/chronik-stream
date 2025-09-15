//! EndTxn API types

use serde::{Deserialize, Serialize};
use crate::parser::{Decoder, Encoder, KafkaDecodable, KafkaEncodable};
use chronik_common::{Result, Error};

/// EndTxn request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EndTxnRequest {
    /// Transactional ID
    pub transactional_id: String,
    /// Producer ID
    pub producer_id: i64,
    /// Producer epoch
    pub producer_epoch: i16,
    /// Whether to commit or abort the transaction
    pub committed: bool,
}

impl KafkaDecodable for EndTxnRequest {
    fn decode(decoder: &mut Decoder, _version: i16) -> Result<Self> {
        let transactional_id = decoder.read_string()?
            .ok_or_else(|| Error::Protocol("Transactional ID cannot be null".into()))?;
        let producer_id = decoder.read_i64()?;
        let producer_epoch = decoder.read_i16()?;
        let committed = decoder.read_i8()? != 0;

        Ok(EndTxnRequest {
            transactional_id,
            producer_id,
            producer_epoch,
            committed,
        })
    }
}

impl KafkaEncodable for EndTxnRequest {
    fn encode(&self, encoder: &mut Encoder, _version: i16) -> Result<()> {
        encoder.write_string(Some(&self.transactional_id));
        encoder.write_i64(self.producer_id);
        encoder.write_i16(self.producer_epoch);
        encoder.write_i8(if self.committed { 1 } else { 0 });
        Ok(())
    }
}

/// EndTxn response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EndTxnResponse {
    /// Throttle time in milliseconds
    pub throttle_time_ms: i32,
    /// Error code
    pub error_code: i16,
}

impl KafkaEncodable for EndTxnResponse {
    fn encode(&self, encoder: &mut Encoder, _version: i16) -> Result<()> {
        encoder.write_i32(self.throttle_time_ms);
        encoder.write_i16(self.error_code);
        Ok(())
    }
}

/// Error codes for EndTxn
pub mod error_codes {
    pub const NONE: i16 = 0;
    pub const NOT_COORDINATOR: i16 = 16;
    pub const COORDINATOR_NOT_AVAILABLE: i16 = 15;
    pub const COORDINATOR_LOAD_IN_PROGRESS: i16 = 14;
    pub const INVALID_PRODUCER_ID_MAPPING: i16 = 49;
    pub const INVALID_PRODUCER_EPOCH: i16 = 47;
    pub const INVALID_TXN_STATE: i16 = 48;
    pub const TRANSACTIONAL_ID_AUTHORIZATION_FAILED: i16 = 53;
    pub const UNKNOWN: i16 = -1;
}