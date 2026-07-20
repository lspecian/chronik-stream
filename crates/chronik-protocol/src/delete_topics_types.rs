//! DeleteTopics API types
//!
//! Version notes (Kafka DeleteTopics, api key 20):
//!   - v0:      request `[name STRING]`, response `[name STRING, error_code INT16]`.
//!   - v1-v3:   response gains `throttle_time_ms`. Still non-flexible.
//!   - v4-v5:   FLEXIBLE — compact strings/arrays + trailing tagged fields. v5 adds a
//!              per-result `error_message` (compact nullable string).
//!   - v6:      the request `topics` array becomes `[DeleteTopicState{ name
//!              COMPACT_NULLABLE_STRING, topic_id UUID }]` (delete by name OR by id),
//!              and each response result gains a `topic_id UUID`.
//!
//! The previous implementation encoded/decoded everything as non-flexible (i32 array
//! lengths, non-compact strings, no tagged fields, no topic_id), so v4+ requests were
//! mis-parsed and v4+ responses underflowed the client's parser — no modern Kafka
//! client (which negotiates v6) could delete a topic. This version switches on the
//! API version.

use serde::{Deserialize, Serialize};
use crate::parser::{Decoder, Encoder, KafkaDecodable, KafkaEncodable};
use chronik_common::{Result, Error};

/// A zero UUID (used when a topic is addressed by name and no id is supplied).
pub const ZERO_UUID: [u8; 16] = [0u8; 16];

fn skip_tagged_fields(decoder: &mut Decoder) -> Result<()> {
    let n = decoder.read_unsigned_varint()?;
    for _ in 0..n {
        let _tag = decoder.read_unsigned_varint()?;
        let size = decoder.read_unsigned_varint()? as usize;
        decoder.advance(size)?;
    }
    Ok(())
}

fn read_uuid(decoder: &mut Decoder) -> Result<[u8; 16]> {
    let hi = decoder.read_i64()?;
    let lo = decoder.read_i64()?;
    let mut id = [0u8; 16];
    id[..8].copy_from_slice(&hi.to_be_bytes());
    id[8..].copy_from_slice(&lo.to_be_bytes());
    Ok(id)
}

/// One topic to delete: by name, by id, or both.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteTopicState {
    /// Topic name (present unless the client deletes purely by id).
    pub name: Option<String>,
    /// Topic id (v6+; zero when the client deletes by name).
    pub topic_id: [u8; 16],
}

/// DeleteTopics request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteTopicsRequest {
    /// The topics to delete.
    pub topics: Vec<DeleteTopicState>,
    /// The length of time in milliseconds to wait for the deletions to complete.
    pub timeout_ms: i32,
}

impl DeleteTopicsRequest {
    /// Names of topics addressed by name (ignores pure id-based entries).
    pub fn topic_names(&self) -> Vec<String> {
        self.topics.iter().filter_map(|t| t.name.clone()).collect()
    }
}

impl KafkaDecodable for DeleteTopicsRequest {
    fn decode(decoder: &mut Decoder, version: i16) -> Result<Self> {
        let flexible = version >= 4;
        let mut topics = Vec::new();

        if version >= 6 {
            // Compact array of DeleteTopicState.
            let n = (decoder.read_unsigned_varint()? as i64) - 1;
            for _ in 0..n.max(0) {
                let name = decoder.read_compact_string()?;
                let topic_id = read_uuid(decoder)?;
                skip_tagged_fields(decoder)?;
                topics.push(DeleteTopicState { name, topic_id });
            }
        } else {
            let n = if flexible {
                (decoder.read_unsigned_varint()? as i64) - 1
            } else {
                decoder.read_i32()? as i64
            };
            if n < 0 {
                return Err(Error::Protocol("Invalid topic count".into()));
            }
            for _ in 0..n {
                let name = if flexible {
                    decoder.read_compact_string()?
                } else {
                    decoder.read_string()?
                };
                topics.push(DeleteTopicState { name, topic_id: ZERO_UUID });
            }
        }

        let timeout_ms = decoder.read_i32()?;

        if flexible {
            skip_tagged_fields(decoder)?;
        }

        Ok(DeleteTopicsRequest { topics, timeout_ms })
    }
}

impl KafkaEncodable for DeleteTopicsRequest {
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()> {
        let flexible = version >= 4;
        if version >= 6 {
            encoder.write_unsigned_varint(self.topics.len() as u32 + 1);
            for t in &self.topics {
                encoder.write_compact_string(t.name.as_deref());
                encoder.write_raw_bytes(&t.topic_id);
                encoder.write_tagged_fields();
            }
        } else if flexible {
            encoder.write_unsigned_varint(self.topics.len() as u32 + 1);
            for t in &self.topics {
                encoder.write_compact_string(t.name.as_deref());
            }
        } else {
            encoder.write_i32(self.topics.len() as i32);
            for t in &self.topics {
                encoder.write_string(t.name.as_deref());
            }
        }
        encoder.write_i32(self.timeout_ms);
        if flexible {
            encoder.write_tagged_fields();
        }
        Ok(())
    }
}

/// DeleteTopics response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteTopicsResponse {
    /// The duration in milliseconds for which the request was throttled.
    pub throttle_time_ms: i32,
    /// Results for each topic deletion request.
    pub responses: Vec<DeletableTopicResult>,
}

impl KafkaEncodable for DeleteTopicsResponse {
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()> {
        let flexible = version >= 4;
        if version >= 1 {
            encoder.write_i32(self.throttle_time_ms);
        }
        if flexible {
            encoder.write_unsigned_varint(self.responses.len() as u32 + 1);
        } else {
            encoder.write_i32(self.responses.len() as i32);
        }
        for response in &self.responses {
            response.encode(encoder, version)?;
        }
        if flexible {
            encoder.write_tagged_fields();
        }
        Ok(())
    }
}

/// Result for a single topic deletion
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeletableTopicResult {
    /// The topic name (nullable from v6 — a pure id-based delete may echo a null name).
    pub name: Option<String>,
    /// The topic id (v6+).
    pub topic_id: [u8; 16],
    /// The error code.
    pub error_code: i16,
    /// The error message (v5+).
    pub error_message: Option<String>,
}

impl KafkaEncodable for DeletableTopicResult {
    fn encode(&self, encoder: &mut Encoder, version: i16) -> Result<()> {
        let flexible = version >= 4;
        if flexible {
            encoder.write_compact_string(self.name.as_deref());
        } else {
            // v0-v3: non-nullable name.
            encoder.write_string(Some(self.name.as_deref().unwrap_or("")));
        }
        if version >= 6 {
            encoder.write_raw_bytes(&self.topic_id);
        }
        encoder.write_i16(self.error_code);
        if version >= 5 {
            // v5+ are all flexible → compact nullable string.
            encoder.write_compact_string(self.error_message.as_deref());
        }
        if flexible {
            encoder.write_tagged_fields();
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
