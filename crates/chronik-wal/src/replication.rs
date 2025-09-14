//! WAL Replication Layer (Scaffold)
//!
//! This module provides the foundation for WAL replication across multiple nodes.
//! Currently implemented as a scaffold with mock transports for future development.
//!
//! # Architecture
//! 
//! The replication layer follows a leader-follower model where:
//! - One node acts as the WAL Leader, accepting writes
//! - Multiple nodes act as WAL Followers, replicating writes
//! - Replication events flow: WAL Append → Broadcast → Ack
//!
//! # Future Implementation
//!
//! This scaffold will be extended with:
//! - Network transport (TCP/gRPC)
//! - Consensus protocol (Raft/Paxos)
//! - Snapshot synchronization
//! - Failure detection and recovery

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use async_trait::async_trait;
use parking_lot::RwLock;
use tokio::sync::{mpsc, broadcast};
use tracing::{info, warn, debug, error};
use serde::{Serialize, Deserialize};

use crate::{WalRecord};
use crate::manager::TopicPartition;
use crate::error::{Result, WalError};

/// WAL offset type
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct WalOffset(pub i64);

impl std::fmt::Display for WalOffset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Replica node identifier
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ReplicaId(pub u32);

impl std::fmt::Display for ReplicaId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "replica-{}", self.0)
    }
}

/// Replication event types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ReplicationEvent {
    /// New records appended to WAL
    Append {
        topic_partition: TopicPartition,
        records: Vec<WalRecord>,
        leader_offset: WalOffset,
        timestamp: i64,
    },
    /// Acknowledgment from follower
    Ack {
        replica_id: ReplicaId,
        topic_partition: TopicPartition,
        offset: WalOffset,
        timestamp: i64,
    },
    /// Heartbeat to maintain connection
    Heartbeat {
        replica_id: ReplicaId,
        timestamp: i64,
    },
    /// Request to sync from offset
    SyncRequest {
        replica_id: ReplicaId,
        topic_partition: TopicPartition,
        start_offset: WalOffset,
    },
    /// Response with sync data
    SyncResponse {
        topic_partition: TopicPartition,
        records: Vec<WalRecord>,
        end_offset: WalOffset,
    },
    /// Leader election event
    LeaderElection {
        new_leader: ReplicaId,
        term: u64,
    },
}

/// Transport layer abstraction for replication
#[async_trait]
pub trait ReplicationTransport: Send + Sync {
    /// Send event to specific replica
    async fn send(&self, replica_id: ReplicaId, event: ReplicationEvent) -> Result<()>;
    
    /// Broadcast event to all replicas
    async fn broadcast(&self, event: ReplicationEvent) -> Result<()>;
    
    /// Receive next event
    async fn receive(&mut self) -> Result<ReplicationEvent>;
    
    /// Check if transport is connected
    fn is_connected(&self) -> bool;
}

/// Mock transport for testing
pub struct MockTransport {
    sender: mpsc::UnboundedSender<ReplicationEvent>,
    receiver: mpsc::UnboundedReceiver<ReplicationEvent>,
    connected: Arc<RwLock<bool>>,
}

impl MockTransport {
    pub fn new() -> (Self, Self) {
        let (tx1, rx1) = mpsc::unbounded_channel();
        let (tx2, rx2) = mpsc::unbounded_channel();
        
        let transport1 = Self {
            sender: tx1,
            receiver: rx2,
            connected: Arc::new(RwLock::new(true)),
        };
        
        let transport2 = Self {
            sender: tx2,
            receiver: rx1,
            connected: Arc::clone(&transport1.connected),
        };
        
        (transport1, transport2)
    }
}

#[async_trait]
impl ReplicationTransport for MockTransport {
    async fn send(&self, _replica_id: ReplicaId, event: ReplicationEvent) -> Result<()> {
        self.sender.send(event)
            .map_err(|_| WalError::ReplicationError("Mock transport send failed".into()))
    }
    
