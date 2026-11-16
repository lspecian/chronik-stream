//! gRPC transport for a [`RaftDriver`](crate::driver::RaftDriver).
//!
//! - [`CatalogRaftServer`] receives message batches and forwarded proposals.
//! - [`PeerOutbox`] is the driver's [`Outbox`]: it queues messages per peer
//!   without blocking the driver thread; one task per peer drains its queue
//!   into a batch RPC. A failed send tells raft the peer is unreachable (and
//!   that a snapshot failed, if one was in the batch) so it backs off and
//!   retries instead of pipelining into the void.
//! - [`forward_propose`] sends a command to the leader.
//!
//! Peer addresses live in a shared [`PeerDirectory`], updated when membership
//! changes.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;

use prost::Message as ProstMessage;
use raft::eraftpb::{ConfChangeV2, Message, MessageType};
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tonic::transport::{Channel, Endpoint};
use tonic::{Request, Response, Status};

use crate::driver::{Applied, Outbox, ProposeError, RaftHandle};

pub mod proto {
    tonic::include_proto!("chronik.catalog");
}

use proto::catalog_raft_client::CatalogRaftClient;
use proto::catalog_raft_server::{CatalogRaft, CatalogRaftServer as GeneratedServer};
use proto::propose_response::Outcome;

/// Snapshots of a large catalog run to a few MB; leave generous room.
const MAX_MESSAGE_BYTES: usize = 256 * 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const SEND_TIMEOUT: Duration = Duration::from_secs(10);

/// Fault injection for the acceptance suite, off unless `CHRONIK_FAULT_FILE`
/// names a file. The file is re-read every 200 ms; each line is one of:
///
/// ```text
/// drop <peer-id>      # no catalog traffic to or from that peer (a partition)
/// delay <millis>      # hold every inbound message batch this long
/// ```
///
/// Removing a line (or the file) heals it.
#[derive(Clone, Default)]
pub struct NetFaults(Option<Arc<RwLock<FaultState>>>);

#[derive(Default, Debug, Clone, PartialEq, Eq)]
struct FaultState {
    dropped: std::collections::HashSet<u64>,
    delay_ms: u64,
}

