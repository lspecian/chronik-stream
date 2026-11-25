//! Event-driven Raft driver.
//!
//! One OS thread owns the `RawNode` and its [`RaftLogStore`]. It wakes for a
//! proposal, an incoming message or the next tick deadline, never on a fixed
//! polling interval, so a commit costs a network round trip and an fsync
//! instead of two or three tick boundaries. Running on its own thread keeps it
//! clear of request handling on the async runtimes (issue #58).
//!
//! A proposal returns the state machine's **result**, produced when the entry
//! is applied on the proposing node. Callers can therefore tell a command that
//! was rejected by validation from one that succeeded, and know that the
//! change is visible locally when the future resolves.
//!
//! Proposals are tracked by an id carried in the entry's `context`
//! (`node_id`, `sequence`). If this node stops being leader before an accepted
//! proposal is applied, the proposal fails with [`ProposeError::OutcomeUnknown`]:
//! the entry may still commit under the next leader. Commands are validated at
//! apply time, so retrying one is safe.

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use prost::Message as ProstMessage;
use raft::eraftpb::{ConfChange, ConfChangeV2, ConfState, Entry, EntryType, Message, Snapshot};
use raft::{Config, RawNode, StateRole, Storage};
use tokio::sync::{oneshot, watch};

use crate::log_store::{LogStoreOptions, RaftLogStore};

/// The replicated state machine. Every method runs on the driver thread.
pub trait StateMachine: Send + 'static {
    /// Apply a committed command. Must be deterministic: every node applies
    /// the same commands in the same order and must reach the same state.
    /// The returned bytes are delivered to the proposer.
    fn apply(&mut self, index: u64, term: u64, command: &[u8]) -> Vec<u8>;

    /// Serialize the applied state.
    fn snapshot(&self) -> Vec<u8>;

    /// Replace the state with a snapshot taken at `index`.
    fn restore(&mut self, index: u64, data: &[u8]) -> Result<(), String>;

    /// Called after a membership change is applied.
    fn on_conf_change(&mut self, _index: u64, _conf_state: &ConfState) {}
}

/// Delivers outgoing Raft messages. Must not block the driver thread.
pub trait Outbox: Send + 'static {
    fn send(&mut self, messages: Vec<Message>);
}

#[derive(Debug, Clone)]
pub struct DriverConfig {
    pub id: u64,
    pub tick_interval: Duration,
    pub election_tick: usize,
    pub heartbeat_tick: usize,
    /// Take a snapshot and compact the log after this many applied entries.
    pub snapshot_every: u64,
    pub log_store: LogStoreOptions,
    /// State a brand-new founding node starts from instead of the empty one
    /// (restore from a disaster-recovery image). Used only when the log is
    /// blank and `initial_voters` is non-empty; every founding node must
    /// pass the same one, or their logs disagree from the first index.
    pub bootstrap: Option<BootstrapSnapshot>,
}

/// A snapshot to found a cluster on: the state machine's serialized state as
/// of `index` (applied) in `term`.
#[derive(Debug, Clone, PartialEq)]
pub struct BootstrapSnapshot {
    pub index: u64,
    pub term: u64,
    pub data: Vec<u8>,
}

impl DriverConfig {
    pub fn new(id: u64) -> Self {
        Self {
            id,
            tick_interval: Duration::from_millis(100),
            election_tick: 20,
            heartbeat_tick: 3,
            snapshot_every: 10_000,
            log_store: LogStoreOptions::default(),
            bootstrap: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProposeError {
    /// This node is not the leader. `leader` is the leader it knows of.
    NotLeader { leader: Option<u64> },
    /// Raft refused the proposal (e.g. a leadership transfer is in progress).
    Dropped,
    /// Accepted, but this node lost leadership before it was applied here.
    /// The command may or may not take effect.
    OutcomeUnknown,
    /// The driver has stopped.
    Stopped,
}

impl std::fmt::Display for ProposeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProposeError::NotLeader { leader: Some(l) } => write!(f, "not the raft leader (leader is {})", l),
            ProposeError::NotLeader { leader: None } => write!(f, "not the raft leader (no leader known)"),
            ProposeError::Dropped => write!(f, "proposal dropped by raft"),
            ProposeError::OutcomeUnknown => write!(f, "leadership lost before the proposal was applied"),
            ProposeError::Stopped => write!(f, "raft driver stopped"),
        }
    }
}

impl std::error::Error for ProposeError {}

/// What a successful proposal returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    pub index: u64,
    pub term: u64,
    /// The state machine's result for a command; the encoded `ConfState` for
    /// a membership change.
    pub result: Vec<u8>,
}

/// A snapshot of the driver's view, published after every cycle.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RaftStatus {
    pub id: u64,
    pub term: u64,
    pub leader_id: Option<u64>,
    pub is_leader: bool,
    pub commit: u64,
    pub applied: u64,
    pub voters: Vec<u64>,
    pub learners: Vec<u64>,
    /// Set when the driver stopped on a storage failure.
    pub failed: Option<String>,
}

type Reply = oneshot::Sender<Result<Applied, ProposeError>>;

