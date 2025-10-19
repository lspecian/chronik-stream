//! WAL-based metadata store implementation using event sourcing.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, error};
use uuid::Uuid;

use super::{
    MetadataStore, MetadataError, Result,
    TopicConfig, TopicMetadata, SegmentMetadata, BrokerMetadata, BrokerStatus,
    PartitionAssignment, ConsumerGroupMetadata, ConsumerOffset,
};

use super::events::{
    MetadataEvent, MetadataEventPayload, EventLog, EventApplicator,
    ConfigScope, SnapshotMetadata as EventSnapshotMetadata,
};

/// Internal topic name for metadata storage
pub const METADATA_TOPIC: &str = "__meta";

/// Metadata state that can be reconstructed from events
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
    /// Configuration values
    pub configs: HashMap<String, String>,
    /// Last applied event offset
    pub last_event_offset: u64,
}

impl EventApplicator for MetadataState {
    fn apply_event(&mut self, event: &MetadataEvent) -> std::result::Result<(), String> {
        match &event.payload {
            MetadataEventPayload::TopicCreated { name, config } => {
                let metadata = TopicMetadata {
                    id: Uuid::new_v4(),
                    name: name.clone(),
                    config: config.clone(),
                    created_at: event.timestamp,
                    updated_at: event.timestamp,
                };
                self.topics.insert(name.clone(), metadata);
            }

            MetadataEventPayload::TopicUpdated { name, config } => {
                if let Some(topic) = self.topics.get_mut(name) {
                    topic.config = config.clone();
                    topic.updated_at = event.timestamp;
                }
            }

            MetadataEventPayload::TopicDeleted { name } => {
                self.topics.remove(name);
                // Also remove related segments
                self.segments.retain(|(topic, _), _| topic != name);
                self.partition_assignments.retain(|(topic, _), _| topic != name);
                self.partition_offsets.retain(|(topic, _), _| topic != name);
            }

            MetadataEventPayload::SegmentCreated { metadata } => {
                let key = (metadata.topic.clone(), metadata.segment_id.clone());
                self.segments.insert(key, metadata.clone());
            }

            MetadataEventPayload::SegmentDeleted { topic, segment_id, .. } => {
                let key = (topic.clone(), segment_id.clone());
                self.segments.remove(&key);
            }

            MetadataEventPayload::BrokerRegistered { metadata } => {
                self.brokers.insert(metadata.broker_id, metadata.clone());
            }

            MetadataEventPayload::BrokerStatusChanged { broker_id, status } => {
                if let Some(broker) = self.brokers.get_mut(broker_id) {
                    broker.status = status.clone();
                    broker.updated_at = event.timestamp;
                }
            }

            MetadataEventPayload::BrokerRemoved { broker_id } => {
                self.brokers.remove(broker_id);
            }

            MetadataEventPayload::PartitionAssigned { assignment } => {
                let key = (assignment.topic.clone(), assignment.partition);
                self.partition_assignments.insert(key, assignment.clone());
            }

            MetadataEventPayload::PartitionReassigned { topic, partition, to_broker, .. } => {
                let key = (topic.clone(), *partition);
                if let Some(assignment) = self.partition_assignments.get_mut(&key) {
                    assignment.broker_id = *to_broker;
                }
            }

            MetadataEventPayload::ConsumerGroupCreated { metadata } => {
                self.consumer_groups.insert(metadata.group_id.clone(), metadata.clone());
            }

            MetadataEventPayload::ConsumerGroupUpdated { metadata } => {
                self.consumer_groups.insert(metadata.group_id.clone(), metadata.clone());
            }

            MetadataEventPayload::ConsumerGroupDeleted { group_id } => {
                self.consumer_groups.remove(group_id);
                // Also remove related offsets
                self.consumer_offsets.retain(|(g, _, _), _| g != group_id);
            }

            MetadataEventPayload::OffsetCommitted { offset } => {
                let key = (offset.group_id.clone(), offset.topic.clone(), offset.partition);
                self.consumer_offsets.insert(key, offset.clone());
            }

            MetadataEventPayload::OffsetsReset { group_id, topic, partition, new_offset } => {
                let key = (group_id.clone(), topic.clone(), *partition);
                if let Some(offset) = self.consumer_offsets.get_mut(&key) {
                    offset.offset = *new_offset;
                }
            }

            MetadataEventPayload::TransactionalOffsetCommit {
                transactional_id: _,
                producer_id: _,
                producer_epoch: _,
                group_id,
                offsets
            } => {
                // Atomically commit all offsets in the transaction
                for txn_offset in offsets {
                    let key = (group_id.clone(), txn_offset.topic.clone(), txn_offset.partition);
                    let consumer_offset = ConsumerOffset {
                        group_id: group_id.clone(),
                        topic: txn_offset.topic.clone(),
                        partition: txn_offset.partition,
                        offset: txn_offset.offset,
                        metadata: txn_offset.metadata.clone(),
                        commit_timestamp: event.timestamp,
                    };
                    self.consumer_offsets.insert(key, consumer_offset);
                }
            }

            MetadataEventPayload::ConfigUpdated { scope, key, value } => {
                let config_key = match scope {
                    ConfigScope::Cluster => format!("cluster:{}", key),
                    ConfigScope::Topic(topic) => format!("topic:{}:{}", topic, key),
                    ConfigScope::Broker(broker_id) => format!("broker:{}:{}", broker_id, key),
                };
                self.configs.insert(config_key, value.clone());
            }

            MetadataEventPayload::ConfigDeleted { scope, key } => {
                let config_key = match scope {
                    ConfigScope::Cluster => format!("cluster:{}", key),
                    ConfigScope::Topic(topic) => format!("topic:{}:{}", topic, key),
                    ConfigScope::Broker(broker_id) => format!("broker:{}:{}", broker_id, key),
                };
                self.configs.remove(&config_key);
            }

            _ => {
                // Ignore snapshot and migration events for state building
            }
        }

        Ok(())
    }