impl NetFaults {
    /// Faults from `CHRONIK_FAULT_FILE` (`%n` is replaced by this node's id, so
    /// one environment can give each node its own file), watched on `runtime`;
    /// inert when unset.
    pub fn from_env(runtime: &Handle, node_id: u64) -> Self {
        let Ok(path) = std::env::var("CHRONIK_FAULT_FILE").map(|p| p.replace("%n", &node_id.to_string())) else {
            return Self(None);
        };
        tracing::warn!("Catalog fault injection enabled from {}", path);
        let state = Arc::new(RwLock::new(FaultState::default()));
        let watched = state.clone();
        runtime.spawn(async move {
            loop {
                let next = std::fs::read_to_string(&path).map(|s| parse_faults(&s)).unwrap_or_default();
                let changed = *watched.read().unwrap() != next;
                if changed {
                    tracing::warn!("Catalog faults now: {:?}", next);
                    *watched.write().unwrap() = next;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        });
        Self(Some(state))
    }

    pub fn is_dropped(&self, peer: u64) -> bool {
        self.0.as_ref().map(|s| s.read().unwrap().dropped.contains(&peer)).unwrap_or(false)
    }

    fn inbound_delay(&self) -> Option<Duration> {
        let ms = self.0.as_ref().map(|s| s.read().unwrap().delay_ms).unwrap_or(0);
        (ms > 0).then(|| Duration::from_millis(ms))
    }
}

fn parse_faults(text: &str) -> FaultState {
    let mut state = FaultState::default();
    for line in text.lines() {
        let mut words = line.split_whitespace();
        match (words.next(), words.next().and_then(|w| w.parse::<u64>().ok())) {
            (Some("drop"), Some(peer)) => {
                state.dropped.insert(peer);
            }
            (Some("delay"), Some(ms)) => state.delay_ms = ms,
            _ => {}
        }
    }
    state
}

/// Node id → gRPC address (`host:port`, no scheme).
#[derive(Clone, Default)]
pub struct PeerDirectory(Arc<RwLock<HashMap<u64, String>>>);

impl PeerDirectory {
    pub fn new(peers: impl IntoIterator<Item = (u64, String)>) -> Self {
        Self(Arc::new(RwLock::new(peers.into_iter().collect())))
    }

    pub fn set(&self, id: u64, addr: String) {
        self.0.write().unwrap().insert(id, addr);
    }

    pub fn remove(&self, id: u64) {
        self.0.write().unwrap().remove(&id);
    }

    pub fn get(&self, id: u64) -> Option<String> {
        self.0.read().unwrap().get(&id).cloned()
    }

    pub fn all(&self) -> HashMap<u64, String> {
        self.0.read().unwrap().clone()
    }
}

fn endpoint(addr: &str) -> Result<Endpoint, tonic::transport::Error> {
    let uri = if addr.starts_with("http://") || addr.starts_with("https://") {
        addr.to_string()
    } else {
        format!("http://{}", addr)
    };
    Ok(Endpoint::from_shared(uri)?.connect_timeout(CONNECT_TIMEOUT).timeout(SEND_TIMEOUT).tcp_nodelay(true))
}

fn client(channel: Channel) -> CatalogRaftClient<Channel> {
    CatalogRaftClient::new(channel)
        .max_decoding_message_size(MAX_MESSAGE_BYTES)
        .max_encoding_message_size(MAX_MESSAGE_BYTES)
}

// ---- server ----------------------------------------------------------------

/// Serves one node's raft endpoint.
#[derive(Clone)]
pub struct CatalogRaftServer {
    handle: RaftHandle,
    faults: NetFaults,
}

impl CatalogRaftServer {
    pub fn new(handle: RaftHandle) -> Self {
        Self { handle, faults: NetFaults::default() }
    }

    /// Apply injected faults (acceptance tests) to inbound traffic.
    pub fn with_faults(mut self, faults: NetFaults) -> Self {
        self.faults = faults;
        self
    }

    /// The tonic service, ready to add to a router.
    pub fn into_service(self) -> GeneratedServer<Self> {
        GeneratedServer::new(self)
            .max_decoding_message_size(MAX_MESSAGE_BYTES)
            .max_encoding_message_size(MAX_MESSAGE_BYTES)
    }
}

#[tonic::async_trait]
impl CatalogRaft for CatalogRaftServer {
    async fn send(&self, request: Request<proto::MessageBatch>) -> Result<Response<proto::SendAck>, Status> {
        let batch = request.into_inner();
        if self.faults.is_dropped(batch.from) {
            return Err(Status::unavailable("partitioned (fault injection)"));
        }
        if let Some(delay) = self.faults.inbound_delay() {
            tokio::time::sleep(delay).await;
        }
        for bytes in batch.messages {
            match Message::decode(bytes.as_slice()) {
                Ok(msg) => self.handle.step(msg),
                Err(e) => return Err(Status::invalid_argument(format!("undecodable raft message: {}", e))),
            }
        }
        Ok(Response::new(proto::SendAck {}))
    }

    async fn propose(&self, request: Request<proto::ProposeRequest>) -> Result<Response<proto::ProposeResponse>, Status> {
        let req = request.into_inner();
        let result = if req.conf_change {
            let cc = ConfChangeV2::decode(req.data.as_slice())
                .map_err(|e| Status::invalid_argument(format!("undecodable conf change: {}", e)))?;
            self.handle.propose_conf_change(cc).await
        } else {
            self.handle.propose(req.data).await
        };
        let outcome = match result {
            Ok(a) => Outcome::Applied(proto::Applied { index: a.index, term: a.term, result: a.result }),
            Err(ProposeError::NotLeader { leader }) => Outcome::NotLeader(leader.unwrap_or(0)),
            Err(ProposeError::Dropped) => Outcome::Dropped(true),
            Err(ProposeError::OutcomeUnknown) => Outcome::OutcomeUnknown(true),
            Err(ProposeError::Stopped) => Outcome::Stopped("raft driver stopped".into()),
        };
        Ok(Response::new(proto::ProposeResponse { outcome: Some(outcome) }))
    }
}

// ---- forwarding ------------------------------------------------------------

/// Why a forwarded proposal did not produce an outcome from the leader.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForwardError {
    /// The leader answered; this is its verdict.
    Raft(ProposeError),
    /// No connection could be made: the command was never sent, so
    /// retrying cannot apply it twice.
    Unreachable(String),
    /// The request was sent but no answer came back. The leader may have
    /// applied it; the outcome is unknown.
    Lost(String),
}

impl std::fmt::Display for ForwardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ForwardError::Raft(e) => write!(f, "{}", e),
            ForwardError::Unreachable(m) => write!(f, "leader unreachable: {}", m),
            ForwardError::Lost(m) => write!(f, "forwarded proposal lost: {}", m),
        }
    }
}

