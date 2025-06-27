//! Production-ready Raft node implementation using tikv/raft-rs.

use chronik_common::{Result, Error};
use raft::{
    RawNode, StateRole,
    storage::Storage,
};
use raft::prelude::*;
use protobuf::Message as ProtoMessage;
use slog::{Logger, o, Drain};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};
use tokio::time::interval;
use tracing::{debug, error, info, warn};

use super::{NodeId, RaftConfig as ChronikRaftConfig, RaftStorage, RaftTransport, TransportMessage, ArcRaftStorage};
use super::state_machine::{ControllerStateMachine, Proposal};

/// Commands to the Raft node
#[derive(Debug)]
pub enum RaftCommand {
    /// Propose a change
    Propose {
        proposal: Proposal,
        callback: oneshot::Sender<Result<()>>,
    },
    /// Transfer leadership
    TransferLeader {
        target: NodeId,
        callback: oneshot::Sender<Result<()>>,
    },
    /// Get current state
    GetState {
        callback: oneshot::Sender<super::state_machine::ControllerState>,
    },
    /// Get leader ID
    GetLeader {
        callback: oneshot::Sender<Option<NodeId>>,
    },
    /// Add a learner node
    AddLearner {
        node_id: NodeId,
        callback: oneshot::Sender<Result<()>>,
    },
    /// Promote learner to voter
    PromoteLearner {
        node_id: NodeId,
        callback: oneshot::Sender<Result<()>>,
    },
    /// Remove a node
    RemoveNode {
        node_id: NodeId,
        callback: oneshot::Sender<Result<()>>,
    },
}

/// Raft node implementation
pub struct RaftNode {
    config: ChronikRaftConfig,
    raw_node: RawNode<ArcRaftStorage>,
    state_machine: Arc<ControllerStateMachine>,
    transport: Arc<RaftTransport>,
    storage: Arc<RaftStorage>,
    logger: Logger,
    
    // Pending proposals
    pending_proposals: VecDeque<(Vec<u8>, oneshot::Sender<Result<()>>)>,
    
    // Command channel
    cmd_rx: mpsc::Receiver<RaftCommand>,
    
    // Metrics
    last_tick: Instant,
    tick_count: u64,
}

impl RaftNode {
    /// Create a new Raft node
    pub fn new(
        config: ChronikRaftConfig,
        storage: RaftStorage,
        transport: RaftTransport,
    ) -> Result<(Self, RaftHandle)> {
        // Setup logger
        let decorator = slog_term::TermDecorator::new().build();
        let drain = slog_term::CompactFormat::new(decorator).build().fuse();
        let drain = slog_async::Async::new(drain).build().fuse();
        let logger = Logger::root(drain, o!("node_id" => config.node_id));
        
        // Convert to raft-rs config
        let raft_config = config.to_raft_config();
        
        // Create state machine
        let state_machine = Arc::new(ControllerStateMachine::new());
        
        // Check if this is a new node
        let initial_state = storage.initial_state()
            .map_err(|e| Error::Storage(format!("Failed to get initial state: {:?}", e)))?;
        
        let is_new = initial_state.conf_state.voters.is_empty() && 
                     initial_state.conf_state.learners.is_empty();
        
        // Initialize storage for new node
        if is_new {
            let voters = config.all_nodes();
            storage.initialize_with_conf_state(voters, vec![])?;
        }
        
        // Create storage arc
        let storage_arc = Arc::new(storage);
        
        // Create raw node with wrapper
        let storage_wrapper = ArcRaftStorage(storage_arc.clone());
        let raw_node = RawNode::new(&raft_config, storage_wrapper, &logger)
            .map_err(|e| Error::Internal(format!("Failed to create raw node: {:?}", e)))?;
        
        // Create command channel
        let (cmd_tx, cmd_rx) = mpsc::channel(100);
        
        let node = Self {
            config,
            raw_node,
            state_machine,
            transport: Arc::new(transport),
            storage: storage_arc,
            logger,
            pending_proposals: VecDeque::new(),
            cmd_rx,
            last_tick: Instant::now(),
            tick_count: 0,
        };
        
        let handle = RaftHandle { cmd_tx };
        
        Ok((node, handle))
    }
    
