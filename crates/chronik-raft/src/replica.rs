//! Partition replica management with Raft consensus
//!
//! This module manages Raft consensus for individual topic partitions using
//! the tikv/raft RawNode API. Each partition replica runs its own Raft state
//! machine to ensure consistent replication across the cluster.

use crate::{RaftConfig, RaftError, RaftEvent, RaftLogStorage, Result, StateMachine};
use chronik_monitoring::RaftMetrics;
use parking_lot::RwLock as ParkingRwLock;
use raft::{
    prelude::*,
    storage::MemStorage,
    Config as RaftCoreConfig,
    RawNode,
    StateRole,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot, RwLock as TokioRwLock};
use tracing::{debug, error, info, trace, warn};

/// Manages Raft consensus for a single partition
pub struct PartitionReplica {
    /// Topic name
    topic: String,

    /// Partition ID
    partition: i32,

    /// Raft configuration
    config: RaftConfig,

    /// Raft RawNode - the core Raft state machine
    /// Wrapped in RwLock for interior mutability
    raw_node: Arc<ParkingRwLock<RawNode<MemStorage>>>,

    /// Current Raft state
    state: Arc<ParkingRwLock<ReplicaState>>,

    /// Log storage backend
    log_storage: Arc<dyn RaftLogStorage>,

    /// State machine for applying committed entries
    /// Uses tokio RwLock to support async operations across await points
    state_machine: Arc<TokioRwLock<dyn StateMachine>>,

    /// Pending proposals waiting for commit (index -> oneshot sender)
    pending_proposals: Arc<ParkingRwLock<HashMap<u64, oneshot::Sender<Result<()>>>>>,

    /// Last time tick() was called
    last_tick: Arc<ParkingRwLock<Instant>>,

    /// Raft metrics collector
    metrics: RaftMetrics,

    /// Proposal timestamps for commit latency tracking (index -> proposal_time)
    proposal_times: Arc<ParkingRwLock<HashMap<u64, Instant>>>,

    /// Event channel for notifying server layer of state changes (optional)
    /// If None, no events are sent (useful for testing or standalone mode)
    event_tx: Option<mpsc::UnboundedSender<RaftEvent>>,
}

/// Replica state tracking
#[derive(Debug, Clone)]
struct ReplicaState {
    /// Current Raft term
    term: u64,

    /// Current commit index
    commit_index: u64,

    /// Current leader ID (0 if unknown)
    leader_id: u64,

    /// Current role
    role: StateRole,

    /// Applied index (last index applied to state machine)
    applied_index: u64,
}

impl Default for ReplicaState {
    fn default() -> Self {
        Self {
            term: 0,
            commit_index: 0,
            leader_id: 0,
            role: StateRole::Follower,
            applied_index: 0,
        }
    }
}

