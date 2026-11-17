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
use chronik_raft::net::{CatalogRaftServer, ForwardError, Forwarder, NetFaults, PeerDirectory, PeerOutbox};
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
    /// Kept in step with node addresses recorded in the catalog.
    directory: PeerDirectory,
}

impl StateMachine for CatalogStateMachine {
    fn apply(&mut self, index: u64, term: u64, command: &[u8]) -> Vec<u8> {
        let (result, changes) = self.state.write().apply_entry(index, term, command);
        for c in &changes {
            if let CatalogChange::NodeAddressChanged(id) = c {
                match self.state.read().node_addresses().get(id) {
                    Some(addr) => self.directory.set(*id, addr.clone()),
                    None => self.directory.remove(*id),
                }
            }
        }
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
        for (id, addr) in self.state.read().node_addresses() {
            self.directory.set(*id, addr.clone());
        }
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
            snapshot_every: std::env::var("CHRONIK_CATALOG_SNAPSHOT_EVERY")
                .ok()
                .and_then(|v| v.parse().ok())
                .filter(|&n: &u64| n > 0)
                .unwrap_or(10_000),
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
    leader_flag: Arc<AtomicBool>,
    /// Set while this node imports its pre-catalog metadata (first boot after
    /// upgrading); the node does not count as caught up until it is done.
    import_pending: AtomicBool,
    faults: NetFaults,
    forwarder: Forwarder,
}

impl ClusterCatalog {
    /// Open the catalog log and start its raft node. Transport tasks run on
    /// `runtime`; the raft loop has its own thread.
    pub fn start(config: ClusterCatalogConfig, runtime: tokio::runtime::Handle) -> std::io::Result<Self> {
        std::fs::create_dir_all(&config.data_dir)?;
        let state = Arc::new(RwLock::new(CatalogState::default()));
        let conf_state = Arc::new(RwLock::new(ConfState::default()));
        let (changes, _) = broadcast::channel(4096);
        let directory = PeerDirectory::new(config.peers.clone());
        let sm = CatalogStateMachine {
            state: state.clone(),
            changes: changes.clone(),
            conf_state: conf_state.clone(),
            directory: directory.clone(),
        };
        let runtime_for_watch = runtime.clone();
        let faults = NetFaults::from_env(&runtime, config.node_id);
        let outbox = PeerOutbox::new(config.node_id, directory.clone(), runtime).with_faults(faults.clone());
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
        let leader_flag = Arc::new(AtomicBool::new(handle.status().is_leader));
        {
            let flag = leader_flag.clone();
            let mut status = handle.subscribe();
            runtime_for_watch.spawn(async move {
                loop {
                    flag.store(status.borrow_and_update().is_leader, Ordering::Relaxed);
                    if status.changed().await.is_err() {
                        flag.store(false, Ordering::Relaxed);
                        return;
                    }
                }
            });
        }
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
            leader_flag,
            import_pending: AtomicBool::new(false),
            faults,
            forwarder: Forwarder::new(),
        })
    }

    /// The gRPC service peers use to reach this node's raft endpoint.
    pub fn service(&self) -> chronik_raft::net::proto::catalog_raft_server::CatalogRaftServer<CatalogRaftServer> {
        CatalogRaftServer::new(self.handle.clone()).with_faults(self.faults.clone()).into_service()
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
        !self.import_pending.load(Ordering::Relaxed) && self.raft_caught_up()
    }

    /// The raft half of `is_caught_up`: leader contact within an election
    /// timeout and, since boot, everything the leader had committed applied.
    pub fn raft_caught_up(&self) -> bool {
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

    /// Mark the pre-catalog import as running or finished.
    pub fn set_import_pending(&self, pending: bool) {
        self.import_pending.store(pending, Ordering::Relaxed);
    }

    /// Resolve once the node is caught up, or false at the deadline.
    pub async fn wait_caught_up(&self, timeout: Duration) -> bool {
        self.wait_until(timeout, || self.is_caught_up()).await
    }

    /// Resolve once raft has caught up (the import may still be running).
    pub async fn wait_raft_caught_up(&self, timeout: Duration) -> bool {
        self.wait_until(timeout, || self.raft_caught_up()).await
    }

    async fn wait_until(&self, timeout: Duration, ready: impl Fn() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        let mut status = self.handle.subscribe();
        loop {
            if ready() {
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
                    if let Some(addr) = self.directory.get(leader).filter(|_| !self.faults.is_dropped(leader)) {
                        let remaining = deadline.saturating_duration_since(Instant::now()).max(Duration::from_millis(100));
                        match self.forwarder.propose(&addr, bytes.clone(), conf_change, remaining).await {
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

    // ---- cluster-role queries used across the server ----------------------

    /// Whether this node leads the catalog (and therefore runs the
    /// cluster-wide controllers: failover, preferred-leader return,
    /// rebalancing, repair of unassigned partitions).
    pub fn am_i_leader(&self) -> bool {
        self.handle.status().is_leader
    }

    /// A flag that follows catalog leadership, for components that check it
    /// on hot or synchronous paths.
    pub fn is_leader_flag(&self) -> Arc<AtomicBool> {
        self.leader_flag.clone()
    }

    /// `(a leader is known, leader id or 0, this node's role)`.
    pub fn is_leader_ready(&self) -> (bool, u64, &'static str) {
        let s = self.handle.status();
        let role = if s.is_leader { "leader" } else if s.leader_id.is_some() { "follower" } else { "candidate" };
        (s.leader_id.is_some(), s.leader_id.unwrap_or(0), role)
    }

    /// Voting members of the catalog group, sorted.
    pub fn voters(&self) -> Vec<u64> {
        let mut v = self.conf_state.read().voters.clone();
        if v.is_empty() {
            v = self.handle.status().voters;
        }
        v.sort_unstable();
        v
    }

    /// Raft gRPC address of every node this one knows.
    pub fn node_addresses(&self) -> Vec<(u64, String)> {
        let mut v: Vec<_> = self.directory.all().into_iter().collect();
        v.sort();
        v
    }

    /// On the leader, the nodes heard from within an election timeout; `None`
    /// elsewhere. A sample, not a verdict.
    pub async fn sample_live_nodes(&self) -> Option<Vec<u64>> {
        self.handle.sample_live(self.election_timeout).await
    }

    // ---- membership --------------------------------------------------------

    /// Add a node: record its raft address and broker endpoint in the catalog,
    /// then make it a voter. The new node must be started with
    /// `CHRONIK_CLUSTER_JOIN=true` so it does not found a cluster of its own;
    /// it starts blank and the leader brings it up to date by snapshot.
    pub async fn add_node(&self, node_id: u64, kafka_addr: &str, raft_addr: &str) -> Result<ConfState, String> {
        use chronik_common::metadata::{BrokerMetadata, BrokerStatus};
        if self.voters().contains(&node_id) {
            return Err(format!("node {} is already a member", node_id));
        }
        let (host, port) = split_host_port(kafka_addr)?;
        self.expect_ok(CatalogCommand::SetNodeAddress { node_id, raft_addr: raft_addr.to_string() }).await?;
        let now = chrono::Utc::now();
        let broker = BrokerMetadata {
            broker_id: node_id as i32,
            host,
            port,
            rack: None,
            status: BrokerStatus::Online,
            created_at: now,
            updated_at: now,
        };
        self.expect_ok(CatalogCommand::RegisterBroker { broker }).await?;
        let mut cc = ConfChangeV2::default();
        let mut single = raft::eraftpb::ConfChangeSingle::default();
        single.set_change_type(raft::eraftpb::ConfChangeType::AddNode);
        single.node_id = node_id;
        cc.changes = vec![single];
        self.change_membership(cc).await.map_err(|e| e.to_string())
    }

    /// Remove a node. Unless `force`, every partition replica it holds is
    /// moved to another broker first (the catalog leader decides, fenced by
    /// leader epoch). Refuses to go below three voters, and refuses to remove
    /// this node without `force`.
    pub async fn remove_node(&self, node_id: u64, force: bool) -> Result<ConfState, String> {
        let voters = self.voters();
        if !voters.contains(&node_id) {
            return Err(format!("node {} is not a member", node_id));
        }
        if voters.len() <= 3 && !force {
            return Err(format!("removing node {} would leave {} voters; a cluster needs at least 3", node_id, voters.len() - 1));
        }
        if node_id == self.node_id && !force {
            return Err("refusing to remove this node without --force".into());
        }
        if !force {
            self.move_replicas_off(node_id).await?;
        }
        let mut cc = ConfChangeV2::default();
        let mut single = raft::eraftpb::ConfChangeSingle::default();
        single.set_change_type(raft::eraftpb::ConfChangeType::RemoveNode);
        single.node_id = node_id;
        cc.changes = vec![single];
        let cs = self.change_membership(cc).await.map_err(|e| e.to_string())?;
        self.expect_ok(CatalogCommand::UnregisterBroker { broker_id: node_id as i32 }).await.ok();
        self.expect_ok(CatalogCommand::RemoveNodeAddress { node_id }).await?;
        Ok(cs)
    }

    async fn move_replicas_off(&self, node_id: u64) -> Result<(), String> {
        use chronik_common::metadata::catalog::AssignmentChange;
        let changes: Vec<AssignmentChange> = {
            let state = self.read();
            let others: Vec<u64> =
                state.brokers().map(|b| b.broker_id as u64).filter(|&b| b != node_id).collect();
            state
                .all_assignments()
                .filter(|a| a.replicas.contains(&node_id))
                .map(|a| {
                    let mut replicas: Vec<u64> = a.replicas.iter().copied().filter(|&r| r != node_id).collect();
                    if let Some(spare) = others.iter().find(|b| !replicas.contains(b)) {
                        replicas.push(*spare);
                    }
                    AssignmentChange {
                        topic: a.topic.clone(),
                        partition: a.partition,
                        leader: (a.leader_id == node_id).then(|| replicas[0]),
                        replicas: Some(replicas),
                        isr: None,
                        expected_epoch: Some(a.leader_epoch),
                    }
                })
                .collect()
        };
        for chunk in changes.chunks(500) {
            match self.execute(CatalogCommand::SetAssignments { changes: chunk.to_vec() }).await.map_err(|e| e.to_string())? {
                CatalogResult::Items(items) => {
                    if let Some((i, Err(e))) = items.iter().enumerate().find(|(_, r)| r.is_err()) {
                        return Err(format!("moving {}-{} off node {}: {}", chunk[i].topic, chunk[i].partition, node_id, e));
                    }
                }
                other => return Err(format!("unexpected result {:?}", other)),
            }
        }
        Ok(())
    }

    async fn expect_ok(&self, cmd: CatalogCommand) -> Result<(), String> {
        match self.execute(cmd).await.map_err(|e| e.to_string())? {
            CatalogResult::Err(e) => Err(e.to_string()),
            _ => Ok(()),
        }
    }

    /// Stop the raft node.
    pub fn stop(self) {
        self.driver.stop();
    }
}

fn split_host_port(addr: &str) -> Result<(String, i32), String> {
    let (host, port) = addr.rsplit_once(':').ok_or_else(|| format!("{} is not host:port", addr))?;
    let port = port.parse::<i32>().map_err(|_| format!("{} has no valid port", addr))?;
    Ok((host.to_string(), port))
}
