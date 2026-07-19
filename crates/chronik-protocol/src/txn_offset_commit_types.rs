//! TxnOffsetCommit API types
//!
//! TxnOffsetCommit (API 28) is flexible from v3 (KIP-447 also added
//! generation_id / member_id / group_instance_id to the request at v3). Getting
//! the v3 wire format wrong makes the Java producer's `sendOffsetsToTransaction`
//! hang: the request must be read as compact strings/arrays with trailing tagged
//! fields, and the response must be written the same way or the client's parser
//! underflows. v0-v2 use the classic non-compact layout.

use serde::{Deserialize, Serialize};
use crate::parser::{Decoder, Encoder, KafkaDecodable, KafkaEncodable};
use chronik_common::{Result, Error};

/// Read and discard a body/struct's trailing tagged fields (flexible versions).
fn skip_tagged_fields(decoder: &mut Decoder) -> Result<()> {
    let tag_count = decoder.read_unsigned_varint()?;
    for _ in 0..tag_count {
        let _tag_id = decoder.read_unsigned_varint()?;
        let tag_size = decoder.read_unsigned_varint()? as usize;
        decoder.advance(tag_size)?;
    }
    Ok(())
}

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
        let flexible = version >= 3;
        let partition_index = decoder.read_i32()?;
        let committed_offset = decoder.read_i64()?;

        let committed_leader_epoch = if version >= 2 {
            Some(decoder.read_i32()?)
        } else {
            None
        };

        let metadata = if flexible {
            decoder.read_compact_string()?
        } else {
            decoder.read_string()?
        };

        if flexible {
            skip_tagged_fields(decoder)?;
        }

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
        let flexible = version >= 3;
        encoder.write_i32(self.partition_index);
        encoder.write_i64(self.committed_offset);

        if version >= 2 {
            encoder.write_i32(self.committed_leader_epoch.unwrap_or(-1));
        }

        if flexible {
            encoder.write_compact_string(self.metadata.as_deref());
            encoder.write_tagged_fields();
        } else {
            encoder.write_string(self.metadata.as_deref());
        }
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
        let flexible = version >= 3;
        let name = if flexible {
            decoder.read_compact_string()?
        } else {
            decoder.read_string()?
        }.ok_or_else(|| Error::Protocol("Topic name cannot be null".into()))?;

        let partition_count = if flexible {
            (decoder.read_unsigned_varint()? as i32) - 1
        } else {
            decoder.read_i32()?
        };
        if partition_count < 0 {
            return Err(Error::Protocol("Invalid partition count".into()));
        }

        let mut partitions = Vec::with_capacity(partition_count as usize);
        for _ in 0..partition_count {
            partitions.push(TxnOffsetCommitPartition::decode(decoder, version)?);
        }

        if flexible {
            skip_tagged_fields(decoder)?;
        }

        Ok(TxnOffsetCommitTopic {
            name,
            partitions,
        })
    }
}