    async fn broadcast(&self, event: ReplicationEvent) -> Result<()> {
        self.sender.send(event)
            .map_err(|_| WalError::ReplicationError("Mock transport broadcast failed".into()))
    }
    
    async fn receive(&mut self) -> Result<ReplicationEvent> {
        self.receiver.recv().await
            .ok_or_else(|| WalError::ReplicationError("Mock transport receive failed".into()))
    }
    
    fn is_connected(&self) -> bool {
        *self.connected.read()
    }
}

/// Replica set configuration
#[derive(Debug, Clone)]
pub struct ReplicaSetConfig {
    /// This node's replica ID
    pub replica_id: ReplicaId,
    /// All replica IDs in the set
    pub replicas: HashSet<ReplicaId>,
    /// Replication factor (minimum replicas for acknowledgment)
    pub replication_factor: usize,
    /// Timeout for replication acknowledgments
    pub ack_timeout: Duration,
    /// Heartbeat interval
    pub heartbeat_interval: Duration,
    /// Maximum lag before sync required
    pub max_lag_offset: u64,
}

impl Default for ReplicaSetConfig {
    fn default() -> Self {
        let mut replicas = HashSet::new();
        replicas.insert(ReplicaId(1));
        replicas.insert(ReplicaId(2));
        replicas.insert(ReplicaId(3));
        
        Self {
            replica_id: ReplicaId(1),
            replicas,
            replication_factor: 2,
            ack_timeout: Duration::from_secs(5),
            heartbeat_interval: Duration::from_secs(1),
            max_lag_offset: 1000,
        }
    }
}

/// Manages a set of replicas
pub struct ReplicaSet {
    config: ReplicaSetConfig,
    transports: Arc<RwLock<HashMap<ReplicaId, Arc<dyn ReplicationTransport>>>>,
    follower_offsets: Arc<RwLock<HashMap<(ReplicaId, TopicPartition), WalOffset>>>,
    last_heartbeat: Arc<RwLock<HashMap<ReplicaId, Instant>>>,
}

impl ReplicaSet {
    pub fn new(config: ReplicaSetConfig) -> Self {
        Self {
            config,
            transports: Arc::new(RwLock::new(HashMap::new())),
            follower_offsets: Arc::new(RwLock::new(HashMap::new())),
            last_heartbeat: Arc::new(RwLock::new(HashMap::new())),
        }
    }
    
    /// Add a replica with its transport
    pub fn add_replica(&self, replica_id: ReplicaId, transport: Arc<dyn ReplicationTransport>) {
        self.transports.write().insert(replica_id, transport);
        self.last_heartbeat.write().insert(replica_id, Instant::now());
    }
    
    /// Remove a replica
    pub fn remove_replica(&self, replica_id: ReplicaId) {
        self.transports.write().remove(&replica_id);
        self.last_heartbeat.write().remove(&replica_id);
    }
    
    /// Get healthy replicas (those with recent heartbeats)
    pub fn healthy_replicas(&self) -> Vec<ReplicaId> {
        let now = Instant::now();
        let timeout = self.config.heartbeat_interval * 3;
        
        self.last_heartbeat.read()
            .iter()
            .filter(|(_, last)| now.duration_since(**last) < timeout)
            .map(|(id, _)| *id)
            .collect()
    }
    
    /// Check if we have enough healthy replicas
    pub fn has_quorum(&self) -> bool {
        self.healthy_replicas().len() >= self.config.replication_factor
    }
    
    /// Update follower offset
    pub fn update_follower_offset(
        &self,
        replica_id: ReplicaId,
        topic_partition: TopicPartition,
        offset: WalOffset,
    ) {
        self.follower_offsets.write()
            .insert((replica_id, topic_partition), offset);
    }
    
    /// Get minimum replicated offset across all followers
    pub fn min_replicated_offset(&self, topic_partition: &TopicPartition) -> Option<WalOffset> {
        let offsets = self.follower_offsets.read();
        
        self.healthy_replicas()
            .iter()
            .filter_map(|replica_id| {
                offsets.get(&(*replica_id, topic_partition.clone()))
            })
            .min()
            .map(|offset| *offset)
    }
}