    fn version(&self) -> u64 {
        self.last_event_offset
    }
}

/// Interface for WAL operations needed by MetaLogStore
#[async_trait]
pub trait MetaLogWalInterface: Send + Sync {
    /// Append an event to the metadata topic
    async fn append_metadata_event(&self, event: &MetadataEvent) -> Result<u64>;

    /// Read all events from the metadata topic
    async fn read_metadata_events(&self, from_offset: u64) -> Result<Vec<MetadataEvent>>;

    /// Get the latest offset in the metadata topic
    async fn get_latest_offset(&self) -> Result<u64>;
}

/// Mock WAL implementation for testing
pub struct MockWal {
    events: Arc<RwLock<Vec<MetadataEvent>>>,
}

impl MockWal {
    pub fn new() -> Self {
        Self {
            events: Arc::new(RwLock::new(Vec::new())),
        }
    }
}

#[async_trait]
impl MetaLogWalInterface for MockWal {
    async fn append_metadata_event(&self, event: &MetadataEvent) -> Result<u64> {
        let mut events = self.events.write();
        events.push(event.clone());
        Ok(events.len() as u64 - 1)
    }

    async fn read_metadata_events(&self, from_offset: u64) -> Result<Vec<MetadataEvent>> {
        let events = self.events.read();
        Ok(events.iter().skip(from_offset as usize).cloned().collect())
    }

    async fn get_latest_offset(&self) -> Result<u64> {
        Ok(self.events.read().len() as u64)
    }
}

/// WAL-based metadata store using event sourcing
pub struct ChronikMetaLogStore {
    /// WAL interface for event storage
    wal: Arc<dyn MetaLogWalInterface>,
    /// Current materialized state
    state: Arc<RwLock<MetadataState>>,
    /// Event log for quick access
    event_log: Arc<RwLock<EventLog>>,
    /// Path for snapshots
    snapshot_path: PathBuf,
    /// Whether to auto-snapshot
    auto_snapshot: bool,
    /// Events between snapshots
    snapshot_interval: usize,
}

