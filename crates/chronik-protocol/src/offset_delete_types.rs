//! OffsetDelete API types

use serde::{Deserialize, Serialize};
use crate::parser::{Decoder, Encoder, KafkaDecodable, KafkaEncodable};
use chronik_common::{Result, Error};

/// Partition to delete offsets for
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OffsetDeleteRequestPartition {
    /// Partition index
    pub partition_index: i32,
}

impl KafkaDecodable for OffsetDeleteRequestPartition {
    fn decode(decoder: &mut Decoder, _version: i16) -> Result<Self> {
        let partition_index = decoder.read_i32()?;
        Ok(OffsetDeleteRequestPartition { partition_index })
    }
}

impl KafkaEncodable for OffsetDeleteRequestPartition {
    fn encode(&self, encoder: &mut Encoder, _version: i16) -> Result<()> {
        encoder.write_i32(self.partition_index);
        Ok(())
    }
}

/// Topic to delete offsets for
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OffsetDeleteRequestTopic {
    /// Topic name
    pub name: String,
    /// Partitions to delete offsets for
    pub partitions: Vec<OffsetDeleteRequestPartition>,
}

impl KafkaDecodable for OffsetDeleteRequestTopic {
    fn decode(decoder: &mut Decoder, version: i16) -> Result<Self> {
        let name = decoder.read_string()?
            .ok_or_else(|| Error::Protocol("Topic name cannot be null".into()))?;

        let partition_count = decoder.read_i32()?;
        if partition_count < 0 {
            return Err(Error::Protocol("Invalid partition count".into()));
        }

        let mut partitions = Vec::with_capacity(partition_count as usize);
        for _ in 0..partition_count {
            partitions.push(OffsetDeleteRequestPartition::decode(decoder, version)?);
        }

        Ok(OffsetDeleteRequestTopic {
            name,
            partitions,
        })
    }
}

impl KafkaEncodable for OffsetDeleteRequestTopic {
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()> {
        encoder.write_string(Some(&self.name));
        encoder.write_i32(self.partitions.len() as i32);
        for partition in &self.partitions {
            partition.encode(encoder, version)?;
        }
        Ok(())
    }
}

/// OffsetDelete request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OffsetDeleteRequest {
    /// Consumer group ID
    pub group_id: String,
    /// Topics to delete offsets for
    pub topics: Vec<OffsetDeleteRequestTopic>,
}

impl KafkaDecodable for OffsetDeleteRequest {
    fn decode(decoder: &mut Decoder, version: i16) -> Result<Self> {
        let group_id = decoder.read_string()?
            .ok_or_else(|| Error::Protocol("Group ID cannot be null".into()))?;

        let topic_count = decoder.read_i32()?;
        if topic_count < 0 {
            return Err(Error::Protocol("Invalid topic count".into()));
        }

        let mut topics = Vec::with_capacity(topic_count as usize);
        for _ in 0..topic_count {
            topics.push(OffsetDeleteRequestTopic::decode(decoder, version)?);
        }

        Ok(OffsetDeleteRequest {
            group_id,
            topics,
        })
    }
}

impl KafkaEncodable for OffsetDeleteRequest {
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()> {
        encoder.write_string(Some(&self.group_id));
        encoder.write_i32(self.topics.len() as i32);
        for topic in &self.topics {
            topic.encode(encoder, version)?;
        }
        Ok(())
    }
}

/// OffsetDelete response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OffsetDeleteResponse {
    /// Error code
    pub error_code: i16,
    /// Throttle time in milliseconds
    pub throttle_time_ms: i32,
    /// Topics results
    pub topics: Vec<OffsetDeleteResponseTopic>,
}

impl KafkaEncodable for OffsetDeleteResponse {
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()> {
        encoder.write_i16(self.error_code);
        encoder.write_i32(self.throttle_time_ms);
        encoder.write_i32(self.topics.len() as i32);
        for topic in &self.topics {
            topic.encode(encoder, version)?;
        }
        Ok(())
    }
}

/// Response for a single topic
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OffsetDeleteResponseTopic {
    /// Topic name
    pub name: String,
    /// Partition results
    pub partitions: Vec<OffsetDeleteResponsePartition>,
}

impl KafkaEncodable for OffsetDeleteResponseTopic {
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()> {
        encoder.write_string(Some(&self.name));
        encoder.write_i32(self.partitions.len() as i32);
        for partition in &self.partitions {
            partition.encode(encoder, version)?;
        }
        Ok(())
    }
}

/// Response for a single partition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OffsetDeleteResponsePartition {
    /// Partition index
    pub partition_index: i32,
    /// Error code
    pub error_code: i16,
}

impl KafkaEncodable for OffsetDeleteResponsePartition {
    fn encode(&self, encoder: &mut Encoder, _version: i16) -> Result<()> {
        encoder.write_i32(self.partition_index);
        encoder.write_i16(self.error_code);
        Ok(())
    }
}

/// Error codes for OffsetDelete
pub mod error_codes {
    pub const NONE: i16 = 0;
    pub const GROUP_AUTHORIZATION_FAILED: i16 = 30;
    pub const INVALID_GROUP_ID: i16 = 24;
    pub const COORDINATOR_NOT_AVAILABLE: i16 = 15;
    pub const NOT_COORDINATOR: i16 = 16;
    pub const GROUP_ID_NOT_FOUND: i16 = 69;
    pub const TOPIC_AUTHORIZATION_FAILED: i16 = 29;
    pub const UNKNOWN_TOPIC_OR_PARTITION: i16 = 3;
    pub const GROUP_SUBSCRIBED_TO_TOPIC: i16 = 86;
}