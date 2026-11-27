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

use bytes::{Buf, BufMut, Bytes, BytesMut};
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
    FetchRequestSpec, FetchResponseFrame,
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

/// Budget for the whole SASL exchange. PBKDF2 at 4096 iterations is a few
/// milliseconds; anything near this is a leader that is not answering.
const AUTH_TIMEOUT: Duration = Duration::from_secs(15);

const SASL_HANDSHAKE_API_KEY: i16 = 17;
const SASL_AUTHENTICATE_API_KEY: i16 = 36;

/// The byte stream to a leader: plain TCP, or TLS over it when the Kafka port
/// uses TLS.
pub(crate) trait LeaderIo: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> LeaderIo for T {}
pub(crate) type LeaderStream = Box<dyn LeaderIo>;

/// A lazily-connected, self-healing link to one leader's Kafka port.
pub struct LeaderConnection {
    addr: String,
    stream: Option<LeaderStream>,
    next_correlation_id: i32,
    /// Inter-broker secret. When set, every new socket authenticates as
    /// `__chronik_broker` before its first request.
    secret: Option<String>,
    /// TLS client, when leaders' Kafka ports use TLS.
    tls: Option<tokio_rustls::TlsConnector>,
}

impl LeaderConnection {
    pub fn new(addr: impl Into<String>) -> Self {
        Self {
            addr: addr.into(),
            stream: None,
            next_correlation_id: 1,
            secret: None,
            tls: None,
        }
    }

    /// A connection for replication: authenticates with
    /// `CHRONIK_INTER_BROKER_SECRET` when it is set.
    pub fn for_replication(addr: impl Into<String>) -> Self {
        let mut conn = Self::new(addr);
        conn.secret = crate::inter_broker::secret().map(str::to_string);
        conn.tls = crate::inter_broker::tls().cloned();
        conn
    }

    /// Speak TLS to the leader with this client.
    #[cfg(test)]
    pub fn with_tls(mut self, tls: tokio_rustls::TlsConnector) -> Self {
        self.tls = Some(tls);
        self
    }

    pub fn addr(&self) -> &str {
        &self.addr
    }

