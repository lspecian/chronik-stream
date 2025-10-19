//! Raft-replicated metadata store wrapper
//!
//! This module provides a `RaftMetaLog` wrapper around `ChronikMetaLogStore` that replicates
//! all metadata operations via Raft consensus for multi-node clusters.
//!
//! ## Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                     RaftMetaLog                             │
//! │  (Raft wrapper - only for clustered mode)                  │
//! ├─────────────────────────────────────────────────────────────┤
//! │                                                             │
//! │  Write Path:                                                │
//! │    1. Serialize MetadataEvent to bincode                    │
//! │    2. Propose to Raft ("__meta", 0)                         │
//! │    3. Wait for quorum commit                                │
//! │    4. Apply to ChronikMetaLogStore                          │
//! │                                                             │
//! │  Read Path:                                                 │
//! │    1. Query local ChronikMetaLogStore (no Raft)             │
//! │    2. Eventually consistent reads                           │
//! │                                                             │
//! └─────────────────────────────────────────────────────────────┘
//!          ↓                                  ↓
//! ┌──────────────────┐           ┌──────────────────────┐
//! │ RaftReplicaManager│          │ ChronikMetaLogStore  │
//! │  (metadata group) │          │  (local WAL store)   │
//! └──────────────────┘           └──────────────────────┘
//! ```

use async_trait::async_trait;
use std::sync::Arc;
use tracing::{debug, error, info, warn};

use super::{
    MetadataStore, MetadataError, Result,
    TopicConfig, TopicMetadata, SegmentMetadata, BrokerMetadata, BrokerStatus,
    PartitionAssignment, ConsumerGroupMetadata, ConsumerOffset,
    MetadataEvent, MetadataEventPayload,
    ChronikMetaLogStore,
};

/// Metadata topic name (always partition 0)
pub const METADATA_TOPIC: &str = "__meta";
pub const METADATA_PARTITION: i32 = 0;

/// Raft-replicated metadata store
///
/// This wraps `ChronikMetaLogStore` and replicates all write operations via Raft.
/// Reads query local state for low latency (eventually consistent).
///
/// # Usage
///
/// ```rust,ignore
/// // Clustered mode (3+ nodes)
/// let inner = Arc::new(ChronikMetaLogStore::new(wal, data_dir).await?);
/// let raft_meta_log = RaftMetaLog::new(inner, raft_manager, node_id);
/// let metadata_store: Arc<dyn MetadataStore> = Arc::new(raft_meta_log);
///
/// // Standalone mode (single node)
/// let metadata_store: Arc<dyn MetadataStore> = Arc::new(ChronikMetaLogStore::new(wal, data_dir).await?);
/// ```
pub struct RaftMetaLog {
    /// Underlying metadata store that applies committed entries
    inner: Arc<ChronikMetaLogStore>,

    /// Raft replica manager for proposing operations
    #[cfg(feature = "raft")]
    raft_manager: Arc<crate::RaftReplicaManagerRef>,

    /// This node's ID
    node_id: u64,
}

/// Type alias for Raft manager reference (conditional compilation)
#[cfg(feature = "raft")]
pub type RaftReplicaManagerRef = dyn RaftReplicaManager;

#[cfg(not(feature = "raft"))]
pub type RaftReplicaManagerRef = ();

/// Trait for Raft replica manager operations (abstraction for testing)
#[cfg(feature = "raft")]
pub trait RaftReplicaManager: Send + Sync {
    /// Propose an entry and wait for commit
    fn propose_and_wait(
        &self,
        topic: &str,
        partition: i32,
        data: Vec<u8>,
    ) -> impl std::future::Future<Output = std::result::Result<u64, Box<dyn std::error::Error + Send + Sync>>> + Send;

    /// Check if this node is the leader
    fn is_leader(&self, topic: &str, partition: i32) -> bool;

    /// Get the leader node ID
    fn get_leader(&self, topic: &str, partition: i32) -> Option<u64>;
}

