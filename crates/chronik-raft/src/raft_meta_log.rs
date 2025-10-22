//! Raft-backed metadata store for distributed consensus.
//!
//! This module implements the MetadataStore trait using Raft consensus to replicate
//! metadata operations across a cluster. All write operations are proposed to Raft,
//! committed via consensus, and then applied to local state.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use async_trait::async_trait;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot, Mutex};
use tracing::{debug, error, info};
use uuid::Uuid;

// Import from chronik-common
use chronik_common::metadata::{
    MetadataStore, MetadataError,
    TopicConfig, TopicMetadata, SegmentMetadata, BrokerMetadata, BrokerStatus,
    PartitionAssignment, ConsumerGroupMetadata, ConsumerOffset,
};

// Use local crate types
use crate::{PartitionReplica, RaftConfig, RaftLogStorage, StateMachine, RaftEntry, RaftError};
use bytes::Bytes;

// Result type for metadata operations
type Result<T> = std::result::Result<T, MetadataError>;

/// Metadata operations that are replicated via Raft
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MetadataOp {
    // Topic operations
    CreateTopic {
        name: String,
        config: TopicConfig,
    },
    UpdateTopic {
        name: String,
        config: TopicConfig,
    },
    DeleteTopic {
        name: String,
    },

    // Segment operations
    PersistSegment {
        metadata: SegmentMetadata,
    },
    DeleteSegment {
        topic: String,
        segment_id: String,
    },

    // Broker operations
    RegisterBroker {
        metadata: BrokerMetadata,
    },
    UpdateBrokerStatus {
        broker_id: i32,
        status: BrokerStatus,
    },

    // Partition assignment operations
    AssignPartition {
        assignment: PartitionAssignment,
    },

    // Consumer group operations
    CreateConsumerGroup {
        metadata: ConsumerGroupMetadata,
    },
    UpdateConsumerGroup {
        metadata: ConsumerGroupMetadata,
    },
    CommitOffset {
        offset: ConsumerOffset,
    },
    CommitTransactionalOffsets {
        transactional_id: String,
        producer_id: i64,
        producer_epoch: i16,
        group_id: String,
        offsets: Vec<(String, u32, i64, Option<String>)>,
    },

    // Transaction operations
    BeginTransaction {
        transactional_id: String,
        producer_id: i64,
        producer_epoch: i16,
        timeout_ms: i32,
    },
    AddPartitionsToTransaction {
        transactional_id: String,
        producer_id: i64,
        producer_epoch: i16,
        partitions: Vec<(String, u32)>,
    },
    AddOffsetsToTransaction {
        transactional_id: String,
        producer_id: i64,
        producer_epoch: i16,
        group_id: String,
    },
    PrepareCommitTransaction {
        transactional_id: String,
        producer_id: i64,
        producer_epoch: i16,
    },
    CommitTransaction {
        transactional_id: String,
        producer_id: i64,
        producer_epoch: i16,
    },
    AbortTransaction {
        transactional_id: String,
        producer_id: i64,
        producer_epoch: i16,
    },
    FenceProducer {
        transactional_id: String,
        old_producer_id: i64,
        old_producer_epoch: i16,
        new_producer_id: i64,
        new_producer_epoch: i16,
    },

    // Partition offset operations
    UpdatePartitionOffset {
        topic: String,
        partition: u32,
        high_watermark: i64,
        log_start_offset: i64,
    },

    // Batch operations
    CreateTopicWithAssignments {
        topic_name: String,
        config: TopicConfig,
        assignments: Vec<PartitionAssignment>,
        offsets: Vec<(u32, i64, i64)>,
    },
}

/// Metadata state that can be reconstructed from Raft log
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MetadataState {
    /// Topics by name
    pub topics: HashMap<String, TopicMetadata>,
    /// Segments by topic and segment ID
    pub segments: HashMap<(String, String), SegmentMetadata>,
    /// Brokers by ID
    pub brokers: HashMap<i32, BrokerMetadata>,
    /// Partition assignments by topic and partition
    pub partition_assignments: HashMap<(String, u32), PartitionAssignment>,
    /// Consumer groups by group ID
    pub consumer_groups: HashMap<String, ConsumerGroupMetadata>,
    /// Consumer offsets by (group_id, topic, partition)
    pub consumer_offsets: HashMap<(String, String, u32), ConsumerOffset>,
    /// Partition offsets by (topic, partition) - (high_watermark, log_start_offset)
    pub partition_offsets: HashMap<(String, u32), (i64, i64)>,
}

