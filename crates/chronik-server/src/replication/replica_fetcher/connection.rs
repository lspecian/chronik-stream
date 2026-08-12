//! A follower's connection to one partition leader (RP-2.4).
//!
//! One socket per leader, one request in flight at a time. A follower's fetch
//! loop is a serial pipeline — issue, apply, advance — so there is nothing to
//! gain from pipelining, and a single outstanding request means the correlation
//! id can be checked exactly rather than tracked in a map.
//!
//! The connection is lazy and self-healing: it dials on first use and drops
//! itself on any error so the next call redials. A follower that cannot reach
//! its leader is not a special state to model — it is just a fetch that failed
//! and will be retried.

use std::time::Duration;

use bytes::Bytes;
use chronik_common::{Error, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tracing::{debug, warn};

use chronik_protocol::offset_for_leader_epoch_types::{
    encode_request as encode_epoch_request, parse_response as parse_epoch_response,
    OffsetForLeaderEpochRequest, OffsetForLeaderEpochResponse,
};
use chronik_protocol::parser::Decoder;

use super::protocol::{
    decode_fetch_response, encode_epoch_request_header, encode_fetch_request, frame_request,
    FetchRequestSpec, FetchResponseFrame, OFFSET_FOR_LEADER_EPOCH_API_VERSION,
};

/// Ceiling on a response frame, independent of what the leader claims. A
/// corrupt or hostile length prefix must not be able to make a follower
/// allocate without bound.
const MAX_RESPONSE_BYTES: i32 = 256 * 1024 * 1024;

/// How long to wait for the TCP connect itself.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Slack added to the request's `max_wait_ms` before the read is considered
/// hung. The leader may legitimately hold a long-poll open for the full
/// `max_wait_ms`; anything beyond that plus this margin is a stall.
const READ_TIMEOUT_SLACK: Duration = Duration::from_secs(30);

/// Budget for an epoch query. Unlike a fetch there is no long poll — the leader
/// answers from memory — so a slow reply means a struggling leader, and waiting
/// on it stalls the truncation that has to happen before replication resumes.
const EPOCH_QUERY_TIMEOUT: Duration = Duration::from_secs(15);

/// A lazily-connected, self-healing link to one leader's Kafka port.
pub struct LeaderConnection {
    addr: String,
    stream: Option<TcpStream>,
    next_correlation_id: i32,
}

impl LeaderConnection {
    pub fn new(addr: impl Into<String>) -> Self {
        Self {
            addr: addr.into(),
            stream: None,
            next_correlation_id: 1,
        }
    }

    pub fn addr(&self) -> &str {
        &self.addr
    }

    /// True when a socket is currently held. Only useful for tests and
    /// diagnostics — callers should not branch on it, because a held socket
    /// says nothing about whether the peer is alive (a TCP write lands in the
    /// local send buffer long after the peer is gone; that mistake cost the
    /// push stack an ISR-eviction bug that unit tests could not see).
    pub fn is_connected(&self) -> bool {
        self.stream.is_some()
    }

    /// Drop the socket so the next fetch redials.
    pub fn disconnect(&mut self) {
        if self.stream.take().is_some() {
            debug!("Replica fetcher dropped its connection to {}", self.addr);
        }
    }

    /// Issue one Fetch and return the decoded response.
    ///
    /// `correlation_id` on the spec is overwritten with this connection's next
    /// id, so callers cannot accidentally reuse one.
    pub async fn fetch(&mut self, mut spec: FetchRequestSpec) -> Result<FetchResponseFrame> {
        spec.correlation_id = self.take_correlation_id();

        let read_budget = Duration::from_millis(spec.max_wait_ms.max(0) as u64) + READ_TIMEOUT_SLACK;

        match self.fetch_once(&spec, read_budget).await {
            Ok(response) => Ok(response),
            Err(e) => {
                // Any failure — transport or protocol — invalidates the stream.
                // A protocol error means the byte stream is out of sync and
                // every subsequent read would be garbage.
                self.disconnect();
                Err(e)
            }
        }
    }

    async fn fetch_once(
        &mut self,
        spec: &FetchRequestSpec,
        read_budget: Duration,
    ) -> Result<FetchResponseFrame> {
        let payload = self
            .round_trip("Fetch", &encode_fetch_request(spec), read_budget)
            .await?;

        let response = decode_fetch_response(payload)?;

        if response.correlation_id != spec.correlation_id {
            return Err(Error::Protocol(format!(
                "Fetch response from {} carried correlation id {} but {} was sent",
                self.addr, response.correlation_id, spec.correlation_id
            )));
        }

        Ok(response)
    }

    /// Ask the leader where a set of leader epochs ended (API 23, RP-3.3).
    ///
    /// A follower issues this before it resumes fetching from a leader it has
    /// not been following, and again whenever the fetched records disagree with
    /// its own log. The answer is what it truncates to, so an error here must
    /// stay an error — a follower that guesses discards a correct log or keeps a
    /// divergent one, which is the damage epochs exist to prevent.
    pub async fn offset_for_leader_epoch(
        &mut self,
        request: &OffsetForLeaderEpochRequest,
    ) -> Result<OffsetForLeaderEpochResponse> {
        let correlation_id = self.take_correlation_id();

        match self.epoch_query_once(request, correlation_id).await {
            Ok(response) => Ok(response),
            Err(e) => {
                self.disconnect();
                Err(e)
            }
        }
    }

    async fn epoch_query_once(
        &mut self,
        request: &OffsetForLeaderEpochRequest,
        correlation_id: i32,
    ) -> Result<OffsetForLeaderEpochResponse> {
        let mut body = encode_epoch_request_header(correlation_id);
        encode_epoch_request(&mut body, request);

        let mut payload = self
            .round_trip("OffsetForLeaderEpoch", &body, EPOCH_QUERY_TIMEOUT)
            .await?;

        // Response header v0: a bare correlation id. API 23 only becomes
        // flexible at v4, and this client speaks v0.
        if payload.len() < 4 {
            return Err(Error::Protocol(format!(
                "OffsetForLeaderEpoch response from {} was {} bytes, too short for a header",
                self.addr,
                payload.len()
            )));
        }
        let echoed = i32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
        if echoed != correlation_id {
            return Err(Error::Protocol(format!(
                "OffsetForLeaderEpoch response from {} carried correlation id {} but {} was sent",
                self.addr, echoed, correlation_id
            )));
        }
        let _ = payload.split_to(4);

        let mut decoder = Decoder::new(&mut payload);
        parse_epoch_response(&mut decoder)
    }

    /// Write one framed request and read one framed response.
    ///
    /// Only one request is ever in flight, so this is the whole request
    /// pipeline: any failure here means the byte stream is out of sync and the
    /// caller drops the socket.
    async fn round_trip(&mut self, what: &str, body: &[u8], read_budget: Duration) -> Result<Bytes> {
        self.ensure_connected().await?;
        let stream = self
            .stream
            .as_mut()
            .expect("ensure_connected leaves a stream in place");

        let framed = frame_request(body);
        stream
            .write_all(&framed)
            .await
            .map_err(|e| Error::Network(format!("{what} write to {} failed: {e}", self.addr)))?;
        stream
            .flush()
            .await
            .map_err(|e| Error::Network(format!("{what} flush to {} failed: {e}", self.addr)))?;

        timeout(read_budget, read_frame(stream, &self.addr))
            .await
            .map_err(|_| {
                Error::Network(format!(
                    "{what} to {} timed out after {read_budget:?}",
                    self.addr
                ))
            })?
    }

    async fn ensure_connected(&mut self) -> Result<()> {
        if self.stream.is_some() {
            return Ok(());
        }

        let stream = timeout(CONNECT_TIMEOUT, TcpStream::connect(&self.addr))
            .await
            .map_err(|_| {
                Error::Network(format!(
                    "Connect to leader {} timed out after {:?}",
                    self.addr, CONNECT_TIMEOUT
                ))
            })?
            .map_err(|e| Error::Network(format!("Connect to leader {} failed: {e}", self.addr)))?;

        // Replication is latency-sensitive and its requests are small; Nagle
        // would add up to a round trip per fetch for no benefit.
        if let Err(e) = stream.set_nodelay(true) {
            warn!("Could not disable Nagle on the link to {}: {}", self.addr, e);
        }

        debug!("Replica fetcher connected to leader {}", self.addr);
        self.stream = Some(stream);
        Ok(())
    }

    fn take_correlation_id(&mut self) -> i32 {
        let id = self.next_correlation_id;
        self.next_correlation_id = self.next_correlation_id.wrapping_add(1).max(1);
        id
    }
}

/// Read one length-prefixed Kafka frame.
async fn read_frame(stream: &mut TcpStream, addr: &str) -> Result<Bytes> {
    let mut len_buf = [0u8; 4];
    stream
        .read_exact(&mut len_buf)
        .await
        .map_err(|e| Error::Network(format!("Fetch response header read from {addr} failed: {e}")))?;

    let len = i32::from_be_bytes(len_buf);
    if len <= 0 {
        return Err(Error::Protocol(format!(
            "Leader {addr} announced a {len}-byte response frame"
        )));
    }
    if len > MAX_RESPONSE_BYTES {
        return Err(Error::Protocol(format!(
            "Leader {addr} announced a {len}-byte response frame, above the {MAX_RESPONSE_BYTES} cap"
        )));
    }

    let mut payload = vec![0u8; len as usize];
    stream
        .read_exact(&mut payload)
        .await
        .map_err(|e| Error::Network(format!("Fetch response body read from {addr} failed: {e}")))?;

    Ok(Bytes::from(payload))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replication::replica_fetcher::protocol::{
        FetchPartitionRequest, FetchTopicRequest, FETCH_API_VERSION,
    };
    use bytes::BytesMut;
    use chronik_protocol::handler::ProtocolHandler;
    use chronik_protocol::parser::{parse_request_header, write_response_header, ResponseHeader};
    use chronik_protocol::types::{
        FetchResponse as ServerFetchResponse, FetchResponsePartition as ServerPartition,
        FetchResponseTopic as ServerTopic,
    };
    use std::sync::atomic::{AtomicI32, Ordering};
    use std::sync::Arc;
    use tokio::net::TcpListener;

    fn spec(max_wait_ms: i32) -> FetchRequestSpec {
        FetchRequestSpec {
            correlation_id: 0,
            replica_id: 2,
            max_wait_ms,
            min_bytes: 1,
            max_bytes: 1024 * 1024,
            topics: vec![FetchTopicRequest {
                name: "orders".to_string(),
                partitions: vec![FetchPartitionRequest {
                    partition: 0,
                    fetch_offset: 10,
                    log_start_offset: 0,
                    current_leader_epoch: -1,
                    max_bytes: 1024 * 1024,
                }],
            }],
        }
    }

    fn leader_response(correlation_id: i32, records: Vec<u8>) -> Bytes {
        let response = ServerFetchResponse {
            header: ResponseHeader { correlation_id },
            throttle_time_ms: 0,
            error_code: 0,
            session_id: 0,
            topics: vec![ServerTopic {
                name: "orders".to_string(),
                partitions: vec![ServerPartition {
                    partition: 0,
                    error_code: 0,
                    high_watermark: 100,
                    last_stable_offset: 100,
                    log_start_offset: 0,
                    aborted: None,
                    preferred_read_replica: -1,
                    records,
                }],
            }],
        };

        let mut buf = BytesMut::new();
        let mut header = BytesMut::new();
        write_response_header(&mut header, &response.header);
        buf.extend_from_slice(&header);

        let mut body = BytesMut::new();
        ProtocolHandler::new()
            .encode_fetch_response(&mut body, &response, FETCH_API_VERSION)
            .unwrap();
        buf.extend_from_slice(&body);
        buf.freeze()
    }

    /// A stand-in leader that parses the request with the *server's* codec and
    /// answers with the server's encoder. It exercises the same bytes a real
    /// broker would, over a real socket.
    ///
    /// `observed_replica_id` records what the leader saw, which is the single
    /// field that decides whether a fetch counts as replication.
    async fn spawn_fake_leader(
        records: Vec<u8>,
        observed_replica_id: Arc<AtomicI32>,
        observed_fetch_offset: Arc<AtomicI32>,
    ) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();

        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let records = records.clone();
                let observed_replica_id = Arc::clone(&observed_replica_id);
                let observed_fetch_offset = Arc::clone(&observed_fetch_offset);

                tokio::spawn(async move {
                    loop {
                        let mut len_buf = [0u8; 4];
                        if socket.read_exact(&mut len_buf).await.is_err() {
                            return;
                        }
                        let len = i32::from_be_bytes(len_buf) as usize;
                        let mut payload = vec![0u8; len];
                        if socket.read_exact(&mut payload).await.is_err() {
                            return;
                        }

                        let mut wire = Bytes::from(payload);
                        let header = parse_request_header(&mut wire).unwrap();
                        let request = ProtocolHandler::new()
                            .parse_fetch_request(&header, &mut wire)
                            .unwrap();

                        observed_replica_id.store(request.replica_id, Ordering::SeqCst);
                        observed_fetch_offset.store(
                            request.topics[0].partitions[0].fetch_offset as i32,
                            Ordering::SeqCst,
                        );

                        let response = leader_response(header.correlation_id, records.clone());
                        let mut framed = BytesMut::new();
                        framed.extend_from_slice(&(response.len() as i32).to_be_bytes());
                        framed.extend_from_slice(&response);
                        if socket.write_all(&framed).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });

        addr
    }

    #[tokio::test]
    async fn fetch_round_trips_over_a_real_socket() {
        let payload = vec![7u8; 64];
        let replica_id = Arc::new(AtomicI32::new(-99));
        let fetch_offset = Arc::new(AtomicI32::new(-99));
        let addr = spawn_fake_leader(
            payload.clone(),
            Arc::clone(&replica_id),
            Arc::clone(&fetch_offset),
        )
        .await;

        let mut conn = LeaderConnection::new(addr);
        let response = conn.fetch(spec(100)).await.expect("fetch succeeds");

        assert_eq!(response.topics.len(), 1);
        assert_eq!(response.topics[0].partitions[0].records.as_ref(), payload.as_slice());
        assert_eq!(response.topics[0].partitions[0].high_watermark, 100);

        // The leader must have seen a replica, not a consumer, at the offset asked for.
        assert_eq!(replica_id.load(Ordering::SeqCst), 2);
        assert_eq!(fetch_offset.load(Ordering::SeqCst), 10);
        assert!(conn.is_connected());
    }

    /// The socket is reused across fetches — a follower issues these
    /// continuously, and redialing per fetch would add a connect round trip to
    /// every batch.
    #[tokio::test]
    async fn successive_fetches_reuse_one_socket_with_fresh_correlation_ids() {
        let replica_id = Arc::new(AtomicI32::new(-99));
        let fetch_offset = Arc::new(AtomicI32::new(-99));
        let addr = spawn_fake_leader(vec![1, 2, 3], replica_id, fetch_offset).await;

        let mut conn = LeaderConnection::new(addr);
        let first = conn.fetch(spec(50)).await.unwrap();
        let second = conn.fetch(spec(50)).await.unwrap();

        assert_ne!(
            first.correlation_id, second.correlation_id,
            "each fetch needs its own correlation id"
        );
        assert!(conn.is_connected(), "the socket should have been reused");
    }

    /// A leader that dies mid-stream must surface as an error and leave the
    /// connection dropped, so the next attempt redials rather than writing into
    /// a dead socket.
    #[tokio::test]
    async fn a_dead_leader_drops_the_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();

        tokio::spawn(async move {
            // Accept once, then hang up without answering.
            if let Ok((socket, _)) = listener.accept().await {
                drop(socket);
            }
        });

        let mut conn = LeaderConnection::new(addr);
        let result = conn.fetch(spec(10)).await;

        assert!(result.is_err(), "a hung-up leader must not look like success");
        assert!(!conn.is_connected(), "a failed fetch must drop the socket");
    }

    /// An unreachable leader is an ordinary, recoverable condition — it must be
    /// an error, not a panic, and must not leave a half-open connection.
    #[tokio::test]
    async fn an_unreachable_leader_is_an_error() {
        // Port 1 on loopback: reserved and never listening.
        let mut conn = LeaderConnection::new("127.0.0.1:1");
        let result = conn.fetch(spec(10)).await;

        assert!(result.is_err());
        assert!(!conn.is_connected());
    }

    /// A mismatched correlation id means the stream is out of sync. Continuing
    /// to read from it would attribute one partition's records to another, so
    /// it has to be treated as fatal for the connection.
    #[tokio::test]
    async fn a_mismatched_correlation_id_is_fatal() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut len_buf = [0u8; 4];
            socket.read_exact(&mut len_buf).await.unwrap();
            let len = i32::from_be_bytes(len_buf) as usize;
            let mut payload = vec![0u8; len];
            socket.read_exact(&mut payload).await.unwrap();

            // Answer with a correlation id that was never sent.
            let response = leader_response(999_999, vec![]);
            let mut framed = BytesMut::new();
            framed.extend_from_slice(&(response.len() as i32).to_be_bytes());
            framed.extend_from_slice(&response);
            socket.write_all(&framed).await.unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;
        });

        let mut conn = LeaderConnection::new(addr);
        let result = conn.fetch(spec(10)).await;

        let err = result.expect_err("a stale correlation id must not be accepted");
        assert!(
            err.to_string().contains("correlation"),
            "error should name the mismatch, got: {err}"
        );
        assert!(!conn.is_connected());
    }

    /// The epoch query is the request that decides what gets deleted, so its
    /// wire form is checked against the *broker's own* parser and encoder — the
    /// same anti-drift rule as the fetch codec. A stand-in leader that agreed
    /// with this client but not with a real broker would prove nothing.
    #[tokio::test]
    async fn an_epoch_query_round_trips_against_the_brokers_own_codec() {
        use chronik_protocol::offset_for_leader_epoch_types::{
            encode_response as encode_epoch_response, parse_request as parse_epoch_request,
            OffsetForLeaderEpochResponse, OffsetForLeaderPartition,
            OffsetForLeaderPartitionResponse, OffsetForLeaderTopic, OffsetForLeaderTopicResponse,
        };
        use chronik_protocol::parser::Decoder;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let seen_epoch = Arc::new(AtomicI32::new(-99));
        let seen_epoch_for_task = Arc::clone(&seen_epoch);

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut len_buf = [0u8; 4];
            socket.read_exact(&mut len_buf).await.unwrap();
            let len = i32::from_be_bytes(len_buf) as usize;
            let mut payload = vec![0u8; len];
            socket.read_exact(&mut payload).await.unwrap();

            // Parse exactly as the broker does: header, then body.
            let mut wire = Bytes::from(payload);
            let header = parse_request_header(&mut wire).unwrap();
            assert_eq!(header.api_key as i16, 23);
            assert_eq!(header.api_version, 0);

            let mut decoder = Decoder::new(&mut wire);
            let request = parse_epoch_request(&mut decoder).unwrap();
            assert_eq!(
                decoder.remaining(),
                0,
                "the broker must consume the whole request this client sends"
            );
            seen_epoch_for_task.store(
                request.topics[0].partitions[0].leader_epoch,
                Ordering::SeqCst,
            );

            let mut body = BytesMut::new();
            encode_epoch_response(
                &mut body,
                &OffsetForLeaderEpochResponse {
                    topics: vec![OffsetForLeaderTopicResponse {
                        name: "orders".to_string(),
                        partitions: vec![OffsetForLeaderPartitionResponse {
                            error_code: 0,
                            partition: 0,
                            end_offset: 42,
                        }],
                    }],
                },
            );

            let mut framed = BytesMut::new();
            let mut response = BytesMut::new();
            write_response_header(
                &mut response,
                &ResponseHeader {
                    correlation_id: header.correlation_id,
                },
            );
            response.extend_from_slice(&body);
            framed.extend_from_slice(&(response.len() as i32).to_be_bytes());
            framed.extend_from_slice(&response);
            socket.write_all(&framed).await.unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;
        });

        let mut conn = LeaderConnection::new(addr);
        let response = conn
            .offset_for_leader_epoch(&OffsetForLeaderEpochRequest {
                topics: vec![OffsetForLeaderTopic {
                    name: "orders".to_string(),
                    partitions: vec![OffsetForLeaderPartition {
                        partition: 0,
                        leader_epoch: 7,
                    }],
                }],
            })
            .await
            .expect("the epoch query should round trip");

        assert_eq!(seen_epoch.load(Ordering::SeqCst), 7);
        assert_eq!(response.topics[0].partitions[0].end_offset, 42);
    }

    /// A stale correlation id on the epoch exchange must be fatal for the same
    /// reason it is on fetch: the stream is out of sync, and the next answer
    /// read would be attributed to the wrong partition — here, deciding what to
    /// delete.
    #[tokio::test]
    async fn a_mismatched_correlation_id_on_an_epoch_query_is_fatal() {
        use chronik_protocol::offset_for_leader_epoch_types::{
            encode_response as encode_epoch_response, OffsetForLeaderEpochResponse,
            OffsetForLeaderPartition, OffsetForLeaderTopic,
        };

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut len_buf = [0u8; 4];
            socket.read_exact(&mut len_buf).await.unwrap();
            let len = i32::from_be_bytes(len_buf) as usize;
            let mut payload = vec![0u8; len];
            socket.read_exact(&mut payload).await.unwrap();

            let mut body = BytesMut::new();
            encode_epoch_response(&mut body, &OffsetForLeaderEpochResponse { topics: vec![] });

            let mut response = BytesMut::new();
            write_response_header(
                &mut response,
                &ResponseHeader {
                    correlation_id: 123_456,
                },
            );
            response.extend_from_slice(&body);

            let mut framed = BytesMut::new();
            framed.extend_from_slice(&(response.len() as i32).to_be_bytes());
            framed.extend_from_slice(&response);
            socket.write_all(&framed).await.unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;
        });

        let mut conn = LeaderConnection::new(addr);
        let err = conn
            .offset_for_leader_epoch(&OffsetForLeaderEpochRequest {
                topics: vec![OffsetForLeaderTopic {
                    name: "orders".to_string(),
                    partitions: vec![OffsetForLeaderPartition {
                        partition: 0,
                        leader_epoch: 1,
                    }],
                }],
            })
            .await
            .expect_err("a stale correlation id must not be accepted");

        assert!(err.to_string().contains("correlation"), "got: {err}");
        assert!(!conn.is_connected());
    }

    /// A bogus length prefix must be rejected before the allocation, not after.
    #[tokio::test]
    async fn an_oversized_frame_is_refused() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut len_buf = [0u8; 4];
            socket.read_exact(&mut len_buf).await.unwrap();
            let len = i32::from_be_bytes(len_buf) as usize;
            let mut payload = vec![0u8; len];
            socket.read_exact(&mut payload).await.unwrap();

            socket.write_all(&i32::MAX.to_be_bytes()).await.unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;
        });

        let mut conn = LeaderConnection::new(addr);
        let err = conn
            .fetch(spec(10))
            .await
            .expect_err("an absurd frame length must be refused");
        assert!(err.to_string().contains("cap"), "got: {err}");
    }
}