    /// True when a socket is currently held. Only useful for tests and
    /// diagnostics — callers should not branch on it, because a held socket
    /// says nothing about whether the peer is alive (a TCP write lands in the
    /// local send buffer long after the peer is gone; that mistake cost the
    /// push stack an ISR-eviction bug that unit tests could not see).
    #[cfg(test)]
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
        let stream = dial(&self.addr, self.tls.as_ref(), self.secret.as_deref()).await?;
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

/// Open a broker-to-broker connection to another broker's Kafka port the way
/// a client must: TLS when its port uses TLS, then SASL as `__chronik_broker`
/// when a secret is set. Used by the replica fetcher and by produce
/// forwarding, so both reach a TLS or SASL leader.
pub(crate) async fn dial(
    addr: &str,
    tls: Option<&tokio_rustls::TlsConnector>,
    secret: Option<&str>,
) -> Result<LeaderStream> {
    let stream = timeout(CONNECT_TIMEOUT, TcpStream::connect(addr))
        .await
        .map_err(|_| Error::Network(format!("Connect to leader {addr} timed out after {CONNECT_TIMEOUT:?}")))?
        .map_err(|e| Error::Network(format!("Connect to leader {addr} failed: {e}")))?;

    // Requests between brokers are latency-sensitive and small; Nagle would
    // add up to a round trip per request for no benefit.
    if let Err(e) = stream.set_nodelay(true) {
        warn!("Could not disable Nagle on the link to {}: {}", addr, e);
    }

    let mut stream: LeaderStream = match tls {
        None => Box::new(stream),
        Some(tls) => {
            // The leader's certificate must name the host it is dialled by.
            let host = addr.rsplit_once(':').map_or(addr, |(h, _)| h);
            let host = host.trim_start_matches('[').trim_end_matches(']');
            let name = rustls::pki_types::ServerName::try_from(host.to_string())
                .map_err(|e| Error::Network(format!("Leader address {addr} has no valid TLS server name: {e}")))?;
            let tls_stream = timeout(CONNECT_TIMEOUT, tls.connect(name, stream))
                .await
                .map_err(|_| Error::Network(format!("TLS handshake with leader {addr} timed out after {CONNECT_TIMEOUT:?}")))?
                .map_err(|e| Error::Network(format!("TLS handshake with leader {addr} failed: {e}")))?;
            Box::new(tls_stream)
        }
    };
    if let Some(secret) = secret {
        // A leader with SASL required closes the socket on the first non-SASL
        // request, which surfaced only as "early eof" on every fetch.
        // Authenticate first, and say so when it fails.
        timeout(AUTH_TIMEOUT, authenticate(&mut stream, addr, secret))
            .await
            .map_err(|_| Error::Network(format!("Authentication to leader {addr} timed out after {AUTH_TIMEOUT:?}")))??;
        debug!("Authenticated to leader {} as {}", addr, chronik_protocol::sasl::INTER_BROKER_USER);
    }
    Ok(stream)
}

/// SASL SCRAM-SHA-512 as `__chronik_broker`: SaslHandshake v1, then two
/// SaslAuthenticate v1 rounds. The server's final signature is verified, so a
/// peer that does not hold the secret cannot pose as the leader. Correlation
/// ids 1–3; the requests are answered before the connection is used.
async fn authenticate(stream: &mut LeaderStream, addr: &str, secret: &str) -> Result<()> {
    use chronik_protocol::sasl::{SaslMechanism, INTER_BROKER_USER};
    use chronik_protocol::scram::ScramClient;

    let auth_err = |what: String| Error::Network(format!("Authentication to leader failed: {what}"));

    // SaslHandshake v1: mechanism string.
    let mut body = BytesMut::new();
    put_request_header(&mut body, SASL_HANDSHAKE_API_KEY, 1, 1);
    put_string(&mut body, SaslMechanism::ScramSha512.as_str());
    let mut response = sasl_round_trip(stream, addr, &body, 1).await?;
    let error_code = get_i16(&mut response)?;
    if error_code != 0 {
        return Err(auth_err(format!("leader {addr} refused SCRAM-SHA-512 (error {error_code})")));
    }

    let mut client = ScramClient::new(SaslMechanism::ScramSha512, INTER_BROKER_USER, secret);
    let server_first = sasl_authenticate(stream, addr, &client.client_first(), 2).await?;
    let client_final = client.client_final(&server_first).map_err(|e| auth_err(e.to_string()))?;
    let server_final = sasl_authenticate(stream, addr, &client_final, 3).await?;
    client
        .verify_server_final(&server_final)
        .map_err(|e| auth_err(format!("{e} ({addr})")))?;
    Ok(())
}

/// One SaslAuthenticate v1 round; returns the server's auth bytes.
async fn sasl_authenticate(stream: &mut LeaderStream, addr: &str, auth: &[u8], correlation_id: i32) -> Result<Vec<u8>> {
    let mut body = BytesMut::new();
    put_request_header(&mut body, SASL_AUTHENTICATE_API_KEY, 1, correlation_id);
    body.put_i32(auth.len() as i32);
    body.put_slice(auth);

    let mut response = sasl_round_trip(stream, addr, &body, correlation_id).await?;
    let error_code = get_i16(&mut response)?;
    let message = get_nullable_string(&mut response)?;
    if error_code != 0 {
        return Err(Error::Network(format!(
            "Authentication to leader {} failed as {} (error {}): {}. \
             Check that CHRONIK_INTER_BROKER_SECRET is identical on every broker.",
            addr,
            chronik_protocol::sasl::INTER_BROKER_USER,
            error_code,
            message.unwrap_or_default()
        )));
    }
    let len = get_i32(&mut response)?;
    if len < 0 || response.len() < len as usize {
        return Err(Error::Protocol(format!(
            "SaslAuthenticate response from {addr} has a bad auth_bytes length {len}"
        )));
    }
    Ok(response.split_to(len as usize).to_vec())
}

/// Read one length-prefixed Kafka frame.
async fn read_frame(stream: &mut LeaderStream, addr: &str) -> Result<Bytes> {
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

/// Request header v1: api key, version, correlation id, nullable client id.
/// SaslHandshake and SaslAuthenticate v1 are not flexible, so no tagged fields.
fn put_request_header(buf: &mut BytesMut, api_key: i16, version: i16, correlation_id: i32) {
    buf.put_i16(api_key);
    buf.put_i16(version);
    buf.put_i32(correlation_id);
    put_string(buf, super::protocol::REPLICA_CLIENT_ID);
}

fn put_string(buf: &mut BytesMut, s: &str) {
    buf.put_i16(s.len() as i16);
    buf.put_slice(s.as_bytes());
}

fn get_i16(buf: &mut Bytes) -> Result<i16> {
    if buf.remaining() < 2 {
        return Err(Error::Protocol("SASL response truncated".into()));
    }
    Ok(buf.get_i16())
}

fn get_i32(buf: &mut Bytes) -> Result<i32> {
    if buf.remaining() < 4 {
        return Err(Error::Protocol("SASL response truncated".into()));
    }
    Ok(buf.get_i32())
}

fn get_nullable_string(buf: &mut Bytes) -> Result<Option<String>> {
    let len = get_i16(buf)?;
    if len < 0 {
        return Ok(None);
    }
    if buf.remaining() < len as usize {
        return Err(Error::Protocol("SASL response truncated".into()));
    }
    Ok(Some(String::from_utf8_lossy(&buf.split_to(len as usize)).into_owned()))
}

/// Send one SASL request and return the response body after the correlation
/// id (response header v0, no tagged fields at v1).
async fn sasl_round_trip(
    stream: &mut LeaderStream,
    addr: &str,
    body: &[u8],
    correlation_id: i32,
) -> Result<Bytes> {
    stream
        .write_all(&frame_request(body))
        .await
        .map_err(|e| Error::Network(format!("SASL write to {addr} failed: {e}")))?;
    let mut payload = read_frame(stream, addr).await?;
    let echoed = get_i32(&mut payload)?;
    if echoed != correlation_id {
        return Err(Error::Protocol(format!(
            "SASL response from {addr} carried correlation id {echoed} but {correlation_id} was sent"
        )));
    }
    Ok(payload)
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

    /// A follower reaches a leader whose Kafka port uses TLS (it dialled in
    /// plaintext, and a TLS cluster replicated nothing): the request and the
    /// leader's reply cross the TLS session (the reply here is refused for
    /// its length, which needs it to have been read).
    #[tokio::test]
    async fn a_follower_fetches_from_a_tls_leader() {
        let pki = crate::tls::test_pki::Pki::new();
        let acceptor = crate::tls::create_tls_acceptor(&pki.listener(true)).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            for _ in 0..2 {
                let (tcp, _) = listener.accept().await.unwrap();
                let Ok(mut socket) = acceptor.accept(tcp).await else { continue };
                let mut len_buf = [0u8; 4];
                socket.read_exact(&mut len_buf).await.unwrap();
                let mut payload = vec![0u8; i32::from_be_bytes(len_buf) as usize];
                socket.read_exact(&mut payload).await.unwrap();
                socket.write_all(&i32::MAX.to_be_bytes()).await.unwrap();
                socket.flush().await.unwrap();
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        });

        let mut plain = LeaderConnection::new(format!("localhost:{port}"));
        let err = plain.fetch(spec(10)).await.expect_err("plaintext against a TLS port");
        // What production logged: the TLS alert read as a frame length.
        assert!(!err.to_string().contains(&i32::MAX.to_string()), "a plaintext fetch read the reply: {err}");

        let connector = crate::tls::create_tls_connector(&pki.listener(true)).unwrap();
        let mut conn = LeaderConnection::new(format!("localhost:{port}")).with_tls(connector);
        let err = conn.fetch(spec(10)).await.expect_err("the reply is refused for its length");
        assert!(err.to_string().contains(&i32::MAX.to_string()), "the fetch did not cross TLS: {err}");
    }
}
