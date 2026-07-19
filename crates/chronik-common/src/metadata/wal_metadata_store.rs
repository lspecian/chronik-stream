//! WAL-based metadata store implementation (Option A - Raft-free)
//!
//! This module provides a metadata store that uses Write-Ahead Log (WAL) for
//! all metadata operations, with NO dependency on Raft consensus.
//!
//! **ARCHITECTURE**:
//! - Uses existing MetadataWal (chronik-server/metadata_wal.rs) for WAL writes
//! - MetadataWal wraps GroupCommitWal (proven 90K+ msg/s infrastructure)
//! - Event-sourced: All state built from MetadataEvent log
//! - Replication via existing WalReplicationManager (no new ports/protocols)
//!
//! **PERFORMANCE**:
//! - Topic creation: < 100ms (200x faster than current 20+ seconds with Raft)
//! - Throughput: 10,000+ metadata ops/s (vs 1,500 with Raft)
//! - Leader election impact: NONE (no forwarding, no waiting)

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::pin::Pin;
use std::future::Future;
use async_trait::async_trait;
use tokio::sync::RwLock;
use chrono::Utc;
use dashmap::DashMap;

use super::events::{MetadataEvent, MetadataEventPayload, TransactionPartition};
use super::transaction::{TransactionMetadata, TransactionState};
use super::traits::*;

/// Each node owns a disjoint 2^40 producer-id range so ids are cluster-unique
/// without any cross-node coordination. Within a node they are monotonic; on
/// recovery the counter is advanced past every producer id seen in replayed
/// transaction events so a restart never re-hands-out a live id.
fn producer_id_base(node_id: u64) -> i64 {
    // node 0 -> 1, node 1 -> 2^40, node 2 -> 2^41, ... (avoid 0, which Kafka
    // treats as "no producer id").
    ((node_id as i64) << 40) | 1
}

/// Type alias for WAL append callback (injected by chronik-server)
///
/// Signature: async fn(event_bytes: Vec<u8>) -> Result<i64, String>
/// Returns: WAL offset
pub type WalAppendFn = Arc<dyn Fn(Vec<u8>) -> Pin<Box<dyn Future<Output = std::result::Result<i64, String>> + Send>> + Send + Sync>;

/// Type alias for event bus publish callback (injected by chronik-server)
///
/// Signature: fn(event: MetadataEvent) -> usize
/// Returns: Number of subscribers that received the event
pub type EventBusPublishFn = Arc<dyn Fn(MetadataEvent) -> usize + Send + Sync>;

/// Whether a metadata event should be replicated to followers over the
/// fire-and-forget event bus.
///
/// Segment-index events (Tantivy + Parquet) are HIGH-FREQUENCY derived local
/// state: every node builds its own indexes from its own replicated partition
/// data, so a follower never needs the leader's copy — it can rebuild them from
/// the data WAL on restart. They outnumber catalog events (TopicCreated etc.)
/// ~45:1, and routing that flood through the bounded broadcast channel is what
/// starves catalog replication and diverges the topic catalog across nodes under
/// load. Keep them node-local; replicate only catalog / consumer / watermark state.
fn replicate_to_followers(payload: &MetadataEventPayload) -> bool {
    !matches!(
        payload,
        MetadataEventPayload::SegmentCreated { .. }
            | MetadataEventPayload::SegmentDeleted { .. }
            | MetadataEventPayload::ParquetSegmentCreated { .. }
            | MetadataEventPayload::ParquetSegmentDeleted { .. }
    )
}

/// WAL-based metadata store with event sourcing
pub struct WalMetadataStore {
    /// Node ID for cluster coordination
    node_id: u64,

    /// In-memory state built from events (event sourcing)
    state: Arc<MetadataState>,

    /// WAL append callback (provided by chronik-server's MetadataWal)
    /// This avoids circular dependency between chronik-common and chronik-server
    wal_append: WalAppendFn,

    /// Event bus publish callback (v2.2.9 Phase 7 FIX: for metadata replication)
    /// This publishes events to MetadataWalReplicator so followers receive metadata updates
    event_bus_publish: Option<EventBusPublishFn>,
}

/// In-memory metadata state (built from events)
struct MetadataState {
    topics: RwLock<HashMap<String, TopicMetadata>>,
    brokers: RwLock<HashMap<i32, BrokerMetadata>>,
    consumer_groups: RwLock<HashMap<String, ConsumerGroupMetadata>>,
    consumer_offsets: RwLock<HashMap<(String, String, u32), ConsumerOffset>>,
    // v2.2.14 PERFORMANCE FIX: DashMap for lock-free concurrent reads (80x cluster speedup)
    partition_assignments: DashMap<(String, u32), PartitionAssignment>,
    segments: RwLock<HashMap<(String, String), SegmentMetadata>>,
    partition_offsets: RwLock<HashMap<(String, u32), (i64, i64)>>,
    // Parquet segments for columnar storage (keyed by topic-partition, segment_id)
    parquet_segments: RwLock<HashMap<(String, i32, String), ParquetSegmentMetadata>>,
    /// Transaction coordinator state, keyed by transactional.id (EOS).
    transactions: RwLock<HashMap<String, TransactionMetadata>>,
    /// Next producer id to hand out (node-namespaced, advanced past recovered ids).
    next_producer_id: AtomicI64,
}

impl MetadataState {
    fn new(node_id: u64) -> Self {
        Self {
            topics: RwLock::new(HashMap::new()),
            brokers: RwLock::new(HashMap::new()),
            consumer_groups: RwLock::new(HashMap::new()),
            consumer_offsets: RwLock::new(HashMap::new()),
            // v2.2.14: DashMap doesn't need RwLock wrapper
            partition_assignments: DashMap::new(),
            segments: RwLock::new(HashMap::new()),
            partition_offsets: RwLock::new(HashMap::new()),
            parquet_segments: RwLock::new(HashMap::new()),
            transactions: RwLock::new(HashMap::new()),
            next_producer_id: AtomicI64::new(producer_id_base(node_id)),
        }
    }

    /// Ensure the producer-id counter is past `pid` so a recovered/replicated id
    /// is never re-allocated.
    fn observe_producer_id(&self, pid: i64) {
        self.next_producer_id.fetch_max(pid + 1, Ordering::SeqCst);
    }