    /// Run the Raft node
    pub async fn run(mut self) -> Result<()> {
        info!("Starting Raft node {}", self.config.node_id);
        
        // Start transport
        let transport = self.transport.clone();
        tokio::spawn(async move {
            if let Err(e) = transport.start().await {
                error!("Transport error: {}", e);
            }
        });
        
        // Create ticker
        let tick_interval = Duration::from_millis(self.config.heartbeat_interval);
        let mut ticker = interval(tick_interval);
        
        // Main event loop
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    self.tick();
                }
                
                Some(cmd) = self.cmd_rx.recv() => {
                    self.handle_command(cmd)?;
                }
                
                Some(msg) = self.transport.recv() => {
                    self.handle_message(msg)?;
                }
            }
            
            // Process ready events
            if self.raw_node.has_ready() {
                self.handle_ready().await?;
            }
        }
    }
    
    /// Tick the Raft node
    fn tick(&mut self) {
        self.raw_node.tick();
        self.tick_count += 1;
        
        // Log tick metrics periodically
        if self.tick_count % 100 == 0 {
            let elapsed = self.last_tick.elapsed();
            debug!("Raft tick stats: {} ticks in {:?}", self.tick_count, elapsed);
            self.last_tick = Instant::now();
        }
    }
    
    /// Handle a command
    fn handle_command(&mut self, cmd: RaftCommand) -> Result<()> {
        match cmd {
            RaftCommand::Propose { proposal, callback } => {
                if self.raw_node.raft.state != StateRole::Leader {
                    let _ = callback.send(Err(Error::Internal("Not the leader".into())));
                    return Ok(());
                }
                
                let data = bincode::serialize(&proposal)
                    .map_err(|e| Error::Internal(format!("Failed to serialize proposal: {}", e)))?;
                
                match self.raw_node.propose(vec![], data.clone()) {
                    Ok(()) => {
                        self.pending_proposals.push_back((data, callback));
                    }
                    Err(e) => {
                        let _ = callback.send(Err(Error::Internal(format!("Propose failed: {:?}", e))));
                    }
                }
            }
            
            RaftCommand::TransferLeader { target, callback } => {
                self.raw_node.transfer_leader(target);
                let _ = callback.send(Ok(()));
            }
            
            RaftCommand::GetState { callback } => {
                let state = self.state_machine.get_state();
                let _ = callback.send(state);
            }
            
            RaftCommand::GetLeader { callback } => {
                let leader = if self.raw_node.raft.state == StateRole::Leader {
                    Some(self.config.node_id)
                } else {
                    Some(self.raw_node.raft.leader_id)
                };
                let _ = callback.send(leader);
            }
            
            RaftCommand::AddLearner { node_id, callback } => {
                let mut cc = ConfChange::default();
                cc.set_node_id(node_id);
                cc.set_change_type(ConfChangeType::AddLearnerNode);
                
                match self.raw_node.propose_conf_change(vec![], cc) {
                    Ok(()) => {
                        let _ = callback.send(Ok(()));
                    }
                    Err(e) => {
                        let _ = callback.send(Err(Error::Internal(format!("Failed to add learner: {:?}", e))));
                    }
                }
            }
            
            RaftCommand::PromoteLearner { node_id, callback } => {
                let mut cc = ConfChange::default();
                cc.set_node_id(node_id);
                cc.set_change_type(ConfChangeType::AddNode);
                
                match self.raw_node.propose_conf_change(vec![], cc) {
                    Ok(()) => {
                        let _ = callback.send(Ok(()));
                    }
                    Err(e) => {
                        let _ = callback.send(Err(Error::Internal(format!("Failed to promote learner: {:?}", e))));
                    }
                }
            }
            
            RaftCommand::RemoveNode { node_id, callback } => {
                let mut cc = ConfChange::default();
                cc.set_node_id(node_id);
                cc.set_change_type(ConfChangeType::RemoveNode);
                
                match self.raw_node.propose_conf_change(vec![], cc) {
                    Ok(()) => {
                        let _ = callback.send(Ok(()));
                    }
                    Err(e) => {
                        let _ = callback.send(Err(Error::Internal(format!("Failed to remove node: {:?}", e))));
                    }
                }
            }
        }
        
        Ok(())
    }
    
    /// Handle incoming Raft message
    fn handle_message(&mut self, msg: TransportMessage) -> Result<()> {
        let raft_msg = <Message as ProtoMessage>::parse_from_bytes(&msg.message)
            .map_err(|e| Error::Network(format!("Failed to parse message: {:?}", e)))?;
        
        self.raw_node.step(raft_msg)
            .map_err(|e| Error::Internal(format!("Failed to step message: {:?}", e)))?;
        
        Ok(())
    }
    
    /// Handle ready events
    async fn handle_ready(&mut self) -> Result<()> {
        let mut ready = self.raw_node.ready();
        
        // Send messages
        for msg in ready.take_messages() {
            let to = msg.to;
            if let Err(e) = self.transport.send(to, msg).await {
                warn!("Failed to send message to {}: {}", to, e);
            }
        }
        
        // Apply snapshot
        if *ready.snapshot() != Snapshot::default() {
            let snapshot = ready.snapshot();
            self.storage.create_snapshot(
                snapshot.get_metadata().index,
                snapshot.get_metadata().term,
                snapshot.get_metadata().get_conf_state().clone(),
                snapshot.data.to_vec(),
            )?;
            
            // Restore state machine from snapshot
            self.state_machine.restore(&snapshot.data)?;
        }
        
        // Append entries
        if !ready.entries().is_empty() {
            self.storage.append_entries(ready.entries())?;
        }
        
        // Apply hard state
        if let Some(hs) = ready.hs() {
            self.storage.save_hard_state(hs)?;
        }
        
        // Apply committed entries
        for entry in ready.take_committed_entries() {
            self.apply_entry(entry)?;
        }
        
        // Advance
        let mut light_rd = self.raw_node.advance(ready);
        
        // Send more messages
        for msg in light_rd.take_messages() {
            let to = msg.to;
            if let Err(e) = self.transport.send(to, msg).await {
                warn!("Failed to send message to {}: {}", to, e);
            }
        }
        
        // Apply more committed entries
        for entry in light_rd.take_committed_entries() {
            self.apply_entry(entry)?;
        }
        
        // Update commit index
        if let Some(commit) = light_rd.commit_index() {
            let mut hs = self.storage.initial_state()?.hard_state;
            hs.set_commit(commit);
            self.storage.save_hard_state(&hs)?;
        }
        
        self.raw_node.advance_apply();
        
        // Flush storage
        self.storage.flush()?;
        
        Ok(())
    }
    
    /// Apply a committed entry
    fn apply_entry(&mut self, entry: Entry) -> Result<()> {
        if entry.data.is_empty() {
            // Empty entry, skip
            return Ok(());
        }
        
        match entry.get_entry_type() {
            EntryType::EntryNormal => {
                // Apply proposal
                match bincode::deserialize::<Proposal>(&entry.data) {
                    Ok(proposal) => {
                        self.state_machine.apply(proposal)?;
                        
                        // Notify pending proposal
                        if let Some((data, callback)) = self.pending_proposals.front() {
                            if data == &entry.data {
                                if let Some((_, callback)) = self.pending_proposals.pop_front() {
                                    let _ = callback.send(Ok(()));
                                }
                            }
                        }
                    }
                    Err(e) => {
                        error!("Failed to deserialize proposal: {}", e);
                    }
                }
            }
            
            EntryType::EntryConfChange => {
                let cc = <ConfChange as ProtoMessage>::parse_from_bytes(&entry.data)
                    .map_err(|e| Error::Internal(format!("Failed to parse conf change: {:?}", e)))?;
                
                let cs = self.raw_node.apply_conf_change(&cc)
                    .map_err(|e| Error::Internal(format!("Failed to apply conf change: {:?}", e)))?;
                self.storage.save_conf_state(&cs)?;
                
                info!("Applied conf change: {:?}", cc);
            }
            
            EntryType::EntryConfChangeV2 => {
                let cc = <ConfChangeV2 as ProtoMessage>::parse_from_bytes(&entry.data)
                    .map_err(|e| Error::Internal(format!("Failed to parse conf change v2: {:?}", e)))?;
                
                let cs = self.raw_node.apply_conf_change(&cc)
                    .map_err(|e| Error::Internal(format!("Failed to apply conf change v2: {:?}", e)))?;
                self.storage.save_conf_state(&cs)?;
                
                info!("Applied conf change v2: {:?}", cc);
            }
        }
        
        Ok(())
    }
    
    /// Create a snapshot
    pub async fn snapshot(&mut self) -> Result<()> {
        let applied = self.raw_node.raft.raft_log.applied;
        let snapshot_data = self.state_machine.snapshot();
        
        // Get current configuration from storage
        let raft_state = self.storage.initial_state()
            .map_err(|e| Error::Storage(format!("Failed to get raft state: {:?}", e)))?;
        let cs = raft_state.conf_state;
        
        self.storage.create_snapshot(
            applied,
            self.raw_node.raft.raft_log.term(applied).unwrap(),
            cs,
            snapshot_data,
        )?;
        
        info!("Created snapshot at index {}", applied);
        
        Ok(())
    }
}