impl MetadataState {
    /// Apply a metadata operation to the state
    pub fn apply_operation(&mut self, op: &MetadataOp) -> std::result::Result<(), String> {
        match op {
            MetadataOp::CreateTopic { name, config } => {
                let metadata = TopicMetadata {
                    id: Uuid::new_v4(),
                    name: name.clone(),
                    config: config.clone(),
                    created_at: chrono::Utc::now(),
                    updated_at: chrono::Utc::now(),
                };
                self.topics.insert(name.clone(), metadata);
            }

            MetadataOp::UpdateTopic { name, config } => {
                if let Some(topic) = self.topics.get_mut(name) {
                    topic.config = config.clone();
                    topic.updated_at = chrono::Utc::now();
                }
            }

            MetadataOp::DeleteTopic { name } => {
                self.topics.remove(name);
                self.segments.retain(|(topic, _), _| topic != name);
                self.partition_assignments.retain(|(topic, _), _| topic != name);
                self.partition_offsets.retain(|(topic, _), _| topic != name);
            }

            MetadataOp::PersistSegment { metadata } => {
                let key = (metadata.topic.clone(), metadata.segment_id.clone());
                self.segments.insert(key, metadata.clone());
            }

            MetadataOp::DeleteSegment { topic, segment_id } => {
                let key = (topic.clone(), segment_id.clone());
                self.segments.remove(&key);
            }

            MetadataOp::RegisterBroker { metadata } => {
                self.brokers.insert(metadata.broker_id, metadata.clone());
            }

            MetadataOp::UpdateBrokerStatus { broker_id, status } => {
                if let Some(broker) = self.brokers.get_mut(broker_id) {
                    broker.status = status.clone();
                    broker.updated_at = chrono::Utc::now();
                }
            }

            MetadataOp::AssignPartition { assignment } => {
                let key = (assignment.topic.clone(), assignment.partition);
                self.partition_assignments.insert(key, assignment.clone());
            }

            MetadataOp::CreateConsumerGroup { metadata } => {
                self.consumer_groups.insert(metadata.group_id.clone(), metadata.clone());
            }

            MetadataOp::UpdateConsumerGroup { metadata } => {
                self.consumer_groups.insert(metadata.group_id.clone(), metadata.clone());
            }

            MetadataOp::CommitOffset { offset } => {
                let key = (offset.group_id.clone(), offset.topic.clone(), offset.partition);
                self.consumer_offsets.insert(key, offset.clone());
            }

            MetadataOp::CommitTransactionalOffsets { group_id, offsets, .. } => {
                for (topic, partition, offset, metadata) in offsets {
                    let key = (group_id.clone(), topic.clone(), *partition);
                    let consumer_offset = ConsumerOffset {
                        group_id: group_id.clone(),
                        topic: topic.clone(),
                        partition: *partition,
                        offset: *offset,
                        metadata: metadata.clone(),
                        commit_timestamp: chrono::Utc::now(),
                    };
                    self.consumer_offsets.insert(key, consumer_offset);
                }
            }

            MetadataOp::UpdatePartitionOffset { topic, partition, high_watermark, log_start_offset } => {
                let key = (topic.clone(), *partition);
                self.partition_offsets.insert(key, (*high_watermark, *log_start_offset));
            }

            MetadataOp::CreateTopicWithAssignments { topic_name, config, assignments, offsets } => {
                // Create topic
                let metadata = TopicMetadata {
                    id: Uuid::new_v4(),
                    name: topic_name.clone(),
                    config: config.clone(),
                    created_at: chrono::Utc::now(),
                    updated_at: chrono::Utc::now(),
                };
                self.topics.insert(topic_name.clone(), metadata);

                // Add assignments
                for assignment in assignments {
                    let key = (assignment.topic.clone(), assignment.partition);
                    self.partition_assignments.insert(key, assignment.clone());
                }

                // Set offsets
                for (partition, high_watermark, log_start_offset) in offsets {
                    let key = (topic_name.clone(), *partition);
                    self.partition_offsets.insert(key, (*high_watermark, *log_start_offset));
                }
            }

            // Transaction operations don't directly modify state
            // (they would be tracked in a separate transaction state map)
            MetadataOp::BeginTransaction { .. } => {}
            MetadataOp::AddPartitionsToTransaction { .. } => {}
            MetadataOp::AddOffsetsToTransaction { .. } => {}
            MetadataOp::PrepareCommitTransaction { .. } => {}
            MetadataOp::CommitTransaction { .. } => {}
            MetadataOp::AbortTransaction { .. } => {}
            MetadataOp::FenceProducer { .. } => {}
        }

        Ok(())
    }
}

/// Result of a propose operation with response channel
type ProposeResult<T> = oneshot::Sender<Result<T>>;