impl RaftMetaLog {
    /// Create a new Raft-replicated metadata store
    ///
    /// # Arguments
    /// * `inner` - Underlying ChronikMetaLogStore
    /// * `raft_manager` - Raft replica manager for metadata group
    /// * `node_id` - This node's ID
    #[cfg(feature = "raft")]
    pub fn new(
        inner: Arc<ChronikMetaLogStore>,
        raft_manager: Arc<RaftReplicaManagerRef>,
        node_id: u64,
    ) -> Self {
        info!(
            "Initializing Raft-replicated metadata store on node {}",
            node_id
        );

        Self {
            inner,
            raft_manager,
            node_id,
        }
    }

    /// Create without Raft (for testing/standalone - delegates to inner)
    #[cfg(not(feature = "raft"))]
    pub fn new(
        inner: Arc<ChronikMetaLogStore>,
        _raft_manager: (),
        node_id: u64,
    ) -> Self {
        warn!(
            "Creating RaftMetaLog without Raft feature - falling back to local store on node {}",
            node_id
        );

        Self {
            inner,
            node_id,
        }
    }

    /// Propose a metadata event via Raft and wait for commit
    ///
    /// # Arguments
    /// * `payload` - Metadata event payload to propose
    ///
    /// # Returns
    /// Ok if the event was committed, Err if Raft failed
    #[cfg(feature = "raft")]
    async fn propose_event(&self, payload: MetadataEventPayload) -> Result<()> {
        // Check if we're the leader
        if !self.raft_manager.is_leader(METADATA_TOPIC, METADATA_PARTITION) {
            let leader_id = self.raft_manager.get_leader(METADATA_TOPIC, METADATA_PARTITION);
            return Err(MetadataError::StorageError(format!(
                "Not leader for metadata group (leader: {:?})",
                leader_id
            )));
        }

        // Create event
        let event = MetadataEvent::new(payload.clone());

        // Serialize to bincode
        let data = bincode::serialize(&event)
            .map_err(|e| MetadataError::SerializationError(format!("Failed to serialize metadata event: {}", e)))?;

        debug!(
            "Proposing metadata event via Raft: {:?} ({} bytes)",
            payload,
            data.len()
        );

        // Propose to Raft and wait for commit
        self.raft_manager
            .propose_and_wait(METADATA_TOPIC, METADATA_PARTITION, data)
            .await
            .map_err(|e| MetadataError::StorageError(format!("Raft propose failed: {}", e)))?;

        info!("Metadata event committed via Raft: {:?}", payload);

        Ok(())
    }

    /// Fallback for non-Raft builds - directly apply to inner store
    #[cfg(not(feature = "raft"))]
    async fn propose_event(&self, payload: MetadataEventPayload) -> Result<()> {
        warn!("Raft feature not enabled - applying metadata event locally without replication");
        self.inner.append_event_direct(payload).await
    }
}

// Implement MetadataStore trait - all writes go through Raft, reads query local state
#[async_trait]
impl MetadataStore for RaftMetaLog {
    // ==================== TOPIC OPERATIONS ====================

    async fn create_topic(&self, name: &str, config: TopicConfig) -> Result<TopicMetadata> {
        // Check if topic exists (local read)
        if self.inner.get_topic(name).await?.is_some() {
            return Err(MetadataError::AlreadyExists(format!("Topic {} already exists", name)));
        }

        // Propose via Raft
        let payload = MetadataEventPayload::TopicCreated {
            name: name.to_string(),
            config: config.clone(),
        };
        self.propose_event(payload).await?;

        // After commit, read from local state
        self.inner.get_topic(name).await?
            .ok_or_else(|| MetadataError::StorageError("Failed to create topic".to_string()))
    }

    async fn get_topic(&self, name: &str) -> Result<Option<TopicMetadata>> {
        // Local read (no Raft)
        self.inner.get_topic(name).await
    }

    async fn list_topics(&self) -> Result<Vec<TopicMetadata>> {
        // Local read (no Raft)
        self.inner.list_topics().await
    }

    async fn update_topic(&self, name: &str, config: TopicConfig) -> Result<TopicMetadata> {
        // Check if topic exists
        if self.inner.get_topic(name).await?.is_none() {
            return Err(MetadataError::NotFound(format!("Topic {} not found", name)));
        }

        // Propose via Raft
        let payload = MetadataEventPayload::TopicUpdated {
            name: name.to_string(),
            config: config.clone(),
        };
        self.propose_event(payload).await?;

        // Read updated state
        self.inner.get_topic(name).await?
            .ok_or_else(|| MetadataError::StorageError("Failed to update topic".to_string()))
    }

