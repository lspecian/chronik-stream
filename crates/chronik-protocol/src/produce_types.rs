//! Produce API types

use serde::{Deserialize, Serialize};
use crate::parser::{Decoder, Encoder, KafkaDecodable, KafkaEncodable};
use chronik_common::{Result, Error};

/// Produce request partition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProduceRequestPartition {
    /// Partition index
    pub index: i32,
    /// Encoded records data
    pub records: Vec<u8>,
}

impl KafkaDecodable for ProduceRequestPartition {
    fn decode(decoder: &mut Decoder, _version: i16) -> Result<Self> {
        let index = decoder.read_i32()?;
        let records_size = decoder.read_i32()?;

        if records_size < 0 {
            return Err(Error::Protocol("Invalid records size".into()));
        }

        let records_bytes = decoder.read_bytes()?
            .ok_or_else(|| Error::Protocol("Records cannot be null".into()))?;

        // Convert Bytes to Vec<u8>
        let records = records_bytes.to_vec();

        Ok(ProduceRequestPartition {
            index,
            records,
        })
    }
}

impl KafkaEncodable for ProduceRequestPartition {
    fn encode(&self, encoder: &mut Encoder, _version: i16) -> Result<()> {
        encoder.write_i32(self.index);
        encoder.write_i32(self.records.len() as i32);
        encoder.write_raw_bytes(&self.records);
        Ok(())
    }
}

/// Produce request topic
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProduceRequestTopic {
    /// Topic name
    pub name: String,
    /// Partitions to produce to
    pub partitions: Vec<ProduceRequestPartition>,
}

impl KafkaDecodable for ProduceRequestTopic {
    fn decode(decoder: &mut Decoder, version: i16) -> Result<Self> {
        let name = decoder.read_string()?
            .ok_or_else(|| Error::Protocol("Topic name cannot be null".into()))?;

        let partition_count = decoder.read_i32()?;
        if partition_count < 0 {
            return Err(Error::Protocol("Invalid partition count".into()));
        }

        let mut partitions = Vec::with_capacity(partition_count as usize);
        for _ in 0..partition_count {
            partitions.push(ProduceRequestPartition::decode(decoder, version)?);
        }

        Ok(ProduceRequestTopic {
            name,
            partitions,
        })
    }
}

impl KafkaEncodable for ProduceRequestTopic {
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()> {
        encoder.write_string(Some(&self.name));
        encoder.write_i32(self.partitions.len() as i32);
        for partition in &self.partitions {
            partition.encode(encoder, version)?;
        }
        Ok(())
    }
}

/// Produce request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProduceRequest {
    /// Transactional ID (v3+)
    pub transactional_id: Option<String>,
    /// Required acknowledgments
    pub acks: i16,
    /// Timeout in milliseconds
    pub timeout_ms: i32,
    /// Topics to produce to
    pub topics: Vec<ProduceRequestTopic>,
}

impl KafkaDecodable for ProduceRequest {
    fn decode(decoder: &mut Decoder, version: i16) -> Result<Self> {
        let transactional_id = if version >= 3 {
            decoder.read_string()?
        } else {
            None
        };

        let acks = decoder.read_i16()?;
        let timeout_ms = decoder.read_i32()?;

        let topic_count = decoder.read_i32()?;
        if topic_count < 0 {
            return Err(Error::Protocol("Invalid topic count".into()));
        }

        let mut topics = Vec::with_capacity(topic_count as usize);
        for _ in 0..topic_count {
            topics.push(ProduceRequestTopic::decode(decoder, version)?);
        }

        Ok(ProduceRequest {
            transactional_id,
            acks,
            timeout_ms,
            topics,
        })
    }
}

impl KafkaEncodable for ProduceRequest {
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()> {
        if version >= 3 {
            encoder.write_string(self.transactional_id.as_deref());
        }

        encoder.write_i16(self.acks);
        encoder.write_i32(self.timeout_ms);
        encoder.write_i32(self.topics.len() as i32);

        for topic in &self.topics {
            topic.encode(encoder, version)?;
        }

        Ok(())
    }
}

/// Produce response partition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProduceResponsePartition {
    /// Partition index
    pub index: i32,
    /// Error code
    pub error_code: i16,
    /// Base offset of the first message
    pub base_offset: i64,
    /// Log append time (v2+)
    pub log_append_time: i64,
    /// Log start offset (v5+)
    pub log_start_offset: i64,
    /// Record errors (v8+)
    pub record_errors: Option<Vec<ProduceResponseRecordError>>,
    /// Error message (v8+)
    pub error_message: Option<String>,
}