/// State machine that applies metadata operations to local state
pub struct MetadataStateMachine {
    local_state: Arc<RwLock<MetadataState>>,
    applied_index: Arc<AtomicU64>,
    on_topic_created: Option<TopicCreatedCallback>,
}

impl MetadataStateMachine {
    pub fn new(local_state: Arc<RwLock<MetadataState>>, applied_index: Arc<AtomicU64>) -> Self {
        Self {
            local_state,
            applied_index,
            on_topic_created: None,
        }
    }

    /// Create with optional topic creation callback
    pub fn with_callback(
        local_state: Arc<RwLock<MetadataState>>,
        applied_index: Arc<AtomicU64>,
        callback: Option<TopicCreatedCallback>,
    ) -> Self {
        Self {
            local_state,
            applied_index,
            on_topic_created: callback,
        }
    }

    /// Set callback to be invoked when CreateTopicWithAssignments is applied
    pub fn set_topic_created_callback(&mut self, callback: TopicCreatedCallback) {
        self.on_topic_created = Some(callback);
    }
}

#[async_trait]
impl StateMachine for MetadataStateMachine {
    async fn apply(&mut self, entry: &RaftEntry) -> std::result::Result<Bytes, RaftError> {
        // Skip empty entries (configuration changes)
        if entry.data.is_empty() {
            self.applied_index.store(entry.index, Ordering::SeqCst);
            return Ok(Bytes::new());
        }

        // Deserialize operation
        match bincode::deserialize::<MetadataOp>(&entry.data) {
            Ok(op) => {
                debug!("Applying metadata operation at index {}: {:?}", entry.index, op);

                // Check if this is a CreateTopicWithAssignments operation (before applying)
                let is_topic_creation = matches!(op, MetadataOp::CreateTopicWithAssignments { .. });
                let topic_name = if let MetadataOp::CreateTopicWithAssignments { ref topic_name, .. } = op {
                    Some(topic_name.clone())
                } else {
                    None
                };

                // Apply the operation
                {
                    let mut state = self.local_state.write();
                    if let Err(e) = state.apply_operation(&op) {
                        error!("Failed to apply operation: {}", e);
                    }
                }

                // CRITICAL: Fire callback AFTER successful application (on ALL nodes)
                if is_topic_creation {
                    if let Some(ref topic_name) = topic_name {
                        if let Some(ref callback) = self.on_topic_created {
                            // Get the topic metadata from state (after application)
                            let state = self.local_state.read();
                            if let Some(topic_meta) = state.topics.get(topic_name) {
                                info!("MetadataStateMachine: Firing topic creation callback for '{}'", topic_name);
                                callback(topic_name, topic_meta);
                            } else {
                                error!("MetadataStateMachine: Topic '{}' not found after CreateTopicWithAssignments", topic_name);
                            }
                        }
                    }
                }
            }
            Err(e) => {
                error!("Failed to deserialize operation: {}", e);
            }
        }

        // Update applied index
        self.applied_index.store(entry.index, Ordering::SeqCst);

        Ok(Bytes::new())
    }

    async fn snapshot(&self, last_index: u64, last_term: u64) -> std::result::Result<crate::SnapshotData, RaftError> {
        // Serialize current state
        let state = self.local_state.read();
        let data = bincode::serialize(&*state)
            .map_err(|e| RaftError::SerializationError(e.to_string()))?;

        Ok(crate::SnapshotData {
            last_index,
            last_term,
            conf_state: vec![],
            data,
        })
    }

    async fn restore(&mut self, snapshot: &crate::SnapshotData) -> std::result::Result<(), RaftError> {
        // Deserialize and replace state
        let state: MetadataState = bincode::deserialize(&snapshot.data)
            .map_err(|e| RaftError::SerializationError(e.to_string()))?;

        *self.local_state.write() = state;
        self.applied_index.store(snapshot.last_index, Ordering::SeqCst);

        Ok(())
    }

    fn last_applied(&self) -> u64 {
        self.applied_index.load(Ordering::SeqCst)
    }
}

/// Raft-backed metadata store with linearizable writes
/// Callback type for topic creation events
pub type TopicCreatedCallback = Arc<dyn Fn(&str, &TopicMetadata) + Send + Sync>;

pub struct RaftMetaLog {
    /// Node ID in the Raft cluster
    node_id: u64,

    /// Raft replica for the __meta partition
    raft_replica: Arc<PartitionReplica>,

    /// Current materialized metadata state
    local_state: Arc<RwLock<MetadataState>>,

    /// Last applied Raft index (shared with background processor)
    applied_index: Arc<AtomicU64>,

    /// Background processor handle
    processor_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,

    /// Channel for propose requests
    propose_tx: mpsc::UnboundedSender<(Vec<u8>, oneshot::Sender<Result<u64>>)>,