/// Handle for interacting with the Raft node
#[derive(Clone)]
pub struct RaftHandle {
    cmd_tx: mpsc::Sender<RaftCommand>,
}

impl RaftHandle {
    /// Create a mock handle for testing
    pub fn new_mock() -> Self {
        let (tx, _) = mpsc::channel(1);
        Self {
            cmd_tx: tx,
        }
    }
}

impl RaftHandle {
    /// Propose a change to the Raft cluster
    pub async fn propose(&self, proposal: Proposal) -> Result<()> {
        let (callback, receiver) = oneshot::channel();
        self.cmd_tx.send(RaftCommand::Propose { proposal, callback }).await
            .map_err(|_| Error::Internal("Failed to send propose command".into()))?;
        
        receiver.await
            .map_err(|_| Error::Internal("Failed to receive propose response".into()))?
    }
    
    /// Get the current state
    pub async fn get_state(&self) -> Result<super::state_machine::ControllerState> {
        let (callback, receiver) = oneshot::channel();
        self.cmd_tx.send(RaftCommand::GetState { callback }).await
            .map_err(|_| Error::Internal("Failed to send get state command".into()))?;
        
        Ok(receiver.await
            .map_err(|_| Error::Internal("Failed to receive state response".into()))?)
    }
    
    /// Get the current leader
    pub async fn get_leader(&self) -> Result<Option<NodeId>> {
        let (callback, receiver) = oneshot::channel();
        self.cmd_tx.send(RaftCommand::GetLeader { callback }).await
            .map_err(|_| Error::Internal("Failed to send get leader command".into()))?;
        
        Ok(receiver.await
            .map_err(|_| Error::Internal("Failed to receive leader response".into()))?)
    }
    
