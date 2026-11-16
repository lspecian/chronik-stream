//! The cluster catalog: topics, assignments, brokers, ACLs and SCRAM
//! credentials, replicated through one Raft group.
//!
//! Every node runs a [`RaftDriver`] whose state machine is a
//! [`CatalogState`]. Writes go through [`ClusterCatalog::execute`]: proposed
//! here when this node leads, forwarded to the leader otherwise, and answered
//! only once the command is applied on **this** node, so a client that
//! creates a topic and then asks the same broker for metadata sees it.
//!
//! Reads come from the applied state in memory. Listeners subscribe to
//! [`CatalogChange`]s, emitted on every node as commands are applied.
//!
//! A node that is booting, or has not heard from a leader for an election
//! timeout, is not *caught up* ([`ClusterCatalog::is_caught_up`]): it must not
//! treat "I have not seen this topic" as "this topic does not exist".

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chronik_common::metadata::catalog::{CatalogChange, CatalogCommand, CatalogResult, CatalogState};
use chronik_raft::driver::{DriverConfig, ProposeError, RaftDriver, RaftHandle, RaftStatus, StateMachine};
use chronik_raft::net::{forward_propose, CatalogRaftServer, ForwardError, PeerDirectory, PeerOutbox};
use chronik_raft::LogStoreOptions;
use parking_lot::{RwLock, RwLockReadGuard};
use raft::eraftpb::{ConfChangeV2, ConfState};
use tokio::sync::broadcast;

/// Why a catalog command produced no result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogUnavailable {
    /// No leader could be reached before the deadline.
    NoLeader,
    /// The command may or may not have taken effect.
    OutcomeUnknown,
    /// The catalog's raft node has stopped.
    Stopped(String),
}

impl std::fmt::Display for CatalogUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CatalogUnavailable::NoLeader => write!(f, "no catalog leader available"),
            CatalogUnavailable::OutcomeUnknown => write!(f, "catalog command outcome unknown"),
            CatalogUnavailable::Stopped(m) => write!(f, "catalog stopped: {}", m),
        }
    }
}

impl std::error::Error for CatalogUnavailable {}

impl From<CatalogUnavailable> for chronik_common::metadata::MetadataError {
    fn from(e: CatalogUnavailable) -> Self {
        chronik_common::metadata::MetadataError::StorageError(e.to_string())
    }
}

/// Applies committed catalog entries to the shared read model.
struct CatalogStateMachine {
    state: Arc<RwLock<CatalogState>>,
    changes: broadcast::Sender<CatalogChange>,
    conf_state: Arc<RwLock<ConfState>>,
}

impl StateMachine for CatalogStateMachine {
    fn apply(&mut self, index: u64, term: u64, command: &[u8]) -> Vec<u8> {
        let (result, changes) = self.state.write().apply_entry(index, term, command);
        for c in changes {
            let _ = self.changes.send(c);
        }
        result.encode()
    }

    fn snapshot(&self) -> Vec<u8> {
        self.state.read().to_snapshot()
    }

    fn restore(&mut self, _index: u64, data: &[u8]) -> Result<(), String> {
        let restored = CatalogState::from_snapshot(data)?;
        *self.state.write() = restored;
        let _ = self.changes.send(CatalogChange::Reset);
        Ok(())
    }

    fn on_conf_change(&mut self, _index: u64, conf_state: &ConfState) {
        *self.conf_state.write() = conf_state.clone();
    }
}

#[derive(Debug, Clone)]
pub struct ClusterCatalogConfig {
    pub node_id: u64,
    /// Directory for the catalog's raft log, state and snapshots.
    pub data_dir: PathBuf,
    /// Raft gRPC address of every known node, this one included.
    pub peers: Vec<(u64, String)>,
    /// Founding voters for a brand-new cluster (identical on every founding
    /// node); empty for a node joining an existing cluster.
    pub initial_voters: Vec<u64>,
    pub tick_interval: Duration,
    pub election_tick: usize,
    pub heartbeat_tick: usize,
    pub snapshot_every: u64,
    /// How long `execute` keeps trying to reach a leader.
    pub propose_timeout: Duration,
}