    /// Apply a metadata event to update state
    async fn apply_event(&self, event: &MetadataEvent) -> Result<()> {
        match &event.payload {
            MetadataEventPayload::TopicCreated { name, config } => {
                let mut topics = self.topics.write().await;
                if let Some(existing) = topics.get_mut(name) {
                    // v2.3.2 CRITICAL FIX: If incoming partition count is HIGHER, this
                    // is either a topic recreation with more partitions or an explicit
                    // CreateTopics overriding an auto-created topic. Update the partition
                    // count and add offsets for new partitions.
                    //
                    // NOTE: Only update when incoming > existing, never downgrade.
                    // Auto-create events with fewer partitions must NOT overwrite
                    // an explicit CreateTopics with more partitions.
                    if config.partition_count > existing.config.partition_count {
                        let old_count = existing.config.partition_count;
                        tracing::info!(
                            topic = %name,
                            old_partitions = old_count,
                            new_partitions = config.partition_count,
                            "Topic partition count expanded - updating metadata"
                        );
                        existing.config = config.clone();
                        existing.updated_at = event.timestamp;
                        drop(topics);

                        // Add partition offsets for new partitions only
                        let mut offsets = self.partition_offsets.write().await;
                        for partition in old_count..config.partition_count {
                            let key = (name.clone(), partition);
                            offsets.entry(key).or_insert((0, 0));
                        }
                        return Ok(());
                    }

                    // v2.3.1: Merge config from the new event into the existing topic.
                    // In a Raft cluster, auto_create_topics() on a follower can race
                    // with the real CreateTopics event from the leader. The auto-created
                    // topic has a bare config, so we adopt any keys from the incoming
                    // event that the existing topic doesn't have (e.g., vector.enabled,
                    // searchable, columnar.enabled set by the real CreateTopics call).
                    let mut merged = false;
                    for (key, value) in &config.config {
                        if !existing.config.config.contains_key(key) {
                            existing.config.config.insert(key.clone(), value.clone());
                            merged = true;
                        }
                    }
                    if merged {
                        existing.updated_at = event.timestamp;
                        tracing::debug!(
                            topic = %name,
                            "Merged config keys from duplicate TopicCreated event"
                        );
                    }
                    return Ok(());
                }

                let metadata = TopicMetadata {
                    name: name.clone(),
                    id: event.event_id,
                    config: config.clone(),
                    created_at: event.timestamp,
                    updated_at: event.timestamp,
                };

                topics.insert(name.clone(), metadata);
                drop(topics);

                // v2.2.9 Phase 7 FIX #3: Do NOT create partition assignments here!
                // Partition assignments should be created via separate PartitionAssigned events
                // from assign_partitions_to_nodes() which has proper replica information from cluster config.
                // Creating assignments here with only creator node as replica caused replication to break.
                //
                // OLD CODE (REMOVED - caused single-replica assignments):
                // for partition in 0..config.partition_count {
                //     let assignment = PartitionAssignment { ... replicas: vec![event.created_by_node] };
                //     assignments.insert(key, assignment);
                // }

                // Initialize partition offsets only (assignments come from PartitionAssigned events)
                let mut offsets = self.partition_offsets.write().await;
                for partition in 0..config.partition_count {
                    let key = (name.clone(), partition);
                    offsets.insert(key, (0, 0));
                }

                Ok(())
            }

            MetadataEventPayload::TopicUpdated { name, config } => {
                let mut topics = self.topics.write().await;
                if let Some(metadata) = topics.get_mut(name) {
                    metadata.config = config.clone();
                    metadata.updated_at = event.timestamp;
                }
                Ok(())
            }

            MetadataEventPayload::TopicDeleted { name } => {
                // Remove topic metadata
                let mut topics = self.topics.write().await;
                topics.remove(name);
                drop(topics);

                // Remove all partition assignments for this topic
                // v2.2.14: DashMap provides retain() for lock-free concurrent removal
                self.partition_assignments.retain(|k, _| k.0 != *name);

                // Remove partition offsets for this topic
                let mut offsets = self.partition_offsets.write().await;
                offsets.retain(|k, _| k.0 != *name);
                drop(offsets);

                // Remove consumer offsets for this topic
                let mut consumer_offsets = self.consumer_offsets.write().await;
                consumer_offsets.retain(|k, _| k.1 != *name);
                drop(consumer_offsets);

                // Remove Tantivy segments for this topic
                let mut segments = self.segments.write().await;
                segments.retain(|k, _| k.0 != *name);
                drop(segments);

                // Remove Parquet segments for this topic
                let mut parquet_segments = self.parquet_segments.write().await;
                parquet_segments.retain(|k, _| k.0 != *name);

                tracing::info!("🗑️ Topic '{}' deleted with all associated data", name);
                Ok(())
            }

            MetadataEventPayload::HighWatermarkUpdated { topic, partition, new_watermark } => {
                let mut offsets = self.partition_offsets.write().await;
                let key = (topic.clone(), *partition as u32);
                if let Some((hwm, _lso)) = offsets.get_mut(&key) {
                    // Last-write-wins for watermarks (newer timestamp wins)
                    *hwm = *new_watermark;
                } else {
                    offsets.insert(key, (*new_watermark, 0));
                }
                Ok(())
            }

            MetadataEventPayload::LogStartOffsetUpdated { topic, partition, new_log_start_offset } => {
                // Log start offset is monotonic (only advances). Never regress it,
                // so a stale/replayed event can't resurrect deleted records.
                let mut offsets = self.partition_offsets.write().await;
                let key = (topic.clone(), *partition as u32);
                if let Some((_hwm, lso)) = offsets.get_mut(&key) {
                    if *new_log_start_offset > *lso {
                        *lso = *new_log_start_offset;
                    }
                } else {
                    offsets.insert(key, (0, *new_log_start_offset));
                }
                Ok(())
            }

            MetadataEventPayload::ConsumerOffsetDeleted { group_id, topic, partition } => {
                let mut offsets = self.consumer_offsets.write().await;
                offsets.remove(&(group_id.clone(), topic.clone(), *partition));
                Ok(())
            }

            MetadataEventPayload::BrokerRegistered { metadata } => {
                let mut brokers = self.brokers.write().await;
                brokers.insert(metadata.broker_id, metadata.clone());
                Ok(())
            }

            MetadataEventPayload::BrokerStatusChanged { broker_id, status } => {
                let mut brokers = self.brokers.write().await;
                if let Some(broker) = brokers.get_mut(broker_id) {
                    broker.status = status.clone();
                    broker.updated_at = event.timestamp;
                }
                Ok(())
            }

            MetadataEventPayload::BrokerRemoved { broker_id } => {
                let mut brokers = self.brokers.write().await;
                brokers.remove(broker_id);
                Ok(())
            }

            MetadataEventPayload::OffsetCommitted { offset } => {
                let mut offsets = self.consumer_offsets.write().await;
                let key = (offset.group_id.clone(), offset.topic.clone(), offset.partition);
                offsets.insert(key, offset.clone());
                Ok(())
            }

            MetadataEventPayload::ConsumerGroupCreated { metadata } => {
                let mut groups = self.consumer_groups.write().await;
                groups.insert(metadata.group_id.clone(), metadata.clone());
                Ok(())
            }

            MetadataEventPayload::ConsumerGroupUpdated { metadata } => {
                let mut groups = self.consumer_groups.write().await;
                groups.insert(metadata.group_id.clone(), metadata.clone());
                Ok(())
            }

            MetadataEventPayload::ConsumerGroupDeleted { group_id } => {
                let mut groups = self.consumer_groups.write().await;
                groups.remove(group_id);
                drop(groups);
                // Also purge all committed offsets belonging to this group.
                let mut offsets = self.consumer_offsets.write().await;
                offsets.retain(|(g, _, _), _| g != group_id);
                Ok(())
            }

            MetadataEventPayload::SegmentCreated { metadata } => {
                let mut segments = self.segments.write().await;
                let key = (metadata.topic.clone(), metadata.segment_id.clone());
                segments.insert(key, metadata.clone());
                Ok(())
            }

            MetadataEventPayload::SegmentDeleted { topic, partition: _, segment_id } => {
                let mut segments = self.segments.write().await;
                let key = (topic.clone(), segment_id.clone());
                segments.remove(&key);
                Ok(())
            }

            MetadataEventPayload::ParquetSegmentCreated { metadata } => {
                let mut parquet_segments = self.parquet_segments.write().await;
                let key = (metadata.topic.clone(), metadata.partition, metadata.segment_id.clone());
                parquet_segments.insert(key, metadata.clone());
                tracing::debug!(
                    topic = %metadata.topic,
                    partition = metadata.partition,
                    segment_id = %metadata.segment_id,
                    min_offset = metadata.min_offset,
                    max_offset = metadata.max_offset,
                    "Parquet segment created"
                );
                Ok(())
            }

            MetadataEventPayload::ParquetSegmentDeleted { topic, partition, segment_id } => {
                let mut parquet_segments = self.parquet_segments.write().await;
                let key = (topic.clone(), *partition, segment_id.clone());
                parquet_segments.remove(&key);
                tracing::debug!(
                    topic = %topic,
                    partition = partition,
                    segment_id = %segment_id,
                    "Parquet segment deleted"
                );
                Ok(())
            }

            MetadataEventPayload::PartitionAssigned { assignment } => {
                // v2.2.14: DashMap provides synchronous lock-free insert (no .await needed)
                let key = (assignment.topic.clone(), assignment.partition);
                self.partition_assignments.insert(key, assignment.clone());
                Ok(())
            }

            // ===== Transaction coordinator events (EOS) =====
            // These are event-sourced so the coordinator state is rebuilt identically
            // on recovery and on followers. `observe_producer_id` keeps the allocator
            // past every id we have ever seen so a restart never reuses a live id.
            MetadataEventPayload::BeginTransaction {
                transactional_id, producer_id, producer_epoch, transaction_timeout_ms,
            } => {
                self.observe_producer_id(*producer_id);
                let now_ms = event.timestamp.timestamp_millis();
                let mut txns = self.transactions.write().await;
                let txn = txns.entry(transactional_id.clone()).or_insert_with(|| {
                    TransactionMetadata::new(transactional_id.clone(), *producer_id, *producer_epoch, *transaction_timeout_ms, now_ms)
                });
                // InitProducerId registration (or re-registration with a bumped epoch
                // for fencing): bind the producer and reset to Empty with no enrollment.
                // The transaction becomes Ongoing on the first AddPartitionsToTransaction,
                // matching Kafka — there is no separate server-side "begin" RPC.
                txn.producer_id = *producer_id;
                txn.producer_epoch = *producer_epoch;
                txn.timeout_ms = *transaction_timeout_ms;
                txn.state = TransactionState::Empty;
                txn.partitions.clear();
                txn.groups.clear();
                txn.last_updated_ms = now_ms;
                Ok(())
            }
            MetadataEventPayload::AddPartitionsToTransaction {
                transactional_id, producer_id, partitions, ..
            } => {
                self.observe_producer_id(*producer_id);
                let now_ms = event.timestamp.timestamp_millis();
                let mut txns = self.transactions.write().await;
                if let Some(txn) = txns.get_mut(transactional_id) {
                    for p in partitions {
                        txn.partitions.insert((p.topic.clone(), p.partition));
                    }
                    if txn.state == TransactionState::Empty
                        || txn.state == TransactionState::CompleteCommit
                        || txn.state == TransactionState::CompleteAbort {
                        txn.state = TransactionState::Ongoing;
                    }
                    txn.last_updated_ms = now_ms;
                }
                Ok(())
            }
            MetadataEventPayload::AddOffsetsToTransaction {
                transactional_id, producer_id, group_id, ..
            } => {
                self.observe_producer_id(*producer_id);
                let now_ms = event.timestamp.timestamp_millis();
                let mut txns = self.transactions.write().await;
                if let Some(txn) = txns.get_mut(transactional_id) {
                    txn.groups.insert(group_id.clone());
                    if txn.state == TransactionState::Empty
                        || txn.state == TransactionState::CompleteCommit
                        || txn.state == TransactionState::CompleteAbort {
                        txn.state = TransactionState::Ongoing;
                    }
                    txn.last_updated_ms = now_ms;
                }
                Ok(())
            }
            MetadataEventPayload::PrepareCommit { transactional_id, producer_id, .. } => {
                self.observe_producer_id(*producer_id);
                let now_ms = event.timestamp.timestamp_millis();
                let mut txns = self.transactions.write().await;
                if let Some(txn) = txns.get_mut(transactional_id) {
                    txn.state = TransactionState::PrepareCommit;
                    txn.last_updated_ms = now_ms;
                }
                Ok(())
            }
            MetadataEventPayload::PrepareAbort { transactional_id, producer_id, .. } => {
                self.observe_producer_id(*producer_id);
                let now_ms = event.timestamp.timestamp_millis();
                let mut txns = self.transactions.write().await;
                if let Some(txn) = txns.get_mut(transactional_id) {
                    txn.state = TransactionState::PrepareAbort;
                    txn.last_updated_ms = now_ms;
                }
                Ok(())
            }
            MetadataEventPayload::CommitTransaction { transactional_id, producer_id, .. } => {
                self.observe_producer_id(*producer_id);
                let now_ms = event.timestamp.timestamp_millis();
                let mut txns = self.transactions.write().await;
                if let Some(txn) = txns.get_mut(transactional_id) {
                    txn.state = TransactionState::CompleteCommit;
                    txn.partitions.clear();
                    txn.groups.clear();
                    txn.last_updated_ms = now_ms;
                }
                Ok(())
            }
            MetadataEventPayload::AbortTransaction { transactional_id, producer_id, .. } => {
                self.observe_producer_id(*producer_id);
                let now_ms = event.timestamp.timestamp_millis();
                let mut txns = self.transactions.write().await;
                if let Some(txn) = txns.get_mut(transactional_id) {
                    txn.state = TransactionState::CompleteAbort;
                    txn.partitions.clear();
                    txn.groups.clear();
                    txn.last_updated_ms = now_ms;
                }
                Ok(())
            }
            MetadataEventPayload::ProducerFenced {
                transactional_id, new_producer_id, new_producer_epoch, ..
            } => {
                self.observe_producer_id(*new_producer_id);
                let now_ms = event.timestamp.timestamp_millis();
                let mut txns = self.transactions.write().await;
                if let Some(txn) = txns.get_mut(transactional_id) {
                    txn.producer_id = *new_producer_id;
                    txn.producer_epoch = *new_producer_epoch;
                    txn.last_updated_ms = now_ms;
                }
                Ok(())
            }

            _ => {
                // Other events not yet implemented
                tracing::warn!("Unhandled metadata event: {:?}", event.payload);
                Ok(())
            }
        }
    }
}