impl KafkaEncodable for ProduceResponsePartition {
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()> {
        encoder.write_i32(self.index);
        encoder.write_i16(self.error_code);
        encoder.write_i64(self.base_offset);

        if version >= 2 {
            encoder.write_i64(self.log_append_time);
        }

        if version >= 5 {
            encoder.write_i64(self.log_start_offset);
        }

        if version >= 8 {
            if let Some(ref record_errors) = self.record_errors {
                encoder.write_i32(record_errors.len() as i32);
                for error in record_errors {
                    error.encode(encoder, version)?;
                }
            } else {
                encoder.write_i32(0);
            }

            encoder.write_string(self.error_message.as_deref());
        }

        Ok(())
    }
}

/// Produce response record error (v8+)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProduceResponseRecordError {
    /// Batch index
    pub batch_index: i32,
    /// Batch index error message
    pub batch_index_error_message: Option<String>,
}

impl KafkaEncodable for ProduceResponseRecordError {
    fn encode(&self, encoder: &mut Encoder, _version: i16) -> Result<()> {
        encoder.write_i32(self.batch_index);
        encoder.write_string(self.batch_index_error_message.as_deref());
        Ok(())
    }
}

/// Produce response topic
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProduceResponseTopic {
    /// Topic name
    pub name: String,
    /// Partition responses
    pub partitions: Vec<ProduceResponsePartition>,
}

impl KafkaEncodable for ProduceResponseTopic {
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()> {
        encoder.write_string(Some(&self.name));
        encoder.write_i32(self.partitions.len() as i32);
        for partition in &self.partitions {
            partition.encode(encoder, version)?;
        }
        Ok(())
    }
}

/// Produce response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProduceResponse {
    /// Throttle time in milliseconds (v1+)
    pub throttle_time_ms: i32,
    /// Topic responses
    pub topics: Vec<ProduceResponseTopic>,
}

impl ProduceResponse {
    pub fn make_response(
        throttle_time_ms: i32,
        topics: Vec<ProduceResponseTopic>,
    ) -> Self {
        Self {
            throttle_time_ms,
            topics,
        }
    }
}

impl KafkaEncodable for ProduceResponse {
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()> {
        encoder.write_i32(self.topics.len() as i32);
        for topic in &self.topics {
            topic.encode(encoder, version)?;
        }

        if version >= 1 {
            encoder.write_i32(self.throttle_time_ms);
        }

        Ok(())
    }
}

