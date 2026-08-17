//! OffsetForLeaderEpoch (API 23) — "where did this leader epoch end?"
//!
//! A replica that has been away asks the current leader where its last known
//! epoch ended, and truncates to that offset before resuming. Without it, two
//! logs that agree on offsets and disagree on content have no way to find the
//! point where they diverged (RP-3).
//!
//! Only **v0** is implemented, which is exactly what this broker advertises
//! (`parser.rs`: `VersionRange { min: 0, max: 0 }`). v1 adds a leader epoch to
//! the response and v2 a throttle time; advertising more than is implemented is
//! how clients get handed malformed frames, so the two move together or not at
//! all.
//!
//! ```text
//! Request (v0)                     Response (v0)
//!   topics =>                        topics =>
//!     name         STRING              name           STRING
//!     partitions =>                    partitions =>
//!       partition    INT32               error_code   INT16
//!       leader_epoch INT32               partition    INT32
//!                                        end_offset   INT64
//! ```

use bytes::BytesMut;

use crate::parser::{Decoder, Encoder};
use chronik_common::{Error, Result};

/// One partition's question: where did `leader_epoch` end?
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OffsetForLeaderPartition {
    pub partition: i32,
    pub leader_epoch: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OffsetForLeaderTopic {
    pub name: String,
    pub partitions: Vec<OffsetForLeaderPartition>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OffsetForLeaderEpochRequest {
    pub topics: Vec<OffsetForLeaderTopic>,
}

/// One partition's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OffsetForLeaderPartitionResponse {
    pub error_code: i16,
    pub partition: i32,
    /// Last offset of the requested epoch, or -1 when this broker cannot answer.
    pub end_offset: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OffsetForLeaderTopicResponse {
    pub name: String,
    pub partitions: Vec<OffsetForLeaderPartitionResponse>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OffsetForLeaderEpochResponse {
    pub topics: Vec<OffsetForLeaderTopicResponse>,
}

/// Parse a v0 request body (header already consumed).
pub fn parse_request(decoder: &mut Decoder) -> Result<OffsetForLeaderEpochRequest> {
    let topic_count = decoder.read_i32()?;
    if topic_count < 0 {
        return Err(Error::Protocol(format!(
            "OffsetForLeaderEpoch declared a negative topic count ({topic_count})"
        )));
    }

    let mut topics = Vec::with_capacity(topic_count.min(4096) as usize);
    for _ in 0..topic_count {
        let name = decoder
            .read_string()?
            .ok_or_else(|| Error::Protocol("OffsetForLeaderEpoch topic name was null".into()))?;

        let partition_count = decoder.read_i32()?;
        if partition_count < 0 {
            return Err(Error::Protocol(format!(
                "OffsetForLeaderEpoch declared a negative partition count ({partition_count}) for {name}"
            )));
        }

        let mut partitions = Vec::with_capacity(partition_count.min(8192) as usize);
        for _ in 0..partition_count {
            partitions.push(OffsetForLeaderPartition {
                partition: decoder.read_i32()?,
                leader_epoch: decoder.read_i32()?,
            });
        }

        topics.push(OffsetForLeaderTopic { name, partitions });
    }

    Ok(OffsetForLeaderEpochRequest { topics })
}

/// Encode a v0 response body (caller writes the header).
pub fn encode_response(buf: &mut BytesMut, response: &OffsetForLeaderEpochResponse) {
    let mut encoder = Encoder::new(buf);
    encoder.write_i32(response.topics.len() as i32);
    for topic in &response.topics {
        encoder.write_string(Some(&topic.name));
        encoder.write_i32(topic.partitions.len() as i32);
        for partition in &topic.partitions {
            encoder.write_i16(partition.error_code);
            encoder.write_i32(partition.partition);
            encoder.write_i64(partition.end_offset);
        }
    }
}

/// Encode a v0 request body — used by the follower asking a leader where to
/// truncate, and by the round-trip tests.
pub fn encode_request(buf: &mut BytesMut, request: &OffsetForLeaderEpochRequest) {
    let mut encoder = Encoder::new(buf);
    encoder.write_i32(request.topics.len() as i32);
    for topic in &request.topics {
        encoder.write_string(Some(&topic.name));
        encoder.write_i32(topic.partitions.len() as i32);
        for partition in &topic.partitions {
            encoder.write_i32(partition.partition);
            encoder.write_i32(partition.leader_epoch);
        }
    }
}

/// Decode a v0 response body (correlation id already consumed).
pub fn parse_response(decoder: &mut Decoder) -> Result<OffsetForLeaderEpochResponse> {
    let topic_count = decoder.read_i32()?;
    if topic_count < 0 {
        return Err(Error::Protocol(
            "OffsetForLeaderEpoch response declared a negative topic count".into(),
        ));
    }

    let mut topics = Vec::with_capacity(topic_count.min(4096) as usize);
    for _ in 0..topic_count {
        let name = decoder.read_string()?.ok_or_else(|| {
            Error::Protocol("OffsetForLeaderEpoch response topic name was null".into())
        })?;

        let partition_count = decoder.read_i32()?;
        if partition_count < 0 {
            return Err(Error::Protocol(
                "OffsetForLeaderEpoch response declared a negative partition count".into(),
            ));
        }

        let mut partitions = Vec::with_capacity(partition_count.min(8192) as usize);
        for _ in 0..partition_count {
            partitions.push(OffsetForLeaderPartitionResponse {
                error_code: decoder.read_i16()?,
                partition: decoder.read_i32()?,
                end_offset: decoder.read_i64()?,
            });
        }

        topics.push(OffsetForLeaderTopicResponse { name, partitions });
    }

    Ok(OffsetForLeaderEpochResponse { topics })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    fn sample_request() -> OffsetForLeaderEpochRequest {
        OffsetForLeaderEpochRequest {
            topics: vec![
                OffsetForLeaderTopic {
                    name: "orders".to_string(),
                    partitions: vec![
                        OffsetForLeaderPartition { partition: 0, leader_epoch: 4 },
                        OffsetForLeaderPartition { partition: 1, leader_epoch: 0 },
                    ],
                },
                OffsetForLeaderTopic {
                    name: "events".to_string(),
                    partitions: vec![OffsetForLeaderPartition { partition: 7, leader_epoch: 12 }],
                },
            ],
        }
    }

    #[test]
    fn a_request_round_trips_and_is_fully_consumed() {
        let request = sample_request();
        let mut buf = BytesMut::new();
        encode_request(&mut buf, &request);

        let mut bytes = Bytes::from(buf.to_vec());
        let mut decoder = Decoder::new(&mut bytes);
        let parsed = parse_request(&mut decoder).unwrap();

        assert_eq!(parsed, request);
        assert_eq!(decoder.remaining(), 0, "the whole request must be consumed");
    }

    #[test]
    fn a_response_round_trips_and_is_fully_consumed() {
        let response = OffsetForLeaderEpochResponse {
            topics: vec![OffsetForLeaderTopicResponse {
                name: "orders".to_string(),
                partitions: vec![
                    OffsetForLeaderPartitionResponse { error_code: 0, partition: 0, end_offset: 100 },
                    // The "I cannot answer that" case, which a follower must be
                    // able to tell apart from a real offset of 0.
                    OffsetForLeaderPartitionResponse { error_code: 0, partition: 1, end_offset: -1 },
                    OffsetForLeaderPartitionResponse { error_code: 6, partition: 2, end_offset: -1 },
                ],
            }],
        };

        let mut buf = BytesMut::new();
        encode_response(&mut buf, &response);

        let mut bytes = Bytes::from(buf.to_vec());
        let mut decoder = Decoder::new(&mut bytes);
        let parsed = parse_response(&mut decoder).unwrap();

        assert_eq!(parsed, response);
        assert_eq!(decoder.remaining(), 0);
    }

    /// An empty ask is legal and must not be an error — a follower with nothing
    /// to reconcile still completes the exchange.
    #[test]
    fn an_empty_request_round_trips() {
        let request = OffsetForLeaderEpochRequest { topics: vec![] };
        let mut buf = BytesMut::new();
        encode_request(&mut buf, &request);

        let mut bytes = Bytes::from(buf.to_vec());
        let mut decoder = Decoder::new(&mut bytes);
        assert_eq!(parse_request(&mut decoder).unwrap(), request);
    }

    #[test]
    fn a_truncated_request_is_rejected() {
        let mut buf = BytesMut::new();
        encode_request(&mut buf, &sample_request());
        let cut = buf.to_vec();
        let mut bytes = Bytes::from(cut[..cut.len() - 6].to_vec());
        let mut decoder = Decoder::new(&mut bytes);

        assert!(parse_request(&mut decoder).is_err());
    }

    /// A negative count is the classic capacity-overflow crash vector — this
    /// broker has already shipped one of those (CreateTopics, v2.10.2).
    #[test]
    fn a_negative_count_is_refused_not_allocated() {
        let mut buf = BytesMut::new();
        {
            let mut encoder = Encoder::new(&mut buf);
            encoder.write_i32(-1);
        }
        let mut bytes = Bytes::from(buf.to_vec());
        let mut decoder = Decoder::new(&mut bytes);

        assert!(parse_request(&mut decoder).is_err());
    }
}