impl WalMetadataStore {
    /// Create a new WAL-based metadata store
    ///
    /// # Arguments
    /// - `node_id`: This node's ID for cluster coordination
    /// - `wal_append`: Callback to append events to WAL (provided by chronik-server)
    ///
    /// # Example (in chronik-server)
    /// ```ignore
    /// let metadata_wal = MetadataWal::new(data_dir).await?;
    /// let wal_append = Arc::new(move |bytes: Vec<u8>| {
    ///     let wal = metadata_wal.clone();
    ///     Box::pin(async move {
    ///         // Deserialize event
    ///         let event = MetadataEvent::from_bytes(&bytes)?;
    ///         // Write to WAL
    ///         let offset = wal.append(&event).await?;
    ///         Ok(offset)
    ///     }) as Pin<Box<dyn Future<Output = Result<i64, String>> + Send>>
    /// });
    ///
    /// let store = WalMetadataStore::new(node_id, wal_append);
    /// ```
    pub fn new(node_id: u64, wal_append: WalAppendFn) -> Self {
        Self {
            node_id,
            state: Arc::new(MetadataState::new(node_id)),
            wal_append,
            event_bus_publish: None, // Set later via set_event_bus()
        }
    }

    /// Set the event bus publish callback (v2.2.9 Phase 7 FIX)
    ///
    /// This is called after construction to wire up metadata replication.
    /// Late binding is necessary to avoid circular dependencies during initialization.
    pub fn set_event_bus(&mut self, event_bus_publish: EventBusPublishFn) {
        self.event_bus_publish = Some(event_bus_publish);
    }