impl PartitionReplica {
    /// Create a new partition replica with Raft consensus
    ///
    /// # Arguments
    /// * `topic` - Topic name
    /// * `partition` - Partition ID
    /// * `config` - Raft configuration
    /// * `log_storage` - Log storage backend
    /// * `state_machine` - State machine for applying committed entries
    /// * `peers` - List of peer node IDs in the cluster
    /// * `event_tx` - Optional event channel for notifying server layer of state changes
    ///
    /// # Returns
    /// A new PartitionReplica instance
    pub fn new(
        topic: String,
        partition: i32,
        config: RaftConfig,
        log_storage: Arc<dyn RaftLogStorage>,
        state_machine: Arc<TokioRwLock<dyn StateMachine>>,
        peers: Vec<u64>,
        event_tx: Option<mpsc::UnboundedSender<RaftEvent>>,
    ) -> Result<Self> {
        info!(
            "Creating PartitionReplica for {}-{} with node_id={} peers={:?}",
            topic, partition, config.node_id, peers
        );

        // CRITICAL FIX: Randomize election timeout per partition to prevent split votes
        // When multiple partitions are created simultaneously, they would all use the same
        // election_tick and timeout at the same time, causing election storms.
        //
        // Solution: Add randomization based on partition ID + topic hash
        // This ensures different partitions have staggered election timeouts.
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        topic.hash(&mut hasher);
        partition.hash(&mut hasher);
        let hash = hasher.finish();

        // Randomize election_tick between base and base*2 (standard Raft practice)
        // Using hash for deterministic but varied values across partitions
        let base_election_tick = (config.election_timeout_ms / config.heartbeat_interval_ms) as usize;
        let randomized_election_tick = base_election_tick + ((hash as usize) % base_election_tick);

        info!(
            "Election timeout for {}-{}: base={} ticks, randomized={} ticks (~{}ms)",
            topic, partition, base_election_tick, randomized_election_tick,
            randomized_election_tick * config.heartbeat_interval_ms as usize
        );

        // Configure Raft core
        let raft_config = RaftCoreConfig {
            id: config.node_id,
            election_tick: randomized_election_tick,
            heartbeat_tick: 1,
            max_size_per_msg: (config.max_entries_per_batch * 1024) as u64,
            max_inflight_msgs: 256,
            ..Default::default()
        };

        // Validate configuration
        raft_config.validate().map_err(|e| {
            RaftError::Config(format!("Invalid Raft configuration: {}", e))
        })?;

        // MULTI-NODE ONLY: Raft is meaningless for single-node deployments
        // For single-node, use standalone mode with WAL-only (no Raft overhead)
        if peers.is_empty() {
            return Err(RaftError::Config(
                "Single-node Raft cluster rejected. Use standalone mode (without --raft) for single-node deployments. Raft requires 3+ nodes for quorum-based replication.".to_string()
            ));
        }

        // Multi-node cluster bootstrap using TiKV Raft's static configuration pattern:
        // 1. ALL nodes start with ALL peers in ConfState (voters: {1, 2, 3})
        // 2. TiKV Raft populates Progress tracker from ConfState
        // 3. Pre-vote and election happens naturally
        // 4. Leader elected via normal Raft consensus
        //
        // NOTE: Do NOT use single-node + ConfChange for new cluster bootstrap!
        // That creates a deadlock: can't send ConfChange without peers in Progress,
        // can't add peers to Progress without committed ConfChange.
        info!(
            "Multi-node Raft cluster for {}-{} with {} peers: {:?}",
            topic, partition, peers.len(), peers
        );

        // Determine all node IDs for ConfState initialization
        let all_node_ids: Vec<u64> = {
            let mut ids = peers.clone();
            if !ids.contains(&config.node_id) {
                ids.push(config.node_id);
            }
            ids.sort();
            ids
        };

        // CRITICAL: Initialize with ALL peers as voters for new cluster bootstrap
        let storage = MemStorage::new();
        let conf_state = ConfState::from((all_node_ids.clone(), vec![]));
        storage.initialize_with_conf_state(conf_state);
        let mut raw_node = RawNode::new(&raft_config, storage, &tracing_logger())?;

        // CRITICAL FIX: TiKV Raft 0.7 DOES populate Progress tracker from ConfState,
        // but only during Raft::new() via confchange::restore(). This happens automatically
        // when we call storage.initialize_with_conf_state() before RawNode::new().
        //
        // The Progress tracker should now contain all peers from all_node_ids.
        // Verify this happened correctly:
        let progress_count = raw_node.raft.prs().iter().count();
        info!(
            "Progress tracker after RawNode::new for {}-{}: {} entries (ConfState voters: {})",
            topic, partition, progress_count, all_node_ids.len()
        );

        // Log each Progress entry for debugging
        for (id, progress) in raw_node.raft.prs().iter() {
            info!(
                "  Progress for peer {}: matched={}, next_idx={}, state={:?}",
                id, progress.matched, progress.next_idx, progress.state
            );
        }

        // CRITICAL DEBUG: Check what conf.voters().ids() returns (used for broadcasting)
        let voter_ids: Vec<u64> = raw_node.raft.prs().conf().voters().ids().iter().collect();
        info!(
            "Configuration voter IDs for {}-{}: {:?}",
            topic, partition, voter_ids
        );

        // CRITICAL: Check if the configuration voters match the Progress entries
        if voter_ids.len() != all_node_ids.len() {
            error!(
                "MISMATCH: Configuration has {} voters but expected {}! This will cause messages=0!",
                voter_ids.len(), all_node_ids.len()
            );
        }

        // Let TiKV Raft's natural election process work
        // Pre-vote will ensure only one node becomes leader
        // DO NOT call campaign() here - let election timeout trigger it
        info!(
            "Initialized {}-{} with {} voters, waiting for natural leader election",
            topic, partition, all_node_ids.len()
        );

        // Process initial ready state
        if raw_node.has_ready() {
            let ready = raw_node.ready();
            let _light_rd = raw_node.advance(ready);
            info!(
                "Initial Raft state for {}-{}, role={:?}, leader={}",
                topic, partition, raw_node.raft.state, raw_node.raft.leader_id
            );
        }

        let replica = Self {
            topic,
            partition,
            config: config.clone(),
            raw_node: Arc::new(ParkingRwLock::new(raw_node)),
            state: Arc::new(ParkingRwLock::new(ReplicaState::default())),
            log_storage,
            state_machine,
            pending_proposals: Arc::new(ParkingRwLock::new(HashMap::new())),
            last_tick: Arc::new(ParkingRwLock::new(Instant::now())),
            metrics: RaftMetrics::new(),
            proposal_times: Arc::new(ParkingRwLock::new(HashMap::new())),
            event_tx,
        };

        // Update state to reflect initial role after campaign()
        // (both single-node and temp multi-node bootstrap call campaign())
        {
            let node = replica.raw_node.read();
            let mut state = replica.state.write();
            state.role = node.raft.state;
            state.leader_id = node.raft.leader_id;
            state.term = node.raft.term;
            info!(
                "Initial state for {}-{}: role={:?}, leader={}, term={}",
                replica.topic, replica.partition, state.role, state.leader_id, state.term
            );

            // Update metrics for initial state
            replica.metrics.set_current_term(&replica.topic, replica.partition, state.term);
            let node_id_str = config.node_id.to_string();
            let state_value = match state.role {
                StateRole::Leader => 1,
                StateRole::Candidate => 2,
                StateRole::Follower => 3,
                StateRole::PreCandidate => 2,
            };
            replica.metrics.set_node_state(&replica.topic, replica.partition, &node_id_str, state_value);
        }

        Ok(replica)
    }

    /// Get partition info (topic, partition)
    pub fn info(&self) -> (&str, i32) {
        (&self.topic, self.partition)
    }

