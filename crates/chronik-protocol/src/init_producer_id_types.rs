//! InitProducerId API types

use serde::{Deserialize, Serialize};
use crate::parser::{Decoder, Encoder, KafkaDecodable, KafkaEncodable};
use chronik_common::{Result, Error};

/// InitProducerId request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitProducerIdRequest {
    /// Transactional ID
    pub transactional_id: Option<String>,
    /// Transaction timeout in milliseconds
    pub transaction_timeout_ms: i32,
    /// Producer ID (v3+)
    pub producer_id: Option<i64>,
    /// Producer epoch (v3+)
    pub producer_epoch: Option<i16>,
}

impl KafkaDecodable for InitProducerIdRequest {
    fn decode(decoder: &mut Decoder, version: i16) -> Result<Self> {
        let is_flexible = version >= 2;

        // Read transactional_id with correct encoding based on version
        let transactional_id = if is_flexible {
            decoder.read_compact_string()?
        } else {
            decoder.read_string()?
        };

        let transaction_timeout_ms = decoder.read_i32()?;

        // producer_id and producer_epoch are only in v3+
        let (producer_id, producer_epoch) = if version >= 3 {
            (Some(decoder.read_i64()?), Some(decoder.read_i16()?))
        } else {
            (None, None)
        };

        // Consume tagged fields for flexible versions (v2+)
        if is_flexible {
            let tag_count = decoder.read_unsigned_varint()?;
            for _ in 0..tag_count {
                let _tag_id = decoder.read_unsigned_varint()?;
                let tag_size = decoder.read_unsigned_varint()? as usize;
                decoder.advance(tag_size)?;
            }
        }

        Ok(InitProducerIdRequest {
            transactional_id,
            transaction_timeout_ms,
            producer_id,
            producer_epoch,
        })
    }
}

impl KafkaEncodable for InitProducerIdRequest {
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()> {
        encoder.write_string(self.transactional_id.as_deref());
        encoder.write_i32(self.transaction_timeout_ms);

        if version >= 3 {
            encoder.write_i64(self.producer_id.unwrap_or(-1));
            encoder.write_i16(self.producer_epoch.unwrap_or(-1));
        }

        Ok(())
    }
}

/// InitProducerId response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitProducerIdResponse {
    /// Throttle time in milliseconds
    pub throttle_time_ms: i32,
    /// Error code
    pub error_code: i16,
    /// Producer ID
    pub producer_id: i64,
    /// Producer epoch
    pub producer_epoch: i16,
}

impl KafkaEncodable for InitProducerIdResponse {
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()> {
        encoder.write_i32(self.throttle_time_ms);
        encoder.write_i16(self.error_code);
        encoder.write_i64(self.producer_id);
        encoder.write_i16(self.producer_epoch);

        // Add tagged fields for flexible versions (v2+)
        if version >= 2 {
            encoder.write_unsigned_varint(0);  // Empty tagged fields
        }

        Ok(())
    }
}

/// Error codes for InitProducerId
pub mod error_codes {
    pub const NONE: i16 = 0;
    pub const NOT_COORDINATOR: i16 = 16;
    pub const COORDINATOR_NOT_AVAILABLE: i16 = 15;
    pub const COORDINATOR_LOAD_IN_PROGRESS: i16 = 14;
    pub const TRANSACTIONAL_ID_AUTHORIZATION_FAILED: i16 = 53;
    pub const INVALID_PRODUCER_ID_MAPPING: i16 = 49;
    pub const INVALID_PRODUCER_EPOCH: i16 = 47;
    pub const UNKNOWN: i16 = -1;
}