    /// Write event to WAL and apply to local state
    async fn write_and_apply(&self, event: MetadataEvent) -> Result<()> {
        // 1. Serialize event to bytes
        let bytes = event.to_bytes()
            .map_err(|e| MetadataError::StorageError(format!("Failed to serialize event: {}", e)))?;

        // 2. Write to WAL via injected callback (durable, 1-2ms)
        let _offset = (self.wal_append)(bytes).await
            .map_err(|e| MetadataError::StorageError(format!("WAL write failed: {}", e)))?;

        // 3. Apply to local state machine (in-memory)
        self.state.apply_event(&event).await?;

        // 4. v2.2.9 Phase 7 FIX: Publish event to event bus for replication
        // This allows MetadataWalReplicator to hear the event and replicate to followers.
        //
        // v2.7.4: Do NOT replicate high-frequency segment-index events. Each node
        // builds its own Tantivy/Parquet segments from its own replicated partition
        // data, so a follower gains nothing from the leader's SegmentCreated events —
        // they are derived local state, recoverable on restart. But they DOMINATE the
        // metadata stream (~45:1 vs TopicCreated), and pushing them through the bounded
        // fire-and-forget broadcast channel saturates it, causing the rare TopicCreated
        // events to be dropped — which is how the topic catalog diverges across nodes
        // under load. Keeping them local drains the channel so the catalog replicates
        // reliably. They are still written to the local WAL + applied above.
        if replicate_to_followers(&event.payload) {
            if let Some(ref publish_fn) = self.event_bus_publish {
                let subscriber_count = publish_fn(event.clone());
                tracing::debug!(
                    "Published metadata event to {} subscribers: {:?}",
                    subscriber_count,
                    event.payload
                );
            } else {
                tracing::warn!(
                    "⚠️  Event bus not wired up - metadata will NOT replicate to followers! Event: {:?}",
                    event.payload
                );
            }
        }

        Ok(())
    }

    /// Apply event from replication (follower mode).
    ///
    /// Persists the event to the follower's local metadata WAL before applying
    /// to in-memory state. This ensures replicated metadata (TopicCreated,
    /// TopicUpdated, PartitionAssigned, etc.) survives pod restarts.
    ///
    /// NOTE: We intentionally do NOT publish to the event bus here — that would
    /// cause infinite replication loops (leader → follower → leader → ...).
    pub async fn apply_replicated_event(&self, event: MetadataEvent) -> Result<()> {
        // 1. Persist to local WAL for durability across restarts
        let bytes = event.to_bytes()
            .map_err(|e| MetadataError::StorageError(
                format!("Failed to serialize replicated event: {}", e),
            ))?;
        if let Err(e) = (self.wal_append)(bytes).await {
            tracing::warn!(
                "Failed to persist replicated metadata event to local WAL: {}. \
                 Event will be applied to in-memory state but may be lost on restart.",
                e
            );
        }

        // 2. Apply to in-memory state
        self.state.apply_event(&event).await
    }

    /// Replay events from WAL (recovery on startup)
    pub async fn replay_events(&self, events: Vec<MetadataEvent>) -> Result<()> {
        tracing::info!("Replaying {} metadata events from WAL", events.len());
        for event in events {
            self.state.apply_event(&event).await?;
        }
        Ok(())
    }

    /// Re-broadcast all topic metadata via the event bus.
    ///
    /// Called by the leader after startup once metadata replication is wired up.
    /// This ensures followers receive TopicCreated events for topics that were
    /// created in previous pod lifecycles. Without this, followers that restart
    /// would lose topic metadata (since replicated events weren't persisted to
    /// their local WAL before the apply_replicated_event fix).
    ///
    /// Safe to call multiple times — followers' apply_replicated_event is
    /// idempotent (creates topic only if it doesn't exist).
    pub async fn broadcast_all_topics(&self) -> usize {
        let publish_fn = match self.event_bus_publish {
            Some(ref f) => f,
            None => {
                tracing::warn!("Cannot broadcast topics: event bus not wired up");
                return 0;
            }
        };

        let topics = self.state.topics.read().await;
        let mut count = 0;
        for (name, meta) in topics.iter() {
            // Skip internal topics — they're managed separately
            if name.starts_with("__") {
                continue;
            }

            let event = MetadataEvent::new_with_node(
                MetadataEventPayload::TopicCreated {
                    name: name.clone(),
                    config: meta.config.clone(),
                },
                self.node_id,
            );
            let subscribers = publish_fn(event);
            count += 1;
            tracing::info!(
                topic = %name,
                subscribers,
                "Re-broadcast TopicCreated to followers"
            );
        }
        tracing::info!(topics_broadcast = count, "Metadata re-broadcast complete");
        count
    }
}