impl KafkaEncodable for TxnOffsetCommitTopic {
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()> {
        let flexible = version >= 3;
        if flexible {
            encoder.write_compact_string(Some(&self.name));
            encoder.write_unsigned_varint(self.partitions.len() as u32 + 1);
        } else {
            encoder.write_string(Some(&self.name));
            encoder.write_i32(self.partitions.len() as i32);
        }
        for partition in &self.partitions {
            partition.encode(encoder, version)?;
        }
        if flexible {
            encoder.write_tagged_fields();
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
    /// Consumer group generation (v3+, KIP-447). Unused by the handler but must be
    /// parsed to stay aligned with the rest of the request.
    pub generation_id: i32,
    /// Consumer group member id (v3+).
    pub member_id: Option<String>,
    /// Consumer group static instance id (v3+, nullable).
    pub group_instance_id: Option<String>,
    /// Topics to commit offsets for
    pub topics: Vec<TxnOffsetCommitTopic>,
}

impl KafkaDecodable for TxnOffsetCommitRequest {
    fn decode(decoder: &mut Decoder, version: i16) -> Result<Self> {
        let flexible = version >= 3;
        let transactional_id = if flexible {
            decoder.read_compact_string()?
        } else {
            decoder.read_string()?
        }.ok_or_else(|| Error::Protocol("Transactional ID cannot be null".into()))?;
        let consumer_group_id = if flexible {
            decoder.read_compact_string()?
        } else {
            decoder.read_string()?
        }.ok_or_else(|| Error::Protocol("Consumer group ID cannot be null".into()))?;
        let producer_id = decoder.read_i64()?;
        let producer_epoch = decoder.read_i16()?;

        // v3+ (KIP-447) adds generation_id, member_id, group_instance_id.
        let (generation_id, member_id, group_instance_id) = if version >= 3 {
            let generation_id = decoder.read_i32()?;
            let member_id = decoder.read_compact_string()?;
            let group_instance_id = decoder.read_compact_string()?;
            (generation_id, member_id, group_instance_id)
        } else {
            (-1, None, None)
        };

        let topic_count = if flexible {
            (decoder.read_unsigned_varint()? as i32) - 1
        } else {
            decoder.read_i32()?
        };
        if topic_count < 0 {
            return Err(Error::Protocol("Invalid topic count".into()));
        }

        let mut topics = Vec::with_capacity(topic_count as usize);
        for _ in 0..topic_count {
            topics.push(TxnOffsetCommitTopic::decode(decoder, version)?);
        }

        if flexible {
            skip_tagged_fields(decoder)?;
        }

        Ok(TxnOffsetCommitRequest {
            transactional_id,
            consumer_group_id,
            producer_id,
            producer_epoch,
            generation_id,
            member_id,
            group_instance_id,
            topics,
        })
    }
}

impl KafkaEncodable for TxnOffsetCommitRequest {
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()> {
        let flexible = version >= 3;
        if flexible {
            encoder.write_compact_string(Some(&self.transactional_id));
            encoder.write_compact_string(Some(&self.consumer_group_id));
        } else {
            encoder.write_string(Some(&self.transactional_id));
            encoder.write_string(Some(&self.consumer_group_id));
        }
        encoder.write_i64(self.producer_id);
        encoder.write_i16(self.producer_epoch);
        if version >= 3 {
            encoder.write_i32(self.generation_id);
            encoder.write_compact_string(self.member_id.as_deref());
            encoder.write_compact_string(self.group_instance_id.as_deref());
        }
        if flexible {
            encoder.write_unsigned_varint(self.topics.len() as u32 + 1);
        } else {
            encoder.write_i32(self.topics.len() as i32);
        }
        for topic in &self.topics {
            topic.encode(encoder, version)?;
        }
        if flexible {
            encoder.write_tagged_fields();
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
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()> {
        encoder.write_i32(self.partition_index);
        encoder.write_i16(self.error_code);
        if version >= 3 {
            encoder.write_tagged_fields();
        }
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
        let flexible = version >= 3;
        if flexible {
            encoder.write_compact_string(Some(&self.name));
            encoder.write_unsigned_varint(self.partitions.len() as u32 + 1);
        } else {
            encoder.write_string(Some(&self.name));
            encoder.write_i32(self.partitions.len() as i32);
        }
        for partition in &self.partitions {
            partition.encode(encoder, version)?;
        }
        if flexible {
            encoder.write_tagged_fields();
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
        let flexible = version >= 3;
        encoder.write_i32(self.throttle_time_ms);
        if flexible {
            encoder.write_unsigned_varint(self.topics.len() as u32 + 1);
        } else {
            encoder.write_i32(self.topics.len() as i32);
        }
        for topic in &self.topics {
            topic.encode(encoder, version)?;
        }
        if flexible {
            encoder.write_tagged_fields();
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
