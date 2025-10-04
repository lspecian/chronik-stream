//! AddPartitionsToTxn API types

use serde::{Deserialize, Serialize};
use crate::parser::{Decoder, Encoder, KafkaDecodable, KafkaEncodable};
use chronik_common::{Result, Error};

/// Partition to add to transaction
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddPartitionsToTxnPartition {
    /// Partition index
    pub partition: i32,
}

impl KafkaDecodable for AddPartitionsToTxnPartition {
    fn decode(decoder: &mut Decoder, _version: i16) -> Result<Self> {
        Ok(AddPartitionsToTxnPartition {
            partition: decoder.read_i32()?,
        })
    }
}

impl KafkaEncodable for AddPartitionsToTxnPartition {
    fn encode(&self, encoder: &mut Encoder, _version: i16) -> Result<()> {
        encoder.write_i32(self.partition);
        Ok(())
    }
}

/// Topic partitions to add to transaction
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddPartitionsToTxnTopic {
    /// Topic name
    pub name: String,
    /// Partitions to add
    pub partitions: Vec<AddPartitionsToTxnPartition>,
}

impl KafkaDecodable for AddPartitionsToTxnTopic {
    fn decode(decoder: &mut Decoder, version: i16) -> Result<Self> {
        let name = decoder.read_string()?
            .ok_or_else(|| Error::Protocol("Topic name cannot be null".into()))?;

        let partition_count = decoder.read_i32()?;
        if partition_count < 0 {
            return Err(Error::Protocol("Invalid partition count".into()));
        }

        let mut partitions = Vec::with_capacity(partition_count as usize);
        for _ in 0..partition_count {
            partitions.push(AddPartitionsToTxnPartition::decode(decoder, version)?);
        }

        Ok(AddPartitionsToTxnTopic {
            name,
            partitions,
        })
    }
}

impl KafkaEncodable for AddPartitionsToTxnTopic {
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()> {
        encoder.write_string(Some(&self.name));
        encoder.write_i32(self.partitions.len() as i32);
        for partition in &self.partitions {
            partition.encode(encoder, version)?;
        }
        Ok(())
    }
}

/// AddPartitionsToTxn request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddPartitionsToTxnRequest {
    /// Transactional ID
    pub transactional_id: String,
    /// Producer ID
    pub producer_id: i64,
    /// Producer epoch
    pub producer_epoch: i16,
    /// Topics to add to transaction
    pub topics: Vec<AddPartitionsToTxnTopic>,
}

impl KafkaDecodable for AddPartitionsToTxnRequest {
    fn decode(decoder: &mut Decoder, version: i16) -> Result<Self> {
        let transactional_id = decoder.read_string()?
            .ok_or_else(|| Error::Protocol("Transactional ID cannot be null".into()))?;
        let producer_id = decoder.read_i64()?;
        let producer_epoch = decoder.read_i16()?;

        let topic_count = decoder.read_i32()?;
        if topic_count < 0 {
            return Err(Error::Protocol("Invalid topic count".into()));
        }

        let mut topics = Vec::with_capacity(topic_count as usize);
        for _ in 0..topic_count {
            topics.push(AddPartitionsToTxnTopic::decode(decoder, version)?);
        }

        Ok(AddPartitionsToTxnRequest {
            transactional_id,
            producer_id,
            producer_epoch,
            topics,
        })
    }
}

impl KafkaEncodable for AddPartitionsToTxnRequest {
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()> {
        encoder.write_string(Some(&self.transactional_id));
        encoder.write_i64(self.producer_id);
        encoder.write_i16(self.producer_epoch);
        encoder.write_i32(self.topics.len() as i32);
        for topic in &self.topics {
            topic.encode(encoder, version)?;
        }
        Ok(())
    }
}

/// AddPartitionsToTxn response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddPartitionsToTxnResponse {
    /// Throttle time in milliseconds
    pub throttle_time_ms: i32,
    /// Results for each topic
    pub results: Vec<AddPartitionsToTxnTopicResult>,
}

impl KafkaEncodable for AddPartitionsToTxnResponse {
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()> {
        let flexible = version >= 3;

        encoder.write_i32(self.throttle_time_ms);

        if flexible {
            encoder.write_compact_array_len(self.results.len());
        } else {
            encoder.write_i32(self.results.len() as i32);
        }

        for result in &self.results {
            result.encode(encoder, version)?;
        }

        // Tagged fields for v3+
        if flexible {
            encoder.write_tagged_fields();
        }

        Ok(())
    }
}

/// Result for a topic's partitions
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddPartitionsToTxnTopicResult {
    /// Topic name
    pub name: String,
    /// Results for each partition
    pub results: Vec<AddPartitionsToTxnPartitionResult>,
}

impl KafkaEncodable for AddPartitionsToTxnTopicResult {
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()> {
        let flexible = version >= 3;

        if flexible {
            encoder.write_compact_string(Some(&self.name));
            encoder.write_compact_array_len(self.results.len());
        } else {
            encoder.write_string(Some(&self.name));
            encoder.write_i32(self.results.len() as i32);
        }

        for result in &self.results {
            result.encode(encoder, version)?;
        }

        // Tagged fields for v3+
        if flexible {
            encoder.write_tagged_fields();
        }

        Ok(())
    }
}

/// Result for a single partition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddPartitionsToTxnPartitionResult {
    /// Partition index
    pub partition: i32,
    /// Error code
    pub error_code: i16,
}

impl KafkaEncodable for AddPartitionsToTxnPartitionResult {
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()> {
        let flexible = version >= 3;

        encoder.write_i32(self.partition);
        encoder.write_i16(self.error_code);

        // Tagged fields for v3+
        if flexible {
            encoder.write_tagged_fields();
        }

        Ok(())
    }
}

/// Error codes for AddPartitionsToTxn
pub mod error_codes {
    pub const NONE: i16 = 0;
    pub const NOT_COORDINATOR: i16 = 16;
    pub const COORDINATOR_NOT_AVAILABLE: i16 = 15;
    pub const INVALID_PRODUCER_ID_MAPPING: i16 = 49;
    pub const INVALID_PRODUCER_EPOCH: i16 = 47;
    pub const INVALID_TXN_STATE: i16 = 48;
    pub const TRANSACTIONAL_ID_AUTHORIZATION_FAILED: i16 = 53;
    pub const OPERATION_NOT_ATTEMPTED: i16 = 55;
    pub const UNKNOWN_TOPIC_OR_PARTITION: i16 = 3;
    pub const TOPIC_AUTHORIZATION_FAILED: i16 = 29;
}