#[async_trait]
impl MetadataStore for WalMetadataStore {
    async fn create_topic(&self, name: &str, config: TopicConfig) -> Result<TopicMetadata> {
        // Check if topic already exists
        {
            let topics = self.state.topics.read().await;
            if topics.contains_key(name) {
                return Err(MetadataError::AlreadyExists(format!("Topic {} already exists", name)));
            }
        }

        // Create event
        let event = MetadataEvent::new_with_node(
            MetadataEventPayload::TopicCreated {
                name: name.to_string(),
                config: config.clone(),
            },
            self.node_id,
        );

        // Write to WAL and apply
        self.write_and_apply(event).await?;

        // Return created topic metadata
        let topics = self.state.topics.read().await;
        topics.get(name).cloned()
            .ok_or_else(|| MetadataError::NotFound(format!("Topic {} not found after creation", name)))
    }

    async fn get_topic(&self, name: &str) -> Result<Option<TopicMetadata>> {
        let topics = self.state.topics.read().await;
        Ok(topics.get(name).cloned())
    }

    async fn list_topics(&self) -> Result<Vec<TopicMetadata>> {
        let topics = self.state.topics.read().await;
        Ok(topics.values().cloned().collect())
    }

    async fn update_topic(&self, name: &str, config: TopicConfig) -> Result<TopicMetadata> {
        // Create event
        let event = MetadataEvent::new_with_node(
            MetadataEventPayload::TopicUpdated {
                name: name.to_string(),
                config: config.clone(),
            },
            self.node_id,
        );

        // Write to WAL and apply
        self.write_and_apply(event).await?;

        // Return updated topic metadata
        let topics = self.state.topics.read().await;
        topics.get(name).cloned()
            .ok_or_else(|| MetadataError::NotFound(format!("Topic {} not found", name)))
    }

    async fn delete_topic(&self, name: &str) -> Result<()> {
        let event = MetadataEvent::new_with_node(
            MetadataEventPayload::TopicDeleted {
                name: name.to_string(),
            },
            self.node_id,
        );

        self.write_and_apply(event).await
    }

    async fn persist_segment_metadata(&self, metadata: SegmentMetadata) -> Result<()> {
        let event = MetadataEvent::new_with_node(
            MetadataEventPayload::SegmentCreated {
                metadata: metadata.clone(),
            },
            self.node_id,
        );

        self.write_and_apply(event).await
    }

    async fn get_segment_metadata(&self, topic: &str, segment_id: &str) -> Result<Option<SegmentMetadata>> {
        let segments = self.state.segments.read().await;
        let key = (topic.to_string(), segment_id.to_string());
        Ok(segments.get(&key).cloned())
    }

    async fn list_segments(&self, topic: &str, partition: Option<u32>) -> Result<Vec<SegmentMetadata>> {
        let segments = self.state.segments.read().await;
        Ok(segments.values()
            .filter(|s| s.topic == topic && (partition.is_none() || s.partition == partition.unwrap()))
            .cloned()
            .collect())
    }

    async fn delete_segment(&self, topic: &str, segment_id: &str) -> Result<()> {
        let event = MetadataEvent::new_with_node(
            MetadataEventPayload::SegmentDeleted {
                topic: topic.to_string(),
                partition: 0, // Will be looked up from segment metadata
                segment_id: segment_id.to_string(),
            },
            self.node_id,
        );

        self.write_and_apply(event).await
    }

    async fn persist_parquet_segment(&self, metadata: ParquetSegmentMetadata) -> Result<()> {
        let event = MetadataEvent::new_with_node(
            MetadataEventPayload::ParquetSegmentCreated {
                metadata: metadata.clone(),
            },
            self.node_id,
        );

        self.write_and_apply(event).await
    }

    async fn get_parquet_segment(&self, topic: &str, partition: i32, segment_id: &str) -> Result<Option<ParquetSegmentMetadata>> {
        let parquet_segments = self.state.parquet_segments.read().await;
        let key = (topic.to_string(), partition, segment_id.to_string());
        Ok(parquet_segments.get(&key).cloned())
    }

    async fn list_parquet_segments(&self, topic: &str, partition: Option<i32>) -> Result<Vec<ParquetSegmentMetadata>> {
        let parquet_segments = self.state.parquet_segments.read().await;
        Ok(parquet_segments.values()
            .filter(|s| s.topic == topic && (partition.is_none() || s.partition == partition.unwrap()))
            .cloned()
            .collect())
    }

    async fn get_parquet_paths(&self, topic: &str) -> Result<Vec<String>> {
        let parquet_segments = self.state.parquet_segments.read().await;
        let mut paths: Vec<String> = parquet_segments.values()
            .filter(|s| s.topic == topic)
            .map(|s| s.object_store_path.clone())
            .collect();
        paths.sort();
        Ok(paths)
    }

    async fn delete_parquet_segment(&self, topic: &str, partition: i32, segment_id: &str) -> Result<()> {
        let event = MetadataEvent::new_with_node(
            MetadataEventPayload::ParquetSegmentDeleted {
                topic: topic.to_string(),
                partition,
                segment_id: segment_id.to_string(),
            },
            self.node_id,
        );

        self.write_and_apply(event).await
    }

    async fn register_broker(&self, broker: BrokerMetadata) -> Result<()> {
        let event = MetadataEvent::new_with_node(
            MetadataEventPayload::BrokerRegistered {
                metadata: broker.clone(),
            },
            self.node_id,
        );

        self.write_and_apply(event).await
    }

    async fn get_broker(&self, id: i32) -> Result<Option<BrokerMetadata>> {
        let brokers = self.state.brokers.read().await;
        Ok(brokers.get(&id).cloned())
    }

    async fn list_brokers(&self) -> Result<Vec<BrokerMetadata>> {
        let brokers = self.state.brokers.read().await;
        Ok(brokers.values().cloned().collect())
    }

    async fn update_broker_status(&self, broker_id: i32, status: BrokerStatus) -> Result<()> {
        let event = MetadataEvent::new_with_node(
            MetadataEventPayload::BrokerStatusChanged {
                broker_id,
                status,
            },
            self.node_id,
        );

        self.write_and_apply(event).await
    }

    async fn assign_partition(&self, assignment: PartitionAssignment) -> Result<()> {
        let event = MetadataEvent::new_with_node(
            MetadataEventPayload::PartitionAssigned {
                assignment: assignment.clone(),
            },
            self.node_id,
        );

        self.write_and_apply(event).await
    }