    /// Optional callback fired after a topic is successfully created (for Raft replica creation)
    on_topic_created: Option<TopicCreatedCallback>,
}

impl RaftMetaLog {
    /// Create a new RaftMetaLog instance using an existing PartitionReplica
    ///
    /// This is the preferred constructor when integrating with RaftReplicaManager,
    /// as it reuses the existing Raft network infrastructure.
    ///
    /// # Arguments
    /// * `node_id` - This node's ID in the Raft cluster
    /// * `raft_replica` - Pre-configured PartitionReplica for the __meta partition
    /// * `raft_client` - RaftClient for sending messages to peers (required for cluster mode)
    pub async fn from_replica(
        node_id: u64,
        raft_replica: Arc<PartitionReplica>,
        raft_client: Option<Arc<crate::RaftClient>>,
        local_state: Arc<RwLock<MetadataState>>,
        applied_index: Arc<AtomicU64>,
    ) -> Result<Self> {
        info!("Creating RaftMetaLog for node {} using existing replica with shared state", node_id);

        if raft_client.is_none() {
            error!("RaftClient not provided - RaftMetaLog will not be able to send messages to peers!");
        }

        // Create propose channel
        let (propose_tx, propose_rx) = mpsc::unbounded_channel();

        let store = Self {
            node_id,
            raft_replica: raft_replica.clone(),
            local_state: local_state.clone(),
            applied_index: applied_index.clone(),
            processor_handle: Mutex::new(None),
            propose_tx,
            on_topic_created: None,  // Callback will be set via set_topic_created_callback()
        };

        // Start background processor
        let processor = BackgroundProcessor {
            raft_replica: raft_replica.clone(),
            local_state: local_state.clone(),
            applied_index: applied_index.clone(),
            propose_rx: Mutex::new(propose_rx),
            raft_client,
        };

        let handle = tokio::spawn(async move {
            processor.run().await;
        });

        *store.processor_handle.lock().await = Some(handle);

        info!("RaftMetaLog created for node {} from existing replica with shared state", node_id);

        Ok(store)
    }

    /// Create a new RaftMetaLog instance (creates its own Raft replica)
    ///
    /// NOTE: This is mainly for testing. In production, use `from_replica()` instead
    /// to reuse the Raft network infrastructure from RaftReplicaManager.
    ///
    /// # Arguments
    /// * `node_id` - This node's ID in the Raft cluster
    /// * `config` - Raft configuration
    /// * `log_storage` - Log storage backend
    /// * `peers` - List of peer node IDs
    /// * `data_dir` - Data directory for snapshots
    pub async fn new(
        node_id: u64,
        config: RaftConfig,
        log_storage: Arc<dyn RaftLogStorage>,
        peers: Vec<u64>,
        _data_dir: impl AsRef<Path>,
    ) -> Result<Self> {
        info!("Creating RaftMetaLog for node {} with peers {:?}", node_id, peers);

        // Create state machine for metadata partition
        let state_machine = Arc::new(tokio::sync::RwLock::new(crate::MemoryStateMachine::new()));

        // Create Raft replica for metadata partition
        let raft_replica = PartitionReplica::new(
            "__meta".to_string(),
            0,
            config,
            log_storage,
            state_machine,
            peers,
        ).map_err(|e| MetadataError::StorageError(format!("Failed to create Raft replica: {}", e)))?;

        let raft_replica = Arc::new(raft_replica);

        // Create propose channel
        let (propose_tx, propose_rx) = mpsc::unbounded_channel();

        let applied_index = Arc::new(AtomicU64::new(0));

        let store = Self {
            node_id,
            raft_replica: raft_replica.clone(),
            local_state: Arc::new(RwLock::new(MetadataState::default())),
            applied_index: applied_index.clone(),
            processor_handle: Mutex::new(None),
            propose_tx,
            on_topic_created: None,  // Callback will be set via set_topic_created_callback()
        };

        // Start background processor (no RaftClient for standalone test mode)
        let processor = BackgroundProcessor {
            raft_replica: raft_replica.clone(),
            local_state: store.local_state.clone(),
            applied_index,
            propose_rx: Mutex::new(propose_rx),
            raft_client: None,  // Test mode - no external message sending
        };

        let handle = tokio::spawn(async move {
            processor.run().await;
        });

        *store.processor_handle.lock().await = Some(handle);

        info!("RaftMetaLog created for node {} (test mode, no RaftClient)", node_id);

        Ok(store)
    }

    /// Set callback to be invoked after successful topic creation
    /// This is used to trigger Raft replica creation for new topics
    pub fn set_topic_created_callback(&mut self, callback: TopicCreatedCallback) {
        self.on_topic_created = Some(callback);
    }

