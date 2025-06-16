//! Production-ready Raft implementation using tikv/raft-rs.

use chronik_common::{Result, Error};
use raft::{
    Config as RaftConfig, RawNode, Ready, StateRole,
    storage::MemStorage,
};
use protobuf::Message;
use serde::{Deserialize, Serialize};
use slog::{Logger, o, Drain};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};

use crate::raft_simple::{ControllerState, Proposal};

/// Raft node ID type
pub type NodeId = u64;

/// Messages between Raft nodes
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RaftMessage {
    pub from: NodeId,
    pub to: NodeId,
    pub message: Vec<u8>,
}

/// Commands to the Raft node
#[derive(Debug)]
pub enum RaftCommand {
    Propose {
        proposal: Proposal,
        callback: oneshot::Sender<Result<()>>,
    },
    TransferLeader {
        target: NodeId,
        callback: oneshot::Sender<Result<()>>,
    },
    GetState {
        callback: oneshot::Sender<ControllerState>,
    },
    GetLeader {
        callback: oneshot::Sender<Option<NodeId>>,
    },
}

/// Production Raft node implementation
pub struct RaftNode {
    id: NodeId,
    raw_node: RawNode<MemStorage>,
    state_machine: Arc<Mutex<ControllerState>>,
    msg_sender: mpsc::UnboundedSender<RaftMessage>,
    cmd_receiver: mpsc::Receiver<RaftCommand>,
    logger: Logger,
}

impl RaftNode {
    /// Create a new Raft node
    pub fn new(
        id: NodeId,
        peers: Vec<NodeId>,
        msg_sender: mpsc::UnboundedSender<RaftMessage>,
        cmd_receiver: mpsc::Receiver<RaftCommand>,
    ) -> Result<Self> {
        // Setup logger
        let decorator = slog_term::TermDecorator::new().build();
        let drain = slog_term::CompactFormat::new(decorator).build().fuse();
        let drain = slog_async::Async::new(drain).build().fuse();
        let logger = Logger::root(drain, o!("node_id" => id));
        
        // Configure Raft
        let config = RaftConfig {
            id,
            election_tick: 10,
            heartbeat_tick: 3,
            max_size_per_msg: 1024 * 1024,
            max_inflight_msgs: 256,
            applied: 0,
            ..Default::default()
        };
        
        // Create storage
        let storage = MemStorage::new();
        
        // Create voters configuration
        let voters: Vec<u64> = std::iter::once(id).chain(peers.iter().cloned()).collect();
        
        // Initialize storage with peers
        storage.initialize_with_conf_state((voters, vec![]));
        
        // Create raw node
        let raw_node = RawNode::new(&config, storage.clone(), &logger)
            .map_err(|e| Error::Internal(format!("Failed to create raw node: {:?}", e)))?;
        
        Ok(Self {
            id,
            raw_node,
            state_machine: Arc::new(Mutex::new(ControllerState::default())),
            msg_sender,
            cmd_receiver,
            logger,
        })
    }
    
    /// Run the Raft node
    pub async fn run(mut self) -> Result<()> {
        let mut ticker = tokio::time::interval(std::time::Duration::from_millis(100));
        
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    self.tick();
                }
                