    async fn get_partition_assignments(&self, topic: &str) -> Result<Vec<PartitionAssignment>> {
        // v2.2.14 PERFORMANCE FIX: DashMap provides lock-free concurrent reads (80x speedup)
        // No .await needed - synchronous access eliminates async lock contention
        Ok(self.state.partition_assignments
            .iter()
            .filter(|entry| entry.key().0 == topic)
            .map(|entry| entry.value().clone())
            .collect())
    }

    async fn get_partition_leader(&self, topic: &str, partition: u32) -> Result<Option<i32>> {
        // v2.2.14 PERFORMANCE FIX: DashMap lock-free get (no .await)
        let key = (topic.to_string(), partition);
        // v2.2.9 Phase 7 FIX: Use leader_id field instead of deprecated is_leader/broker_id
        // Bug: was checking is_leader (always false) and returning broker_id (always -1)
        // Fix: return leader_id which is set correctly by ProduceHandler
        Ok(self.state.partition_assignments.get(&key).map(|a| a.leader_id as i32))
    }

    async fn get_partition_replicas(&self, topic: &str, partition: u32) -> Result<Option<Vec<i32>>> {
        // v2.2.14 PERFORMANCE FIX: DashMap lock-free get (no .await)
        let key = (topic.to_string(), partition);

        // v2.2.9 Phase 7 FIX: Return the replicas field, not deprecated broker_id
        // Each partition has ONE assignment with a Vec<u64> of replica node IDs
        match self.state.partition_assignments.get(&key) {
            Some(assignment) => {
                // Convert Vec<u64> to Vec<i32>
                let replicas: Vec<i32> = assignment.replicas.iter().map(|&id| id as i32).collect();
                Ok(Some(replicas))
            }
            None => Ok(None)
        }
    }

    async fn create_consumer_group(&self, group: ConsumerGroupMetadata) -> Result<()> {
        let event = MetadataEvent::new_with_node(
            MetadataEventPayload::ConsumerGroupCreated {
                metadata: group.clone(),
            },
            self.node_id,
        );

        self.write_and_apply(event).await
    }

    async fn get_consumer_group(&self, group_id: &str) -> Result<Option<ConsumerGroupMetadata>> {
        let groups = self.state.consumer_groups.read().await;
        Ok(groups.get(group_id).cloned())
    }

    async fn update_consumer_group(&self, group: ConsumerGroupMetadata) -> Result<()> {
        let event = MetadataEvent::new_with_node(
            MetadataEventPayload::ConsumerGroupUpdated {
                metadata: group.clone(),
            },
            self.node_id,
        );

        self.write_and_apply(event).await
    }

    async fn commit_offset(&self, offset: ConsumerOffset) -> Result<()> {
        let event = MetadataEvent::new_with_node(
            MetadataEventPayload::OffsetCommitted {
                offset: offset.clone(),
            },
            self.node_id,
        );

        self.write_and_apply(event).await
    }

    async fn get_consumer_offset(&self, group_id: &str, topic: &str, partition: u32) -> Result<Option<ConsumerOffset>> {
        let offsets = self.state.consumer_offsets.read().await;
        let key = (group_id.to_string(), topic.to_string(), partition);
        Ok(offsets.get(&key).cloned())
    }

    async fn update_partition_offset(&self, topic: &str, partition: u32, high_watermark: i64, log_start_offset: i64) -> Result<()> {
        // v2.2.13 CRITICAL FIX: Deduplicate high watermark events to prevent event storm
        // Before publishing event, check if watermark has actually changed
        let key = (topic.to_string(), partition);
        let watermark_changed = {
            let offsets = self.state.partition_offsets.read().await;
            match offsets.get(&key) {
                Some((current_hwm, _)) => *current_hwm != high_watermark,
                None => true, // First time, always publish
            }
        };

        // Only publish event if watermark actually changed
        if watermark_changed {
            let event = MetadataEvent::new_with_node(
                MetadataEventPayload::HighWatermarkUpdated {
                    topic: topic.to_string(),
                    partition: partition as i32,
                    new_watermark: high_watermark,
                },
                self.node_id,
            );

            self.write_and_apply(event).await?;
        }

        // Update log_start_offset MONOTONICALLY. The high-watermark hot path calls
        // this with log_start_offset=0; without the max() guard those calls would
        // reset a DeleteRecords low watermark back to 0 and resurrect deleted
        // records. Durable advancement goes through update_log_start_offset().
        let mut offsets = self.state.partition_offsets.write().await;
        if let Some((_hwm, lso)) = offsets.get_mut(&key) {
            if log_start_offset > *lso {
                *lso = log_start_offset;
            }
        } else {
            // Initialize if missing (shouldn't happen, but be defensive)
            offsets.insert(key, (high_watermark, log_start_offset));
        }

        Ok(())
    }

    async fn update_log_start_offset(&self, topic: &str, partition: u32, log_start_offset: i64) -> Result<i64> {
        let key = (topic.to_string(), partition);

        // Only advance; compute the effective value under the read lock first.
        let (should_write, effective) = {
            let offsets = self.state.partition_offsets.read().await;
            match offsets.get(&key) {
                Some((_hwm, current_lso)) => {
                    if log_start_offset > *current_lso {
                        (true, log_start_offset)
                    } else {
                        (false, *current_lso)
                    }
                }
                None => (true, log_start_offset),
            }
        };

        if should_write {
            let event = MetadataEvent::new_with_node(
                MetadataEventPayload::LogStartOffsetUpdated {
                    topic: topic.to_string(),
                    partition: partition as i32,
                    new_log_start_offset: log_start_offset,
                },
                self.node_id,
            );
            // write_and_apply persists to the metadata WAL and applies to state
            // (the apply arm enforces monotonicity again, defensively).
            self.write_and_apply(event).await?;
        }

        Ok(effective)
    }

    async fn delete_consumer_group(&self, group_id: &str) -> Result<()> {
        let event = MetadataEvent::new_with_node(
            MetadataEventPayload::ConsumerGroupDeleted {
                group_id: group_id.to_string(),
            },
            self.node_id,
        );
        self.write_and_apply(event).await
    }

    async fn delete_consumer_offsets(&self, group_id: &str, topic_partitions: &[(String, u32)]) -> Result<()> {
        // Resolve the concrete (topic, partition) set to delete. Empty input means
        // "all offsets for this group".
        let targets: Vec<(String, u32)> = if topic_partitions.is_empty() {
            let offsets = self.state.consumer_offsets.read().await;
            offsets
                .keys()
                .filter(|(g, _, _)| g == group_id)
                .map(|(_, t, p)| (t.clone(), *p))
                .collect()
        } else {
            topic_partitions.to_vec()
        };

        for (topic, partition) in targets {
            let event = MetadataEvent::new_with_node(
                MetadataEventPayload::ConsumerOffsetDeleted {
                    group_id: group_id.to_string(),
                    topic,
                    partition,
                },
                self.node_id,
            );
            self.write_and_apply(event).await?;
        }
        Ok(())
    }

