//! DeleteTopics API types

use serde::{Deserialize, Serialize};
use crate::parser::{Decoder, Encoder, KafkaDecodable, KafkaEncodable};
use chronik_common::{Result, Error};

/// DeleteTopics request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteTopicsRequest {
    /// The topics to delete
    pub topic_names: Vec<String>,
    /// The length of time in milliseconds to wait for the deletions to complete
    pub timeout_ms: i32,
}

impl KafkaDecodable for DeleteTopicsRequest {
    fn decode(decoder: &mut Decoder, version: i16) -> Result<Self> {
        // Read array of topic names
        let topic_count = decoder.read_i32()?;
        if topic_count < 0 {
            return Err(Error::Protocol("Invalid topic count".into()));
        }

        let mut topic_names = Vec::with_capacity(topic_count as usize);
        for _ in 0..topic_count {
            let topic_name = decoder.read_string()?
                .ok_or_else(|| Error::Protocol("Topic name cannot be null".into()))?;
            topic_names.push(topic_name);
        }

        let timeout_ms = decoder.read_i32()?;

        Ok(DeleteTopicsRequest {
            topic_names,
            timeout_ms,
        })
    }
}

impl KafkaEncodable for DeleteTopicsRequest {
    fn encode(&self, encoder: &mut Encoder, _version: i16) -> Result<()> {
        // Write array of topic names
        encoder.write_i32(self.topic_names.len() as i32);
        for topic_name in &self.topic_names {
            encoder.write_string(Some(topic_name));
        }

        encoder.write_i32(self.timeout_ms);
        Ok(())
    }
}

/// DeleteTopics response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteTopicsResponse {
    /// The duration in milliseconds for which the request was throttled
    pub throttle_time_ms: i32,
    /// Results for each topic deletion request
    pub responses: Vec<DeletableTopicResult>,
}

impl KafkaEncodable for DeleteTopicsResponse {
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()> {
        if version >= 1 {
            encoder.write_i32(self.throttle_time_ms);
        }

        // Write array of responses
        encoder.write_i32(self.responses.len() as i32);
        for response in &self.responses {
            response.encode(encoder, version)?;
        }

        Ok(())
    }
}

/// Result for a single topic deletion
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeletableTopicResult {
    /// The topic name
    pub name: String,
    /// The error code
    pub error_code: i16,
    /// The error message (v5+)
    pub error_message: Option<String>,
}

impl KafkaEncodable for DeletableTopicResult {
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()> {
        encoder.write_string(Some(&self.name));
        encoder.write_i16(self.error_code);

        if version >= 5 {
            encoder.write_string(self.error_message.as_deref());
        }

        Ok(())
    }
}

/// Error codes for DeleteTopics
pub mod error_codes {
    pub const NONE: i16 = 0;
    pub const UNKNOWN_SERVER_ERROR: i16 = -1;
    pub const UNKNOWN_TOPIC_OR_PARTITION: i16 = 3;
    pub const REQUEST_TIMED_OUT: i16 = 7;
    pub const INVALID_TOPIC_EXCEPTION: i16 = 17;
    pub const TOPIC_AUTHORIZATION_FAILED: i16 = 29;
    pub const NOT_CONTROLLER: i16 = 41;
    pub const INVALID_REQUEST: i16 = 42;
    pub const TOPIC_DELETION_DISABLED: i16 = 73;
}