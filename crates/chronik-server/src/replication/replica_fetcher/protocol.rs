//! Kafka Fetch *client* codec for follower replication (RP-2.4).
//!
//! `chronik-server` deliberately has no Kafka client library — `rdkafka` pulls in
//! librdkafka, which does not cross-compile against musl without a vendored
//! zlib/OpenSSL. A follower still needs to issue Fetch to its leader, so this
//! module hand-rolls the two halves the server does not already have:
//!
//! - **encode** a Fetch *request* (the server only decodes them)
//! - **decode** a Fetch *response* (the server only encodes them)
//!
//! The authority for the wire format is the server's own codec —
//! `ProtocolHandler::parse_fetch_request` and `ProtocolHandler::encode_fetch_response`.
//! This module must agree with those exactly, so the tests below round-trip
//! against them rather than against a hand-written byte fixture. If either side
//! is changed, the round-trip test fails and names the drift.
//!
//! ## Why v11
//!
//! v11 is the highest **non-flexible** Fetch version: no varints, no tagged
//! fields, and a v0 response header (bare correlation id). It still carries
//! everything replication needs — `current_leader_epoch` (v9) for the epoch
//! fencing RP-3 will add, `log_start_offset` (v5), and per-partition
//! `last_stable_offset` (v4). Going to v12+ would buy nothing but flexible
//! encoding.

use bytes::{Buf, BufMut, Bytes, BytesMut};
use chronik_common::{Error, Result};

/// Fetch API key.
pub const FETCH_API_KEY: i16 = 1;

/// Fetch version this client speaks. See the module docs for why v11.
pub const FETCH_API_VERSION: i16 = 11;

/// Client id sent in the request header. Shows up in leader-side logs, so it is
/// worth being explicit that the caller is a replica and not an application.
pub const REPLICA_CLIENT_ID: &str = "chronik-replica-fetcher";

/// One partition's ask within a Fetch request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchPartitionRequest {
    pub partition: i32,
    /// The offset to read from — for a follower this is its own LEO, which is
    /// also what the leader records as this replica's position (RP-2.1).
    pub fetch_offset: i64,
    /// Earliest offset this replica still holds. `-1` when unknown.
    pub log_start_offset: i64,
    /// Leader epoch this replica believes is current. `-1` until RP-3 populates it.
    pub current_leader_epoch: i32,
    /// Per-partition response cap.
    pub max_bytes: i32,
}

/// One topic's ask within a Fetch request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchTopicRequest {
    pub name: String,
    pub partitions: Vec<FetchPartitionRequest>,
}

/// Everything needed to build a Fetch request frame.
#[derive(Debug, Clone)]
pub struct FetchRequestSpec {
    pub correlation_id: i32,
    /// This node's id. Must be `>= 0` or the leader treats the caller as a
    /// consumer and does not record its position.
    pub replica_id: i32,
    /// Long-poll budget: how long the leader may hold the request open waiting
    /// for `min_bytes`.
    pub max_wait_ms: i32,
    pub min_bytes: i32,
    pub max_bytes: i32,
    pub topics: Vec<FetchTopicRequest>,
}

/// One partition's data in a Fetch response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedPartition {
    pub partition: i32,
    pub error_code: i16,
    pub high_watermark: i64,
    pub last_stable_offset: i64,
    pub log_start_offset: i64,
    pub preferred_read_replica: i32,
    /// Raw Kafka RecordBatch bytes, exactly as the leader stored them.
    pub records: Bytes,
}

/// One topic's data in a Fetch response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedTopic {
    pub name: String,
    pub partitions: Vec<FetchedPartition>,
}

/// A decoded Fetch response, including the correlation id from its header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchResponseFrame {
    pub correlation_id: i32,
    pub throttle_time_ms: i32,
    pub error_code: i16,
    pub session_id: i32,
    pub topics: Vec<FetchedTopic>,
}