    async fn delete_topic(&self, name: &str) -> Result<()> {
        // Check if topic exists
        if self.inner.get_topic(name).await?.is_none() {
            return Err(MetadataError::NotFound(format!("Topic {} not found", name)));
        }

        // Propose via Raft
        let payload = MetadataEventPayload::TopicDeleted {
            name: name.to_string(),
        };
        self.propose_event(payload).await
    }

    // ==================== SEGMENT OPERATIONS ====================

    async fn persist_segment_metadata(&self, metadata: SegmentMetadata) -> Result<()> {
        let payload = MetadataEventPayload::SegmentCreated {
            metadata: metadata.clone(),
        };
        self.propose_event(payload).await
    }

    async fn get_segment_metadata(&self, topic: &str, segment_id: &str) -> Result<Option<SegmentMetadata>> {
        self.inner.get_segment_metadata(topic, segment_id).await
    }

    async fn list_segments(&self, topic: &str, partition: Option<u32>) -> Result<Vec<SegmentMetadata>> {
        self.inner.list_segments(topic, partition).await
    }

    async fn delete_segment(&self, topic: &str, segment_id: &str) -> Result<()> {
        let payload = MetadataEventPayload::SegmentDeleted {
            topic: topic.to_string(),
            partition: 0, // TODO: Get partition from segment_id
            segment_id: segment_id.to_string(),
        };
        self.propose_event(payload).await
    }

    // ==================== BROKER OPERATIONS ====================

    async fn register_broker(&self, metadata: BrokerMetadata) -> Result<()> {
        let payload = MetadataEventPayload::BrokerRegistered {
            metadata: metadata.clone(),
        };
        self.propose_event(payload).await
    }

    async fn get_broker(&self, broker_id: i32) -> Result<Option<BrokerMetadata>> {
        self.inner.get_broker(broker_id).await
    }

    async fn list_brokers(&self) -> Result<Vec<BrokerMetadata>> {
        self.inner.list_brokers().await
    }

    async fn update_broker_status(&self, broker_id: i32, status: BrokerStatus) -> Result<()> {
        let payload = MetadataEventPayload::BrokerStatusChanged {
            broker_id,
            status,
        };
        self.propose_event(payload).await
    }

    // ==================== PARTITION ASSIGNMENT ====================

    async fn assign_partition(&self, assignment: PartitionAssignment) -> Result<()> {
        let payload = MetadataEventPayload::PartitionAssigned {
            assignment: assignment.clone(),
        };
        self.propose_event(payload).await
    }

    async fn get_partition_assignments(&self, topic: &str) -> Result<Vec<PartitionAssignment>> {
        self.inner.get_partition_assignments(topic).await
    }

    async fn get_partition_leader(&self, topic: &str, partition: u32) -> Result<Option<i32>> {
        // Local read (no Raft)
        self.inner.get_partition_leader(topic, partition).await
    }

    async fn get_partition_replicas(&self, topic: &str, partition: u32) -> Result<Option<Vec<i32>>> {
        // Local read (no Raft)
        self.inner.get_partition_replicas(topic, partition).await
    }

    // ==================== CONSUMER GROUP OPERATIONS ====================

    async fn create_consumer_group(&self, metadata: ConsumerGroupMetadata) -> Result<()> {
        let payload = MetadataEventPayload::ConsumerGroupCreated {
            metadata: metadata.clone(),
        };
        self.propose_event(payload).await
    }

    async fn get_consumer_group(&self, group_id: &str) -> Result<Option<ConsumerGroupMetadata>> {
        self.inner.get_consumer_group(group_id).await
    }

    async fn update_consumer_group(&self, metadata: ConsumerGroupMetadata) -> Result<()> {
        let payload = MetadataEventPayload::ConsumerGroupUpdated {
            metadata: metadata.clone(),
        };
        self.propose_event(payload).await
    }