impl ChronikMetaLogStore {
    /// Create a new ChronikMetaLogStore
    pub async fn new(
        wal: Arc<dyn MetaLogWalInterface>,
        data_dir: impl AsRef<Path>,
    ) -> Result<Self> {
        let snapshot_path = data_dir.as_ref().join("metadata_snapshots");
        tokio::fs::create_dir_all(&snapshot_path).await
            .map_err(|e| MetadataError::StorageError(format!("Failed to create snapshot dir: {}", e)))?;

        let store = Self {
            wal,
            state: Arc::new(RwLock::new(MetadataState::default())),
            event_log: Arc::new(RwLock::new(EventLog::new())),
            snapshot_path,
            auto_snapshot: true,
            snapshot_interval: 1000, // Snapshot every 1000 events
        };

        // Recover state from WAL
        store.recover().await?;

        Ok(store)
    }

    /// Recover state from WAL and snapshots
    pub async fn recover(&self) -> Result<()> {
        info!("Starting metadata recovery from WAL");

        // Try to load latest snapshot
        let snapshot_offset = self.load_latest_snapshot().await?;

        // Read events from WAL starting from snapshot offset
        let events = self.wal.read_metadata_events(snapshot_offset).await?;
        info!("Replaying {} events from WAL", events.len());

        // Get the latest offset before locking
        let latest_offset = self.wal.get_latest_offset().await?;

        // Apply events to rebuild state
        {
            let mut state = self.state.write();
            let mut event_log = self.event_log.write();

            for event in events {
                state.apply_event(&event).map_err(|e| MetadataError::StorageError(e))?;
                event_log.append(event);
            }

            state.last_event_offset = latest_offset;
            info!("Metadata recovery complete. State version: {}", state.last_event_offset);
        }

        Ok(())
    }

    /// Load the latest snapshot if available
    async fn load_latest_snapshot(&self) -> Result<u64> {
        let snapshot_file = self.snapshot_path.join("latest.snapshot");

        if !snapshot_file.exists() {
            debug!("No snapshot found, starting from beginning");
            return Ok(0);
        }

        let data = tokio::fs::read(&snapshot_file).await
            .map_err(|e| MetadataError::StorageError(format!("Failed to read snapshot: {}", e)))?;

        let snapshot: MetadataStateSnapshot = bincode::deserialize(&data)
            .map_err(|e| MetadataError::SerializationError(format!("Failed to deserialize snapshot: {}", e)))?;

        let mut state = self.state.write();
        *state = snapshot.state;

        info!("Loaded snapshot at offset {}", snapshot.offset);

        Ok(snapshot.offset)
    }

    /// Save a snapshot of the current state
    async fn save_snapshot(&self) -> Result<()> {
        let snapshot = {
            let state = self.state.read();
            MetadataStateSnapshot {
                offset: state.last_event_offset,
                timestamp: Utc::now(),
                state: state.clone(),
            }
        };

        let data = bincode::serialize(&snapshot)
            .map_err(|e| MetadataError::SerializationError(format!("Failed to serialize snapshot: {}", e)))?;

        let snapshot_file = self.snapshot_path.join("latest.snapshot");
        let temp_file = self.snapshot_path.join("latest.snapshot.tmp");

        tokio::fs::write(&temp_file, data).await
            .map_err(|e| MetadataError::StorageError(format!("Failed to write snapshot: {}", e)))?;

        tokio::fs::rename(&temp_file, &snapshot_file).await
            .map_err(|e| MetadataError::StorageError(format!("Failed to rename snapshot: {}", e)))?;

        // Also write a snapshot event to WAL
        let snapshot_event = MetadataEvent::new(MetadataEventPayload::SnapshotCreated {
            snapshot_id: Uuid::new_v4().to_string(),
            offset: snapshot.offset,
            metadata: EventSnapshotMetadata {
                path: snapshot_file.to_string_lossy().to_string(),
                size: 0, // Will be updated
                event_count: self.event_log.read().len() as u64,
                compression: "bincode".to_string(),
                checksum: "".to_string(), // Could add checksumming
            },
        });

        self.wal.append_metadata_event(&snapshot_event).await?;

        info!("Saved snapshot at offset {}", snapshot.offset);

        Ok(())
    }