impl ClusterCatalogConfig {
    pub fn new(node_id: u64, data_dir: PathBuf, peers: Vec<(u64, String)>, initial_voters: Vec<u64>) -> Self {
        Self {
            node_id,
            data_dir,
            peers,
            initial_voters,
            tick_interval: Duration::from_millis(100),
            election_tick: 20,
            heartbeat_tick: 3,
            snapshot_every: 10_000,
            propose_timeout: Duration::from_secs(30),
        }
    }

    fn election_timeout(&self) -> Duration {
        self.tick_interval * self.election_tick as u32
    }
}

pub struct ClusterCatalog {
    node_id: u64,
    driver: RaftDriver,
    handle: RaftHandle,
    state: Arc<RwLock<CatalogState>>,
    conf_state: Arc<RwLock<ConfState>>,
    changes: broadcast::Sender<CatalogChange>,
    directory: PeerDirectory,
    propose_timeout: Duration,
    election_timeout: Duration,
    /// Set once this node has applied everything the leader had committed
    /// when it first heard from it after boot.
    booted: AtomicBool,
}

impl ClusterCatalog {
    /// Open the catalog log and start its raft node. Transport tasks run on
    /// `runtime`; the raft loop has its own thread.
    pub fn start(config: ClusterCatalogConfig, runtime: tokio::runtime::Handle) -> std::io::Result<Self> {
        std::fs::create_dir_all(&config.data_dir)?;
        let state = Arc::new(RwLock::new(CatalogState::default()));
        let conf_state = Arc::new(RwLock::new(ConfState::default()));
        let (changes, _) = broadcast::channel(4096);
        let sm = CatalogStateMachine { state: state.clone(), changes: changes.clone(), conf_state: conf_state.clone() };
        let directory = PeerDirectory::new(config.peers.clone());
        let outbox = PeerOutbox::new(config.node_id, directory.clone(), runtime);
        let binder = outbox.binder();
        let driver_config = DriverConfig {
            id: config.node_id,
            tick_interval: config.tick_interval,
            election_tick: config.election_tick,
            heartbeat_tick: config.heartbeat_tick,
            snapshot_every: config.snapshot_every,
            log_store: LogStoreOptions::default(),
        };
        let driver = RaftDriver::start(&config.data_dir, driver_config, &config.initial_voters, sm, outbox)?;
        let handle = driver.handle();
        binder.bind(handle.clone());
        Ok(Self {
            node_id: config.node_id,
            driver,
            handle,
            state,
            conf_state,
            changes,
            directory,
            propose_timeout: config.propose_timeout,
            election_timeout: config.election_timeout(),
            booted: AtomicBool::new(false),
        })
    }

    /// The gRPC service peers use to reach this node's raft endpoint.
    pub fn service(&self) -> chronik_raft::net::proto::catalog_raft_server::CatalogRaftServer<CatalogRaftServer> {
        CatalogRaftServer::new(self.handle.clone()).into_service()
    }

    pub fn node_id(&self) -> u64 {
        self.node_id
    }

    pub fn raft_handle(&self) -> &RaftHandle {
        &self.handle
    }

    pub fn directory(&self) -> &PeerDirectory {
        &self.directory
    }

    // ---- reads -------------------------------------------------------------

    /// The applied catalog. Hold the guard briefly; never across an await.
    pub fn read(&self) -> RwLockReadGuard<'_, CatalogState> {
        self.state.read()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<CatalogChange> {
        self.changes.subscribe()
    }

    pub fn status(&self) -> RaftStatus {
        self.handle.status()
    }

    pub fn is_leader(&self) -> bool {
        self.handle.status().is_leader
    }

    pub fn leader_id(&self) -> Option<u64> {
        self.handle.status().leader_id
    }

    /// Voters and learners of the raft group.
    pub fn conf_state(&self) -> ConfState {
        self.conf_state.read().clone()
    }

    /// True when this node's catalog can be trusted to be complete: it has
    /// heard from a leader within an election timeout (or is the leader) and,
    /// since boot, has applied everything that leader had committed.
    pub fn is_caught_up(&self) -> bool {
        let s = self.handle.status();
        if s.failed.is_some() || s.leader_id.is_none() {
            return false;
        }
        match self.handle.leader_contact_age() {
            Some(age) if age <= self.election_timeout => {}
            _ => return false,
        }
        if self.booted.load(Ordering::Relaxed) {
            return true;
        }
        if s.applied >= s.commit {
            self.booted.store(true, Ordering::Relaxed);
            return true;
        }
        false
    }