enum Event {
    Propose { data: Vec<u8>, reply: Reply },
    ConfChange { cc: ConfChangeV2, reply: Reply },
    Step(Message),
    Unreachable(u64),
    SnapshotFailed(u64),
    Campaign,
    SampleLive { within: Duration, reply: oneshot::Sender<Option<Vec<u64>>> },
    Stop,
}

/// Handle to a running driver. Cheap to clone.
#[derive(Clone)]
pub struct RaftHandle {
    tx: Sender<Event>,
    status: watch::Receiver<RaftStatus>,
    /// Milliseconds since `epoch` when this node last heard from (or was) the leader; 0 = never.
    leader_contact: Arc<AtomicU64>,
    epoch: Instant,
}

impl RaftHandle {
    /// Propose a command; resolves once it is applied on this node.
    pub async fn propose(&self, data: Vec<u8>) -> Result<Applied, ProposeError> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(Event::Propose { data, reply }).map_err(|_| ProposeError::Stopped)?;
        rx.await.unwrap_or(Err(ProposeError::Stopped))
    }

    /// Propose a membership change; resolves once it is applied on this node.
    pub async fn propose_conf_change(&self, cc: ConfChangeV2) -> Result<Applied, ProposeError> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(Event::ConfChange { cc, reply }).map_err(|_| ProposeError::Stopped)?;
        rx.await.unwrap_or(Err(ProposeError::Stopped))
    }

    /// Hand an incoming message to the driver. Never blocks.
    pub fn step(&self, msg: Message) {
        let _ = self.tx.send(Event::Step(msg));
    }

    /// Tell raft a peer could not be reached, so it stops pipelining to it.
    pub fn report_unreachable(&self, peer: u64) {
        let _ = self.tx.send(Event::Unreachable(peer));
    }

    /// Tell raft a snapshot could not be delivered, so it retries instead of
    /// waiting on the peer forever.
    pub fn report_snapshot_failed(&self, peer: u64) {
        let _ = self.tx.send(Event::SnapshotFailed(peer));
    }

    /// Start an election now instead of waiting for the election timeout.
    /// On the leader: this node plus every peer it heard from within
    /// `within`. `None` on a follower, whose view of liveness is not the
    /// cluster's. A sample, not a verdict: callers accumulate over a window.
    pub async fn sample_live(&self, within: Duration) -> Option<Vec<u64>> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(Event::SampleLive { within, reply }).ok()?;
        rx.await.ok().flatten()
    }

    pub fn campaign(&self) {
        let _ = self.tx.send(Event::Campaign);
    }

    /// How long ago this node last heard from the leader, or was leader
    /// itself. `None` if it never has since starting.
    pub fn leader_contact_age(&self) -> Option<Duration> {
        match self.leader_contact.load(Ordering::Relaxed) {
            0 => None,
            ms => Some(self.epoch.elapsed().saturating_sub(Duration::from_millis(ms))),
        }
    }

    pub fn status(&self) -> RaftStatus {
        self.status.borrow().clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<RaftStatus> {
        self.status.clone()
    }

    /// Resolve once this node has applied `index`. Fails if the driver stops.
    pub async fn wait_applied(&self, index: u64) -> Result<(), ProposeError> {
        let mut rx = self.status.clone();
        loop {
            {
                let s = rx.borrow_and_update();
                if s.applied >= index {
                    return Ok(());
                }
                if s.failed.is_some() {
                    return Err(ProposeError::Stopped);
                }
            }
            rx.changed().await.map_err(|_| ProposeError::Stopped)?;
        }
    }

    /// Resolve once some node is leader (this one or another).
    pub async fn wait_for_leader(&self) -> Result<u64, ProposeError> {
        let mut rx = self.status.clone();
        loop {
            if let Some(l) = rx.borrow_and_update().leader_id {
                return Ok(l);
            }
            rx.changed().await.map_err(|_| ProposeError::Stopped)?;
        }
    }
}

/// A running driver; dropping it stops the thread.
pub struct RaftDriver {
    handle: RaftHandle,
    thread: Option<JoinHandle<()>>,
}