    /// Propose an operation to Raft and wait for commit with timeout
    async fn propose_and_wait<T, F>(&self, op: MetadataOp, extract: F) -> Result<T>
    where
        F: FnOnce(&MetadataState) -> Result<T>,
    {
        // Serialize operation
        let data = bincode::serialize(&op)
            .map_err(|e| MetadataError::SerializationError(format!("Failed to serialize operation: {}", e)))?;

        // Create response channel
        let (tx, rx) = oneshot::channel();

        // Send to background processor
        self.propose_tx.send((data, tx))
            .map_err(|_| MetadataError::StorageError("Failed to send propose request".to_string()))?;

        // Wait for commit with timeout (5 seconds)
        // This prevents indefinite blocking when cluster doesn't have quorum
        let timeout = tokio::time::Duration::from_secs(5);
        let index = match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Ok(idx))) => idx,
            Ok(Ok(Err(e))) => return Err(e),
            Ok(Err(_)) => return Err(MetadataError::StorageError("Propose response channel closed".to_string())),
            Err(_) => {
                return Err(MetadataError::StorageError(
                    "Raft propose timeout - cluster may not have quorum".to_string()
                ));
            }
        };

        debug!("Operation committed at Raft index {}", index);

        // Wait for application (applied_index >= index) with timeout
        let apply_timeout = tokio::time::Duration::from_millis(500);
        let start = tokio::time::Instant::now();
        while start.elapsed() < apply_timeout {
            if self.applied_index.load(Ordering::SeqCst) >= index {
                break;
            }
            tokio::task::yield_now().await;
        }

        // Extract result from local state
        let state = self.local_state.read();
        extract(&state)
    }

    /// Check if this node is the Raft leader
    pub fn is_leader(&self) -> bool {
        self.raft_replica.is_leader()
    }

    /// Get current Raft term
    pub fn term(&self) -> u64 {
        self.raft_replica.term()
    }

    /// Get current commit index
    pub fn commit_index(&self) -> u64 {
        self.raft_replica.commit_index()
    }

    /// Get current applied index
    pub fn applied_index(&self) -> u64 {
        self.applied_index.load(Ordering::SeqCst)
    }
}

/// Background processor that drives Raft and applies committed entries
struct BackgroundProcessor {
    raft_replica: Arc<PartitionReplica>,
    local_state: Arc<RwLock<MetadataState>>,
    applied_index: Arc<AtomicU64>,
    propose_rx: Mutex<mpsc::UnboundedReceiver<(Vec<u8>, oneshot::Sender<Result<u64>>)>>,
    raft_client: Option<Arc<crate::RaftClient>>,
}

impl BackgroundProcessor {
    async fn run(self) {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(10));

