// DeleteRecords API types for Kafka protocol
// API key 21, version 0-2

use bytes::{Buf, BufMut, BytesMut};
use chronik_common::Result;
use serde::{Deserialize, Serialize};

// DeleteRecords Request Types

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteRecordsRequest {
    pub topics: Vec<DeleteRecordsTopic>,
    pub timeout_ms: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteRecordsTopic {
    pub name: String,
    pub partitions: Vec<DeleteRecordsPartition>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteRecordsPartition {
    pub partition_index: i32,
    pub offset: i64,  // Delete all records with offset < this value
}

// DeleteRecords Response Types

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteRecordsResponse {
    pub throttle_time_ms: i32,
    pub topics: Vec<DeleteRecordsTopicResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteRecordsTopicResult {
    pub name: String,
    pub partitions: Vec<DeleteRecordsPartitionResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteRecordsPartitionResult {
    pub partition_index: i32,
    pub low_watermark: i64,  // New low watermark after deletion
    pub error_code: i16,
    pub error_message: Option<String>,  // Available in v2+
}

// Decoder implementations

impl DeleteRecordsRequest {
    pub fn decode(decoder: &mut crate::parser::Decoder, version: i16) -> Result<Self> {
        // Topics array
        let topic_count = decoder.read_i32()?;
        let mut topics = Vec::with_capacity(topic_count as usize);

        for _ in 0..topic_count {
            // Topic name
            let name = decoder.read_string()?.unwrap_or_default();

            // Partitions array
            let partition_count = decoder.read_i32()?;
            let mut partitions = Vec::with_capacity(partition_count as usize);

            for _ in 0..partition_count {
                let partition_index = decoder.read_i32()?;
                let offset = decoder.read_i64()?;

                partitions.push(DeleteRecordsPartition {
                    partition_index,
                    offset,
                });

                // Tagged fields in v2+
                if version >= 2 {
                    let _tag_count = decoder.read_unsigned_varint()?;
                    // Skip any tagged fields for now
                }
            }

            topics.push(DeleteRecordsTopic {
                name,
                partitions,
            });

            // Tagged fields in v2+
            if version >= 2 {
                let _tag_count = decoder.read_unsigned_varint()?;
                // Skip any tagged fields for now
            }
        }

        // Timeout
        let timeout_ms = decoder.read_i32()?;

        // Tagged fields in v2+
        if version >= 2 {
            let _tag_count = decoder.read_unsigned_varint()?;
            // Skip any tagged fields for now
        }

        Ok(DeleteRecordsRequest {
            topics,
            timeout_ms,
        })
    }
}

// Encoder implementations

impl DeleteRecordsResponse {
    pub fn encode_to_bytes(&self, version: i16) -> Result<BytesMut> {
        let mut buffer = BytesMut::new();

        // Throttle time
        buffer.put_i32(self.throttle_time_ms);

        // Topics array
        buffer.put_i32(self.topics.len() as i32);

        for topic in &self.topics {
            // Topic name (length + string)
            let name_bytes = topic.name.as_bytes();
            buffer.put_i16(name_bytes.len() as i16);
            buffer.put_slice(name_bytes);

            // Partitions array
            buffer.put_i32(topic.partitions.len() as i32);

            for partition in &topic.partitions {
                buffer.put_i32(partition.partition_index);
                buffer.put_i64(partition.low_watermark);
                buffer.put_i16(partition.error_code);

                // Error message in v2+
                if version >= 2 {
                    // Nullable string (only in v2+)
                    if let Some(ref msg) = partition.error_message {
                        let msg_bytes = msg.as_bytes();
                        buffer.put_i16(msg_bytes.len() as i16);
                        buffer.put_slice(msg_bytes);
                    } else {
                        buffer.put_i16(-1);  // null string
                    }
                    // Tagged fields
                    buffer.put_u8(0); // No tagged fields
                }
            }

            // Tagged fields in v2+
            if version >= 2 {
                buffer.put_u8(0); // No tagged fields
            }
        }

        // Tagged fields in v2+
        if version >= 2 {
            buffer.put_u8(0); // No tagged fields
        }

        Ok(buffer)
    }
}

// Error codes specific to DeleteRecords
pub mod error_codes {
    pub const NONE: i16 = 0;
    pub const UNKNOWN_TOPIC_OR_PARTITION: i16 = 3;
    pub const NOT_LEADER_OR_FOLLOWER: i16 = 6;
    pub const REQUEST_TIMED_OUT: i16 = 7;
    pub const REPLICA_NOT_AVAILABLE: i16 = 9;
    pub const OFFSET_OUT_OF_RANGE: i16 = 1;
    pub const INVALID_REQUEST: i16 = 42;
    pub const POLICY_VIOLATION: i16 = 44;
    pub const LOG_DIR_NOT_FOUND: i16 = 57;
    pub const NOT_AUTHORIZED: i16 = 29;
}