    async fn get_partition_offset(&self, topic: &str, partition: u32) -> Result<Option<(i64, i64)>> {
        let offsets = self.state.partition_offsets.read().await;
        let key = (topic.to_string(), partition);
        Ok(offsets.get(&key).cloned())
    }

    async fn apply_replicated_event(&self, event: super::events::MetadataEvent) -> Result<()> {
        // v2.2.9 Phase 7: Apply replicated events from leader WITHOUT writing to WAL
        // This is for followers receiving events via WalReplicationManager
        self.state.apply_event(&event).await
    }

    async fn init_system_state(&self) -> Result<()> {
        // Replay events from WAL on startup will be called externally
        Ok(())
    }

    async fn create_topic_with_assignments(&self,
        topic_name: &str,
        config: TopicConfig,
        assignments: Vec<PartitionAssignment>,
        offsets: Vec<(u32, i64, i64)>
    ) -> Result<TopicMetadata> {
        // Create topic first
        let topic_metadata = self.create_topic(topic_name, config).await?;

        // Create partition assignments by emitting PartitionAssigned events
        for assignment in assignments {
            self.assign_partition(assignment).await?;
        }

        // Initialize partition offsets (high watermark and log start offset)
        // Note: offsets are already initialized to (0, 0) in TopicCreated event handler,
        // but we update them here in case caller provided different values
        for (partition, high_watermark, log_start_offset) in offsets {
            if high_watermark != 0 || log_start_offset != 0 {
                self.update_partition_offset(topic_name, partition, high_watermark, log_start_offset).await?;
            }
        }

        Ok(topic_metadata)
    }

    async fn commit_transactional_offsets(
        &self,
        _transactional_id: String,
        _producer_id: i64,
        _producer_epoch: i16,
        group_id: String,
        offsets: Vec<(String, u32, i64, Option<String>)>,
    ) -> Result<()> {
        // For now, commit offsets normally (transaction state tracking TBD)
        for (topic, partition, offset, metadata) in offsets {
            let consumer_offset = ConsumerOffset {
                group_id: group_id.clone(),
                topic,
                partition,
                offset,
                metadata,
                commit_timestamp: Utc::now(),
            };
            self.commit_offset(consumer_offset).await?;
        }
        Ok(())
    }

    // ===== Transaction coordinator methods (EOS) =====
    // Each writes an event-sourced coordinator state transition to the metadata WAL
    // (durable + replicated), then applies it to in-memory state via write_and_apply.

    async fn allocate_producer_id(&self) -> Result<i64> {
        Ok(self.state.next_producer_id.fetch_add(1, Ordering::SeqCst))
    }

    async fn get_transaction(&self, transactional_id: &str) -> Result<Option<TransactionMetadata>> {
        Ok(self.state.transactions.read().await.get(transactional_id).cloned())
    }

    async fn begin_transaction(&self, transactional_id: String, producer_id: i64, producer_epoch: i16, timeout_ms: i32) -> Result<()> {
        let event = MetadataEvent::new(MetadataEventPayload::BeginTransaction {
            transactional_id, producer_id, producer_epoch, transaction_timeout_ms: timeout_ms,
        });
        self.write_and_apply(event).await
    }

    async fn add_partitions_to_transaction(&self, transactional_id: String, producer_id: i64, producer_epoch: i16, partitions: Vec<(String, u32)>) -> Result<()> {
        let partitions = partitions.into_iter()
            .map(|(topic, partition)| TransactionPartition { topic, partition })
            .collect();
        let event = MetadataEvent::new(MetadataEventPayload::AddPartitionsToTransaction {
            transactional_id, producer_id, producer_epoch, partitions,
        });
        self.write_and_apply(event).await
    }

    async fn add_offsets_to_transaction(&self, transactional_id: String, producer_id: i64, producer_epoch: i16, group_id: String) -> Result<()> {
        let event = MetadataEvent::new(MetadataEventPayload::AddOffsetsToTransaction {
            transactional_id, producer_id, producer_epoch, group_id,
        });
        self.write_and_apply(event).await
    }

    async fn prepare_commit_transaction(&self, transactional_id: String, producer_id: i64, producer_epoch: i16) -> Result<()> {
        let event = MetadataEvent::new(MetadataEventPayload::PrepareCommit {
            transactional_id, producer_id, producer_epoch,
        });
        self.write_and_apply(event).await
    }

    async fn commit_transaction(&self, transactional_id: String, producer_id: i64, producer_epoch: i16) -> Result<()> {
        // Record the partitions/groups that were part of the transaction in the event
        // so the log is self-describing (used by control-marker writing).
        let committed_partitions = self.state.transactions.read().await
            .get(&transactional_id)
            .map(|t| t.partitions.iter().map(|(topic, partition)| TransactionPartition { topic: topic.clone(), partition: *partition }).collect())
            .unwrap_or_default();
        let event = MetadataEvent::new(MetadataEventPayload::CommitTransaction {
            transactional_id, producer_id, producer_epoch,
            committed_partitions, committed_offsets: Vec::new(),
        });
        self.write_and_apply(event).await
    }

    async fn abort_transaction(&self, transactional_id: String, producer_id: i64, producer_epoch: i16) -> Result<()> {
        let aborted_partitions = self.state.transactions.read().await
            .get(&transactional_id)
            .map(|t| t.partitions.iter().map(|(topic, partition)| TransactionPartition { topic: topic.clone(), partition: *partition }).collect())
            .unwrap_or_default();
        let event = MetadataEvent::new(MetadataEventPayload::AbortTransaction {
            transactional_id, producer_id, producer_epoch, aborted_partitions,
        });
        self.write_and_apply(event).await
    }

    async fn fence_producer(&self, transactional_id: String, old_producer_id: i64, old_producer_epoch: i16, new_producer_id: i64, new_producer_epoch: i16) -> Result<()> {
        let event = MetadataEvent::new(MetadataEventPayload::ProducerFenced {
            transactional_id, old_producer_id, old_producer_epoch, new_producer_id, new_producer_epoch,
        });
        self.write_and_apply(event).await
    }
}

#[cfg(test)]
mod transaction_coordinator_tests {
    use super::*;

    fn noop_store(node_id: u64) -> WalMetadataStore {
        // wal_append is a no-op sink: the coordinator state is exercised via the
        // in-memory apply path, which is exactly what recovery replays.
        let wal_append: WalAppendFn = Arc::new(|_bytes| Box::pin(async { Ok(0i64) }));
        WalMetadataStore::new(node_id, wal_append)
    }