    /// Transfer leadership to another node
    pub async fn transfer_leader(&self, target: NodeId) -> Result<()> {
        let (callback, receiver) = oneshot::channel();
        self.cmd_tx.send(RaftCommand::TransferLeader { target, callback }).await
            .map_err(|_| Error::Internal("Failed to send transfer leader command".into()))?;
        
        receiver.await
            .map_err(|_| Error::Internal("Failed to receive transfer response".into()))?
    }
    
    /// Add a learner node
    pub async fn add_learner(&self, node_id: NodeId) -> Result<()> {
        let (callback, receiver) = oneshot::channel();
        self.cmd_tx.send(RaftCommand::AddLearner { node_id, callback }).await
            .map_err(|_| Error::Internal("Failed to send add learner command".into()))?;
        
        receiver.await
            .map_err(|_| Error::Internal("Failed to receive add learner response".into()))?
    }
    
    /// Promote a learner to voter
    pub async fn promote_learner(&self, node_id: NodeId) -> Result<()> {
        let (callback, receiver) = oneshot::channel();
        self.cmd_tx.send(RaftCommand::PromoteLearner { node_id, callback }).await
            .map_err(|_| Error::Internal("Failed to send promote learner command".into()))?;
        
        receiver.await
            .map_err(|_| Error::Internal("Failed to receive promote response".into()))?
    }
    
    /// Remove a node from the cluster
    pub async fn remove_node(&self, node_id: NodeId) -> Result<()> {
        let (callback, receiver) = oneshot::channel();
        self.cmd_tx.send(RaftCommand::RemoveNode { node_id, callback }).await
            .map_err(|_| Error::Internal("Failed to send remove node command".into()))?;
        
        receiver.await
            .map_err(|_| Error::Internal("Failed to receive remove response".into()))?
    }
}