/// Error codes for Produce API
pub mod error_codes {
    pub const NONE: i16 = 0;
    pub const OFFSET_OUT_OF_RANGE: i16 = 1;
    pub const CORRUPT_MESSAGE: i16 = 2;
    pub const UNKNOWN_TOPIC_OR_PARTITION: i16 = 3;
    pub const INVALID_FETCH_SIZE: i16 = 4;
    pub const LEADER_NOT_AVAILABLE: i16 = 5;
    pub const NOT_LEADER_FOR_PARTITION: i16 = 6;
    pub const REQUEST_TIMED_OUT: i16 = 7;
    pub const BROKER_NOT_AVAILABLE: i16 = 8;
    pub const REPLICA_NOT_AVAILABLE: i16 = 9;
    pub const MESSAGE_TOO_LARGE: i16 = 10;
    pub const STALE_CONTROLLER_EPOCH: i16 = 11;
    pub const OFFSET_METADATA_TOO_LARGE: i16 = 12;
    pub const NETWORK_EXCEPTION: i16 = 13;
    pub const COORDINATOR_LOAD_IN_PROGRESS: i16 = 14;
    pub const COORDINATOR_NOT_AVAILABLE: i16 = 15;
    pub const NOT_COORDINATOR: i16 = 16;
    pub const INVALID_TOPIC_EXCEPTION: i16 = 17;
    pub const RECORD_LIST_TOO_LARGE: i16 = 18;
    pub const NOT_ENOUGH_REPLICAS: i16 = 19;
    pub const NOT_ENOUGH_REPLICAS_AFTER_APPEND: i16 = 20;
    pub const INVALID_REQUIRED_ACKS: i16 = 21;
    pub const ILLEGAL_GENERATION: i16 = 22;
    pub const INCONSISTENT_GROUP_PROTOCOL: i16 = 23;
    pub const INVALID_GROUP_ID: i16 = 24;
    pub const UNKNOWN_MEMBER_ID: i16 = 25;
    pub const INVALID_SESSION_TIMEOUT: i16 = 26;
    pub const REBALANCE_IN_PROGRESS: i16 = 27;
    pub const INVALID_COMMIT_OFFSET_SIZE: i16 = 28;
    pub const TOPIC_AUTHORIZATION_FAILED: i16 = 29;
    pub const GROUP_AUTHORIZATION_FAILED: i16 = 30;
    pub const CLUSTER_AUTHORIZATION_FAILED: i16 = 31;
    pub const INVALID_TIMESTAMP: i16 = 32;
    pub const UNSUPPORTED_SASL_MECHANISM: i16 = 33;
    pub const ILLEGAL_SASL_STATE: i16 = 34;
    pub const UNSUPPORTED_VERSION: i16 = 35;
    pub const TOPIC_ALREADY_EXISTS: i16 = 36;
    pub const INVALID_PARTITIONS: i16 = 37;
    pub const INVALID_REPLICATION_FACTOR: i16 = 38;
    pub const INVALID_REPLICA_ASSIGNMENT: i16 = 39;
    pub const INVALID_CONFIG: i16 = 40;
    pub const NOT_CONTROLLER: i16 = 41;
    pub const INVALID_REQUEST: i16 = 42;
    pub const UNSUPPORTED_FOR_MESSAGE_FORMAT: i16 = 43;
    pub const POLICY_VIOLATION: i16 = 44;
    pub const OUT_OF_ORDER_SEQUENCE_NUMBER: i16 = 45;
    pub const DUPLICATE_SEQUENCE_NUMBER: i16 = 46;
    pub const INVALID_PRODUCER_EPOCH: i16 = 47;
    pub const INVALID_TXN_STATE: i16 = 48;
    pub const INVALID_PRODUCER_ID_MAPPING: i16 = 49;
    pub const INVALID_TRANSACTION_TIMEOUT: i16 = 50;
    pub const CONCURRENT_TRANSACTIONS: i16 = 51;
    pub const TRANSACTION_COORDINATOR_FENCED: i16 = 52;
    pub const TRANSACTIONAL_ID_AUTHORIZATION_FAILED: i16 = 53;
    pub const SECURITY_DISABLED: i16 = 54;
    pub const OPERATION_NOT_ATTEMPTED: i16 = 55;
    pub const KAFKA_STORAGE_ERROR: i16 = 56;
    pub const LOG_DIR_NOT_FOUND: i16 = 57;
    pub const SASL_AUTHENTICATION_FAILED: i16 = 58;
    pub const UNKNOWN_PRODUCER_ID: i16 = 59;
    pub const REASSIGNMENT_IN_PROGRESS: i16 = 60;
    pub const DELEGATION_TOKEN_AUTH_DISABLED: i16 = 61;
    pub const DELEGATION_TOKEN_NOT_FOUND: i16 = 62;
    pub const DELEGATION_TOKEN_OWNER_MISMATCH: i16 = 63;
    pub const DELEGATION_TOKEN_REQUEST_NOT_ALLOWED: i16 = 64;
    pub const DELEGATION_TOKEN_AUTHORIZATION_FAILED: i16 = 65;
    pub const DELEGATION_TOKEN_EXPIRED: i16 = 66;
    pub const INVALID_PRINCIPAL_TYPE: i16 = 67;
    pub const NON_EMPTY_GROUP: i16 = 68;
    pub const GROUP_ID_NOT_FOUND: i16 = 69;
    pub const FETCH_SESSION_ID_NOT_FOUND: i16 = 70;
    pub const INVALID_FETCH_SESSION_EPOCH: i16 = 71;
    pub const LISTENER_NOT_FOUND: i16 = 72;
    pub const TOPIC_DELETION_DISABLED: i16 = 73;
    pub const FENCED_LEADER_EPOCH: i16 = 74;
    pub const UNKNOWN_LEADER_EPOCH: i16 = 75;
    pub const UNSUPPORTED_COMPRESSION_TYPE: i16 = 76;
    pub const STALE_BROKER_EPOCH: i16 = 77;
    pub const OFFSET_NOT_AVAILABLE: i16 = 78;
    pub const MEMBER_ID_REQUIRED: i16 = 79;
    pub const PREFERRED_LEADER_NOT_AVAILABLE: i16 = 80;
    pub const GROUP_MAX_SIZE_REACHED: i16 = 81;
    pub const FENCED_INSTANCE_ID: i16 = 82;
    pub const ELIGIBLE_LEADERS_NOT_AVAILABLE: i16 = 83;
    pub const ELECTION_NOT_NEEDED: i16 = 84;
    pub const NO_REASSIGNMENT_IN_PROGRESS: i16 = 85;
    pub const GROUP_SUBSCRIBED_TO_TOPIC: i16 = 86;
    pub const INVALID_RECORD: i16 = 87;
    pub const UNSTABLE_OFFSET_COMMIT: i16 = 88;
}