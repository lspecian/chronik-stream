//! CreatePartitions API types

use serde::{Deserialize, Serialize};
use crate::parser::{Decoder, Encoder, KafkaDecodable, KafkaEncodable};
use chronik_common::{Result, Error};

/// Partition assignment for new partitions
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewPartitionAssignment {
    /// Broker IDs for replicas
    pub broker_ids: Vec<i32>,
}

impl KafkaDecodable for NewPartitionAssignment {
    fn decode(decoder: &mut Decoder, _version: i16) -> Result<Self> {
        let broker_count = decoder.read_i32()?;
        if broker_count < 0 {
            return Err(Error::Protocol("Invalid broker count".into()));
        }

        let mut broker_ids = Vec::with_capacity(broker_count as usize);
        for _ in 0..broker_count {
            broker_ids.push(decoder.read_i32()?);
        }

        Ok(NewPartitionAssignment { broker_ids })
    }
}

impl KafkaEncodable for NewPartitionAssignment {
    fn encode(&self, encoder: &mut Encoder, _version: i16) -> Result<()> {
        encoder.write_i32(self.broker_ids.len() as i32);
        for broker_id in &self.broker_ids {
            encoder.write_i32(*broker_id);
        }
        Ok(())
    }
}

/// Topic to create partitions for
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreatePartitionsTopic {
    /// Topic name
    pub name: String,
    /// New total partition count
    pub count: i32,
    /// Optional assignments for new partitions
    pub assignments: Option<Vec<NewPartitionAssignment>>,
}

impl KafkaDecodable for CreatePartitionsTopic {
    fn decode(decoder: &mut Decoder, version: i16) -> Result<Self> {
        let name = decoder.read_string()?
            .ok_or_else(|| Error::Protocol("Topic name cannot be null".into()))?;
        let count = decoder.read_i32()?;

        // Read assignments (nullable array)
        let assignment_count = decoder.read_i32()?;
        let assignments = if assignment_count < 0 {
            None
        } else {
            let mut assignments = Vec::with_capacity(assignment_count as usize);
            for _ in 0..assignment_count {
                assignments.push(NewPartitionAssignment::decode(decoder, version)?);
            }
            Some(assignments)
        };

        Ok(CreatePartitionsTopic {
            name,
            count,
            assignments,
        })
    }
}

impl KafkaEncodable for CreatePartitionsTopic {
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()> {
        encoder.write_string(Some(&self.name));
        encoder.write_i32(self.count);

        // Write assignments (nullable array)
        if let Some(ref assignments) = self.assignments {
            encoder.write_i32(assignments.len() as i32);
            for assignment in assignments {
                assignment.encode(encoder, version)?;
            }
        } else {
            encoder.write_i32(-1); // null array
        }

        Ok(())
    }
}

/// CreatePartitions request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreatePartitionsRequest {
    /// Topics to create partitions for
    pub topics: Vec<CreatePartitionsTopic>,
    /// Timeout in milliseconds
    pub timeout_ms: i32,
    /// Whether to validate only without creating
    pub validate_only: bool,
}

impl KafkaDecodable for CreatePartitionsRequest {
    fn decode(decoder: &mut Decoder, version: i16) -> Result<Self> {
        // Read topics array
        let topic_count = decoder.read_i32()?;
        if topic_count < 0 {
            return Err(Error::Protocol("Invalid topic count".into()));
        }

        let mut topics = Vec::with_capacity(topic_count as usize);
        for _ in 0..topic_count {
            topics.push(CreatePartitionsTopic::decode(decoder, version)?);
        }

        let timeout_ms = decoder.read_i32()?;

        // validate_only is only in v1+
        let validate_only = if version >= 1 {
            decoder.read_i8()? != 0
        } else {
            false
        };

        Ok(CreatePartitionsRequest {
            topics,
            timeout_ms,
            validate_only,
        })
    }
}

impl KafkaEncodable for CreatePartitionsRequest {
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()> {
        // Write topics array
        encoder.write_i32(self.topics.len() as i32);
        for topic in &self.topics {
            topic.encode(encoder, version)?;
        }

        encoder.write_i32(self.timeout_ms);

        if version >= 1 {
            encoder.write_i8(if self.validate_only { 1 } else { 0 });
        }

        Ok(())
    }
}

/// CreatePartitions response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreatePartitionsResponse {
    /// Throttle time in milliseconds
    pub throttle_time_ms: i32,
    /// Results for each topic
    pub results: Vec<CreatePartitionsTopicResult>,
}

impl KafkaEncodable for CreatePartitionsResponse {
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()> {
        encoder.write_i32(self.throttle_time_ms);

        // Write results array
        encoder.write_i32(self.results.len() as i32);
        for result in &self.results {
            result.encode(encoder, version)?;
        }

        Ok(())
    }
}

/// Result for a single topic
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreatePartitionsTopicResult {
    /// Topic name
    pub name: String,
    /// Error code
    pub error_code: i16,
    /// Error message (v1+)
    pub error_message: Option<String>,
}

impl KafkaEncodable for CreatePartitionsTopicResult {
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()> {
        encoder.write_string(Some(&self.name));
        encoder.write_i16(self.error_code);

        // error_message is only in v1+
        if version >= 1 {
            encoder.write_string(self.error_message.as_deref());
        }

        Ok(())
    }
}

/// Error codes for CreatePartitions
pub mod error_codes {
    pub const NONE: i16 = 0;
    pub const TOPIC_AUTHORIZATION_FAILED: i16 = 29;
    pub const INVALID_TOPIC_EXCEPTION: i16 = 17;
    pub const INVALID_PARTITIONS: i16 = 37;
    pub const INVALID_REPLICATION_FACTOR: i16 = 38;
    pub const INVALID_REPLICA_ASSIGNMENT: i16 = 39;
    pub const INVALID_REQUEST: i16 = 42;
    pub const REASSIGNMENT_IN_PROGRESS: i16 = 60;
    pub const UNKNOWN_TOPIC_OR_PARTITION: i16 = 3;
}