    async fn commit_offset(&self, offset: ConsumerOffset) -> Result<()> {
        let payload = MetadataEventPayload::OffsetCommitted {
            offset: offset.clone(),
        };
        self.propose_event(payload).await
    }

    async fn get_consumer_offset(&self, group_id: &str, topic: &str, partition: u32) -> Result<Option<ConsumerOffset>> {
        self.inner.get_consumer_offset(group_id, topic, partition).await
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

        self.propose_event(payload).await
    }

    // ==================== TRANSACTION OPERATIONS ====================

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
        self.propose_event(payload).await
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

        self.propose_event(payload).await
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
        self.propose_event(payload).await
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
        self.propose_event(payload).await
    }

    async fn commit_transaction(
        &self,
        transactional_id: String,
        producer_id: i64,
        producer_epoch: i16,
    ) -> Result<()> {
        let payload = MetadataEventPayload::CommitTransaction {
            transactional_id,
            producer_id,
            producer_epoch,
            committed_partitions: Vec::new(),
            committed_offsets: Vec::new(),
        };
        self.propose_event(payload).await
    }

    async fn abort_transaction(
        &self,
        transactional_id: String,
        producer_id: i64,
        producer_epoch: i16,
    ) -> Result<()> {
        let payload = MetadataEventPayload::AbortTransaction {
            transactional_id,
            producer_id,
            producer_epoch,
            aborted_partitions: Vec::new(),
        };
        self.propose_event(payload).await
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
        self.propose_event(payload).await
    }

    // ==================== PARTITION OFFSET OPERATIONS ====================

    async fn update_partition_offset(&self, topic: &str, partition: u32, high_watermark: i64, log_start_offset: i64) -> Result<()> {
        // Partition offsets are high-frequency updates - use local store directly
        // This avoids Raft overhead for every produce request
        // Trade-off: Offsets may be slightly stale on followers
        self.inner.update_partition_offset(topic, partition, high_watermark, log_start_offset).await
    }

    async fn get_partition_offset(&self, topic: &str, partition: u32) -> Result<Option<(i64, i64)>> {
        self.inner.get_partition_offset(topic, partition).await
    }

    // ==================== SYSTEM INITIALIZATION ====================

    async fn init_system_state(&self) -> Result<()> {
        // System initialization should be done on inner store directly
        self.inner.init_system_state().await
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

        // Set initial offsets (local updates)
        for (partition, high_watermark, log_start_offset) in offsets {
            self.update_partition_offset(topic_name, partition, high_watermark, log_start_offset).await?;
        }

        Ok(metadata)
    }
}

/// Extension trait for ChronikMetaLogStore to support direct event application
/// (bypassing Raft, used by MetadataStateMachine)
impl ChronikMetaLogStore {
    /// Append an event directly without going through Raft
    /// This is used by the Raft state machine when applying committed entries
    pub async fn append_event_direct(&self, payload: MetadataEventPayload) -> Result<()> {
        self.append_event(payload).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::MockWal;
    use std::path::PathBuf;

    #[tokio::test]
    async fn test_raft_meta_log_reads() {
        // Create inner store
        let wal = Arc::new(MockWal::new());
        let inner = Arc::new(
            ChronikMetaLogStore::new(wal, PathBuf::from("/tmp/test_raft_meta_log"))
                .await
                .unwrap(),
        );

        // Create RaftMetaLog without Raft (standalone mode)
        #[cfg(not(feature = "raft"))]
        let raft_meta_log = RaftMetaLog::new(inner.clone(), (), 1);

        #[cfg(feature = "raft")]
        {
            // For raft feature, we'd need a mock RaftReplicaManager
            // Skipping test compilation when raft feature is enabled
            // Real integration tests will cover this
            return;
        }

        #[cfg(not(feature = "raft"))]
        {
            // Test topic operations
            let config = TopicConfig::default();
            let topic = raft_meta_log.create_topic("test-topic", config.clone()).await.unwrap();
            assert_eq!(topic.name, "test-topic");

            // Verify read works
            let retrieved = raft_meta_log.get_topic("test-topic").await.unwrap();
            assert!(retrieved.is_some());

            // List topics
            let topics = raft_meta_log.list_topics().await.unwrap();
            assert_eq!(topics.len(), 1);
        }
    }
}
