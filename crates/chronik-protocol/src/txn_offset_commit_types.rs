//! TxnOffsetCommit API types

use serde::{Deserialize, Serialize};
use crate::parser::{Decoder, Encoder, KafkaDecodable, KafkaEncodable};
use chronik_common::{Result, Error};

/// TxnOffsetCommit partition request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxnOffsetCommitPartition {
    /// Partition index
    pub partition_index: i32,
    /// Committed offset
    pub committed_offset: i64,
    /// Committed leader epoch (v2+)
    pub committed_leader_epoch: Option<i32>,
    /// Metadata
    pub metadata: Option<String>,
}

impl KafkaDecodable for TxnOffsetCommitPartition {
    fn decode(decoder: &mut Decoder, version: i16) -> Result<Self> {
        let partition_index = decoder.read_i32()?;
        let committed_offset = decoder.read_i64()?;

        let committed_leader_epoch = if version >= 2 {
            Some(decoder.read_i32()?)
        } else {
            None
        };

        let metadata = decoder.read_string()?;

        Ok(TxnOffsetCommitPartition {
            partition_index,
            committed_offset,
            committed_leader_epoch,
            metadata,
        })
    }
}

impl KafkaEncodable for TxnOffsetCommitPartition {
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()> {
        encoder.write_i32(self.partition_index);
        encoder.write_i64(self.committed_offset);

        if version >= 2 {
            encoder.write_i32(self.committed_leader_epoch.unwrap_or(-1));
        }

        encoder.write_string(self.metadata.as_deref());
        Ok(())
    }
}

/// TxnOffsetCommit topic request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxnOffsetCommitTopic {
    /// Topic name
    pub name: String,
    /// Partitions to commit
    pub partitions: Vec<TxnOffsetCommitPartition>,
}

impl KafkaDecodable for TxnOffsetCommitTopic {
    fn decode(decoder: &mut Decoder, version: i16) -> Result<Self> {
        let name = decoder.read_string()?
            .ok_or_else(|| Error::Protocol("Topic name cannot be null".into()))?;

        let partition_count = decoder.read_i32()?;
        if partition_count < 0 {
            return Err(Error::Protocol("Invalid partition count".into()));
        }

        let mut partitions = Vec::with_capacity(partition_count as usize);
        for _ in 0..partition_count {
            partitions.push(TxnOffsetCommitPartition::decode(decoder, version)?);
        }

        Ok(TxnOffsetCommitTopic {
            name,
            partitions,
        })
    }
}

impl KafkaEncodable for TxnOffsetCommitTopic {
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()> {
        encoder.write_string(Some(&self.name));
        encoder.write_i32(self.partitions.len() as i32);
        for partition in &self.partitions {
            partition.encode(encoder, version)?;
        }
        Ok(())
    }
}

/// TxnOffsetCommit request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxnOffsetCommitRequest {
    /// Transactional ID
    pub transactional_id: String,
    /// Consumer group ID
    pub consumer_group_id: String,
    /// Producer ID
    pub producer_id: i64,
    /// Producer epoch
    pub producer_epoch: i16,
    /// Topics to commit offsets for
    pub topics: Vec<TxnOffsetCommitTopic>,
}

impl KafkaDecodable for TxnOffsetCommitRequest {
    fn decode(decoder: &mut Decoder, version: i16) -> Result<Self> {
        let transactional_id = decoder.read_string()?
            .ok_or_else(|| Error::Protocol("Transactional ID cannot be null".into()))?;
        let consumer_group_id = decoder.read_string()?
            .ok_or_else(|| Error::Protocol("Consumer group ID cannot be null".into()))?;
        let producer_id = decoder.read_i64()?;
        let producer_epoch = decoder.read_i16()?;

        let topic_count = decoder.read_i32()?;
        if topic_count < 0 {
            return Err(Error::Protocol("Invalid topic count".into()));
        }

        let mut topics = Vec::with_capacity(topic_count as usize);
        for _ in 0..topic_count {
            topics.push(TxnOffsetCommitTopic::decode(decoder, version)?);
        }

        Ok(TxnOffsetCommitRequest {
            transactional_id,
            consumer_group_id,
            producer_id,
            producer_epoch,
            topics,
        })
    }
}

impl KafkaEncodable for TxnOffsetCommitRequest {
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()> {
        encoder.write_string(Some(&self.transactional_id));
        encoder.write_string(Some(&self.consumer_group_id));
        encoder.write_i64(self.producer_id);
        encoder.write_i16(self.producer_epoch);
        encoder.write_i32(self.topics.len() as i32);
        for topic in &self.topics {
            topic.encode(encoder, version)?;
        }
        Ok(())
    }
}

/// TxnOffsetCommit partition response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxnOffsetCommitPartitionResponse {
    /// Partition index
    pub partition_index: i32,
    /// Error code
    pub error_code: i16,
}

impl KafkaEncodable for TxnOffsetCommitPartitionResponse {
    fn encode(&self, encoder: &mut Encoder, _version: i16) -> Result<()> {
        encoder.write_i32(self.partition_index);
        encoder.write_i16(self.error_code);
        Ok(())
    }
}

/// TxnOffsetCommit topic response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxnOffsetCommitTopicResponse {
    /// Topic name
    pub name: String,
    /// Partition responses
    pub partitions: Vec<TxnOffsetCommitPartitionResponse>,
}

impl KafkaEncodable for TxnOffsetCommitTopicResponse {
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()> {
        encoder.write_string(Some(&self.name));
        encoder.write_i32(self.partitions.len() as i32);
        for partition in &self.partitions {
            partition.encode(encoder, version)?;
        }
        Ok(())
    }
}

/// TxnOffsetCommit response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxnOffsetCommitResponse {
    /// Throttle time in milliseconds
    pub throttle_time_ms: i32,
    /// Topic responses
    pub topics: Vec<TxnOffsetCommitTopicResponse>,
}

impl KafkaEncodable for TxnOffsetCommitResponse {
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()> {
        encoder.write_i32(self.throttle_time_ms);
        encoder.write_i32(self.topics.len() as i32);
        for topic in &self.topics {
            topic.encode(encoder, version)?;
        }
        Ok(())
    }
}

/// Error codes for TxnOffsetCommit
pub mod error_codes {
    pub const NONE: i16 = 0;
    pub const COORDINATOR_NOT_AVAILABLE: i16 = 15;
    pub const NOT_COORDINATOR: i16 = 16;
    pub const ILLEGAL_GENERATION: i16 = 22;
    pub const UNKNOWN_MEMBER_ID: i16 = 25;
    pub const INVALID_SESSION_TIMEOUT: i16 = 26;
    pub const REBALANCE_IN_PROGRESS: i16 = 27;
    pub const INVALID_COMMIT_OFFSET_SIZE: i16 = 28;
    pub const TOPIC_AUTHORIZATION_FAILED: i16 = 29;
    pub const GROUP_AUTHORIZATION_FAILED: i16 = 30;
    pub const OFFSET_METADATA_TOO_LARGE: i16 = 12;
    pub const INVALID_PRODUCER_ID_MAPPING: i16 = 49;
    pub const INVALID_PRODUCER_EPOCH: i16 = 47;
    pub const TRANSACTIONAL_ID_AUTHORIZATION_FAILED: i16 = 53;
}