    #[tokio::test]
    async fn producer_ids_are_monotonic_and_node_namespaced() {
        let node0 = noop_store(0);
        let a = node0.allocate_producer_id().await.unwrap();
        let b = node0.allocate_producer_id().await.unwrap();
        assert!(b > a, "producer ids must be monotonic");

        // Different nodes hand out disjoint ranges (no coordination needed).
        let node1 = noop_store(1);
        let c = node1.allocate_producer_id().await.unwrap();
        assert!(c > b + 1_000_000, "node 1's range must be far above node 0's early ids");
    }

    #[tokio::test]
    async fn full_transaction_lifecycle_tracks_state_and_partitions() {
        let store = noop_store(0);
        let txn_id = "orders-tx".to_string();
        let pid = store.allocate_producer_id().await.unwrap();

        // InitProducerId registration: Empty, no partitions.
        store.begin_transaction(txn_id.clone(), pid, 0, 60000).await.unwrap();
        let t = store.get_transaction(&txn_id).await.unwrap().unwrap();
        assert_eq!(t.state, TransactionState::Empty);
        assert!(t.partitions.is_empty());

        // First AddPartitionsToTxn starts the transaction (Ongoing) and enrolls partitions.
        store.add_partitions_to_transaction(txn_id.clone(), pid, 0, vec![("orders".into(), 0), ("orders".into(), 1)]).await.unwrap();
        let t = store.get_transaction(&txn_id).await.unwrap().unwrap();
        assert_eq!(t.state, TransactionState::Ongoing);
        assert_eq!(t.partitions.len(), 2);
        assert!(t.partitions.contains(&("orders".to_string(), 1)));

        // Commit: PrepareCommit -> CommitTransaction clears enrollment, state CompleteCommit.
        store.prepare_commit_transaction(txn_id.clone(), pid, 0).await.unwrap();
        assert_eq!(store.get_transaction(&txn_id).await.unwrap().unwrap().state, TransactionState::PrepareCommit);
        store.commit_transaction(txn_id.clone(), pid, 0).await.unwrap();
        let t = store.get_transaction(&txn_id).await.unwrap().unwrap();
        assert_eq!(t.state, TransactionState::CompleteCommit);
        assert!(t.partitions.is_empty(), "enrollment cleared after commit");
    }

    #[tokio::test]
    async fn abort_marks_complete_abort_and_clears_partitions() {
        let store = noop_store(0);
        let txn_id = "t".to_string();
        let pid = store.allocate_producer_id().await.unwrap();
        store.begin_transaction(txn_id.clone(), pid, 0, 60000).await.unwrap();
        store.add_partitions_to_transaction(txn_id.clone(), pid, 0, vec![("t".into(), 0)]).await.unwrap();
        store.abort_transaction(txn_id.clone(), pid, 0).await.unwrap();
        let t = store.get_transaction(&txn_id).await.unwrap().unwrap();
        assert_eq!(t.state, TransactionState::CompleteAbort);
        assert!(t.partitions.is_empty());
    }

    #[tokio::test]
    async fn recovery_replays_events_into_identical_state() {
        // Simulate a producer session, capturing the events, then replay them into a
        // fresh store (as recovery does) and assert the coordinator state matches.
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::<MetadataEvent>::new()));
        let cap = events.clone();
        let wal_append: WalAppendFn = Arc::new(move |bytes| {
            let cap = cap.clone();
            Box::pin(async move {
                if let Ok(ev) = MetadataEvent::from_bytes(&bytes) {
                    cap.lock().unwrap().push(ev);
                }
                Ok(0i64)
            })
        });
        let store = WalMetadataStore::new(0, wal_append);
        let pid = store.allocate_producer_id().await.unwrap();
        store.begin_transaction("tx".into(), pid, 0, 60000).await.unwrap();
        store.add_partitions_to_transaction("tx".into(), pid, 0, vec![("t".into(), 3)]).await.unwrap();

        // Replay captured events into a fresh store.
        let fresh = noop_store(0);
        for ev in events.lock().unwrap().iter() {
            fresh.state.apply_event(ev).await.unwrap();
        }
        let t = fresh.get_transaction("tx").await.unwrap().unwrap();
        assert_eq!(t.state, TransactionState::Ongoing);
        assert_eq!(t.producer_id, pid);
        assert!(t.partitions.contains(&("t".to_string(), 3)));
        // Recovery must advance the allocator past the recovered producer id.
        assert!(fresh.allocate_producer_id().await.unwrap() > pid);
    }

    #[tokio::test]
    async fn is_fenced_detects_stale_producer() {
        let store = noop_store(0);
        let pid = store.allocate_producer_id().await.unwrap();
        store.begin_transaction("tx".into(), pid, 5, 60000).await.unwrap();
        let t = store.get_transaction("tx").await.unwrap().unwrap();
        assert!(t.is_fenced(pid, 4), "lower epoch is fenced");
        assert!(t.is_fenced(pid + 1, 5), "different producer id is fenced");
        assert!(!t.is_fenced(pid, 5), "current producer/epoch is not fenced");
    }
}

#[cfg(test)]
mod replication_filter_tests {
    use super::*;

    fn segment_meta() -> SegmentMetadata {
        SegmentMetadata {
            segment_id: "s0".to_string(),
            topic: "t".to_string(),
            partition: 0,
            start_offset: 0,
            end_offset: 1,
            size: 1,
            record_count: 1,
            path: "p".to_string(),
            created_at: chrono::Utc::now(),
        }
    }

    /// Catalog + consumer + watermark events MUST replicate to followers.
    #[test]
    fn catalog_events_replicate() {
        assert!(replicate_to_followers(&MetadataEventPayload::TopicCreated {
            name: "t".to_string(),
            config: TopicConfig::default(),
        }));
        assert!(replicate_to_followers(&MetadataEventPayload::TopicDeleted {
            name: "t".to_string(),
        }));
        // Watermarks are low-volume and kept replicated (only segment-index events
        // are filtered); guards against over-filtering.
        assert!(replicate_to_followers(&MetadataEventPayload::HighWatermarkUpdated {
            topic: "t".to_string(),
            partition: 0,
            new_watermark: 42,
        }));
    }

    /// High-frequency segment-index events (derived local state, ~45:1 vs
    /// TopicCreated) MUST NOT replicate — keeping them off the bus is what stops
    /// the flood that starved catalog replication.
    #[test]
    fn segment_index_events_do_not_replicate() {
        assert!(!replicate_to_followers(&MetadataEventPayload::SegmentCreated {
            metadata: segment_meta(),
        }));
        assert!(!replicate_to_followers(&MetadataEventPayload::SegmentDeleted {
            topic: "t".to_string(),
            partition: 0,
            segment_id: "s0".to_string(),
        }));
        assert!(!replicate_to_followers(&MetadataEventPayload::ParquetSegmentDeleted {
            topic: "t".to_string(),
            partition: 0,
            segment_id: "s0".to_string(),
        }));
    }
}