/// Encode a Fetch v11 request *body* — header included, length prefix excluded.
///
/// The caller frames it; see [`frame_request`] for the length-prefixed form.
pub fn encode_fetch_request(spec: &FetchRequestSpec) -> BytesMut {
    let mut buf = BytesMut::with_capacity(256);

    // Request header v1: api_key, api_version, correlation_id, client_id.
    // Fetch is non-flexible through v11, so there are no header tagged fields.
    buf.put_i16(FETCH_API_KEY);
    buf.put_i16(FETCH_API_VERSION);
    buf.put_i32(spec.correlation_id);
    put_nullable_string(&mut buf, Some(REPLICA_CLIENT_ID));

    buf.put_i32(spec.replica_id);
    buf.put_i32(spec.max_wait_ms);
    buf.put_i32(spec.min_bytes);
    buf.put_i32(spec.max_bytes); // v3+
    buf.put_i8(0); // isolation_level (v4+): a follower always replicates uncommitted
    buf.put_i32(0); // session_id (v7+): full fetch, no incremental session
    buf.put_i32(-1); // session_epoch (v7+): -1 = no session

    buf.put_i32(spec.topics.len() as i32);
    for topic in &spec.topics {
        put_nullable_string(&mut buf, Some(&topic.name));
        buf.put_i32(topic.partitions.len() as i32);
        for partition in &topic.partitions {
            buf.put_i32(partition.partition);
            buf.put_i32(partition.current_leader_epoch); // v9+
            buf.put_i64(partition.fetch_offset);
            buf.put_i64(partition.log_start_offset); // v5+
            buf.put_i32(partition.max_bytes);
        }
    }

    // forgotten_topics_data (v7+): always empty — this client uses full fetches.
    buf.put_i32(0);
    // rack_id (v11+): empty, no rack awareness.
    put_nullable_string(&mut buf, Some(""));

    buf
}

/// OffsetForLeaderEpoch API key.
pub const OFFSET_FOR_LEADER_EPOCH_API_KEY: i16 = 23;

/// Version this client speaks. v0 only, matching what the broker advertises —
/// the body codec lives in `chronik_protocol::offset_for_leader_epoch_types`,
/// and advertising past what is implemented hands peers malformed frames.
pub const OFFSET_FOR_LEADER_EPOCH_API_VERSION: i16 = 0;

/// Request header v1 for an OffsetForLeaderEpoch call. The body is appended by
/// `chronik_protocol::offset_for_leader_epoch_types::encode_request`, which the
/// broker's own parser is tested against.
pub fn encode_epoch_request_header(correlation_id: i32) -> BytesMut {
    let mut buf = BytesMut::with_capacity(64);
    buf.put_i16(OFFSET_FOR_LEADER_EPOCH_API_KEY);
    buf.put_i16(OFFSET_FOR_LEADER_EPOCH_API_VERSION);
    buf.put_i32(correlation_id);
    put_nullable_string(&mut buf, Some(REPLICA_CLIENT_ID));
    buf
}

/// Wrap an encoded request in the 4-byte big-endian length prefix Kafka uses.
pub fn frame_request(body: &[u8]) -> BytesMut {
    let mut framed = BytesMut::with_capacity(body.len() + 4);
    framed.put_i32(body.len() as i32);
    framed.put_slice(body);
    framed
}

/// Decode a Fetch v11 response, starting at the response header.
///
/// `payload` is the frame body with the 4-byte length prefix already stripped.
pub fn decode_fetch_response(payload: Bytes) -> Result<FetchResponseFrame> {
    let mut buf = payload;

    // Response header v0 — Fetch only becomes flexible at v12, so there is no
    // tagged-field section here.
    let correlation_id = get_i32(&mut buf, "correlation_id")?;
    let throttle_time_ms = get_i32(&mut buf, "throttle_time_ms")?; // v1+
    let error_code = get_i16(&mut buf, "error_code")?; // v7+
    let session_id = get_i32(&mut buf, "session_id")?; // v7+

    let topic_count = get_i32(&mut buf, "topic_count")?;
    if topic_count < 0 {
        return Err(Error::Protocol(format!(
            "Fetch response declared a negative topic count ({topic_count})"
        )));
    }

    let mut topics = Vec::with_capacity(topic_count.min(1024) as usize);
    for _ in 0..topic_count {
        let name = get_nullable_string(&mut buf, "topic_name")?.ok_or_else(|| {
            Error::Protocol("Fetch response contained a null topic name".into())
        })?;

        let partition_count = get_i32(&mut buf, "partition_count")?;
        if partition_count < 0 {
            return Err(Error::Protocol(format!(
                "Fetch response declared a negative partition count ({partition_count}) for topic {name}"
            )));
        }

        let mut partitions = Vec::with_capacity(partition_count.min(4096) as usize);
        for _ in 0..partition_count {
            partitions.push(decode_partition(&mut buf)?);
        }

        topics.push(FetchedTopic { name, partitions });
    }

    Ok(FetchResponseFrame {
        correlation_id,
        throttle_time_ms,
        error_code,
        session_id,
        topics,
    })
}