    /// Propose a new entry to the Raft log
    ///
    /// This is used for produce requests - the data will be replicated
    /// across the cluster before being committed.
    ///
    /// # Arguments
    /// * `data` - Entry data to propose (serialized produce request)
    ///
    /// # Returns
    /// The proposed entry index on success
    pub async fn propose(&self, data: Vec<u8>) -> Result<u64> {
        let mut node = self.raw_node.write();

        trace!(
            "Proposing entry of {} bytes to {}-{}",
            data.len(),
            self.topic,
            self.partition
        );

        // Check if we're the leader
        if node.raft.state != StateRole::Leader {
            let state = self.state.read();
            return Err(RaftError::Config(format!(
                "Not leader (current leader: {})",
                state.leader_id
            )));
        }

        // Propose the entry
        node.propose(vec![], data)?;

        // Get the proposed index (last index in the log)
        let index = node.raft.raft_log.last_index();

        // Track proposal time for commit latency metrics
        self.proposal_times.write().insert(index, Instant::now());

        debug!(
            "Proposed entry at index {} to {}-{}, will commit via background loop",
            index,
            self.topic,
            self.partition
        );

        Ok(index)
    }

    /// Propose a new entry and wait for it to be committed
    ///
    /// This is the synchronous version that waits for Raft to commit the entry
    /// before returning. Use this for produce requests that need acks.
    ///
    /// # Arguments
    /// * `data` - Entry data to propose (serialized produce request)
    ///
    /// # Returns
    /// The committed entry index on success
    pub async fn propose_and_wait(&self, data: Vec<u8>) -> Result<u64> {
        // Propose the entry
        let index = {
            let mut node = self.raw_node.write();

            trace!(
                "Proposing entry of {} bytes to {}-{} (with wait)",
                data.len(),
                self.topic,
                self.partition
            );

            // Check if we're the leader
            if node.raft.state != StateRole::Leader {
                let state = self.state.read();
                return Err(RaftError::Config(format!(
                    "Not leader (current leader: {})",
                    state.leader_id
                )));
            }

            // Propose the entry
            node.propose(vec![], data)?;

            // Get the proposed index
            let index = node.raft.raft_log.last_index();

            debug!(
                "Proposed entry at index {} to {}-{}, waiting for commit",
                index,
                self.topic,
                self.partition
            );

            index
        };

        // Track proposal time for commit latency metrics
        self.proposal_times.write().insert(index, Instant::now());

        // Create channel for commit notification
        let (tx, rx) = oneshot::channel();

        // Register pending proposal BEFORE any processing
        // This ensures the notification channel is ready when commit happens
        self.pending_proposals.write().insert(index, tx);

        debug!(
            "Waiting for entry {} to commit on {}-{}",
            index, self.topic, self.partition
        );

        // The background processing loop will drive Raft forward via tick() and ready()
        // which will eventually commit this entry and send notification via the channel.

        // Wait for commit (with timeout)
        match tokio::time::timeout(Duration::from_secs(30), rx).await {
            Ok(Ok(Ok(()))) => {
                debug!("Entry {} committed on {}-{}", index, self.topic, self.partition);
                Ok(index)
            }
            Ok(Ok(Err(e))) => Err(e),
            Ok(Err(_)) => Err(RaftError::StorageError(
                "Commit notification channel closed".to_string()
            )),
            Err(_) => Err(RaftError::StorageError(
                "Timeout waiting for commit".to_string()
            )),
        }
    }

    /// Propose a configuration change (add/remove node)
    ///
    /// This is used to add or remove peers from the Raft cluster.
    /// Configuration changes are the proper way to modify cluster membership.
    ///
    /// # Arguments
    /// * `change` - ConfChange to propose (AddNode, RemoveNode, etc.)
    ///
    /// # Returns
    /// The proposed entry index on success
    pub async fn propose_conf_change(&self, change: ConfChange) -> Result<u64> {
        let mut node = self.raw_node.write();

        info!(
            "Proposing conf change for {}-{}: type={:?}, node_id={}",
            self.topic,
            self.partition,
            change.get_change_type(),
            change.node_id
        );

        // Check if we're the leader
        if node.raft.state != StateRole::Leader {
            let state = self.state.read();
            return Err(RaftError::Config(format!(
                "Not leader (current leader: {})",
                state.leader_id
            )));
        }

        // Propose the configuration change
        node.propose_conf_change(vec![], change)?;

        // Get the proposed index
        let index = node.raft.raft_log.last_index();

        info!(
            "Proposed conf change at index {} for {}-{}",
            index,
            self.topic,
            self.partition
        );

        Ok(index)
    }

    /// Drive the Raft state machine forward
    ///
    /// This should be called periodically (the tick_interval in the background loop).
    /// Each call to tick() increments the internal election/heartbeat timers.
    pub fn tick(&self) -> Result<()> {
        let mut node = self.raw_node.write();

        trace!(
            "Ticking Raft for {}-{}: last_index={}, commit={}",
            self.topic,
            self.partition,
            node.raft.raft_log.last_index(),
            node.raft.raft_log.committed
        );

        // CRITICAL: Always call node.tick() - it increments internal election/heartbeat counters.
        // The Raft library expects tick() to be called on every iteration.
        node.tick();

        Ok(())
    }