/// WAL Leader implementation
pub struct WALLeader {
    replica_set: Arc<ReplicaSet>,
    event_sender: broadcast::Sender<ReplicationEvent>,
    shutdown: Arc<RwLock<bool>>,
}

impl WALLeader {
    pub fn new(replica_set: Arc<ReplicaSet>) -> Self {
        let (event_sender, _) = broadcast::channel(1024);
        
        Self {
            replica_set,
            event_sender,
            shutdown: Arc::new(RwLock::new(false)),
        }
    }
    
    /// Replicate append event to followers
    pub async fn replicate_append(
        &self,
        topic_partition: TopicPartition,
        records: Vec<WalRecord>,
        leader_offset: WalOffset,
    ) -> Result<()> {
        let event = ReplicationEvent::Append {
            topic_partition: topic_partition.clone(),
            records,
            leader_offset,
            timestamp: chrono::Utc::now().timestamp_millis(),
        };
        
        // Broadcast to all followers
        let transports = self.replica_set.transports.read();
        let mut send_futures = vec![];
        
        for (replica_id, transport) in transports.iter() {
            if *replica_id != self.replica_set.config.replica_id {
                let event = event.clone();
                let transport = transport.as_ref();
                send_futures.push(async move {
                    transport.send(*replica_id, event).await
                });
            }
        }
        
        // Wait for sends to complete
        let results = futures::future::join_all(send_futures).await;
        
        // Check if enough succeeded for quorum
        let successful = results.iter().filter(|r| r.is_ok()).count();
        if successful < self.replica_set.config.replication_factor - 1 {
            return Err(WalError::ReplicationError(
                format!("Insufficient replicas: {} < {}", successful, self.replica_set.config.replication_factor - 1)
            ));
        }
        
        // Send event for monitoring
        let _ = self.event_sender.send(event);
        
        Ok(())
    }
    
    /// Wait for replication acknowledgments
    pub async fn wait_for_acks(
        &self,
        topic_partition: &TopicPartition,
        target_offset: WalOffset,
        timeout: Duration,
    ) -> Result<()> {
        let start = Instant::now();
        
        loop {
            if start.elapsed() > timeout {
                return Err(WalError::ReplicationError("Ack timeout".into()));
            }
            
            if let Some(min_offset) = self.replica_set.min_replicated_offset(topic_partition) {
                if min_offset >= target_offset {
                    return Ok(());
                }
            }
            
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    
    /// Start leader background tasks
    pub async fn start(&self) -> Result<()> {
        info!("Starting WAL leader for replica {}", self.replica_set.config.replica_id);
        
        // Start heartbeat sender
        let replica_set = Arc::clone(&self.replica_set);
        let shutdown = Arc::clone(&self.shutdown);
        
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(replica_set.config.heartbeat_interval);
            
            while !*shutdown.read() {
                interval.tick().await;
                
                let event = ReplicationEvent::Heartbeat {
                    replica_id: replica_set.config.replica_id,
                    timestamp: chrono::Utc::now().timestamp_millis(),
                };
                
                // Collect transport ids to avoid holding lock across await
                let transport_ids: Vec<_> = {
                    let transports = replica_set.transports.read();
                    transports.keys().cloned().collect()
                };
                
                for transport_id in transport_ids {
                    let transport_option = {
                        replica_set.transports.read().get(&transport_id).cloned()
                    };
                    if let Some(transport) = transport_option {
                        let _ = transport.broadcast(event.clone()).await;
                    }
                }
            }
        });
        
        Ok(())
    }
    
    /// Shutdown the leader
    pub async fn shutdown(&self) {
        *self.shutdown.write() = true;
    }
}