impl RaftDriver {
    /// Open the log in `dir` and start driving it.
    ///
    /// `initial_voters` bootstraps a brand-new cluster: when the log is empty
    /// and holds no snapshot, it is initialized with a snapshot at index 1
    /// whose configuration is `initial_voters` and whose data is the empty
    /// state machine's snapshot. Every founding node must pass the same set.
    /// A node joining an existing cluster passes an empty set and starts
    /// blank; the leader brings it up to date with a snapshot.
    pub fn start<S: StateMachine, O: Outbox>(
        dir: impl Into<PathBuf>,
        config: DriverConfig,
        initial_voters: &[u64],
        mut state_machine: S,
        outbox: O,
    ) -> io::Result<Self> {
        let store = RaftLogStore::open(dir.into(), config.log_store.clone())?;
        let (snap_index, _) = store.snapshot_index_term();
        let blank = snap_index == 0 && store.last_index().unwrap_or(0) == 0;
        if blank && !initial_voters.is_empty() {
            let mut snap = Snapshot::default();
            let (index, term) = match &config.bootstrap {
                Some(b) => {
                    snap.data = b.data.clone().into();
                    (b.index.max(1), b.term.max(1))
                }
                None => {
                    snap.data = state_machine.snapshot().into();
                    (1, 1)
                }
            };
            let meta = snap.mut_metadata();
            meta.index = index;
            meta.term = term;
            meta.mut_conf_state().voters = initial_voters.to_vec();
            store
                .apply_snapshot(snap)?
                .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("bootstrap snapshot: {}", e)))?;
        }
        // Rebuild the state machine from the snapshot; raft re-delivers every
        // committed entry after it.
        let (snap_index, _) = store.snapshot_index_term();
        if let Some(data) = store.snapshot_data() {
            state_machine
                .restore(snap_index, &data)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("restoring snapshot: {}", e)))?;
        }

        let raft_config = Config {
            id: config.id,
            election_tick: config.election_tick,
            heartbeat_tick: config.heartbeat_tick,
            applied: snap_index,
            check_quorum: true,
            pre_vote: true,
            max_size_per_msg: 1024 * 1024,
            max_inflight_msgs: 256,
            ..Default::default()
        };
        raft_config.validate().map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
        let logger = slog::Logger::root(slog::Drain::fuse(slog_stdlog::StdLog), slog::o!("raft_id" => config.id));
        let node = RawNode::new(&raft_config, store.clone(), &logger)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("raft: {}", e)))?;

        let (tx, rx) = mpsc::channel();
        let (status_tx, status_rx) = watch::channel(RaftStatus { id: config.id, applied: snap_index, ..Default::default() });
        let leader_contact = Arc::new(AtomicU64::new(0));
        let epoch = Instant::now();
        let mut core = DriverCore {
            id: config.id,
            node,
            store,
            sm: state_machine,
            outbox,
            pending: HashMap::new(),
            next_seq: 1,
            applied: snap_index,
            snapshot_every: config.snapshot_every,
            status: status_tx,
            leader_contact: leader_contact.clone(),
            last_heard: HashMap::new(),
            fatal: None,
            epoch,
        };
        core.publish_status(None);
        let tick = config.tick_interval;
        let thread = std::thread::Builder::new()
            .name(format!("raft-{}", config.id))
            .spawn(move || core.run(rx, tick))?;
        Ok(Self { handle: RaftHandle { tx, status: status_rx, leader_contact, epoch }, thread: Some(thread) })
    }

    pub fn handle(&self) -> RaftHandle {
        self.handle.clone()
    }

    /// Stop the driver and wait for its thread.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        let _ = self.handle.tx.send(Event::Stop);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for RaftDriver {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct Pending {
    reply: Reply,
}

struct DriverCore<S: StateMachine, O: Outbox> {
    id: u64,
    node: RawNode<RaftLogStore>,
    store: RaftLogStore,
    sm: S,
    outbox: O,
    pending: HashMap<u64, Pending>,
    next_seq: u64,
    applied: u64,
    snapshot_every: u64,
    status: watch::Sender<RaftStatus>,
    leader_contact: Arc<AtomicU64>,
    /// When each peer was last heard from.
    last_heard: HashMap<u64, Instant>,
    /// Set when this node must stop: storage it cannot trust, or a log it lost.
    fatal: Option<String>,
    epoch: Instant,
}

/// Fails the driver: raft must not continue on storage it cannot trust.
struct Fatal(String);

impl From<io::Error> for Fatal {
    fn from(e: io::Error) -> Self {
        Fatal(format!("raft storage: {}", e))
    }
}