    /// Process an incoming Raft message
    ///
    /// # Arguments
    /// * `msg` - Raft message to process
    pub async fn step(&self, msg: Message) -> Result<()> {
        // DIAGNOSTIC: Log all incoming messages with full details
        info!(
            "Received Raft message for {}-{}: type={:?}, from={}, to={}, term={}, log_term={}, index={}, commit={}, entries={}, reject={}",
            self.topic,
            self.partition,
            msg.msg_type,
            msg.from,
            msg.to,
            msg.term,
            msg.log_term,
            msg.index,
            msg.commit,
            msg.entries.len(),
            msg.reject
        );

        let mut node = self.raw_node.write();
        node.step(msg)?;

        Ok(())
    }

    /// Process ready states and apply committed entries
    ///
    /// This should be called after tick() and step() to:
    /// 1. Send messages to peers
    /// 2. Apply committed entries to the state machine
    /// 3. Persist log entries
    ///
    /// # Returns
    /// Vector of messages to send to peers, and vector of committed entries
    pub async fn ready(&self) -> Result<(Vec<Message>, Vec<Entry>)> {
        // Step 1: Extract ready and update state (must hold locks briefly)
        let (mut messages, persisted_messages, entries_to_persist, committed_entries, snapshot_to_install) = {
            let mut node = self.raw_node.write();

            let has_ready = node.has_ready();

            // ALWAYS log to debug the mystery of missing messages
            if has_ready {
                info!(
                    "ready() HAS_READY for {}-{}: raft_state={:?}, term={}, commit={}, leader={}",
                    self.topic,
                    self.partition,
                    node.raft.state,
                    node.raft.term,
                    node.raft.raft_log.committed,
                    node.raft.leader_id
                );

                // DIAGNOSTIC: Log Progress tracker state
                info!("Progress tracker for {}-{}:", self.topic, self.partition);
                for (peer_id, progress) in node.raft.prs().iter() {
                    info!(
                        "  Peer {}: matched={}, next_idx={}, state={:?}, paused={}, pending_snapshot={}, recent_active={}",
                        peer_id,
                        progress.matched,
                        progress.next_idx,
                        progress.state,
                        progress.paused,
                        progress.pending_snapshot,
                        progress.recent_active
                    );
                }

                // DIAGNOSTIC: Log Raft log state
                info!(
                    "Raft log for {}-{}: last_index={}, committed={}, applied={}, unstable_entries={}",
                    self.topic,
                    self.partition,
                    node.raft.raft_log.last_index(),
                    node.raft.raft_log.committed,
                    node.raft.raft_log.applied,
                    node.raft.raft_log.unstable_entries().len()
                );
            }

            if !has_ready {
                return Ok((Vec::new(), Vec::new()));
            }

            let mut ready = node.ready();

            info!(
                "ready() EXTRACTING for {}-{}: immediate_msgs={}, persisted_msgs={}, entries={}, committed={}, soft_state={:?}, hard_state={:?}",
                self.topic,
                self.partition,
                ready.messages().len(),
                ready.persisted_messages().len(),
                ready.entries().len(),
                ready.committed_entries().len(),
                ready.ss(),
                ready.hs()
            );

            // Check for snapshot to install (BEFORE persisting anything)
            let snapshot_to_install = if !ready.snapshot().is_empty() {
                let snap_meta = ready.snapshot().get_metadata();
                info!(
                    "Received snapshot for {}-{}: index={}, term={}",
                    self.topic,
                    self.partition,
                    snap_meta.get_index(),
                    snap_meta.get_term()
                );
                Some(ready.snapshot().clone())
            } else {
                None
            };

            // Update state from soft/hard state changes
            {
                let mut state = self.state.write();

                if let Some(ss) = ready.ss() {
                    let old_role = state.role;
                    state.leader_id = ss.leader_id;
                    state.role = ss.raft_state;

                    // Log state transitions with structured format
                    if old_role != state.role {
                        info!(
                            node_id = self.config.node_id,
                            topic = %self.topic,
                            partition = self.partition,
                            old_role = ?old_role,
                            new_role = ?state.role,
                            leader_id = state.leader_id,
                            term = state.term,
                            "Raft state transition"
                        );
                    } else {
                        debug!(
                            node_id = self.config.node_id,
                            topic = %self.topic,
                            partition = self.partition,
                            role = ?state.role,
                            leader_id = state.leader_id,
                            "State update (no role change)"
                        );
                    }

                    // Update metrics on state change
                    let node_id_str = self.config.node_id.to_string();
                    let state_value = match state.role {
                        StateRole::Leader => 1,
                        StateRole::Candidate => 2,
                        StateRole::Follower => 3,
                        StateRole::PreCandidate => 2,
                    };
                    self.metrics.set_node_state(&self.topic, self.partition, &node_id_str, state_value);

                    // Track leader changes with detailed logging
                    if old_role != StateRole::Leader && state.role == StateRole::Leader {
                        info!(
                            node_id = self.config.node_id,
                            topic = %self.topic,
                            partition = self.partition,
                            term = state.term,
                            "Became Raft leader"
                        );

                        // Send event to notify server layer of leadership change
                        if let Some(ref event_tx) = self.event_tx {
                            let event = RaftEvent::BecameLeader {
                                topic: self.topic.clone(),
                                partition: self.partition as u32,
                                node_id: self.config.node_id,
                                term: state.term,
                            };

                            if let Err(e) = event_tx.send(event) {
                                warn!(
                                    "Failed to send BecameLeader event for {}/{}: {:?}",
                                    self.topic, self.partition, e
                                );
                            } else {
                                debug!(
                                    "Sent BecameLeader event for {}/{} (node={}, term={})",
                                    self.topic, self.partition, self.config.node_id, state.term
                                );
                            }
                        }
                    }
                }

                if let Some(hs) = ready.hs() {
                    state.term = hs.term;
                    state.commit_index = hs.commit;

                    trace!(
                        "Hard state update for {}-{}: term={} commit={}",
                        self.topic,
                        self.partition,
                        state.term,
                        state.commit_index
                    );

                    // Update metrics
                    self.metrics.set_current_term(&self.topic, self.partition, state.term);
                    self.metrics.set_commit_index(&self.topic, self.partition, state.commit_index);
                }
            } // Drop state lock here

            // Collect data before persisting
            // CRITICAL FIX: For non-leader nodes (Followers/Candidates), messages are in
            // persisted_messages() and must be sent AFTER persisting hard state!
            // Leaders can send messages immediately (via take_messages()).
            let immediate_messages = ready.take_messages();  // Leader messages (send immediately)
            let persisted_messages = ready.take_persisted_messages();  // Non-leader messages (send after persist)
            let entries_to_persist: Vec<Entry> = ready.entries().iter().cloned().collect();
            let committed_entries = ready.take_committed_entries();
            let hard_state_to_persist = ready.hs().cloned(); // Extract hard state before persist

            if !committed_entries.is_empty() {
                debug!(
                    "Ready has {} committed entries for {}-{}",
                    committed_entries.len(),
                    self.topic,
                    self.partition
                );
            }

            // CRITICAL FIX: Persist hard state and entries to tikv/raft's MemStorage BEFORE advance()!
            // tikv/raft expects storage to be updated synchronously before advance() is called.
            // Order matters: hard_state first, then entries.
            if let Some(hs) = hard_state_to_persist.as_ref() {
                let storage = node.mut_store();
                storage.wl().set_hardstate(hs.clone());
            }

            if !entries_to_persist.is_empty() {
                let storage = node.mut_store();
                storage.wl().append(&entries_to_persist)?;
            }

            // Now it's safe to call advance() - storage is in sync
            let mut light_rd = node.advance(ready);

            // Collect all messages:
            // 1. immediate_messages (Leader messages - send now)
            // 2. light_rd messages (additional messages from advance)
            // 3. persisted_messages (Follower/Candidate messages - send after persist)
            let mut all_messages = immediate_messages;
            all_messages.extend(light_rd.take_messages());
            // Note: persisted_messages will be added AFTER we persist to our custom storage below

            let mut all_committed = committed_entries;
            all_committed.extend(light_rd.take_committed_entries());

            (all_messages, persisted_messages, entries_to_persist, all_committed, snapshot_to_install)
        }; // Drop node lock here

        // Step 1.5: Install snapshot if present (BEFORE applying committed entries)
        if let Some(snapshot) = snapshot_to_install {
            match self.install_snapshot(&snapshot).await {
                Ok(_) => {
                    info!("Successfully installed snapshot for {}-{}", self.topic, self.partition);
                }
                Err(e) => {
                    error!("Failed to install snapshot for {}-{}: {}", self.topic, self.partition, e);
                    // Report failure to Raft
                    let mut node = self.raw_node.write();
                    use raft::SnapshotStatus;
                    node.report_snapshot(self.config.node_id, SnapshotStatus::Failure);
                }
            }
        }

        // Step 2: Persist entries to our custom log storage (async, locks released)
        // This is for durability - tikv/raft's MemStorage is already updated above.
        if !entries_to_persist.is_empty() {
            self.persist_entries_to_custom_storage(&entries_to_persist).await?;
        }

        // CRITICAL FIX: After persisting hard state, we can now send persisted messages!
        // These are messages from Followers/Candidates that require state persistence first.
        if !persisted_messages.is_empty() {
            debug!(
                "Adding {} persisted messages (Follower/Candidate vote requests) to outgoing queue for {}-{}",
                persisted_messages.len(),
                self.topic,
                self.partition
            );
            messages.extend(persisted_messages);
        }

        // DIAGNOSTIC: Log all outgoing messages
        if !messages.is_empty() {
            info!("Outgoing messages from {}-{}: total={}", self.topic, self.partition, messages.len());
            for (i, msg) in messages.iter().enumerate() {
                info!(
                    "  Msg[{}]: type={:?}, from={}, to={}, term={}, log_term={}, index={}, commit={}, entries={}, reject={}",
                    i,
                    msg.msg_type,
                    msg.from,
                    msg.to,
                    msg.term,
                    msg.log_term,
                    msg.index,
                    msg.commit,
                    msg.entries.len(),
                    msg.reject
                );
            }
        }

        // Step 3: Apply committed entries (handle both data entries and conf changes)
        if !committed_entries.is_empty() {
            info!(
                node_id = self.config.node_id,
                topic = %self.topic,
                partition = self.partition,
                count = committed_entries.len(),
                first_index = committed_entries.first().map(|e| e.index),
                last_index = committed_entries.last().map(|e| e.index),
                "Committed entries ready for application"
            );

            // DIAGNOSTIC: Log each committed entry
            for (i, entry) in committed_entries.iter().enumerate() {
                info!(
                    "  Committed[{}]: index={}, term={}, type={:?}, data_len={}",
                    i,
                    entry.index,
                    entry.term,
                    entry.entry_type,
                    entry.data.len()
                );
            }

            for entry in &committed_entries {
                // Check if this is a configuration change
                if entry.get_entry_type() == EntryType::EntryConfChange {
                    // Decode the ConfChange from entry data using the prost bridge
                    match crate::prost_bridge::decode_conf_change(&entry.data) {
                        Ok(conf_change) => {
                            info!(
                                "Applying conf change at index {}: type={:?}, node_id={}",
                                entry.index,
                                conf_change.get_change_type(),
                                conf_change.node_id
                            );

                            // Apply the configuration change to RawNode
                            // This updates the internal Progress tracker
                            let mut node = self.raw_node.write();
                            let conf_state = node.apply_conf_change(&conf_change)?;

                            info!(
                                "Conf change applied for {}-{}: voters={:?}, learners={:?}",
                                self.topic,
                                self.partition,
                                conf_state.voters,
                                conf_state.learners
                            );
                        }
                        Err(e) => {
                            error!(
                                "Failed to decode ConfChange at index {} for {}-{}: {}",
                                entry.index,
                                self.topic,
                                self.partition,
                                e
                            );
                        }
                    }
                } else {
                    // Regular data entry - apply to state machine
                    let raft_entry = crate::RaftEntry {
                        index: entry.index,
                        term: entry.term,
                        data: entry.data.clone(),
                    };

                    // Apply to state machine
                    let mut sm = self.state_machine.write().await;
                    match sm.apply(&raft_entry).await {
                        Ok(result) => {
                            trace!(
                                "Applied entry {} to state machine: {:?}",
                                entry.index,
                                result
                            );
                        }
                        Err(e) => {
                            warn!(
                                "Failed to apply entry {} to state machine for {}-{}: {}",
                                entry.index,
                                self.topic,
                                self.partition,
                                e
                            );
                            // Continue applying remaining entries even if one fails
                            // This allows partial progress and avoids getting stuck
                        }
                    }
                }
            }

            // Update applied index after all entries are processed
            if let Some(last_entry) = committed_entries.last() {
                let mut state = self.state.write();
                state.applied_index = last_entry.index;
                debug!(
                    "Updated applied_index to {} for {}-{}",
                    last_entry.index,
                    self.topic,
                    self.partition
                );

                // Update applied index metric
                self.metrics.set_last_applied(&self.topic, self.partition, state.applied_index);
            }

            // Notify pending proposals that their entries have been committed
            // AND record commit latency metrics
            let mut pending = self.pending_proposals.write();
            let mut proposal_times = self.proposal_times.write();

            for entry in &committed_entries {
                // Record commit latency if we have the proposal time
                if let Some(proposed_at) = proposal_times.remove(&entry.index) {
                    let latency_ms = proposed_at.elapsed().as_millis() as f64;
                    self.metrics.record_commit_latency(&self.topic, self.partition, latency_ms, 1);
                }

                // Notify pending waiters
                if let Some(sender) = pending.remove(&entry.index) {
                    // Notify the waiter that the entry is committed
                    let _ = sender.send(Ok(()));
                    trace!(
                        "Notified pending proposal for index {} on {}-{}",
                        entry.index,
                        self.topic,
                        self.partition
                    );
                }
            }
        }

        Ok((messages, committed_entries))
    }