/// Send a command (or an encoded `ConfChangeV2`) to the node at `addr`.
pub async fn forward_propose(addr: &str, data: Vec<u8>, conf_change: bool, timeout: Duration) -> Result<Applied, ForwardError> {
    let ep = endpoint(addr).map_err(|e| ForwardError::Unreachable(e.to_string()))?.timeout(timeout);
    let channel = ep.connect().await.map_err(|e| ForwardError::Unreachable(e.to_string()))?;
    let resp = client(channel)
        .propose(proto::ProposeRequest { data, conf_change })
        .await
        .map_err(|s| ForwardError::Lost(s.to_string()))?
        .into_inner();
    match resp.outcome {
        Some(Outcome::Applied(a)) => Ok(Applied { index: a.index, term: a.term, result: a.result }),
        Some(Outcome::NotLeader(l)) => Err(ForwardError::Raft(ProposeError::NotLeader { leader: (l != 0).then_some(l) })),
        Some(Outcome::Dropped(_)) => Err(ForwardError::Raft(ProposeError::Dropped)),
        Some(Outcome::OutcomeUnknown(_)) => Err(ForwardError::Raft(ProposeError::OutcomeUnknown)),
        Some(Outcome::Stopped(_)) | None => Err(ForwardError::Raft(ProposeError::Stopped)),
    }
}

// ---- outbox ----------------------------------------------------------------

/// The driver's outbox. Construct it, start the driver with it, then call
/// [`PeerOutbox::bind`] with the driver's handle so send failures can be
/// reported back to raft.
pub struct PeerOutbox {
    from: u64,
    directory: PeerDirectory,
    runtime: Handle,
    queues: HashMap<u64, mpsc::UnboundedSender<Message>>,
    handle: Arc<OnceLock<RaftHandle>>,
    faults: NetFaults,
}

impl PeerOutbox {
    pub fn new(from: u64, directory: PeerDirectory, runtime: Handle) -> Self {
        Self {
            from,
            directory,
            runtime,
            queues: HashMap::new(),
            handle: Arc::new(OnceLock::new()),
            faults: NetFaults::default(),
        }
    }

    /// Apply injected faults (acceptance tests) to outbound traffic.
    pub fn with_faults(mut self, faults: NetFaults) -> Self {
        self.faults = faults;
        self
    }

    /// A binder to call once the driver is running.
    pub fn binder(&self) -> OutboxBinder {
        OutboxBinder(self.handle.clone())
    }

    fn queue(&mut self, to: u64) -> &mpsc::UnboundedSender<Message> {
        if self.queues.get(&to).map(|q| q.is_closed()).unwrap_or(true) {
            let (tx, rx) = mpsc::unbounded_channel();
            let task = PeerTask {
                from: self.from,
                to,
                directory: self.directory.clone(),
                handle: self.handle.clone(),
                client: None,
                client_addr: None,
                faults: self.faults.clone(),
            };
            self.runtime.spawn(task.run(rx));
            self.queues.insert(to, tx);
        }
        &self.queues[&to]
    }
}