fn decode_partition(buf: &mut Bytes) -> Result<FetchedPartition> {
    let partition = get_i32(buf, "partition")?;
    let error_code = get_i16(buf, "partition_error_code")?;
    let high_watermark = get_i64(buf, "high_watermark")?;
    let last_stable_offset = get_i64(buf, "last_stable_offset")?; // v4+
    let log_start_offset = get_i64(buf, "log_start_offset")?; // v5+

    // Aborted transactions (v4+). A follower replicates the log verbatim,
    // including the control markers, so it only needs to step over this.
    let aborted_count = get_i32(buf, "aborted_count")?;
    if aborted_count > 0 {
        let skip = (aborted_count as usize).saturating_mul(16); // producer_id + first_offset
        if buf.remaining() < skip {
            return Err(Error::Protocol(
                "Fetch response truncated inside the aborted-transactions array".into(),
            ));
        }
        buf.advance(skip);
    }

    let preferred_read_replica = get_i32(buf, "preferred_read_replica")?; // v11+

    let records_len = get_i32(buf, "records_len")?;
    let records = if records_len <= 0 {
        // Kafka writes -1 for null; this server writes 0 for empty. Both mean
        // "no records", and neither consumes any further bytes.
        Bytes::new()
    } else {
        let len = records_len as usize;
        if buf.remaining() < len {
            return Err(Error::Protocol(format!(
                "Fetch response declared {len} record bytes but only {} remain",
                buf.remaining()
            )));
        }
        buf.split_to(len)
    };

    Ok(FetchedPartition {
        partition,
        error_code,
        high_watermark,
        last_stable_offset,
        log_start_offset,
        preferred_read_replica,
        records,
    })
}

fn put_nullable_string(buf: &mut BytesMut, value: Option<&str>) {
    match value {
        Some(s) => {
            buf.put_i16(s.len() as i16);
            buf.put_slice(s.as_bytes());
        }
        None => buf.put_i16(-1),
    }
}

fn get_i16(buf: &mut Bytes, field: &str) -> Result<i16> {
    if buf.remaining() < 2 {
        return Err(truncated(field));
    }
    Ok(buf.get_i16())
}

fn get_i32(buf: &mut Bytes, field: &str) -> Result<i32> {
    if buf.remaining() < 4 {
        return Err(truncated(field));
    }
    Ok(buf.get_i32())
}

fn get_i64(buf: &mut Bytes, field: &str) -> Result<i64> {
    if buf.remaining() < 8 {
        return Err(truncated(field));
    }
    Ok(buf.get_i64())
}

fn get_nullable_string(buf: &mut Bytes, field: &str) -> Result<Option<String>> {
    let len = get_i16(buf, field)?;
    if len < 0 {
        return Ok(None);
    }
    let len = len as usize;
    if buf.remaining() < len {
        return Err(truncated(field));
    }
    let bytes = buf.split_to(len);
    String::from_utf8(bytes.to_vec())
        .map(Some)
        .map_err(|e| Error::Protocol(format!("Fetch response field {field} was not valid UTF-8: {e}")))
}