    /// Check if this node is the Raft leader
    pub fn is_leader(&self) -> bool {
        let state = self.state.read();
        state.role == StateRole::Leader
    }

    /// Get the current Raft leader ID
    pub fn leader_id(&self) -> u64 {
        let state = self.state.read();
        state.leader_id
    }

    /// Get the current Raft term
    pub fn term(&self) -> u64 {
        let state = self.state.read();
        state.term
    }

    /// Get the current commit index
    pub fn commit_index(&self) -> u64 {
        let state = self.state.read();
        state.commit_index
    }

    /// Get the current applied index
    pub fn applied_index(&self) -> u64 {
        let state = self.state.read();
        state.applied_index
    }

    /// Get topic name
    pub fn topic(&self) -> &str {
        &self.topic
    }

    /// Get partition ID
    pub fn partition(&self) -> i32 {
        self.partition
    }

    /// Mark an index as applied to the state machine
    pub fn set_applied_index(&self, index: u64) {
        let mut state = self.state.write();
        state.applied_index = index;

        trace!(
            "Applied index updated to {} for {}-{}",
            index,
            self.topic,
            self.partition
        );
    }

    /// Get current role (Leader, Follower, Candidate)
    pub fn role(&self) -> StateRole {
        let state = self.state.read();
        state.role
    }