/// WAL Follower implementation
pub struct WALFollower {
    replica_id: ReplicaId,
    replica_set: Arc<ReplicaSet>,
    transport: Arc<Mutex<dyn ReplicationTransport>>,
    offsets: Arc<RwLock<HashMap<TopicPartition, WalOffset>>>,
    shutdown: Arc<RwLock<bool>>,
}

impl WALFollower {
    pub fn new(
        replica_id: ReplicaId,
        replica_set: Arc<ReplicaSet>,
        transport: Arc<Mutex<dyn ReplicationTransport>>,
    ) -> Self {
        Self {
            replica_id,
            replica_set,
            transport,
            offsets: Arc::new(RwLock::new(HashMap::new())),
            shutdown: Arc::new(RwLock::new(false)),
        }
    }
    
    /// Process replication event
    async fn process_event(&self, event: ReplicationEvent) -> Result<()> {
        match event {
            ReplicationEvent::Append { topic_partition, records, leader_offset, .. } => {
                // TODO: Apply records to local WAL
                debug!(
                    "Follower {} received {} records for {:?} at offset {}",
                    self.replica_id,
                    records.len(),
                    topic_partition,
                    leader_offset
                );
                
                // Update local offset
                self.offsets.write().insert(topic_partition.clone(), leader_offset);
                
                // Send acknowledgment
                let ack = ReplicationEvent::Ack {
                    replica_id: self.replica_id,
                    topic_partition,
                    offset: leader_offset,
                    timestamp: chrono::Utc::now().timestamp_millis(),
                };
                
                self.transport.lock().unwrap().broadcast(ack).await?;
            }
            ReplicationEvent::Heartbeat { replica_id, .. } => {
                debug!("Follower {} received heartbeat from {}", self.replica_id, replica_id);
                self.replica_set.last_heartbeat.write().insert(replica_id, Instant::now());
            }
            ReplicationEvent::SyncRequest { replica_id, topic_partition, start_offset } => {
                // TODO: Implement sync response
                debug!(
                    "Follower {} received sync request from {} for {:?} from offset {}",
                    self.replica_id, replica_id, topic_partition, start_offset
                );
            }
            _ => {
                debug!("Follower {} received event: {:?}", self.replica_id, event);
            }
        }
        
        Ok(())
    }
    