    /// Append an event and update state
    async fn append_event(&self, payload: MetadataEventPayload) -> Result<()> {
        let event = MetadataEvent::new(payload);

        // Write to WAL first
        let offset = self.wal.append_metadata_event(&event).await?;

        // Update in-memory state
        let mut state = self.state.write();
        state.apply_event(&event).map_err(|e| MetadataError::StorageError(e))?;
        state.last_event_offset = offset;

        // Add to event log
        self.event_log.write().append(event);

        // Check if we need to snapshot
        if self.auto_snapshot && self.event_log.read().len() % self.snapshot_interval == 0 {
            // Run snapshot in background
            let store = self.clone();
            tokio::spawn(async move {
                if let Err(e) = store.save_snapshot().await {
                    error!("Failed to save snapshot: {}", e);
                }
            });
        }

        Ok(())
    }
}

impl Clone for ChronikMetaLogStore {
    fn clone(&self) -> Self {
        Self {
            wal: self.wal.clone(),
            state: self.state.clone(),
            event_log: self.event_log.clone(),
            snapshot_path: self.snapshot_path.clone(),
            auto_snapshot: self.auto_snapshot,
            snapshot_interval: self.snapshot_interval,
        }
    }
}

/// Snapshot of metadata state
#[derive(Debug, Clone, Serialize, Deserialize)]
struct MetadataStateSnapshot {
    /// Offset at which this snapshot was taken
    offset: u64,
    /// Timestamp of the snapshot
    timestamp: DateTime<Utc>,
    /// The actual state
    state: MetadataState,
}

#[async_trait]
impl MetadataStore for ChronikMetaLogStore {
    async fn create_topic(&self, name: &str, config: TopicConfig) -> Result<TopicMetadata> {
        // Check if topic exists
        if self.state.read().topics.contains_key(name) {
            return Err(MetadataError::AlreadyExists(format!("Topic {} already exists", name)));
        }

        // Create and append event
        let payload = MetadataEventPayload::TopicCreated {
            name: name.to_string(),
            config: config.clone(),
        };
        self.append_event(payload).await?;

        // Return the created metadata
        self.state.read()
            .topics
            .get(name)
            .cloned()
            .ok_or_else(|| MetadataError::StorageError("Failed to create topic".to_string()))
    }

    async fn get_topic(&self, name: &str) -> Result<Option<TopicMetadata>> {
        Ok(self.state.read().topics.get(name).cloned())
    }

    async fn list_topics(&self) -> Result<Vec<TopicMetadata>> {
        Ok(self.state.read().topics.values().cloned().collect())
    }

    async fn update_topic(&self, name: &str, config: TopicConfig) -> Result<TopicMetadata> {
        // Check if topic exists
        if !self.state.read().topics.contains_key(name) {
            return Err(MetadataError::NotFound(format!("Topic {} not found", name)));
        }

        // Create and append event
        let payload = MetadataEventPayload::TopicUpdated {
            name: name.to_string(),
            config: config.clone(),
        };
        self.append_event(payload).await?;

        // Return the updated metadata
        self.state.read()
            .topics
            .get(name)
            .cloned()
            .ok_or_else(|| MetadataError::StorageError("Failed to update topic".to_string()))
    }

    async fn delete_topic(&self, name: &str) -> Result<()> {
        // Check if topic exists
        if !self.state.read().topics.contains_key(name) {
            return Err(MetadataError::NotFound(format!("Topic {} not found", name)));
        }

        // Create and append event
        let payload = MetadataEventPayload::TopicDeleted {
            name: name.to_string(),
        };
        self.append_event(payload).await?;

        Ok(())
    }

    async fn persist_segment_metadata(&self, metadata: SegmentMetadata) -> Result<()> {
        let payload = MetadataEventPayload::SegmentCreated {
            metadata: metadata.clone(),
        };
        self.append_event(payload).await
    }

    async fn get_segment_metadata(&self, topic: &str, segment_id: &str) -> Result<Option<SegmentMetadata>> {
        let key = (topic.to_string(), segment_id.to_string());
        Ok(self.state.read().segments.get(&key).cloned())
    }

    async fn list_segments(&self, topic: &str, partition: Option<u32>) -> Result<Vec<SegmentMetadata>> {
        let segments: Vec<SegmentMetadata> = self.state.read()
            .segments
            .values()
            .filter(|s| {
                s.topic == topic && partition.map_or(true, |p| s.partition == p)
            })
            .cloned()
            .collect();
        Ok(segments)
    }