        loop {
            interval.tick().await;

            // NOTE: We do NOT call tick() or process_ready() here because
            // RaftReplicaManager already has a background loop doing that
            // for the __meta replica. Calling them from two places causes
            // race conditions and prevents proposals from being processed.

            // Process propose requests (forward to leader if needed)
            self.process_propose_requests().await;

            // Apply state machine (reads committed entries and applies to local state)
            if let Err(e) = self.apply_state_machine().await {
                error!("Failed to apply state machine: {}", e);
            }
        }
    }

    async fn process_propose_requests(&self) {
        let mut rx = self.propose_rx.lock().await;

        while let Ok((data, response_tx)) = rx.try_recv() {
            // Try to propose locally first
            let result = self.raft_replica.propose(data.clone()).await;

            // Check if it's a "Not leader" error and forward to leader if so
            let final_result = match result {
                Err(ref e) if e.to_string().contains("Not leader") => {
                    // Extract leader ID from error message
                    if let Some(leader_id) = self.extract_leader_id(&e.to_string()) {
                        debug!("Not leader, forwarding proposal to leader {}", leader_id);

                        // Forward to leader via RaftClient
                        if let Some(ref client) = self.raft_client {
                            match client.propose_metadata_to_leader(leader_id, data).await {
                                Ok(index) => {
                                    debug!("Proposal forwarded successfully to leader {}, index {}", leader_id, index);
                                    Ok(index)
                                }
                                Err(e) => {
                                    error!("Failed to forward proposal to leader {}: {}", leader_id, e);
                                    Err(MetadataError::StorageError(format!("Forward to leader failed: {}", e)))
                                }
                            }
                        } else {
                            error!("Cannot forward to leader: RaftClient not available");
                            Err(MetadataError::StorageError("Cannot forward to leader: RaftClient not available".to_string()))
                        }
                    } else {
                        error!("Could not extract leader ID from error: {}", e);
                        result.map_err(|e| MetadataError::StorageError(format!("Raft propose failed: {}", e)))
                    }
                }
                other => other.map_err(|e| MetadataError::StorageError(format!("Raft propose failed: {}", e))),
            };

            let _ = response_tx.send(final_result);
        }
    }

    /// Extract leader ID from "Not leader (current leader: X)" error message
    fn extract_leader_id(&self, error_msg: &str) -> Option<u64> {
        // Error format: "Not leader (current leader: 3)"
        if let Some(start) = error_msg.find("current leader: ") {
            let id_start = start + "current leader: ".len();
            if let Some(end) = error_msg[id_start..].find(')') {
                if let Ok(id) = error_msg[id_start..id_start + end].parse::<u64>() {
                    return Some(id);
                }
            }
        }
        None
    }

    async fn apply_state_machine(&self) -> std::result::Result<(), Box<dyn std::error::Error>> {
        // NO-OP: MetadataStateMachine (driven by RaftReplicaManager) applies entries
        // We don't need to do anything here
        Ok(())
    }

    async fn process_ready(&self) -> std::result::Result<(), Box<dyn std::error::Error>> {
        let (messages, committed_entries) = self.raft_replica.ready().await?;

        // Send messages to peers via RaftClient
        if !messages.is_empty() {
            if let Some(ref client) = self.raft_client {
                debug!("Sending {} Raft messages to peers for __meta-0", messages.len());
                for msg in messages {
                    let to = msg.to;
                    let client = client.clone();
                    tokio::spawn(async move {
                        if let Err(e) = client.send_message("__meta", 0, to, msg).await {
                            error!("Failed to send __meta message to peer {}: {}", to, e);
                        }
                    });
                }
            } else {
                error!("Cannot send {} messages: RaftClient not available (cluster mode not properly initialized)", messages.len());
            }
        }

        // Apply committed entries to state machine
        if !committed_entries.is_empty() {
            debug!("Applying {} committed entries", committed_entries.len());

            let mut state = self.local_state.write();

            for entry in &committed_entries {
                // Skip empty entries (configuration changes)
                if entry.data.is_empty() {
                    continue;
                }

                // Deserialize operation
                match bincode::deserialize::<MetadataOp>(&entry.data) {
                    Ok(op) => {
                        if let Err(e) = state.apply_operation(&op) {
                            error!("Failed to apply operation: {}", e);
                        }
                    }
                    Err(e) => {
                        error!("Failed to deserialize operation: {}", e);
                    }
                }

                // Update applied index
                self.applied_index.store(entry.index, Ordering::SeqCst);
                self.raft_replica.set_applied_index(entry.index);
            }

            debug!("Applied entries up to index {}", self.applied_index.load(Ordering::SeqCst));
        }

        Ok(())
    }
}

#[async_trait]
impl MetadataStore for RaftMetaLog {
    async fn create_topic(&self, name: &str, config: TopicConfig) -> Result<TopicMetadata> {
        // Check if exists (read-only check)
        if self.local_state.read().topics.contains_key(name) {
            return Err(MetadataError::AlreadyExists(format!("Topic {} already exists", name)));
        }

        let op = MetadataOp::CreateTopic {
            name: name.to_string(),
            config,
        };

        self.propose_and_wait(op, |state| {
            state.topics.get(name)
                .cloned()
                .ok_or_else(|| MetadataError::StorageError("Topic not found after creation".to_string()))
        }).await
    }

    async fn get_topic(&self, name: &str) -> Result<Option<TopicMetadata>> {
        // Read from local state (eventually consistent)
        Ok(self.local_state.read().topics.get(name).cloned())
    }

    async fn list_topics(&self) -> Result<Vec<TopicMetadata>> {
        Ok(self.local_state.read().topics.values().cloned().collect())
    }

    async fn update_topic(&self, name: &str, config: TopicConfig) -> Result<TopicMetadata> {
        let op = MetadataOp::UpdateTopic {
            name: name.to_string(),
            config,
        };

        self.propose_and_wait(op, |state| {
            state.topics.get(name)
                .cloned()
                .ok_or_else(|| MetadataError::NotFound(format!("Topic {} not found", name)))
        }).await
    }

    async fn delete_topic(&self, name: &str) -> Result<()> {
        let op = MetadataOp::DeleteTopic {
            name: name.to_string(),
        };

        self.propose_and_wait(op, |_| Ok(())).await
    }

    async fn persist_segment_metadata(&self, metadata: SegmentMetadata) -> Result<()> {
        let op = MetadataOp::PersistSegment { metadata };
        self.propose_and_wait(op, |_| Ok(())).await
    }