    /// Resolve once the node is caught up, or false at the deadline.
    pub async fn wait_caught_up(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut status = self.handle.subscribe();
        loop {
            if self.is_caught_up() {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            // Leader contact is not part of the status, so poll as well.
            let wait = (deadline - now).min(Duration::from_millis(50));
            let _ = tokio::time::timeout(wait, status.changed()).await;
        }
    }

    // ---- writes ------------------------------------------------------------

    /// Run a command through the log and return its result once it is applied
    /// on this node.
    pub async fn execute(&self, cmd: CatalogCommand) -> Result<CatalogResult, CatalogUnavailable> {
        let bytes = cmd.encode();
        let applied = self.propose_bytes(bytes, false).await?;
        CatalogResult::decode(&applied.result).map_err(|e| CatalogUnavailable::Stopped(format!("undecodable result: {}", e)))
    }

    /// Change raft membership; returns the resulting configuration.
    pub async fn change_membership(&self, cc: ConfChangeV2) -> Result<ConfState, CatalogUnavailable> {
        use prost::Message;
        let applied = self.propose_bytes(cc.encode_to_vec(), true).await?;
        if applied.result.is_empty() {
            return Err(CatalogUnavailable::Stopped("membership change rejected by raft".into()));
        }
        ConfState::decode(applied.result.as_slice()).map_err(|e| CatalogUnavailable::Stopped(e.to_string()))
    }

    async fn propose_bytes(&self, bytes: Vec<u8>, conf_change: bool) -> Result<chronik_raft::Applied, CatalogUnavailable> {
        let deadline = Instant::now() + self.propose_timeout;
        let mut backoff = Duration::from_millis(20);
        loop {
            let local = if conf_change {
                use prost::Message;
                let cc = ConfChangeV2::decode(bytes.as_slice()).map_err(|e| CatalogUnavailable::Stopped(e.to_string()))?;
                self.handle.propose_conf_change(cc).await
            } else {
                self.handle.propose(bytes.clone()).await
            };
            match local {
                Ok(a) => return Ok(a),
                Err(ProposeError::OutcomeUnknown) => return Err(CatalogUnavailable::OutcomeUnknown),
                Err(ProposeError::Stopped) => {
                    let why = self.handle.status().failed.unwrap_or_else(|| "raft driver stopped".into());
                    return Err(CatalogUnavailable::Stopped(why));
                }
                Err(ProposeError::NotLeader { leader: Some(leader) }) if leader != self.node_id => {
                    if let Some(addr) = self.directory.get(leader) {
                        let remaining = deadline.saturating_duration_since(Instant::now()).max(Duration::from_millis(100));
                        match forward_propose(&addr, bytes.clone(), conf_change, remaining).await {
                            Ok(a) => {
                                // Read-your-writes: answer only once applied here too.
                                let wait = deadline.saturating_duration_since(Instant::now()).max(self.election_timeout);
                                return match tokio::time::timeout(wait, self.handle.wait_applied(a.index)).await {
                                    Ok(Ok(())) => Ok(a),
                                    Ok(Err(_)) => Err(CatalogUnavailable::Stopped("raft driver stopped".into())),
                                    // Committed on the leader, not yet visible here.
                                    Err(_) => Err(CatalogUnavailable::OutcomeUnknown),
                                };
                            }
                            Err(ForwardError::Raft(ProposeError::OutcomeUnknown)) | Err(ForwardError::Lost(_)) => {
                                return Err(CatalogUnavailable::OutcomeUnknown)
                            }
                            // Never sent, or refused before acceptance: safe to retry.
                            Err(ForwardError::Unreachable(e)) => {
                                tracing::debug!("Catalog leader {} at {} unreachable: {}", leader, addr, e);
                            }
                            Err(ForwardError::Raft(e)) => {
                                tracing::debug!("Catalog leader {} refused a forwarded command: {}", leader, e);
                            }
                        }
                    }
                }
                // No leader known yet, or raft dropped it (e.g. leadership transfer).
                Err(_) => {}
            }
            if Instant::now() + backoff >= deadline {
                return Err(CatalogUnavailable::NoLeader);
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_millis(500));
        }
    }

    /// Stop the raft node.
    pub fn stop(self) {
        self.driver.stop();
    }
}