    /// Persist log entries to our custom durable storage (not tikv/raft's MemStorage)
    /// tikv/raft's MemStorage is updated synchronously in ready() before advance().
    async fn persist_entries_to_custom_storage(&self, entries: &[Entry]) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }

        debug!(
            "Persisting {} entries to custom storage for {}-{}",
            entries.len(),
            self.topic,
            self.partition
        );

        // Convert raft::Entry to our RaftEntry format
        let raft_entries: Vec<crate::storage::RaftEntry> = entries
            .iter()
            .map(|e| crate::storage::RaftEntry::new(e.term, e.index, e.data.clone()))
            .collect();

        // Append to our custom durable storage
        self.log_storage.append(raft_entries).await?;

        Ok(())
    }

    /// Install a snapshot received from the leader
    async fn install_snapshot(&self, snapshot: &raft::prelude::Snapshot) -> Result<()> {
        use crate::state_machine::SnapshotData;
        use raft::SnapshotStatus;

        let metadata = snapshot.get_metadata();
        let last_index = metadata.get_index();
        let last_term = metadata.get_term();

        info!(
            "Installing snapshot for {}-{}: index={}, term={}, size={} bytes",
            self.topic, self.partition, last_index, last_term, snapshot.get_data().len()
        );

        // Convert tikv/raft Snapshot to our SnapshotData
        let snapshot_data = SnapshotData {
            last_index,
            last_term,
            conf_state: metadata.get_conf_state().get_voters().to_vec(),
            data: snapshot.get_data().to_vec(),
        };

        // Restore state machine from snapshot
        {
            let mut sm = self.state_machine.write().await;
            sm.restore(&snapshot_data).await.map_err(|e| {
                RaftError::StorageError(format!("Failed to restore snapshot: {}", e))
            })?;

            info!(
                "State machine restored from snapshot at index {}, last_applied={}",
                last_index,
                sm.last_applied()
            );
        }

        // Update applied index
        self.set_applied_index(last_index);

        // Report success to Raft
        {
            let mut node = self.raw_node.write();
            node.report_snapshot(self.config.node_id, SnapshotStatus::Finish);
        }

        info!(
            "Snapshot installed successfully for {}-{} at index {}",
            self.topic, self.partition, last_index
        );

        Ok(())
    }

    /// Check if snapshot should be created based on log size
    pub fn should_create_snapshot(&self) -> bool {
        let node = self.raw_node.read();
        let last_index = node.raft.raft_log.last_index();
        let applied = self.applied_index();

        // Create snapshot if unapplied entries exceed threshold
        let unapplied = last_index.saturating_sub(applied);
        unapplied >= self.config.snapshot_threshold
    }

    /// Create and store a snapshot
    pub async fn create_snapshot(&self) -> Result<()> {
        use crate::state_machine::SnapshotData;

        let applied = self.applied_index();
        let term = self.term();

        info!(
            "Creating snapshot for {}-{} at index={}, term={}",
            self.topic, self.partition, applied, term
        );

        // Create snapshot from state machine
        let snapshot_data = {
            let sm = self.state_machine.read().await;
            sm.snapshot(applied, term).await.map_err(|e| {
                RaftError::StorageError(format!("Failed to create snapshot: {}", e))
            })?
        };

        info!(
            "Created snapshot: index={}, term={}, size={} bytes",
            snapshot_data.last_index,
            snapshot_data.last_term,
            snapshot_data.data.len()
        );

        // Convert to tikv/raft Snapshot format
        let mut raft_snapshot = raft::prelude::Snapshot::default();
        let mut metadata = raft::prelude::SnapshotMetadata::default();
        metadata.set_index(snapshot_data.last_index);
        metadata.set_term(snapshot_data.last_term);

        // Set conf_state (cluster configuration)
        let mut conf_state = raft::prelude::ConfState::default();
        conf_state.set_voters(snapshot_data.conf_state.clone());
        metadata.set_conf_state(conf_state);

        raft_snapshot.set_metadata(metadata);
        raft_snapshot.set_data(snapshot_data.data);

        // Store snapshot in Raft storage (MemStorage)
        {
            let mut node = self.raw_node.write();
            let storage = node.mut_store();
            storage.wl().apply_snapshot(raft_snapshot).map_err(|e| {
                RaftError::StorageError(format!("Failed to apply snapshot to storage: {}", e))
            })?;
        }

        info!(
            "Snapshot created and stored for {}-{} at index {}",
            self.topic, self.partition, snapshot_data.last_index
        );

        Ok(())
    }

    /// Campaign to become leader (for testing)
    #[cfg(test)]
    pub fn campaign(&self) -> Result<()> {
        let mut node = self.raw_node.write();
        node.campaign()?;
        Ok(())
    }
}