                Some(cmd) = self.cmd_receiver.recv() => {
                    self.handle_command(cmd)?;
                }
            }
            
            // Process ready events
            if self.raw_node.has_ready() {
                let ready = self.raw_node.ready();
                self.process_ready(ready).await?;
            }
        }
    }
    
    /// Tick the Raft node
    fn tick(&mut self) {
        self.raw_node.tick();
    }
    
    /// Handle a command
    fn handle_command(&mut self, cmd: RaftCommand) -> Result<()> {
        match cmd {
            RaftCommand::Propose { proposal, callback } => {
                let data = serde_json::to_vec(&proposal)?;
                match self.raw_node.propose(vec![], data) {
                    Ok(()) => {
                        let _ = callback.send(Ok(()));
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
                let state = self.state_machine.lock().unwrap().clone();
                let _ = callback.send(state);
            }
            
            RaftCommand::GetLeader { callback } => {
                let leader = if self.raw_node.raft.state == StateRole::Leader {
                    Some(self.id)
                } else {
                    Some(self.raw_node.raft.leader_id)
                };
                let _ = callback.send(leader);
            }
        }
        
        Ok(())
    }
    
    /// Process ready events
    async fn process_ready(&mut self, mut ready: Ready) -> Result<()> {
        // Send messages
        for msg in ready.take_messages() {
            let raft_msg = RaftMessage {
                from: self.id,
                to: msg.to,
                message: msg.write_to_bytes()
                    .map_err(|e| Error::Internal(format!("Failed to serialize message: {:?}", e)))?,
            };
            let _ = self.msg_sender.send(raft_msg);
        }
        
        // Apply committed entries
        for entry in ready.take_committed_entries() {
            if entry.data.is_empty() {
                // Empty entry, skip
                continue;
            }
            
            // Apply to state machine
            match serde_json::from_slice::<Proposal>(&entry.data) {
                Ok(proposal) => {
                    self.apply_proposal(proposal)?;
                }
                Err(e) => {
                    slog::error!(self.logger, "Failed to deserialize proposal"; "error" => %e);
                }
            }
        }
        
        // Advance the node
        let mut light_rd = self.raw_node.advance(ready);
        
        // Apply any additional committed entries
        for entry in light_rd.take_committed_entries() {
            if entry.data.is_empty() {
                continue;
            }
            
            match serde_json::from_slice::<Proposal>(&entry.data) {
                Ok(proposal) => {
                    self.apply_proposal(proposal)?;
                }
                Err(e) => {
                    slog::error!(self.logger, "Failed to deserialize proposal"; "error" => %e);
                }
            }
        }
        
        // Advance with light ready
        self.raw_node.advance_apply();
        
        Ok(())
    }
    
    /// Apply a proposal to the state machine
    fn apply_proposal(&mut self, proposal: Proposal) -> Result<()> {
        let mut state = self.state_machine.lock().unwrap();
        
        match proposal {
            Proposal::CreateTopic(config) => {
                state.topics.insert(config.name.clone(), config);
            }
            Proposal::DeleteTopic(name) => {
                state.topics.remove(&name);
                state.partition_leaders.retain(|tp, _| tp.topic != name);
                state.isr_lists.retain(|tp, _| tp.topic != name);
            }
            Proposal::UpdatePartitionLeader { topic_partition, leader } => {
                state.partition_leaders.insert(topic_partition, leader);
            }
            Proposal::UpdateIsr { topic_partition, isr } => {
                state.isr_lists.insert(topic_partition, isr);
            }
            Proposal::RegisterBroker(broker) => {
                state.brokers.insert(broker.id, broker);
            }
            Proposal::UnregisterBroker(id) => {
                state.brokers.remove(&id);
            }
            Proposal::CreateConsumerGroup(group) => {
                state.consumer_groups.insert(group.id.clone(), group);
            }
            Proposal::UpdateConsumerGroup(group) => {
                state.consumer_groups.insert(group.id.clone(), group);
            }
            Proposal::DeleteConsumerGroup(id) => {
                state.consumer_groups.remove(&id);
            }
        }
        
        Ok(())
    }
    
    /// Handle incoming Raft message
    pub fn handle_message(&mut self, msg: RaftMessage) -> Result<()> {
        let m = raft::prelude::Message::parse_from_bytes(&msg.message)
            .map_err(|e| Error::Internal(format!("Failed to parse message: {:?}", e)))?;
        
        self.raw_node.step(m)
            .map_err(|e| Error::Internal(format!("Failed to step message: {:?}", e)))?;
        
        Ok(())
    }
}

/// Raft node handle for external interaction
#[derive(Clone)]
pub struct RaftHandle {
    cmd_sender: mpsc::Sender<RaftCommand>,
}

impl RaftHandle {
    /// Create a new Raft handle
    pub fn new(cmd_sender: mpsc::Sender<RaftCommand>) -> Self {
        Self { cmd_sender }
    }
    
    /// Propose a change to the Raft cluster
    pub async fn propose(&self, proposal: Proposal) -> Result<()> {
        let (callback, receiver) = oneshot::channel();
        self.cmd_sender.send(RaftCommand::Propose { proposal, callback }).await
            .map_err(|_| Error::Internal("Failed to send propose command".into()))?;
        
        receiver.await
            .map_err(|_| Error::Internal("Failed to receive propose response".into()))?
    }
    
    /// Get the current state
    pub async fn get_state(&self) -> Result<ControllerState> {
        let (callback, receiver) = oneshot::channel();
        self.cmd_sender.send(RaftCommand::GetState { callback }).await
            .map_err(|_| Error::Internal("Failed to send get state command".into()))?;
        
        Ok(receiver.await
            .map_err(|_| Error::Internal("Failed to receive state response".into()))?)
    }
    
    /// Get the current leader
    pub async fn get_leader(&self) -> Result<Option<NodeId>> {
        let (callback, receiver) = oneshot::channel();
        self.cmd_sender.send(RaftCommand::GetLeader { callback }).await
            .map_err(|_| Error::Internal("Failed to send get leader command".into()))?;
        
        Ok(receiver.await
            .map_err(|_| Error::Internal("Failed to receive leader response".into()))?)
    }
    
    /// Transfer leadership to another node
    pub async fn transfer_leader(&self, target: NodeId) -> Result<()> {
        let (callback, receiver) = oneshot::channel();
        self.cmd_sender.send(RaftCommand::TransferLeader { target, callback }).await
            .map_err(|_| Error::Internal("Failed to send transfer leader command".into()))?;
        
        receiver.await
            .map_err(|_| Error::Internal("Failed to receive transfer response".into()))?
    }
}