    async fn get_segment_metadata(&self, topic: &str, segment_id: &str) -> Result<Option<SegmentMetadata>> {
        let key = (topic.to_string(), segment_id.to_string());
        Ok(self.local_state.read().segments.get(&key).cloned())
    }

    async fn list_segments(&self, topic: &str, partition: Option<u32>) -> Result<Vec<SegmentMetadata>> {
        let state = self.local_state.read();
        let segments: Vec<SegmentMetadata> = state.segments
            .values()
            .filter(|s| s.topic == topic && partition.map_or(true, |p| s.partition == p))
            .cloned()
            .collect();
        Ok(segments)
    }

    async fn delete_segment(&self, topic: &str, segment_id: &str) -> Result<()> {
        let op = MetadataOp::DeleteSegment {
            topic: topic.to_string(),
            segment_id: segment_id.to_string(),
        };
        self.propose_and_wait(op, |_| Ok(())).await
    }

    async fn register_broker(&self, metadata: BrokerMetadata) -> Result<()> {
        let op = MetadataOp::RegisterBroker { metadata };
        self.propose_and_wait(op, |_| Ok(())).await
    }

    async fn get_broker(&self, broker_id: i32) -> Result<Option<BrokerMetadata>> {
        Ok(self.local_state.read().brokers.get(&broker_id).cloned())
    }

    async fn list_brokers(&self) -> Result<Vec<BrokerMetadata>> {
        Ok(self.local_state.read().brokers.values().cloned().collect())
    }

    async fn update_broker_status(&self, broker_id: i32, status: BrokerStatus) -> Result<()> {
        let op = MetadataOp::UpdateBrokerStatus { broker_id, status };
        self.propose_and_wait(op, |_| Ok(())).await
    }

    async fn assign_partition(&self, assignment: PartitionAssignment) -> Result<()> {
        let op = MetadataOp::AssignPartition { assignment };
        self.propose_and_wait(op, |_| Ok(())).await
    }

    async fn get_partition_assignments(&self, topic: &str) -> Result<Vec<PartitionAssignment>> {
        let state = self.local_state.read();
        let assignments: Vec<PartitionAssignment> = state.partition_assignments
            .values()
            .filter(|a| a.topic == topic)
            .cloned()
            .collect();
        Ok(assignments)
    }

    async fn get_partition_leader(&self, topic: &str, partition: u32) -> Result<Option<i32>> {
        let state = self.local_state.read();
        let key = (topic.to_string(), partition);
        Ok(state.partition_assignments.get(&key).and_then(|a| {
            if a.is_leader {
                Some(a.broker_id)
            } else {
                None
            }
        }))
    }

    async fn get_partition_replicas(&self, topic: &str, partition: u32) -> Result<Option<Vec<i32>>> {
        let state = self.local_state.read();

        // Get all assignments for this partition
        let replicas: Vec<i32> = state.partition_assignments.values()
            .filter(|a| a.topic == topic && a.partition == partition)
            .map(|a| a.broker_id)
            .collect();

        if replicas.is_empty() {
            Ok(None)
        } else {
            Ok(Some(replicas))
        }
    }

    async fn create_consumer_group(&self, metadata: ConsumerGroupMetadata) -> Result<()> {
        let op = MetadataOp::CreateConsumerGroup { metadata };
        self.propose_and_wait(op, |_| Ok(())).await
    }

    async fn get_consumer_group(&self, group_id: &str) -> Result<Option<ConsumerGroupMetadata>> {
        Ok(self.local_state.read().consumer_groups.get(group_id).cloned())
    }

    async fn update_consumer_group(&self, metadata: ConsumerGroupMetadata) -> Result<()> {
        let op = MetadataOp::UpdateConsumerGroup { metadata };
        self.propose_and_wait(op, |_| Ok(())).await
    }

    async fn commit_offset(&self, offset: ConsumerOffset) -> Result<()> {
        let op = MetadataOp::CommitOffset { offset };
        self.propose_and_wait(op, |_| Ok(())).await
    }

    async fn get_consumer_offset(&self, group_id: &str, topic: &str, partition: u32) -> Result<Option<ConsumerOffset>> {
        let key = (group_id.to_string(), topic.to_string(), partition);
        Ok(self.local_state.read().consumer_offsets.get(&key).cloned())
    }

    async fn commit_transactional_offsets(
        &self,
        transactional_id: String,
        producer_id: i64,
        producer_epoch: i16,
        group_id: String,
        offsets: Vec<(String, u32, i64, Option<String>)>,
    ) -> Result<()> {
        let op = MetadataOp::CommitTransactionalOffsets {
            transactional_id,
            producer_id,
            producer_epoch,
            group_id,
            offsets,
        };
        self.propose_and_wait(op, |_| Ok(())).await
    }