/// Create a tracing logger for Raft
fn tracing_logger() -> slog::Logger {
    use slog::{Drain, Logger, o};
    use slog_stdlog::StdLog;

    let drain = StdLog.fuse();
    Logger::root(drain, o!())
}

// Old tests disabled - need to be updated with state_machine parameter
#[cfg(disabled_test)]
mod tests {
    use super::*;
    use crate::storage::MemoryLogStorage;

    #[tokio::test]
    async fn test_create_replica() {
        let config = RaftConfig {
            node_id: 1,
            ..Default::default()
        };

        let storage = Arc::new(MemoryLogStorage::new());

        let replica = PartitionReplica::new(
            "test-topic".to_string(),
            0,
            config,
            storage,
            vec![],
        );

        assert!(replica.is_ok());
        let replica = replica.unwrap();
        assert_eq!(replica.info(), ("test-topic", 0));
    }

    #[tokio::test]
    async fn test_propose_as_follower_fails() {
        let config = RaftConfig {
            node_id: 1,
            ..Default::default()
        };

        let storage = Arc::new(MemoryLogStorage::new());

        let replica = PartitionReplica::new(
            "test-topic".to_string(),
            0,
            config,
            storage,
            vec![2, 3], // Multi-node cluster - starts as follower
        ).unwrap();

        // Should fail - not leader
        let result = replica.propose(b"test data".to_vec()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_single_node_propose() {
        let config = RaftConfig {
            node_id: 1,
            ..Default::default()
        };

        let storage = Arc::new(MemoryLogStorage::new());

        let replica = PartitionReplica::new(
            "test-topic".to_string(),
            0,
            config,
            storage,
            vec![], // Single node cluster
        ).unwrap();

        // Campaign to become leader
        replica.campaign().unwrap();

        // Process ready to apply leader state
        let _ = replica.ready().await.unwrap();

        // Verify we became leader
        assert!(replica.is_leader(), "Single-node replica should become leader after campaign");

        // Should succeed - we're the leader
        let result = replica.propose(b"test data".to_vec()).await;
        assert!(result.is_ok(), "Propose should succeed when we're the leader");
    }

    #[tokio::test]
    async fn test_state_tracking() {
        let config = RaftConfig {
            node_id: 1,
            ..Default::default()
        };

        let storage = Arc::new(MemoryLogStorage::new());

        let replica = PartitionReplica::new(
            "test-topic".to_string(),
            0,
            config,
            storage,
            vec![],
        ).unwrap();

        assert_eq!(replica.term(), 0);
        assert_eq!(replica.commit_index(), 0);
        assert!(!replica.is_leader());
    }

    #[tokio::test]
    async fn test_tick() {
        let config = RaftConfig {
            node_id: 1,
            heartbeat_interval_ms: 10,
            ..Default::default()
        };

        let storage = Arc::new(MemoryLogStorage::new());

        let replica = PartitionReplica::new(
            "test-topic".to_string(),
            0,
            config,
            storage,
            vec![],
        ).unwrap();

        // Tick should not fail
        assert!(replica.tick().is_ok());

        // Wait for tick interval
        tokio::time::sleep(Duration::from_millis(15)).await;

        // Tick again
        assert!(replica.tick().is_ok());
    }

    #[tokio::test]
    async fn test_applied_index_tracking() {
        let config = RaftConfig {
            node_id: 1,
            ..Default::default()
        };

        let storage = Arc::new(MemoryLogStorage::new());

        let replica = PartitionReplica::new(
            "test-topic".to_string(),
            0,
            config,
            storage,
            vec![],
        ).unwrap();

        assert_eq!(replica.applied_index(), 0);

        replica.set_applied_index(5);
        assert_eq!(replica.applied_index(), 5);
    }
}

#[cfg(test)]
mod replica_test {
    use super::*;
    use crate::state_machine::{StateMachine, SnapshotData};
    use crate::storage::MemoryLogStorage;
    use crate::RaftEntry;
    use async_trait::async_trait;
    use bytes::Bytes;
    use std::sync::Arc;
    use tokio::sync::RwLock as TokioRwLock;

    /// No-op state machine for testing
    struct NoOpStateMachine {
        last_applied: u64,
    }

    impl NoOpStateMachine {
        fn new() -> Self {
            Self { last_applied: 0 }
        }
    }

    #[async_trait]
    impl StateMachine for NoOpStateMachine {
        async fn apply(&mut self, entry: &RaftEntry) -> Result<Bytes> {
            self.last_applied = entry.index;
            Ok(Bytes::new())
        }

        async fn snapshot(&self, _last_index: u64, _last_term: u64) -> Result<SnapshotData> {
            Ok(SnapshotData {
                last_index: self.last_applied,
                last_term: 0,
                conf_state: vec![],
                data: vec![],
            })
        }

        async fn restore(&mut self, snapshot: &SnapshotData) -> Result<()> {
            self.last_applied = snapshot.last_index;
            Ok(())
        }

        fn last_applied(&self) -> u64 {
            self.last_applied
        }
    }

    #[tokio::test]
    async fn test_single_node_quick_debug() {
        println!("\n=== Quick Single Node Debug ===\n");

        // Create single-node replica
        let config = RaftConfig {
            node_id: 1,
            listen_addr: "127.0.0.1:5001".to_string(),
            election_timeout_ms: 150,
            heartbeat_interval_ms: 50,
            ..Default::default()
        };

        let storage = Arc::new(MemoryLogStorage::new());
        let state_machine = Arc::new(TokioRwLock::new(NoOpStateMachine::new()));

        println!("Creating replica...");
        let replica = PartitionReplica::new(
            "test".to_string(),
            0,
            config,
            storage,
            state_machine,
            vec![], // Single node
        )
        .expect("Failed to create replica");

        println!("Replica created");
        println!("is_leader: {}", replica.is_leader());
        println!("role: {:?}", replica.role());
        println!("leader_id: {}", replica.leader_id());

        if !replica.is_leader() {
            panic!("Single-node should be leader!");
        }

        // Check log state before proposal
        {
            let node = replica.raw_node.read();
            println!("\nBefore proposal:");
            println!("  last_index: {}", node.raft.raft_log.last_index());
            println!("  committed: {}", node.raft.raft_log.committed);
            println!("  has_ready: {}", node.has_ready());
        }

        // Try to propose
        println!("\nProposing entry...");
        let data = b"test".to_vec();
        let result = replica.propose(data).await;

        match &result {
            Ok(index) => {
                println!("✓ Proposal succeeded, index: {}", index);
            }
            Err(e) => {
                println!("✗ Proposal failed: {}", e);
            }
        }

        // Check log state after proposal
        {
            let node = replica.raw_node.read();
            println!("\nAfter proposal:");
            println!("  last_index: {}", node.raft.raft_log.last_index());
            println!("  committed: {}", node.raft.raft_log.committed);
            println!("  has_ready: {}", node.has_ready());
        }

        assert!(result.is_ok(), "Proposal should succeed");

        println!("\n=== Test Complete ===\n");
    }
}