fn truncated(field: &str) -> Error {
    Error::Protocol(format!("Fetch response truncated while reading {field}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chronik_protocol::handler::ProtocolHandler;
    use chronik_protocol::parser::{parse_request_header, ResponseHeader};
    use chronik_protocol::types::{
        FetchResponse as ServerFetchResponse, FetchResponsePartition as ServerPartition,
        FetchResponseTopic as ServerTopic,
    };

    fn sample_spec() -> FetchRequestSpec {
        FetchRequestSpec {
            correlation_id: 77,
            replica_id: 3,
            max_wait_ms: 500,
            min_bytes: 1,
            max_bytes: 10 * 1024 * 1024,
            topics: vec![
                FetchTopicRequest {
                    name: "orders".to_string(),
                    partitions: vec![
                        FetchPartitionRequest {
                            partition: 0,
                            fetch_offset: 42,
                            log_start_offset: 0,
                            current_leader_epoch: -1,
                            max_bytes: 1024 * 1024,
                        },
                        FetchPartitionRequest {
                            partition: 1,
                            fetch_offset: 0,
                            log_start_offset: -1,
                            current_leader_epoch: 7,
                            max_bytes: 1024 * 1024,
                        },
                    ],
                },
                FetchTopicRequest {
                    name: "events".to_string(),
                    partitions: vec![FetchPartitionRequest {
                        partition: 5,
                        fetch_offset: 999,
                        log_start_offset: 100,
                        current_leader_epoch: -1,
                        max_bytes: 512 * 1024,
                    }],
                },
            ],
        }
    }

    /// The whole point of this module: what this client encodes is what the
    /// server this cluster runs actually parses. Asserting against a byte
    /// fixture would only prove the client agrees with itself.
    #[test]
    fn client_request_is_parsed_by_the_server_codec() {
        let spec = sample_spec();
        let encoded = encode_fetch_request(&spec);

        let mut wire = Bytes::from(encoded.to_vec());
        let header = parse_request_header(&mut wire).expect("header parses");
        assert_eq!(header.api_key as i16, FETCH_API_KEY);
        assert_eq!(header.api_version, FETCH_API_VERSION);
        assert_eq!(header.correlation_id, spec.correlation_id);
        assert_eq!(header.client_id.as_deref(), Some(REPLICA_CLIENT_ID));

        let handler = ProtocolHandler::new();
        let parsed = handler
            .parse_fetch_request(&header, &mut wire)
            .expect("server parses the request this client produces");

        assert_eq!(parsed.replica_id, spec.replica_id);
        assert_eq!(parsed.max_wait_ms, spec.max_wait_ms);
        assert_eq!(parsed.min_bytes, spec.min_bytes);
        assert_eq!(parsed.max_bytes, spec.max_bytes);
        assert_eq!(parsed.isolation_level, 0);
        assert_eq!(parsed.topics.len(), spec.topics.len());

        for (got, want) in parsed.topics.iter().zip(spec.topics.iter()) {
            assert_eq!(got.name, want.name);
            assert_eq!(got.partitions.len(), want.partitions.len());
            for (gp, wp) in got.partitions.iter().zip(want.partitions.iter()) {
                assert_eq!(gp.partition, wp.partition);
                assert_eq!(gp.fetch_offset, wp.fetch_offset);
                assert_eq!(gp.log_start_offset, wp.log_start_offset);
                assert_eq!(gp.current_leader_epoch, wp.current_leader_epoch);
                assert_eq!(gp.partition_max_bytes, wp.max_bytes);
            }
        }

        // Every byte must be consumed. A trailing remainder would mean the
        // server stopped early and silently tolerated a malformed tail —
        // exactly the drift this test exists to catch.
        assert_eq!(wire.remaining(), 0, "server did not consume the whole request");
    }

    /// `replica_id` is what makes a Fetch a *replication* fetch: the leader
    /// only records a follower's position when it is `>= 0` (RP-2.1).
    #[test]
    fn replica_id_survives_the_round_trip() {
        let mut spec = sample_spec();
        spec.replica_id = 2;
        let encoded = encode_fetch_request(&spec);

        let mut wire = Bytes::from(encoded.to_vec());
        let header = parse_request_header(&mut wire).unwrap();
        let parsed = ProtocolHandler::new()
            .parse_fetch_request(&header, &mut wire)
            .unwrap();

        assert_eq!(parsed.replica_id, 2);
        assert!(parsed.replica_id >= 0, "a follower must not look like a consumer");
    }

    fn server_response(records: Vec<u8>) -> ServerFetchResponse {
        ServerFetchResponse {
            header: ResponseHeader { correlation_id: 77 },
            throttle_time_ms: 0,
            error_code: 0,
            session_id: 0,
            topics: vec![ServerTopic {
                name: "orders".to_string(),
                partitions: vec![
                    ServerPartition {
                        partition: 0,
                        error_code: 0,
                        high_watermark: 500,
                        last_stable_offset: 500,
                        log_start_offset: 0,
                        aborted: None,
                        preferred_read_replica: -1,
                        records,
                    },
                    ServerPartition {
                        partition: 1,
                        error_code: 0,
                        high_watermark: 12,
                        last_stable_offset: 12,
                        log_start_offset: 4,
                        aborted: None,
                        preferred_read_replica: -1,
                        records: vec![],
                    },
                ],
            }],
        }
    }

    fn encode_server_response(response: &ServerFetchResponse) -> Bytes {
        let mut buf = BytesMut::new();
        let mut header = BytesMut::new();
        chronik_protocol::parser::write_response_header(&mut header, &response.header);
        buf.extend_from_slice(&header);

        let mut body = BytesMut::new();
        ProtocolHandler::new()
            .encode_fetch_response(&mut body, response, FETCH_API_VERSION)
            .expect("server encodes");
        buf.extend_from_slice(&body);
        buf.freeze()
    }

    /// The mirror of the request test: this client must decode exactly what the
    /// server encodes, including the record bytes byte-for-byte. Those bytes
    /// carry the producer's original CRC-32C, so any mangling here would
    /// corrupt the follower's log in a way only a Java client would notice.
    #[test]
    fn client_decodes_what_the_server_encodes() {
        let payload: Vec<u8> = (0u8..=255).collect();
        let response = server_response(payload.clone());
        let wire = encode_server_response(&response);

        let decoded = decode_fetch_response(wire).expect("client decodes the server's response");

        assert_eq!(decoded.correlation_id, 77);
        assert_eq!(decoded.throttle_time_ms, 0);
        assert_eq!(decoded.error_code, 0);
        assert_eq!(decoded.topics.len(), 1);

        let topic = &decoded.topics[0];
        assert_eq!(topic.name, "orders");
        assert_eq!(topic.partitions.len(), 2);

        let p0 = &topic.partitions[0];
        assert_eq!(p0.partition, 0);
        assert_eq!(p0.error_code, 0);
        assert_eq!(p0.high_watermark, 500);
        assert_eq!(p0.log_start_offset, 0);
        assert_eq!(p0.records.as_ref(), payload.as_slice());

        let p1 = &topic.partitions[1];
        assert_eq!(p1.partition, 1);
        assert_eq!(p1.high_watermark, 12);
        assert_eq!(p1.log_start_offset, 4);
        assert!(p1.records.is_empty());
    }

    /// An empty response is the steady state of a caught-up follower — it must
    /// decode cleanly rather than erroring, or the fetch loop would treat
    /// "nothing new" as a failure and reconnect forever.
    #[test]
    fn empty_response_decodes_cleanly() {
        let mut response = server_response(vec![]);
        response.topics.clear();
        let wire = encode_server_response(&response);

        let decoded = decode_fetch_response(wire).expect("empty response decodes");
        assert!(decoded.topics.is_empty());
    }

    /// A partition-level error (leader moved, offset out of range) arrives as a
    /// normal response with a non-zero code, not as a transport failure. The
    /// fetch loop branches on it, so it must survive decoding.
    #[test]
    fn partition_error_code_is_preserved() {
        let mut response = server_response(vec![]);
        response.topics[0].partitions[0].error_code = 6; // NOT_LEADER_FOR_PARTITION
        response.topics[0].partitions[0].high_watermark = -1;
        let wire = encode_server_response(&response);

        let decoded = decode_fetch_response(wire).unwrap();
        assert_eq!(decoded.topics[0].partitions[0].error_code, 6);
        assert_eq!(decoded.topics[0].partitions[0].high_watermark, -1);
    }

    /// Aborted transactions are a variable-length section sitting *before* the
    /// records. Mis-stepping it would shift every subsequent field, so decode
    /// has to walk it even though a follower ignores its contents.
    #[test]
    fn aborted_transactions_are_stepped_over() {
        let payload = vec![9u8; 32];
        let mut response = server_response(payload.clone());
        response.topics[0].partitions[0].aborted = Some(vec![
            chronik_protocol::types::AbortedTransaction { producer_id: 1, first_offset: 10 },
            chronik_protocol::types::AbortedTransaction { producer_id: 2, first_offset: 20 },
        ]);
        let wire = encode_server_response(&response);

        let decoded = decode_fetch_response(wire).expect("decodes past aborted transactions");
        assert_eq!(decoded.topics[0].partitions[0].records.as_ref(), payload.as_slice());
        assert_eq!(decoded.topics[0].partitions[1].high_watermark, 12);
    }

    /// A short read must be an error, never a partial record batch — appending
    /// a truncated batch to the follower's WAL would be silent corruption.
    #[test]
    fn truncated_payload_is_rejected() {
        let response = server_response(vec![1, 2, 3, 4, 5, 6, 7, 8]);
        let wire = encode_server_response(&response);

        let cut = wire.slice(0..wire.len() - 4);
        assert!(decode_fetch_response(cut).is_err(), "truncated frame must not decode");
    }

    #[test]
    fn framing_prefixes_the_length() {
        let body = vec![0xAAu8; 37];
        let framed = frame_request(&body);
        assert_eq!(framed.len(), 41);
        assert_eq!(i32::from_be_bytes([framed[0], framed[1], framed[2], framed[3]]), 37);
        assert_eq!(&framed[4..], body.as_slice());
    }
}