    async fn delete_segment(&self, topic: &str, segment_id: &str) -> Result<()> {
        let key = (topic.to_string(), segment_id.to_string());
        if !self.state.read().segments.contains_key(&key) {
            return Err(MetadataError::NotFound(format!("Segment {} not found", segment_id)));
        }

        let payload = MetadataEventPayload::SegmentDeleted {
            topic: topic.to_string(),
            partition: 0, // We'd need to look this up
            segment_id: segment_id.to_string(),
        };
        self.append_event(payload).await
    }

    async fn register_broker(&self, metadata: BrokerMetadata) -> Result<()> {
        let payload = MetadataEventPayload::BrokerRegistered {
            metadata: metadata.clone(),
        };
        self.append_event(payload).await
    }

    async fn get_broker(&self, broker_id: i32) -> Result<Option<BrokerMetadata>> {
        Ok(self.state.read().brokers.get(&broker_id).cloned())
    }

    async fn list_brokers(&self) -> Result<Vec<BrokerMetadata>> {
        Ok(self.state.read().brokers.values().cloned().collect())
    }

    async fn update_broker_status(&self, broker_id: i32, status: BrokerStatus) -> Result<()> {
        if !self.state.read().brokers.contains_key(&broker_id) {
            return Err(MetadataError::NotFound(format!("Broker {} not found", broker_id)));
        }

        let payload = MetadataEventPayload::BrokerStatusChanged {
            broker_id,
            status,
        };
        self.append_event(payload).await
    }

    async fn assign_partition(&self, assignment: PartitionAssignment) -> Result<()> {
        let payload = MetadataEventPayload::PartitionAssigned {
            assignment: assignment.clone(),
        };
        self.append_event(payload).await
    }

    async fn get_partition_assignments(&self, topic: &str) -> Result<Vec<PartitionAssignment>> {
        let assignments: Vec<PartitionAssignment> = self.state.read()
            .partition_assignments
            .values()
            .filter(|a| a.topic == topic)
            .cloned()
            .collect();
        Ok(assignments)
    }

    async fn get_partition_leader(&self, topic: &str, partition: u32) -> Result<Option<i32>> {
        let state = self.state.read();
        // The key in partition_assignments is (topic, partition) tuple
        // There may be multiple assignments per partition (one leader + replicas)
        // Find the one marked as leader
        Ok(state.partition_assignments
            .values()
            .find(|a| a.topic == topic && a.partition == partition && a.is_leader)
            .map(|a| a.broker_id))
    }

    async fn get_partition_replicas(&self, topic: &str, partition: u32) -> Result<Option<Vec<i32>>> {
        let state = self.state.read();

        // Get all assignments for this partition (leader + replicas)
        let replicas: Vec<i32> = state.partition_assignments
            .values()
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
        let payload = MetadataEventPayload::ConsumerGroupCreated {
            metadata: metadata.clone(),
        };
        self.append_event(payload).await
    }

    async fn get_consumer_group(&self, group_id: &str) -> Result<Option<ConsumerGroupMetadata>> {
        Ok(self.state.read().consumer_groups.get(group_id).cloned())
    }

    async fn update_consumer_group(&self, metadata: ConsumerGroupMetadata) -> Result<()> {
        let payload = MetadataEventPayload::ConsumerGroupUpdated {
            metadata: metadata.clone(),
        };
        self.append_event(payload).await
    }

    async fn commit_offset(&self, offset: ConsumerOffset) -> Result<()> {
        let payload = MetadataEventPayload::OffsetCommitted {
            offset: offset.clone(),
        };
        self.append_event(payload).await
    }

    async fn get_consumer_offset(&self, group_id: &str, topic: &str, partition: u32) -> Result<Option<ConsumerOffset>> {
        let key = (group_id.to_string(), topic.to_string(), partition);
        Ok(self.state.read().consumer_offsets.get(&key).cloned())
    }

