// DescribeLogDirs API types for Kafka protocol
// API key 35, version 0-4

use bytes::{Buf, BufMut, BytesMut};
use chronik_common::Result;
use serde::{Deserialize, Serialize};

// DescribeLogDirs Request Types

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DescribeLogDirsRequest {
    pub topics: Option<Vec<DescribeLogDirsTopic>>,  // null = all topics
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DescribeLogDirsTopic {
    pub topic: String,
    pub partitions: Vec<i32>,
}

// DescribeLogDirs Response Types

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DescribeLogDirsResponse {
    pub throttle_time_ms: i32,
    pub error_code: i16,  // v4+
    pub results: Vec<DescribeLogDirsResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DescribeLogDirsResult {
    pub error_code: i16,
    pub log_dir: String,
    pub topics: Vec<DescribeLogDirsTopicResult>,
    pub total_bytes: i64,  // v4+ - total size of the log directory
    pub usable_bytes: i64,  // v4+ - usable bytes in the log directory
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DescribeLogDirsTopicResult {
    pub topic: String,
    pub partitions: Vec<DescribeLogDirsPartitionResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DescribeLogDirsPartitionResult {
    pub partition: i32,
    pub size: i64,
    pub offset_lag: i64,
    pub is_future_key: bool,  // v1+
}

// Decoder implementations

impl DescribeLogDirsRequest {
    pub fn decode(decoder: &mut crate::parser::Decoder, version: i16) -> Result<Self> {
        // Topics array (nullable)
        let topics = if version >= 2 {
            // Compact array in v2+
            let topic_count = decoder.read_unsigned_varint()?;
            if topic_count == 0 {
                None
            } else {
                let mut topics = Vec::with_capacity((topic_count - 1) as usize);
                for _ in 0..(topic_count - 1) {
                    let topic = decoder.read_compact_string()?.unwrap_or_default();

                    let partition_count = decoder.read_unsigned_varint()?;
                    let mut partitions = Vec::with_capacity((partition_count - 1) as usize);
                    for _ in 0..(partition_count - 1) {
                        partitions.push(decoder.read_i32()?);
                    }

                    topics.push(DescribeLogDirsTopic { topic, partitions });

                    // Tagged fields
                    let _tag_count = decoder.read_unsigned_varint()?;
                }
                Some(topics)
            }
        } else {
            // Regular array in v0-1
            let topic_count = decoder.read_i32()?;
            if topic_count < 0 {
                None
            } else {
                let mut topics = Vec::with_capacity(topic_count as usize);
                for _ in 0..topic_count {
                    let topic = decoder.read_string()?.unwrap_or_default();

                    let partition_count = decoder.read_i32()?;
                    let mut partitions = Vec::with_capacity(partition_count as usize);
                    for _ in 0..partition_count {
                        partitions.push(decoder.read_i32()?);
                    }

                    topics.push(DescribeLogDirsTopic { topic, partitions });
                }
                Some(topics)
            }
        };

        // Tagged fields in v2+
        if version >= 2 {
            let _tag_count = decoder.read_unsigned_varint()?;
        }

        Ok(DescribeLogDirsRequest { topics })
    }
}

// Encoder implementations

impl DescribeLogDirsResponse {
    pub fn encode_to_bytes(&self, version: i16) -> Result<BytesMut> {
        let mut buffer = BytesMut::new();

        // Throttle time
        buffer.put_i32(self.throttle_time_ms);

        // Error code (v4+)
        if version >= 4 {
            buffer.put_i16(self.error_code);
        }

        // Results array
        if version >= 2 {
            // Compact array
            buffer.put_u8((self.results.len() + 1) as u8);
        } else {
            buffer.put_i32(self.results.len() as i32);
        }

        for result in &self.results {
            // Error code
            buffer.put_i16(result.error_code);

            // Log dir
            if version >= 2 {
                // Compact string
                let bytes = result.log_dir.as_bytes();
                buffer.put_u8((bytes.len() + 1) as u8);
                buffer.put_slice(bytes);
            } else {
                let bytes = result.log_dir.as_bytes();
                buffer.put_i16(bytes.len() as i16);
                buffer.put_slice(bytes);
            }

            // Topics array
            if version >= 2 {
                buffer.put_u8((result.topics.len() + 1) as u8);
            } else {
                buffer.put_i32(result.topics.len() as i32);
            }

            for topic in &result.topics {
                // Topic name
                if version >= 2 {
                    let bytes = topic.topic.as_bytes();
                    buffer.put_u8((bytes.len() + 1) as u8);
                    buffer.put_slice(bytes);
                } else {
                    let bytes = topic.topic.as_bytes();
                    buffer.put_i16(bytes.len() as i16);
                    buffer.put_slice(bytes);
                }

                // Partitions array
                if version >= 2 {
                    buffer.put_u8((topic.partitions.len() + 1) as u8);
                } else {
                    buffer.put_i32(topic.partitions.len() as i32);
                }

                for partition in &topic.partitions {
                    buffer.put_i32(partition.partition);
                    buffer.put_i64(partition.size);
                    buffer.put_i64(partition.offset_lag);

                    // is_future_key (v1+)
                    if version >= 1 {
                        buffer.put_i8(if partition.is_future_key { 1 } else { 0 });
                    }

                    // Tagged fields in v2+
                    if version >= 2 {
                        buffer.put_u8(0);  // No tagged fields
                    }
                }

                // Tagged fields in v2+
                if version >= 2 {
                    buffer.put_u8(0);  // No tagged fields
                }
            }

            // Total bytes and usable bytes (v4+)
            if version >= 4 {
                buffer.put_i64(result.total_bytes);
                buffer.put_i64(result.usable_bytes);
            }

            // Tagged fields in v2+
            if version >= 2 {
                buffer.put_u8(0);  // No tagged fields
            }
        }

        // Tagged fields in v2+
        if version >= 2 {
            buffer.put_u8(0);  // No tagged fields
        }

        Ok(buffer)
    }
}

// Error codes specific to DescribeLogDirs
pub mod error_codes {
    pub const NONE: i16 = 0;
    pub const UNKNOWN_TOPIC_OR_PARTITION: i16 = 3;
    pub const KAFKA_STORAGE_ERROR: i16 = 56;
    pub const LOG_DIR_NOT_FOUND: i16 = 57;
    pub const NOT_CONTROLLER: i16 = 41;
    pub const UNKNOWN_SERVER_ERROR: i16 = -1;
}