impl<S: StateMachine, O: Outbox> DriverCore<S, O> {
    fn run(&mut self, rx: Receiver<Event>, tick: Duration) {
        let mut next_tick = Instant::now() + tick;
        loop {
            let timeout = next_tick.saturating_duration_since(Instant::now());
            match rx.recv_timeout(timeout) {
                Ok(Event::Stop) | Err(RecvTimeoutError::Disconnected) => break,
                Ok(ev) => {
                    if self.handle_event(ev) {
                        break;
                    }
                    // Batch whatever else is already queued into this cycle.
                    let mut stop = false;
                    while let Ok(ev) = rx.try_recv() {
                        if matches!(ev, Event::Stop) || self.handle_event(ev) {
                            stop = true;
                            break;
                        }
                    }
                    if stop {
                        break;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
            let now = Instant::now();
            if now >= next_tick {
                self.node.tick();
                next_tick = now + tick;
            }
            if self.node.raft.state == StateRole::Leader {
                self.touch_leader_contact();
            }
            let result = match self.fatal.take() {
                Some(msg) => Err(Fatal(msg)),
                None => self.process_ready(),
            };
            if let Err(Fatal(msg)) = result {
                tracing::error!("Raft node {} stopping: {}", self.id, msg);
                self.fail_pending(ProposeError::Stopped);
                self.publish_status(Some(msg));
                return;
            }
        }
        self.fail_pending(ProposeError::Stopped);
    }

    /// Returns true to stop.
    fn handle_event(&mut self, ev: Event) -> bool {
        match ev {
            Event::Stop => return true,
            Event::Propose { data, reply } => {
                if let Some(reply) = self.check_leader(reply) {
                    let seq = self.next_seq();
                    match self.node.propose(self.context(seq), data) {
                        Ok(()) => {
                            self.pending.insert(seq, Pending { reply });
                        }
                        Err(_) => {
                            let _ = reply.send(Err(ProposeError::Dropped));
                        }
                    }
                }
            }
            Event::ConfChange { cc, reply } => {
                if let Some(reply) = self.check_leader(reply) {
                    let seq = self.next_seq();
                    match self.node.propose_conf_change(self.context(seq), cc) {
                        Ok(()) => {
                            self.pending.insert(seq, Pending { reply });
                        }
                        Err(_) => {
                            let _ = reply.send(Err(ProposeError::Dropped));
                        }
                    }
                }
            }
            Event::Step(msg) => {
                // A heartbeat committing beyond our log means this node lost
                // entries it had acknowledged (its data was wiped or restored
                // from an old copy). raft-rs would abort the process; stop the
                // catalog with the remedy instead.
                if msg.get_msg_type() == raft::eraftpb::MessageType::MsgHeartbeat
                    && msg.commit > self.node.raft.raft_log.last_index()
                {
                    self.fatal = Some(format!(
                        "leader {} has committed through {} but this node's catalog log ends at {}: \
                         this node lost raft state it had acknowledged (data wiped or restored). \
                         Remove it from the cluster (cluster remove-node {} --force), add it back \
                         (cluster add-node), and start it with CHRONIK_CLUSTER_JOIN=true.",
                        msg.from, msg.commit, self.node.raft.raft_log.last_index(), self.id
                    ));
                    return false; // the loop reports `fatal` and stops
                }
                let from = msg.from;
                if from != 0 {
                    self.last_heard.insert(from, Instant::now());
                }
                let term = msg.term;
                if let Err(e) = self.node.step(msg) {
                    tracing::debug!("Raft node {}: step: {}", self.id, e);
                }
                if from != 0 && from == self.node.raft.leader_id && term == self.node.raft.term {
                    self.touch_leader_contact();
                }
            }
            Event::Unreachable(peer) => self.node.report_unreachable(peer),
            Event::SnapshotFailed(peer) => self.node.report_snapshot(peer, raft::SnapshotStatus::Failure),
            Event::SampleLive { within, reply } => {
                let live = (self.node.raft.state == StateRole::Leader).then(|| {
                    let mut ids = vec![self.id];
                    let voters = self.node.raft.prs().conf().voters().ids();
                    for id in voters.iter() {
                        if id != self.id && self.last_heard.get(&id).map(|t| t.elapsed() <= within).unwrap_or(false) {
                            ids.push(id);
                        }
                    }
                    ids.sort_unstable();
                    ids
                });
                let _ = reply.send(live);
            }
            Event::Campaign => {
                let _ = self.node.campaign();
            }
        }
        false
    }

    fn check_leader(&self, reply: Reply) -> Option<Reply> {
        if self.node.raft.state == StateRole::Leader {
            Some(reply)
        } else {
            let leader = Some(self.node.raft.leader_id).filter(|&l| l != 0);
            let _ = reply.send(Err(ProposeError::NotLeader { leader }));
            None
        }
    }

    fn next_seq(&mut self) -> u64 {
        let s = self.next_seq;
        self.next_seq += 1;
        s
    }

    fn context(&self, seq: u64) -> Vec<u8> {
        let mut c = Vec::with_capacity(16);
        c.extend_from_slice(&self.id.to_le_bytes());
        c.extend_from_slice(&seq.to_le_bytes());
        c
    }

    /// The pending sequence an entry answers, if this node proposed it.
    fn own_seq(&self, entry: &Entry) -> Option<u64> {
        let c = &entry.context;
        if c.len() != 16 || u64::from_le_bytes(c[..8].try_into().unwrap()) != self.id {
            return None;
        }
        Some(u64::from_le_bytes(c[8..].try_into().unwrap()))
    }

    fn process_ready(&mut self) -> Result<(), Fatal> {
        if !self.node.has_ready() {
            return Ok(());
        }
        let mut ready = self.node.ready();

        // A leader may send before persisting (raft thesis §10.2.1).
        if !ready.messages().is_empty() {
            self.outbox.send(ready.take_messages());
        }

        if !raft::is_empty_snap(ready.snapshot()) {
            let snap = ready.snapshot().clone();
            let index = snap.get_metadata().index;
            match self.store.apply_snapshot(snap.clone())? {
                Ok(()) => {
                    self.sm
                        .restore(index, &snap.data)
                        .map_err(|e| Fatal(format!("restoring snapshot at {}: {}", index, e)))?;
                    self.applied = index;
                    self.sm.on_conf_change(index, snap.get_metadata().get_conf_state());
                    tracing::info!("Raft node {}: installed snapshot at index {}", self.id, index);
                }
                Err(e) => tracing::warn!("Raft node {}: snapshot at {} not installed: {}", self.id, index, e),
            }
        }

        let committed = ready.take_committed_entries();
        self.apply_entries(committed)?;

        self.store.append(ready.entries())?;
        if let Some(hs) = ready.hs() {
            self.store.set_hardstate(hs.clone())?;
        }
        if !ready.persisted_messages().is_empty() {
            self.outbox.send(ready.take_persisted_messages());
        }
        let lost_leadership = ready.ss().map(|ss| ss.raft_state != StateRole::Leader).unwrap_or(false);

        let mut light = self.node.advance(ready);
        if let Some(commit) = light.commit_index() {
            self.store.set_commit(commit)?;
        }
        if !light.messages().is_empty() {
            self.outbox.send(light.take_messages());
        }
        let committed = light.take_committed_entries();
        self.apply_entries(committed)?;
        self.node.advance_apply();

        if lost_leadership {
            self.fail_pending(ProposeError::OutcomeUnknown);
        }
        self.maybe_snapshot()?;
        self.publish_status(None);
        Ok(())
    }

    fn apply_entries(&mut self, entries: Vec<Entry>) -> Result<(), Fatal> {
        for entry in entries {
            if entry.index <= self.applied {
                continue;
            }
            let result = match entry.get_entry_type() {
                EntryType::EntryNormal => {
                    // An empty normal entry is the new leader's no-op.
                    if entry.data.is_empty() {
                        None
                    } else {
                        Some(self.sm.apply(entry.index, entry.term, &entry.data))
                    }
                }
                EntryType::EntryConfChange | EntryType::EntryConfChangeV2 => Some(self.apply_conf_change(&entry)?),
            };
            self.applied = entry.index;
            if let (Some(seq), Some(result)) = (self.own_seq(&entry), result) {
                if let Some(p) = self.pending.remove(&seq) {
                    let _ = p.reply.send(Ok(Applied { index: entry.index, term: entry.term, result }));
                }
            }
        }
        Ok(())
    }

    fn apply_conf_change(&mut self, entry: &Entry) -> Result<Vec<u8>, Fatal> {
        let applied = if entry.get_entry_type() == EntryType::EntryConfChangeV2 {
            let cc = ConfChangeV2::decode(entry.data.as_ref())
                .map_err(|e| Fatal(format!("undecodable conf change at {}: {}", entry.index, e)))?;
            self.node.apply_conf_change(&cc)
        } else {
            let cc = ConfChange::decode(entry.data.as_ref())
                .map_err(|e| Fatal(format!("undecodable conf change at {}: {}", entry.index, e)))?;
            self.node.apply_conf_change(&cc)
        };
        match applied {
            Ok(cs) => {
                self.store.set_conf_state(cs.clone())?;
                self.sm.on_conf_change(entry.index, &cs);
                tracing::info!(
                    "Raft node {}: membership at {} is voters {:?} learners {:?}",
                    self.id,
                    entry.index,
                    cs.voters,
                    cs.learners
                );
                Ok(cs.encode_to_vec())
            }
            // Raft rejects an invalid change deterministically on every node;
            // the proposer learns it from an empty result.
            Err(e) => {
                tracing::warn!("Raft node {}: conf change at {} rejected: {}", self.id, entry.index, e);
                Ok(Vec::new())
            }
        }
    }

    fn maybe_snapshot(&mut self) -> Result<(), Fatal> {
        let (snap_index, _) = self.store.snapshot_index_term();
        let due = self.applied >= snap_index + self.snapshot_every;
        let asked = self.store.snapshot_requested() && self.applied > snap_index;
        if !(due || asked) || self.applied <= snap_index {
            return Ok(());
        }
        let cs = self.node.raft.prs().conf().to_conf_state();
        self.store.create_snapshot(self.applied, cs, self.sm.snapshot())?;
        tracing::debug!("Raft node {}: snapshot at {}, log compacted", self.id, self.applied);
        Ok(())
    }

    fn touch_leader_contact(&self) {
        let ms = self.epoch.elapsed().as_millis() as u64;
        self.leader_contact.store(ms.max(1), Ordering::Relaxed);
    }

    fn fail_pending(&mut self, err: ProposeError) {
        for (_, p) in self.pending.drain() {
            let _ = p.reply.send(Err(err.clone()));
        }
    }

    fn publish_status(&self, failed: Option<String>) {
        let cs = self.node.raft.prs().conf().to_conf_state();
        let status = RaftStatus {
            id: self.id,
            term: self.node.raft.term,
            leader_id: Some(self.node.raft.leader_id).filter(|&l| l != 0),
            is_leader: self.node.raft.state == StateRole::Leader,
            commit: self.node.raft.raft_log.committed,
            applied: self.applied,
            voters: cs.voters,
            learners: cs.learners,
            failed,
        };
        self.status.send_if_modified(|cur| {
            if *cur != status {
                *cur = status;
                true
            } else {
                false
            }
        });
    }
}

/// An `Outbox` backed by a channel, for transports that send from an async task.
pub struct ChannelOutbox(pub tokio::sync::mpsc::UnboundedSender<Vec<Message>>);

impl Outbox for ChannelOutbox {
    fn send(&mut self, messages: Vec<Message>) {
        let _ = self.0.send(messages);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeSet, HashSet};
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;

    /// `create:<name>` succeeds once per name; the state is shared so tests can read it.
    #[derive(Default, Clone)]
    struct Names(Arc<Mutex<(BTreeSet<String>, Vec<String>)>>);

    impl Names {
        fn set(&self) -> BTreeSet<String> {
            self.0.lock().unwrap().0.clone()
        }
        fn applied_log(&self) -> Vec<String> {
            self.0.lock().unwrap().1.clone()
        }
    }

    impl StateMachine for Names {
        fn apply(&mut self, _index: u64, _term: u64, command: &[u8]) -> Vec<u8> {
            let cmd = String::from_utf8(command.to_vec()).unwrap();
            let mut st = self.0.lock().unwrap();
            st.1.push(cmd.clone());
            let name = cmd.strip_prefix("create:").expect("create command").to_string();
            if st.0.insert(name) { b"created".to_vec() } else { b"exists".to_vec() }
        }
        fn snapshot(&self) -> Vec<u8> {
            serde_json::to_vec(&self.0.lock().unwrap().0).unwrap()
        }
        fn restore(&mut self, _index: u64, data: &[u8]) -> Result<(), String> {
            let set: BTreeSet<String> = serde_json::from_slice(data).map_err(|e| e.to_string())?;
            let mut st = self.0.lock().unwrap();
            st.0 = set;
            st.1.clear();
            Ok(())
        }
    }

    #[derive(Default)]
    struct Net {
        handles: HashMap<u64, RaftHandle>,
        cut: HashSet<(u64, u64)>,
    }

    #[derive(Clone, Default)]
    struct Router(Arc<Mutex<Net>>);

    impl Router {
        fn outbox(&self, from: u64) -> RouterOutbox {
            RouterOutbox { router: self.clone(), from }
        }
        fn register(&self, id: u64, h: RaftHandle) {
            self.0.lock().unwrap().handles.insert(id, h);
        }
        fn unregister(&self, id: u64) {
            self.0.lock().unwrap().handles.remove(&id);
        }
        fn isolate(&self, id: u64, peers: &[u64]) {
            let mut n = self.0.lock().unwrap();
            for &p in peers {
                n.cut.insert((id, p));
                n.cut.insert((p, id));
            }
        }
        fn heal(&self) {
            self.0.lock().unwrap().cut.clear();
        }
    }

    struct RouterOutbox {
        router: Router,
        from: u64,
    }

    impl Outbox for RouterOutbox {
        fn send(&mut self, messages: Vec<Message>) {
            let n = self.router.0.lock().unwrap();
            for m in messages {
                if n.cut.contains(&(self.from, m.to)) {
                    continue;
                }
                if let Some(h) = n.handles.get(&m.to) {
                    h.step(m);
                }
            }
        }
    }

    fn config(id: u64) -> DriverConfig {
        DriverConfig {
            id,
            tick_interval: Duration::from_millis(10),
            election_tick: 10,
            heartbeat_tick: 2,
            snapshot_every: 10_000,
            log_store: LogStoreOptions::default(),
            bootstrap: None,
        }
    }

    struct Cluster {
        router: Router,
        dirs: HashMap<u64, TempDir>,
        drivers: HashMap<u64, RaftDriver>,
        sms: HashMap<u64, Names>,
        ids: Vec<u64>,
        cfg: fn(u64) -> DriverConfig,
    }

    impl Cluster {
        fn new(ids: &[u64], cfg: fn(u64) -> DriverConfig) -> Self {
            let mut c = Cluster {
                router: Router::default(),
                dirs: ids.iter().map(|&i| (i, TempDir::new().unwrap())).collect(),
                drivers: HashMap::new(),
                sms: HashMap::new(),
                ids: ids.to_vec(),
                cfg,
            };
            for &id in ids {
                c.start(id);
            }
            c
        }

        fn start(&mut self, id: u64) {
            let sm = Names::default();
            let d = RaftDriver::start(
                self.dirs[&id].path(),
                (self.cfg)(id),
                &self.ids,
                sm.clone(),
                self.router.outbox(id),
            )
            .unwrap();
            self.router.register(id, d.handle());
            self.sms.insert(id, sm);
            self.drivers.insert(id, d);
        }

        fn stop(&mut self, id: u64) {
            self.router.unregister(id);
            if let Some(d) = self.drivers.remove(&id) {
                d.stop();
            }
        }

        fn h(&self, id: u64) -> RaftHandle {
            self.drivers[&id].handle()
        }

        async fn leader(&self) -> u64 {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let leaders: Vec<u64> =
                    self.drivers.iter().filter(|(_, d)| d.handle().status().is_leader).map(|(&i, _)| i).collect();
                if leaders.len() == 1 {
                    return leaders[0];
                }
                assert!(Instant::now() < deadline, "no single leader elected");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }

        /// Propose `create:<name>` to the current leader, again to whoever
        /// leads next if leadership moves before it is applied. A leader that
        /// was just elected can be deposed by a slow follower's election
        /// (seen on the CI runner: OutcomeUnknown). Creating a name twice is
        /// harmless in this state machine, so a retry is what a client does.
        async fn create_on_leader(&self, name: &str) -> Applied {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                match create(&self.h(self.leader().await), name).await {
                    Ok(applied) => return applied,
                    Err(e) => assert!(Instant::now() < deadline, "create {name} never applied: {e}"),
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }

        /// Wait until every running node has applied `index`.
        async fn settle(&self, index: u64) {
            for d in self.drivers.values() {
                tokio::time::timeout(Duration::from_secs(10), d.handle().wait_applied(index))
                    .await
                    .expect("node did not catch up")
                    .unwrap();
            }
        }
    }

    async fn create(h: &RaftHandle, name: &str) -> Result<Applied, ProposeError> {
        h.propose(format!("create:{}", name).into_bytes()).await
    }

    /// Founding nodes given the same bootstrap snapshot start from its state at
    /// its index (disaster recovery), elect a leader, and commit after it; the
    /// snapshot survives a restart.
    #[tokio::test]
    async fn founding_nodes_start_from_a_bootstrap_snapshot() {
        fn seeded(id: u64) -> DriverConfig {
            let data = serde_json::to_vec(&BTreeSet::from(["restored".to_string()])).unwrap();
            DriverConfig { bootstrap: Some(BootstrapSnapshot { index: 50, term: 3, data }), ..config(id) }
        }
        let mut c = Cluster::new(&[1, 2, 3], seeded);
        for id in [1, 2, 3] {
            assert!(c.sms[&id].set().contains("restored"), "node {id} did not restore");
        }
        c.leader().await;
        let applied = c.create_on_leader("after").await;
        assert!(applied.index > 50, "committed at {} inside the restored range", applied.index);
        assert!(applied.term > 3);
        c.settle(applied.index).await;
        let want = BTreeSet::from(["after".to_string(), "restored".to_string()]);
        for id in [1, 2, 3] {
            assert_eq!(c.sms[&id].set(), want);
        }
        c.stop(3);
        c.start(3);
        c.settle(applied.index).await;
        assert_eq!(c.sms[&3].set(), want, "restart lost the restored state");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn single_node_returns_results_and_survives_restart() {
        let mut c = Cluster::new(&[1], config);
        c.leader().await;
        let a = create(&c.h(1), "a").await.unwrap();
        assert_eq!(a.result, b"created");
        assert_eq!(create(&c.h(1), "a").await.unwrap().result, b"exists");
        c.stop(1);
        c.start(1);
        c.leader().await;
        // The restarted node replays its log before serving.
        assert_eq!(create(&c.h(1), "a").await.unwrap().result, b"exists");
        assert_eq!(c.sms[&1].set(), BTreeSet::from(["a".to_string()]));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn followers_refuse_and_name_the_leader() {
        let c = Cluster::new(&[1, 2, 3], config);
        let leader = c.leader().await;
        let follower = *c.ids.iter().find(|&&i| i != leader).unwrap();
        // The follower learns who leads from the first heartbeat.
        c.h(follower).wait_for_leader().await.unwrap();
        assert_eq!(
            create(&c.h(follower), "x").await,
            Err(ProposeError::NotLeader { leader: Some(leader) })
        );
        let r = c.create_on_leader("x").await;
        c.settle(r.index).await;
        for id in &c.ids {
            assert_eq!(c.sms[id].set(), BTreeSet::from(["x".to_string()]), "node {}", id);
        }
    }

    /// Event-driven: a commit must not wait for tick boundaries. With 100 ms
    /// ticks, the old loop needed 200–300 ms per commit.
    #[tokio::test(flavor = "multi_thread")]
    async fn commits_do_not_wait_for_ticks() {
        fn slow_ticks(id: u64) -> DriverConfig {
            DriverConfig { tick_interval: Duration::from_millis(100), election_tick: 10, ..config(id) }
        }
        let c = Cluster::new(&[1, 2, 3], slow_ticks);
        let leader = c.leader().await;
        let h = c.h(leader);
        let start = Instant::now();
        for i in 0..100 {
            create(&h, &format!("n{}", i)).await.unwrap();
        }
        let per = start.elapsed() / 100;
        assert!(per < Duration::from_millis(60), "{:?} per sequential commit", per);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_creates_succeed_once_per_name() {
        let c = Cluster::new(&[1, 2, 3], config);
        let h = c.h(c.leader().await);
        let mut tasks = Vec::new();
        for i in 0..200 {
            let h = h.clone();
            tasks.push(tokio::spawn(async move { (i % 20, create(&h, &format!("t{}", i % 20)).await.unwrap()) }));
        }
        let mut created: HashMap<u64, usize> = HashMap::new();
        let mut max_index = 0;
        for t in tasks {
            let (name, r) = t.await.unwrap();
            max_index = max_index.max(r.index);
            if r.result == b"created" {
                *created.entry(name).or_default() += 1;
            }
        }
        assert_eq!(created.len(), 20);
        assert!(created.values().all(|&n| n == 1), "{:?}", created);
        c.settle(max_index).await;
        let want = c.sms[&1].set();
        for id in &c.ids {
            assert_eq!(c.sms[id].set(), want);
            assert_eq!(c.sms[id].applied_log(), c.sms[&1].applied_log(), "same order on node {}", id);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn leader_failover_and_rejoin() {
        let mut c = Cluster::new(&[1, 2, 3], config);
        c.create_on_leader("before").await;
        let old = c.leader().await;
        c.stop(old);
        let new = c.leader().await;
        assert_ne!(new, old);
        let r = c.create_on_leader("after").await;
        c.start(old);
        c.settle(r.index).await;
        let want = BTreeSet::from(["before".to_string(), "after".to_string()]);
        for id in &c.ids {
            assert_eq!(c.sms[id].set(), want, "node {}", id);
        }
    }

    /// A leader cut off from the majority steps down (check-quorum); its
    /// in-flight proposal fails as OutcomeUnknown and is never applied
    /// anywhere, because the majority elected a leader without it.
    #[tokio::test(flavor = "multi_thread")]
    async fn isolated_leader_steps_down_and_its_proposal_is_lost() {
        let c = Cluster::new(&[1, 2, 3], config);
        let old = c.leader().await;
        let others: Vec<u64> = c.ids.iter().copied().filter(|&i| i != old).collect();
        c.router.isolate(old, &others);

        let stranded = tokio::time::timeout(Duration::from_secs(10), create(&c.h(old), "stranded")).await.unwrap();
        assert_eq!(stranded, Err(ProposeError::OutcomeUnknown));

        // The majority elects one of its own and keeps going. Its first
        // leader can be deposed before applying (two nodes racing to elect),
        // so propose to whichever of them leads until one applies it.
        let deadline = Instant::now() + Duration::from_secs(10);
        let r = loop {
            if let Some(l) = others.iter().copied().find(|&i| c.h(i).status().is_leader) {
                match create(&c.h(l), "majority").await {
                    Ok(r) => break r,
                    Err(e) => assert!(Instant::now() < deadline, "majority never applied: {e}"),
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        c.router.heal();
        c.settle(r.index).await;
        for id in &c.ids {
            assert_eq!(c.sms[id].set(), BTreeSet::from(["majority".to_string()]), "node {}", id);
        }
    }

    /// A node down long enough for the leader to compact past it catches up
    /// by snapshot, then by log.
    #[tokio::test(flavor = "multi_thread")]
    async fn lagging_node_catches_up_by_snapshot() {
        fn snappy(id: u64) -> DriverConfig {
            DriverConfig { snapshot_every: 20, ..config(id) }
        }
        let mut c = Cluster::new(&[1, 2, 3], snappy);
        let leader = c.leader().await;
        let down = *c.ids.iter().find(|&&i| i != leader).unwrap();
        c.stop(down);
        let mut last = 0;
        for i in 0..100 {
            last = c.create_on_leader(&format!("k{}", i)).await.index;
        }
        c.start(down);
        c.settle(last).await;
        assert_eq!(c.sms[&down].set().len(), 100);
        assert_eq!(c.sms[&down].set(), c.sms[&leader].set());
        // It never replayed the compacted commands one by one.
        assert!(c.sms[&down].applied_log().len() < 100, "{} replayed", c.sms[&down].applied_log().len());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_whole_cluster_restart_keeps_everything() {
        let mut c = Cluster::new(&[1, 2, 3], config);
        for i in 0..30 {
            c.create_on_leader(&format!("r{}", i)).await;
        }
        for id in c.ids.clone() {
            c.stop(id);
        }
        for id in c.ids.clone() {
            c.start(id);
        }
        let r = c.create_on_leader("r0").await;
        assert_eq!(r.result, b"exists");
        c.settle(r.index).await;
        for id in &c.ids {
            assert_eq!(c.sms[id].set().len(), 30, "node {}", id);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn leader_samples_live_peers_and_followers_do_not() {
        let mut c = Cluster::new(&[1, 2, 3], config);
        let leader = c.leader().await;
        let window = Duration::from_millis(300);
        tokio::time::sleep(window).await;
        assert_eq!(c.h(leader).sample_live(window).await, Some(vec![1, 2, 3]));
        let follower = *c.ids.iter().find(|&&i| i != leader).unwrap();
        assert_eq!(c.h(follower).sample_live(window).await, None);
        c.stop(follower);
        tokio::time::sleep(window * 2).await;
        let mut want: Vec<u64> = c.ids.iter().copied().filter(|&i| i != follower).collect();
        want.sort();
        assert_eq!(c.h(leader).sample_live(window).await, Some(want));
    }

    /// A voter whose log was wiped must stop with the remedy, not abort the
    /// process (raft-rs panics when a heartbeat commits beyond its log).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_wiped_voter_stops_with_the_remedy_instead_of_panicking() {
        let mut c = Cluster::new(&[1, 2, 3], config);
        let leader = c.leader().await;
        let mut last = 0;
        for i in 0..20 {
            last = c.create_on_leader(&format!("w{}", i)).await.index;
        }
        c.settle(last).await;
        let victim = *c.ids.iter().find(|&&i| i != leader).unwrap();
        c.stop(victim);
        c.dirs.insert(victim, TempDir::new().unwrap());
        c.start(victim);
        let mut status = c.h(victim).subscribe();
        let failed = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(f) = status.borrow_and_update().failed.clone() {
                    return f;
                }
                status.changed().await.unwrap();
            }
        })
        .await
        .expect("the wiped node never reported a failure");
        assert!(failed.contains("lost raft state"), "{}", failed);
        assert!(failed.contains("CHRONIK_CLUSTER_JOIN"), "{}", failed);
        // The rest of the cluster keeps working.
        c.create_on_leader("after-wipe").await;
    }
}