    async fn commit_transactional_offsets(
        &self,
        transactional_id: String,
        producer_id: i64,
        producer_epoch: i16,
        group_id: String,
        offsets: Vec<(String, u32, i64, Option<String>)>,
    ) -> Result<()> {
        use super::events::TransactionalOffset;

        let transactional_offsets: Vec<TransactionalOffset> = offsets
            .into_iter()
            .map(|(topic, partition, offset, metadata)| TransactionalOffset {
                topic,
                partition,
                offset,
                metadata,
            })
            .collect();

        let payload = MetadataEventPayload::TransactionalOffsetCommit {
            transactional_id,
            producer_id,
            producer_epoch,
            group_id,
            offsets: transactional_offsets,
        };

        self.append_event(payload).await
    }

    async fn begin_transaction(
        &self,
        transactional_id: String,
        producer_id: i64,
        producer_epoch: i16,
        timeout_ms: i32,
    ) -> Result<()> {
        let payload = MetadataEventPayload::BeginTransaction {
            transactional_id,
            producer_id,
            producer_epoch,
            transaction_timeout_ms: timeout_ms,
        };

        self.append_event(payload).await
    }

    async fn add_partitions_to_transaction(
        &self,
        transactional_id: String,
        producer_id: i64,
        producer_epoch: i16,
        partitions: Vec<(String, u32)>,
    ) -> Result<()> {
        use super::events::TransactionPartition;

        let transaction_partitions: Vec<TransactionPartition> = partitions
            .into_iter()
            .map(|(topic, partition)| TransactionPartition { topic, partition })
            .collect();

        let payload = MetadataEventPayload::AddPartitionsToTransaction {
            transactional_id,
            producer_id,
            producer_epoch,
            partitions: transaction_partitions,
        };

        self.append_event(payload).await
    }

    async fn add_offsets_to_transaction(
        &self,
        transactional_id: String,
        producer_id: i64,
        producer_epoch: i16,
        group_id: String,
    ) -> Result<()> {
        let payload = MetadataEventPayload::AddOffsetsToTransaction {
            transactional_id,
            producer_id,
            producer_epoch,
            group_id,
        };

        self.append_event(payload).await
    }

    async fn prepare_commit_transaction(
        &self,
        transactional_id: String,
        producer_id: i64,
        producer_epoch: i16,
    ) -> Result<()> {
        let payload = MetadataEventPayload::PrepareCommit {
            transactional_id,
            producer_id,
            producer_epoch,
        };

        self.append_event(payload).await
    }

    async fn commit_transaction(
        &self,
        transactional_id: String,
        producer_id: i64,
        producer_epoch: i16,
    ) -> Result<()> {
        // Get transaction state from memory to get partitions and offsets
        let state = self.state.read();

        // For now, just record the commit event
        // In a full implementation, we'd track partitions and offsets from the transaction
        let payload = MetadataEventPayload::CommitTransaction {
            transactional_id,
            producer_id,
            producer_epoch,
            committed_partitions: Vec::new(), // Would be populated from transaction state
            committed_offsets: Vec::new(),    // Would be populated from transaction state
        };

        drop(state);
        self.append_event(payload).await
    }

    async fn abort_transaction(
        &self,
        transactional_id: String,
        producer_id: i64,
        producer_epoch: i16,
    ) -> Result<()> {
        // Get transaction state from memory to get partitions
        let state = self.state.read();

        // For now, just record the abort event
        // In a full implementation, we'd track partitions from the transaction
        let payload = MetadataEventPayload::AbortTransaction {
            transactional_id,
            producer_id,
            producer_epoch,
            aborted_partitions: Vec::new(), // Would be populated from transaction state
        };

        drop(state);
        self.append_event(payload).await
    }

    async fn fence_producer(
        &self,
        transactional_id: String,
        old_producer_id: i64,
        old_producer_epoch: i16,
        new_producer_id: i64,
        new_producer_epoch: i16,
    ) -> Result<()> {
        let payload = MetadataEventPayload::ProducerFenced {
            transactional_id,
            old_producer_id,
            old_producer_epoch,
            new_producer_id,
            new_producer_epoch,
        };

        self.append_event(payload).await
    }

    async fn update_partition_offset(&self, topic: &str, partition: u32, high_watermark: i64, log_start_offset: i64) -> Result<()> {
        let mut state = self.state.write();
        let key = (topic.to_string(), partition);
        state.partition_offsets.insert(key, (high_watermark, log_start_offset));
        Ok(())
    }