    /// Start follower background tasks
    pub async fn start(self) -> Result<()> {
        info!("Starting WAL follower {}", self.replica_id);
        
        while !*self.shutdown.read() {
            match self.transport.lock().unwrap().receive().await {
                Ok(event) => {
                    if let Err(e) = self.process_event(event).await {
                        warn!("Follower {} error processing event: {:?}", self.replica_id, e);
                    }
                }
                Err(e) => {
                    error!("Follower {} transport error: {:?}", self.replica_id, e);
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
        
        Ok(())
    }
    
    /// Request sync from leader
    pub async fn request_sync(
        &self,
        topic_partition: TopicPartition,
        start_offset: WalOffset,
    ) -> Result<()> {
        let event = ReplicationEvent::SyncRequest {
            replica_id: self.replica_id,
            topic_partition,
            start_offset,
        };
        
        self.transport.lock().unwrap().broadcast(event).await
    }
    
    /// Get current offset for topic partition
    pub fn get_offset(&self, topic_partition: &TopicPartition) -> Option<WalOffset> {
        self.offsets.read().get(topic_partition).copied()
    }
    
    /// Shutdown the follower
    pub fn shutdown(&self) {
        *self.shutdown.write() = true;
    }
}

/// WAL-to-WAL syncing logic (stub for future implementation)
pub struct WALSyncManager {
    leader: Option<Arc<WALLeader>>,
    followers: Vec<Arc<WALFollower>>,
    replica_set: Arc<ReplicaSet>,
}

impl WALSyncManager {
    pub fn new(replica_set: Arc<ReplicaSet>) -> Self {
        Self {
            leader: None,
            followers: Vec::new(),
            replica_set,
        }
    }
    
    /// Initialize as leader
    pub fn become_leader(&mut self) -> Arc<WALLeader> {
        let leader = Arc::new(WALLeader::new(Arc::clone(&self.replica_set)));
        self.leader = Some(Arc::clone(&leader));
        leader
    }
    
    /// Initialize as follower
    pub fn become_follower(
        &mut self,
        replica_id: ReplicaId,
        transport: Arc<Mutex<dyn ReplicationTransport>>,
    ) -> Arc<WALFollower> {
        let follower = Arc::new(WALFollower::new(
            replica_id,
            Arc::clone(&self.replica_set),
            transport,
        ));
        self.followers.push(Arc::clone(&follower));
        follower
    }
    
    /// Perform full WAL sync between leader and follower
    pub async fn full_sync(
        &self,
        _source_replica: ReplicaId,
        _target_replica: ReplicaId,
        _topic_partition: TopicPartition,
    ) -> Result<()> {
        // TODO: Implement full WAL sync
        // 1. Get checkpoint from target
        // 2. Stream segments from source starting at checkpoint
        // 3. Apply segments to target
        // 4. Update target checkpoint
        
        warn!("Full WAL sync not yet implemented");
        Ok(())
    }
    
    /// Perform incremental sync
    pub async fn incremental_sync(
        &self,
        _source_replica: ReplicaId,
        _target_replica: ReplicaId,
        _topic_partition: TopicPartition,
        _from_offset: WalOffset,
    ) -> Result<()> {
        // TODO: Implement incremental sync
        // 1. Read records from source starting at offset
        // 2. Send records to target
        // 3. Wait for acknowledgment
        // 4. Update sync metadata
        
        warn!("Incremental WAL sync not yet implemented");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[tokio::test]
    async fn test_mock_transport() {
        let (transport1, mut transport2) = MockTransport::new();
        
        let event = ReplicationEvent::Heartbeat {
            replica_id: ReplicaId(1),
            timestamp: 12345,
        };
        
        transport1.send(ReplicaId(2), event.clone()).await.unwrap();
        
        let received = transport2.receive().await.unwrap();
        match received {
            ReplicationEvent::Heartbeat { replica_id, timestamp } => {
                assert_eq!(replica_id, ReplicaId(1));
                assert_eq!(timestamp, 12345);
            }
            _ => panic!("Wrong event type"),
        }
    }
    
    #[tokio::test]
    async fn test_replica_set_quorum() {
        let config = ReplicaSetConfig::default();
        let replica_set = ReplicaSet::new(config);
        
        // Add mock transports
        let (transport1, _) = MockTransport::new();
        let (transport2, _) = MockTransport::new();
        
        replica_set.add_replica(ReplicaId(2), Arc::new(transport1));
        replica_set.add_replica(ReplicaId(3), Arc::new(transport2));
        
        assert!(replica_set.has_quorum());
        
        // Remove one replica
        replica_set.remove_replica(ReplicaId(3));
        
        // Should still have quorum with 2 replicas (self + 1)
        assert!(replica_set.has_quorum());
    }
    
    #[tokio::test]
    async fn test_leader_follower_interaction() {
        let config = ReplicaSetConfig::default();
        let replica_set = Arc::new(ReplicaSet::new(config));
        
        // Create transport pair
        let (leader_transport, follower_transport) = MockTransport::new();
        
        // Setup leader
        let leader = WALLeader::new(Arc::clone(&replica_set));
        replica_set.add_replica(ReplicaId(2), Arc::new(leader_transport));

        // Setup follower
        let follower = WALFollower::new(
            ReplicaId(2),
            Arc::clone(&replica_set),
            Arc::new(Mutex::new(follower_transport)),
        );
        
        // Test sync request
        let tp = TopicPartition {
            topic: "test".to_string(),
            partition: 0,
        };
        
        follower.request_sync(tp.clone(), WalOffset(100)).await.unwrap();
        
        // Verify follower can track offsets
        assert_eq!(follower.get_offset(&tp), None);
    }
}