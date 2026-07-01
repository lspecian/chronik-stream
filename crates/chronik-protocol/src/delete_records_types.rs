//! DeleteRecords API (key 21) types.
//!
//! Used by Kafka UI / AKHQ "Clear messages", the Java AdminClient
//! `deleteRecords`, kafka-python and confluent-kafka. Advances a partition's
//! log start offset (low watermark) so records before the given offset become
//! unreadable and their segments can be reclaimed.
//!
//! Versions 0 and 1 share an identical, non-flexible wire format (v2 adds
//! flexible/tagged fields; we advertise up to v1 only).

use serde::{Deserialize, Serialize};
use crate::parser::{Decoder, Encoder, KafkaDecodable, KafkaEncodable};
use chronik_common::{Result, Error};

/// A partition + the offset before which records should be deleted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteRecordsRequestPartition {
    /// Partition index.
    pub partition_index: i32,
    /// Delete all records before this offset. The special value `-1` means
    /// "the partition high watermark" (delete every record in the partition).
    pub offset: i64,
}

impl KafkaDecodable for DeleteRecordsRequestPartition {
    fn decode(decoder: &mut Decoder, _version: i16) -> Result<Self> {
        let partition_index = decoder.read_i32()?;
        let offset = decoder.read_i64()?;
        Ok(DeleteRecordsRequestPartition { partition_index, offset })
    }
}

impl KafkaEncodable for DeleteRecordsRequestPartition {
    fn encode(&self, encoder: &mut Encoder, _version: i16) -> Result<()> {
        encoder.write_i32(self.partition_index);
        encoder.write_i64(self.offset);
        Ok(())
    }
}

/// A topic and the partitions to delete records from.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteRecordsRequestTopic {
    /// Topic name.
    pub name: String,
    /// Partitions to delete records from.
    pub partitions: Vec<DeleteRecordsRequestPartition>,
}

impl KafkaDecodable for DeleteRecordsRequestTopic {
    fn decode(decoder: &mut Decoder, version: i16) -> Result<Self> {
        let name = decoder.read_string()?
            .ok_or_else(|| Error::Protocol("Topic name cannot be null".into()))?;

        let partition_count = decoder.read_i32()?;
        if partition_count < 0 {
            return Err(Error::Protocol("Invalid partition count".into()));
        }
        let mut partitions = Vec::with_capacity(partition_count as usize);
        for _ in 0..partition_count {
            partitions.push(DeleteRecordsRequestPartition::decode(decoder, version)?);
        }

        Ok(DeleteRecordsRequestTopic { name, partitions })
    }
}

impl KafkaEncodable for DeleteRecordsRequestTopic {
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()> {
        encoder.write_string(Some(&self.name));
        encoder.write_i32(self.partitions.len() as i32);
        for partition in &self.partitions {
            partition.encode(encoder, version)?;
        }
        Ok(())
    }
}

/// DeleteRecords request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteRecordsRequest {
    /// Topics to delete records from.
    pub topics: Vec<DeleteRecordsRequestTopic>,
    /// How long to wait for the deletions to complete, in milliseconds.
    pub timeout_ms: i32,
}

impl KafkaDecodable for DeleteRecordsRequest {
    fn decode(decoder: &mut Decoder, version: i16) -> Result<Self> {
        let topic_count = decoder.read_i32()?;
        if topic_count < 0 {
            return Err(Error::Protocol("Invalid topic count".into()));
        }
        let mut topics = Vec::with_capacity(topic_count as usize);
        for _ in 0..topic_count {
            topics.push(DeleteRecordsRequestTopic::decode(decoder, version)?);
        }

        let timeout_ms = decoder.read_i32()?;

        Ok(DeleteRecordsRequest { topics, timeout_ms })
    }
}

impl KafkaEncodable for DeleteRecordsRequest {
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()> {
        encoder.write_i32(self.topics.len() as i32);
        for topic in &self.topics {
            topic.encode(encoder, version)?;
        }
        encoder.write_i32(self.timeout_ms);
        Ok(())
    }
}

/// Per-partition result: the new low watermark after deletion + error code.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteRecordsResponsePartition {
    /// Partition index.
    pub partition_index: i32,
    /// The partition's new low watermark (log start offset) after deletion.
    pub low_watermark: i64,
    /// Error code (0 = success).
    pub error_code: i16,
}

impl KafkaEncodable for DeleteRecordsResponsePartition {
    fn encode(&self, encoder: &mut Encoder, _version: i16) -> Result<()> {
        encoder.write_i32(self.partition_index);
        encoder.write_i64(self.low_watermark);
        encoder.write_i16(self.error_code);
        Ok(())
    }
}

/// Per-topic results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteRecordsResponseTopic {
    /// Topic name.
    pub name: String,
    /// Per-partition results.
    pub partitions: Vec<DeleteRecordsResponsePartition>,
}

impl KafkaEncodable for DeleteRecordsResponseTopic {
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()> {
        encoder.write_string(Some(&self.name));
        encoder.write_i32(self.partitions.len() as i32);
        for partition in &self.partitions {
            partition.encode(encoder, version)?;
        }
        Ok(())
    }
}

/// DeleteRecords response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteRecordsResponse {
    /// The duration in milliseconds for which the request was throttled.
    pub throttle_time_ms: i32,
    /// Per-topic results.
    pub topics: Vec<DeleteRecordsResponseTopic>,
}

impl KafkaEncodable for DeleteRecordsResponse {
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()> {
        encoder.write_i32(self.throttle_time_ms);
        encoder.write_i32(self.topics.len() as i32);
        for topic in &self.topics {
            topic.encode(encoder, version)?;
        }
        Ok(())
    }
}

/// Error codes for DeleteRecords.
pub mod error_codes {
    pub const NONE: i16 = 0;
    pub const UNKNOWN_TOPIC_OR_PARTITION: i16 = 3;
    pub const OFFSET_OUT_OF_RANGE: i16 = 1;
    pub const NOT_LEADER_OR_FOLLOWER: i16 = 6;
    pub const REQUEST_TIMED_OUT: i16 = 7;
    pub const UNKNOWN_SERVER_ERROR: i16 = -1;
    pub const POLICY_VIOLATION: i16 = 44;
}