    async fn get_partition_offset(&self, topic: &str, partition: u32) -> Result<Option<(i64, i64)>> {
        let key = (topic.to_string(), partition);
        Ok(self.state.read().partition_offsets.get(&key).copied())
    }

    async fn init_system_state(&self) -> Result<()> {
        // Create system topics if they don't exist
        let system_topics = vec![
            ("__consumer_offsets", 50, 3),
            ("__transaction_state", 50, 3),
            (METADATA_TOPIC, 1, 1), // Our metadata topic
        ];

        for (name, partitions, replication_factor) in system_topics {
            if self.get_topic(name).await?.is_none() {
                let config = TopicConfig {
                    partition_count: partitions,
                    replication_factor,
                    retention_ms: None, // Never delete metadata
                    segment_bytes: 100 * 1024 * 1024, // 100MB
                    config: HashMap::new(),
                };
                self.create_topic(name, config).await?;
            }
        }

        Ok(())
    }

    async fn create_topic_with_assignments(
        &self,
        topic_name: &str,
        config: TopicConfig,
        assignments: Vec<PartitionAssignment>,
        offsets: Vec<(u32, i64, i64)>,
    ) -> Result<TopicMetadata> {
        // Create topic
        let metadata = self.create_topic(topic_name, config).await?;

        // Add assignments
        for assignment in assignments {
            self.assign_partition(assignment).await?;
        }

        // Set initial offsets
        for (partition, high_watermark, log_start_offset) in offsets {
            self.update_partition_offset(topic_name, partition, high_watermark, log_start_offset).await?;
        }

        Ok(metadata)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mock WAL implementation for testing
    struct MockWal {
        events: Arc<RwLock<Vec<MetadataEvent>>>,
    }

    impl MockWal {
        fn new() -> Self {
            Self {
                events: Arc::new(RwLock::new(Vec::new())),
            }
        }
    }

    #[async_trait]
    impl MetaLogWalInterface for MockWal {
        async fn append_metadata_event(&self, event: &MetadataEvent) -> Result<u64> {
            let mut events = self.events.write();
            events.push(event.clone());
            Ok(events.len() as u64)
        }

        async fn read_metadata_events(&self, from_offset: u64) -> Result<Vec<MetadataEvent>> {
            let events = self.events.read();
            Ok(events.iter().skip(from_offset as usize).cloned().collect())
        }

        async fn get_latest_offset(&self) -> Result<u64> {
            Ok(self.events.read().len() as u64)
        }
    }

    #[tokio::test]
    async fn test_metalog_store_basic() {
        let wal = Arc::new(MockWal::new());
        let store = ChronikMetaLogStore::new(wal, "/tmp/test_metalog").await.unwrap();

        // Create a topic
        let config = TopicConfig::default();
        let topic = store.create_topic("test-topic", config.clone()).await.unwrap();
        assert_eq!(topic.name, "test-topic");

        // Get the topic
        let retrieved = store.get_topic("test-topic").await.unwrap();
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().name, "test-topic");

        // List topics
        let topics = store.list_topics().await.unwrap();
        assert_eq!(topics.len(), 1);

        // Delete topic
        store.delete_topic("test-topic").await.unwrap();

        // Verify it's gone
        let retrieved = store.get_topic("test-topic").await.unwrap();
        assert!(retrieved.is_none());
    }

    #[tokio::test]
    async fn test_state_reconstruction() {
        let wal = Arc::new(MockWal::new());
        let store = ChronikMetaLogStore::new(wal.clone(), "/tmp/test_metalog2").await.unwrap();

        // Create some state
        store.create_topic("topic1", TopicConfig::default()).await.unwrap();
        store.create_topic("topic2", TopicConfig::default()).await.unwrap();

        // Create a new store with the same WAL (simulating restart)
        let store2 = ChronikMetaLogStore::new(wal, "/tmp/test_metalog2").await.unwrap();

        // Verify state was reconstructed
        let topics = store2.list_topics().await.unwrap();
        assert_eq!(topics.len(), 2);
    }
}