    async fn begin_transaction(
        &self,
        transactional_id: String,
        producer_id: i64,
        producer_epoch: i16,
        timeout_ms: i32,
    ) -> Result<()> {
        let op = MetadataOp::BeginTransaction {
            transactional_id,
            producer_id,
            producer_epoch,
            timeout_ms,
        };
        self.propose_and_wait(op, |_| Ok(())).await
    }

    async fn add_partitions_to_transaction(
        &self,
        transactional_id: String,
        producer_id: i64,
        producer_epoch: i16,
        partitions: Vec<(String, u32)>,
    ) -> Result<()> {
        let op = MetadataOp::AddPartitionsToTransaction {
            transactional_id,
            producer_id,
            producer_epoch,
            partitions,
        };
        self.propose_and_wait(op, |_| Ok(())).await
    }

    async fn add_offsets_to_transaction(
        &self,
        transactional_id: String,
        producer_id: i64,
        producer_epoch: i16,
        group_id: String,
    ) -> Result<()> {
        let op = MetadataOp::AddOffsetsToTransaction {
            transactional_id,
            producer_id,
            producer_epoch,
            group_id,
        };
        self.propose_and_wait(op, |_| Ok(())).await
    }

    async fn prepare_commit_transaction(
        &self,
        transactional_id: String,
        producer_id: i64,
        producer_epoch: i16,
    ) -> Result<()> {
        let op = MetadataOp::PrepareCommitTransaction {
            transactional_id,
            producer_id,
            producer_epoch,
        };
        self.propose_and_wait(op, |_| Ok(())).await
    }

    async fn commit_transaction(
        &self,
        transactional_id: String,
        producer_id: i64,
        producer_epoch: i16,
    ) -> Result<()> {
        let op = MetadataOp::CommitTransaction {
            transactional_id,
            producer_id,
            producer_epoch,
        };
        self.propose_and_wait(op, |_| Ok(())).await
    }

    async fn abort_transaction(
        &self,
        transactional_id: String,
        producer_id: i64,
        producer_epoch: i16,
    ) -> Result<()> {
        let op = MetadataOp::AbortTransaction {
            transactional_id,
            producer_id,
            producer_epoch,
        };
        self.propose_and_wait(op, |_| Ok(())).await
    }

    async fn fence_producer(
        &self,
        transactional_id: String,
        old_producer_id: i64,
        old_producer_epoch: i16,
        new_producer_id: i64,
        new_producer_epoch: i16,
    ) -> Result<()> {
        let op = MetadataOp::FenceProducer {
            transactional_id,
            old_producer_id,
            old_producer_epoch,
            new_producer_id,
            new_producer_epoch,
        };
        self.propose_and_wait(op, |_| Ok(())).await
    }

    async fn update_partition_offset(&self, topic: &str, partition: u32, high_watermark: i64, log_start_offset: i64) -> Result<()> {
        let op = MetadataOp::UpdatePartitionOffset {
            topic: topic.to_string(),
            partition,
            high_watermark,
            log_start_offset,
        };
        self.propose_and_wait(op, |_| Ok(())).await
    }

    async fn get_partition_offset(&self, topic: &str, partition: u32) -> Result<Option<(i64, i64)>> {
        let key = (topic.to_string(), partition);
        Ok(self.local_state.read().partition_offsets.get(&key).copied())
    }

    async fn init_system_state(&self) -> Result<()> {
        // System topics should be created via create_topic on leader
        Ok(())
    }

    async fn create_topic_with_assignments(
        &self,
        topic_name: &str,
        config: TopicConfig,
        assignments: Vec<PartitionAssignment>,
        offsets: Vec<(u32, i64, i64)>,
    ) -> Result<TopicMetadata> {
        let op = MetadataOp::CreateTopicWithAssignments {
            topic_name: topic_name.to_string(),
            config,
            assignments,
            offsets,
        };

        // Propose and wait for commit
        // NOTE: Callback will fire in MetadataStateMachine.apply() on ALL nodes
        self.propose_and_wait(op, |state| {
            state.topics.get(topic_name)
                .cloned()
                .ok_or_else(|| MetadataError::StorageError("Topic not found after creation".to_string()))
        }).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemoryLogStorage;

    #[tokio::test]
    async fn test_create_raft_metalog() {
        let config = RaftConfig {
            node_id: 1,
            ..Default::default()
        };

        let storage = Arc::new(MemoryLogStorage::new());

        let result = RaftMetaLog::new(
            1,
            config,
            storage,
            vec![],
            "/tmp/test_raft_metalog",
        ).await;

        assert!(result.is_ok());
    }
}