/// Connects a [`PeerOutbox`] to the driver it serves.
#[derive(Clone)]
pub struct OutboxBinder(Arc<OnceLock<RaftHandle>>);

impl OutboxBinder {
    pub fn bind(&self, handle: RaftHandle) {
        let _ = self.0.set(handle);
    }
}

impl Outbox for PeerOutbox {
    fn send(&mut self, messages: Vec<Message>) {
        for m in messages {
            let to = m.to;
            if to == self.from || to == 0 {
                continue;
            }
            let _ = self.queue(to).send(m);
        }
    }
}

struct PeerTask {
    from: u64,
    to: u64,
    directory: PeerDirectory,
    handle: Arc<OnceLock<RaftHandle>>,
    client: Option<CatalogRaftClient<Channel>>,
    client_addr: Option<String>,
    faults: NetFaults,
}

impl PeerTask {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Message>) {
        let mut batch = Vec::new();
        while let Some(first) = rx.recv().await {
            batch.push(first);
            while let Ok(m) = rx.try_recv() {
                batch.push(m);
            }
            let had_snapshot = batch.iter().any(|m| m.get_msg_type() == MessageType::MsgSnapshot);
            let messages: Vec<Vec<u8>> = batch.drain(..).map(|m| m.encode_to_vec()).collect();
            if self.faults.is_dropped(self.to) {
                continue;
            }
            if let Err(e) = self.deliver(messages).await {
                tracing::debug!("Raft {} → {}: send failed: {}", self.from, self.to, e);
                self.client = None;
                if let Some(h) = self.handle.get() {
                    h.report_unreachable(self.to);
                    if had_snapshot {
                        h.report_snapshot_failed(self.to);
                    }
                }
            }
        }
    }

    async fn deliver(&mut self, messages: Vec<Vec<u8>>) -> Result<(), String> {
        let addr = self.directory.get(self.to).ok_or_else(|| format!("no address for node {}", self.to))?;
        if self.client_addr.as_deref() != Some(addr.as_str()) {
            self.client = None;
        }
        if self.client.is_none() {
            let channel = endpoint(&addr).map_err(|e| e.to_string())?.connect().await.map_err(|e| e.to_string())?;
            self.client = Some(client(channel));
            self.client_addr = Some(addr);
        }
        let c = self.client.as_mut().expect("connected above");
        c.send(proto::MessageBatch { from: self.from, messages }).await.map_err(|s| s.to_string())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::{DriverConfig, RaftDriver, StateMachine};
    use std::collections::BTreeSet;
    use std::sync::Mutex;
    use tempfile::TempDir;
    use tokio::net::TcpListener;

    #[derive(Clone, Default)]
    struct Names(Arc<Mutex<BTreeSet<String>>>);

    impl StateMachine for Names {
        fn apply(&mut self, _i: u64, _t: u64, cmd: &[u8]) -> Vec<u8> {
            let name = String::from_utf8(cmd.to_vec()).unwrap();
            if self.0.lock().unwrap().insert(name) { b"created".to_vec() } else { b"exists".to_vec() }
        }
        fn snapshot(&self) -> Vec<u8> {
            serde_json::to_vec(&*self.0.lock().unwrap()).unwrap()
        }
        fn restore(&mut self, _i: u64, data: &[u8]) -> Result<(), String> {
            *self.0.lock().unwrap() = serde_json::from_slice(data).map_err(|e| e.to_string())?;
            Ok(())
        }
    }

    struct Node {
        driver: RaftDriver,
        sm: Names,
        addr: String,
        _dir: TempDir,
    }

    async fn start_cluster(n: u64) -> Vec<Node> {
        let mut listeners = Vec::new();
        for _ in 0..n {
            listeners.push(TcpListener::bind("127.0.0.1:0").await.unwrap());
        }
        let directory = PeerDirectory::new(
            listeners.iter().enumerate().map(|(i, l)| (i as u64 + 1, l.local_addr().unwrap().to_string())),
        );
        let voters: Vec<u64> = (1..=n).collect();
        let mut nodes = Vec::new();
        for (i, listener) in listeners.into_iter().enumerate() {
            let id = i as u64 + 1;
            let dir = TempDir::new().unwrap();
            let sm = Names::default();
            let outbox = PeerOutbox::new(id, directory.clone(), Handle::current());
            let binder = outbox.binder();
            let cfg = DriverConfig {
                tick_interval: Duration::from_millis(20),
                election_tick: 10,
                heartbeat_tick: 2,
                ..DriverConfig::new(id)
            };
            let driver = RaftDriver::start(dir.path(), cfg, &voters, sm.clone(), outbox).unwrap();
            binder.bind(driver.handle());
            let addr = listener.local_addr().unwrap().to_string();
            let service = CatalogRaftServer::new(driver.handle()).into_service();
            tokio::spawn(async move {
                tonic::transport::Server::builder()
                    .add_service(service)
                    .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
                    .await
                    .unwrap();
            });
            nodes.push(Node { driver, sm, addr, _dir: dir });
        }
        nodes
    }

    /// Three drivers over real localhost gRPC: elect, propose on the leader,
    /// forward from a follower, and converge.
    #[tokio::test(flavor = "multi_thread")]
    async fn three_nodes_over_grpc() {
        let nodes = start_cluster(3).await;
        let leader = tokio::time::timeout(Duration::from_secs(10), nodes[0].driver.handle().wait_for_leader())
            .await
            .unwrap()
            .unwrap();
        let li = (leader - 1) as usize;
        let a = nodes[li].driver.handle().propose(b"a".to_vec()).await.unwrap();
        assert_eq!(a.result, b"created");

        let follower = (li + 1) % 3;
        // A follower learns the leader from its first append or heartbeat.
        tokio::time::timeout(Duration::from_secs(10), nodes[follower].driver.handle().wait_for_leader())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            nodes[follower].driver.handle().propose(b"b".to_vec()).await,
            Err(ProposeError::NotLeader { leader: Some(leader) })
        );
        // Forwarded from the follower to the leader's address.
        let b = forward_propose(&nodes[li].addr, b"b".to_vec(), false, Duration::from_secs(5)).await.unwrap();
        assert_eq!(b.result, b"created");
        let again = forward_propose(&nodes[li].addr, b"b".to_vec(), false, Duration::from_secs(5)).await.unwrap();
        assert_eq!(again.result, b"exists");
        // A follower refuses a forwarded proposal and names the leader.
        assert_eq!(
            forward_propose(&nodes[follower].addr, b"c".to_vec(), false, Duration::from_secs(5)).await,
            Err(ForwardError::Raft(ProposeError::NotLeader { leader: Some(leader) }))
        );
        for n in &nodes {
            tokio::time::timeout(Duration::from_secs(10), n.driver.handle().wait_applied(again.index)).await.unwrap().unwrap();
            assert_eq!(*n.sm.0.lock().unwrap(), BTreeSet::from(["a".to_string(), "b".to_string()]));
        }
        // Followers track leader contact; the gate relies on it.
        let age = nodes[follower].driver.handle().leader_contact_age().unwrap();
        assert!(age < Duration::from_secs(2), "{:?}", age);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unreachable_leader_is_reported() {
        let r = forward_propose("127.0.0.1:1", b"x".to_vec(), false, Duration::from_millis(500)).await;
        assert!(matches!(r, Err(ForwardError::Unreachable(_))), "{:?}", r);
    }

    #[test]
    fn fault_file_lines() {
        let s = parse_faults("drop 2\n# comment\ndelay 1500\ndrop 3\nbogus\ndrop x\n");
        assert_eq!(s.dropped, std::collections::HashSet::from([2, 3]));
        assert_eq!(s.delay_ms, 1500);
        assert_eq!(parse_faults(""), FaultState::default());
    }
}
