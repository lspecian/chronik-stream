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
    /// ACL bindings (Security Phase 3). Replicated and recovered like topics,
    /// so a rule survives restart and applies on every broker.
    acls: RwLock<Vec<AclBindingRecord>>,
    /// SCRAM credentials, keyed by (username, mechanism code).
    scram_credentials: RwLock<HashMap<(String, i8), ScramCredentialRecord>>,
    /// Timestamp of the newest `CatalogSnapshot` applied, so an older or
    /// replayed one cannot undo a prune. `None` until the first is seen.
    last_catalog_snapshot: RwLock<Option<chrono::DateTime<chrono::Utc>>>,
    /// Topic count seen on the previous anti-entropy pass, so a snapshot is
    /// only published once the local catalog has stopped changing.
    last_snapshot_size: RwLock<Option<usize>>,
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
            acls: RwLock::new(Vec::new()),
            scram_credentials: RwLock::new(HashMap::new()),
            last_catalog_snapshot: RwLock::new(None),
            last_snapshot_size: RwLock::new(None),
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
            MetadataEventPayload::TopicCreated { name, config, auto_created } => {
                let mut topics = self.topics.write().await;
                if let Some(existing) = topics.get_mut(name) {
                    // Provenance settles this, not size.
                    //
                    // An auto-create must never change a config someone asked
                    // for. Comparing partition counts and keeping the larger got
                    // this right in one direction only: `TopicConfig::default()`
                    // carries 3 partitions, so an auto-create racing a topic
                    // created with `--partitions 1` silently widened it to 3.
                    // Measured on a cluster — `/admin/status` reported three
                    // partitions with real leaders and ISRs while every record
                    // sat in partition 0, because the producer had seen one
                    // partition when it wrote. A consumer then subscribes to
                    // partitions the producer never knew about.
                    //
                    // The config-key merge below still runs, so an auto-create
                    // can still contribute keys the explicit config never set.
                    if *auto_created && !existing.auto_created {
                        tracing::debug!(
                            topic = %name,
                            existing_partitions = existing.config.partition_count,
                            incoming_partitions = config.partition_count,
                            "Ignoring auto-create for a topic that was created explicitly"
                        );
                    } else if !*auto_created && existing.auto_created {
                        // The other direction: what is here was guessed, and this
                        // was asked for. It wins whatever the counts say — which
                        // is the point of tracking provenance, because an
                        // explicit `--partitions 1` landing on an auto-created 3
                        // must be able to narrow it.
                        let old_count = existing.config.partition_count;
                        tracing::info!(
                            topic = %name,
                            auto_created_partitions = old_count,
                            explicit_partitions = config.partition_count,
                            "Explicit topic config replaces the auto-created one"
                        );
                        existing.config = config.clone();
                        existing.auto_created = false;
                        existing.updated_at = event.timestamp;
                        drop(topics);

                        let mut offsets = self.partition_offsets.write().await;
                        for partition in 0..config.partition_count {
                            offsets.entry((name.clone(), partition)).or_insert((0, 0));
                        }
                        return Ok(());
                    } else if config.partition_count > existing.config.partition_count {
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
                    auto_created: *auto_created,
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

            MetadataEventPayload::ScramCredentialUpserted { credential } => {
                // Upsert: altering a user's password replaces the credential for
                // that mechanism rather than accumulating stale ones.
                let mut creds = self.scram_credentials.write().await;
                creds.insert(
                    (credential.username.clone(), credential.mechanism),
                    credential.clone(),
                );
                Ok(())
            }

            MetadataEventPayload::ScramCredentialDeleted { username, mechanism } => {
                let mut creds = self.scram_credentials.write().await;
                creds.remove(&(username.clone(), *mechanism));
                Ok(())
            }

            MetadataEventPayload::AclCreated { binding } => {
                let mut acls = self.acls.write().await;
                // Idempotent: replaying the log on recovery, or receiving the
                // same event twice from replication, must not multiply the rule.
                if !acls.iter().any(|existing| existing == binding) {
                    acls.push(binding.clone());
                }
                Ok(())
            }

            MetadataEventPayload::AclDeleted { binding } => {
                let mut acls = self.acls.write().await;
                acls.retain(|existing| existing != binding);
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

            MetadataEventPayload::CatalogSnapshot { .. } => {
                // Handled by `WalMetadataStore::apply_replicated_event`, which
                // turns a snapshot into ordinary `TopicDeleted` events so the
                // prune is durable and survives replay. Nothing to do at the
                // state level, and replaying a historical snapshot must not
                // re-prune against a catalog that has legitimately moved on.
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

                // RP-5: an assignment carrying an OLDER leader epoch describes a
                // leadership that has already been superseded, and applying it
                // would move the partition back to a node that has since lost it.
                //
                // This was blind last-writer-wins, and it undid failover on a
                // real cluster. Node 1 died, the partition failed over to node 2,
                // node 2 took 200 acknowledged records — then node 1 came back,
                // replayed its own metadata WAL (stale by exactly the change that
                // demoted it), and its assignment overwrote the newer one on every
                // node. All three then agreed the leader was node 1, which held
                // no data for that partition, and consumers reading from it got
                // **zero of 400 acknowledged records**. The data was intact on
                // node 2 the whole time; metadata had simply pointed away from it.
                //
                // Leader epochs are monotonic per partition and already derived in
                // exactly one place (`assign_partition`), so they are the
                // causality token this needs. Equal epochs still apply: the
                // anti-entropy loop re-asserts unchanged assignments constantly,
                // and those are idempotent.
                //
                // The `get` guard must not be held across the `insert` — a DashMap
                // Ref live at that point deadlocks the shard.
                let superseded = self
                    .partition_assignments
                    .get(&key)
                    .map(|existing| assignment.leader_epoch < existing.leader_epoch)
                    .unwrap_or(false);

                if superseded {
                    let current = self
                        .partition_assignments
                        .get(&key)
                        .map(|e| (e.leader_id, e.leader_epoch));
                    tracing::warn!(
                        "Ignoring stale assignment for {}-{}: it names leader {} at epoch {}, \
                         but this node already holds {:?} — a returning node must not undo a failover",
                        assignment.topic,
                        assignment.partition,
                        assignment.leader_id,
                        assignment.leader_epoch,
                        current
                    );
                    return Ok(());
                }

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

    /// Reconcile the local catalog against the authoritative set.
    ///
    /// Turns a `CatalogSnapshot` into ordinary `TopicDeleted` events rather than
    /// mutating state directly. Three reasons: the prune becomes durable through
    /// the same path an explicit delete takes, it survives replay without
    /// persisting a multi-thousand-entry snapshot into the metadata WAL every
    /// anti-entropy pass, and it reuses a code path that is already exercised.
    ///
    /// This is the only way a topic is removed without someone asking, so every
    /// ambiguous case fails towards keeping data.
    async fn reconcile_catalog(
        &self,
        authoritative: &[String],
        snapshot_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<()> {
        if std::env::var("CHRONIK_METADATA_CATALOG_PRUNE")
            .map(|v| v.eq_ignore_ascii_case("false") || v == "0")
            .unwrap_or(false)
        {
            return Ok(());
        }

        // An empty catalog is far more likely a node that has not finished
        // recovering than a cluster that genuinely holds no topics. Refusing it
        // costs a stale entry until the next pass; obeying it would wipe the
        // local catalog.
        if authoritative.is_empty() {
            let local = self.state.topics.read().await.len();
            if local > 0 {
                tracing::warn!(
                    local_topics = local,
                    "Ignoring empty CatalogSnapshot: refusing to prune every topic"
                );
            }
            return Ok(());
        }

        // Reject anything not newer than the last snapshot applied, so a delayed
        // or reordered message cannot resurrect what a newer pass pruned.
        {
            let mut last = self.state.last_catalog_snapshot.write().await;
            if let Some(prev) = *last {
                if snapshot_at <= prev {
                    return Ok(());
                }
            }
            *last = Some(snapshot_at);
        }

        let keep: std::collections::HashSet<&String> = authoritative.iter().collect();
        let stale: Vec<String> = {
            let topics = self.state.topics.read().await;
            topics
                .keys()
                // Internal topics are per-node and excluded from the snapshot, so
                // their absence carries no meaning.
                .filter(|name| !name.starts_with("__") && !keep.contains(name))
                .cloned()
                .collect()
        };

        if stale.is_empty() {
            return Ok(());
        }

        // A leader that has not finished rebuilding its own catalog publishes a
        // PARTIAL snapshot, and obeying it would prune everyone down to whatever
        // it had recovered so far. Observed directly: a node 60s into a restart
        // reported 16 topics of 5,611. The empty-snapshot guard above does not
        // catch that — 16 is not zero.
        //
        // A mass prune is therefore treated as evidence of a partial snapshot
        // rather than an instruction. Legitimate bulk deletion still happens, it
        // just arrives as explicit `TopicDeleted` events, which this path does
        // not gate.
        let local_total = {
            let topics = self.state.topics.read().await;
            topics.keys().filter(|n| !n.starts_with("__")).count()
        };
        let max_fraction = std::env::var("CHRONIK_METADATA_MAX_PRUNE_FRACTION")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|f| *f > 0.0 && *f <= 1.0)
            .unwrap_or(0.20);
        if local_total > 0 && (stale.len() as f64) > (local_total as f64) * max_fraction {
            tracing::warn!(
                would_prune = stale.len(),
                local_topics = local_total,
                authoritative = authoritative.len(),
                "Refusing CatalogSnapshot that would prune a large fraction of the catalog - treating it as a partial snapshot from a node that is still recovering"
            );
            return Ok(());
        }

        tracing::info!(
            pruning = stale.len(),
            authoritative = authoritative.len(),
            "CatalogSnapshot: removing topics the leader no longer has"
        );

        for name in stale {
            let event = MetadataEvent::new_with_node(
                MetadataEventPayload::TopicDeleted { name: name.clone() },
                self.node_id,
            );
            if let Err(e) = self.write_and_apply(event).await {
                tracing::warn!(topic = %name, error = %e, "Failed to prune stale topic");
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
    pub async fn apply_replicated_event_durable(&self, event: MetadataEvent) -> Result<()> {
        // A snapshot is a reconciliation signal, not durable state: handle it
        // here (where the WAL is reachable) and never persist the snapshot
        // itself.
        if let MetadataEventPayload::CatalogSnapshot { topics } = &event.payload {
            return self.reconcile_catalog(topics, event.timestamp).await;
        }

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

    /// Re-broadcast the whole catalog — topics **and** their partition
    /// assignments — via the event bus.
    ///
    /// Called by the leader after startup once metadata replication is wired up,
    /// so followers that restarted (or joined late, or missed an event) heal.
    ///
    /// ## Why assignments are not optional here
    ///
    /// This used to re-broadcast `TopicCreated` only. A healed follower then knew
    /// every topic and *no* partition assignment — which under push cost nothing,
    /// because the leader drives replication and only the leader's view has to be
    /// right. Under follower-pull it is fatal: a follower with no assignment
    /// cannot tell who leads a partition, so it fetches nothing and replicates
    /// nothing, silently, while the leader's `/admin/status` still reports
    /// `isr:[1,2,3]` from its own intact view.
    ///
    /// Measured on a 3-node cluster before this fix, same moment, 69 partitions:
    /// node 1 knew a leader for 66, node 3 for 21, node 2 for **zero**.
    ///
    /// Safe to call repeatedly — `apply_replicated_event` is idempotent for both
    /// event types (topic created only if absent; assignment is an upsert).

    /// Publish the complete topic set so followers can prune what they should
    /// no longer have.
    ///
    /// `broadcast_all_topics` is additive: it re-asserts what exists, which
    /// heals a node that is *missing* topics but can never remove one it kept
    /// through a `TopicDeleted` it never received. This states the whole set, so
    /// absence becomes information.
    ///
    /// Internal (`__`-prefixed) topics are excluded here and exempt from pruning
    /// on the apply side — they are managed per-node, not replicated, and a
    /// snapshot that omitted them would otherwise read as "delete them".
    ///
    /// Returns the number of topics published, or 0 if the event bus is not
    /// wired up.
    pub async fn broadcast_catalog_snapshot(&self) -> usize {
        let publish_fn = match self.event_bus_publish {
            Some(ref f) => f,
            None => {
                tracing::warn!("Cannot broadcast catalog snapshot: event bus not wired up");
                return 0;
            }
        };

        let topics: Vec<String> = {
            let topics = self.state.topics.read().await;
            topics
                .keys()
                .filter(|name| !name.starts_with("__"))
                .cloned()
                .collect()
        };

        // Never publish an empty snapshot. The apply side refuses one anyway,
        // but a node that has not finished recovering should not be sending
        // "there are no topics" to its peers in the first place.
        if topics.is_empty() {
            return 0;
        }

        // Nor publish one until this node's own catalog has stopped changing.
        //
        // Recovery rebuilds the catalog incrementally — a node 60s into a
        // restart reported 16 topics of 5,611 — and a snapshot published from
        // that state tells every follower to delete almost everything. Requiring
        // two consecutive passes to agree means the first pass after a restart
        // only ever arms the check; nothing is asserted until the count holds
        // still.
        {
            let mut last = self.state.last_snapshot_size.write().await;
            let settled = *last == Some(topics.len());
            *last = Some(topics.len());
            if !settled {
                tracing::debug!(
                    topics = topics.len(),
                    "Catalog still settling - not publishing a snapshot this pass"
                );
                return 0;
            }
        }

        let count = topics.len();
        let event = MetadataEvent::new_with_node(
            MetadataEventPayload::CatalogSnapshot { topics },
            self.node_id,
        );
        let subscribers = publish_fn(event);
        tracing::debug!(
            topics = count,
            subscribers,
            "Published CatalogSnapshot so followers can prune stale topics"
        );
        count
    }
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
                    auto_created: meta.auto_created,
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
        drop(topics);

        // Partition assignments carry the leader, which is what a pulling
        // follower needs in order to fetch at all. Broadcast them after the
        // topics so a follower applying in order never sees an assignment for a
        // topic it does not yet know.
        let mut assignments_broadcast = 0;
        for entry in self.state.partition_assignments.iter() {
            let assignment = entry.value().clone();
            if assignment.topic.starts_with("__") {
                continue;
            }
            let event = MetadataEvent::new_with_node(
                MetadataEventPayload::PartitionAssigned { assignment },
                self.node_id,
            );
            publish_fn(event);
            assignments_broadcast += 1;
        }

        tracing::info!(
            topics_broadcast = count,
            assignments_broadcast,
            "Metadata re-broadcast complete"
        );
        count
    }

    /// Both with-assignments create paths, differing only in provenance.
    async fn with_assignments_inner(
        &self,
        topic_name: &str,
        config: TopicConfig,
        assignments: Vec<PartitionAssignment>,
        offsets: Vec<(u32, i64, i64)>,
        auto_created: bool,
    ) -> Result<TopicMetadata> {
        let topic_metadata = self.create_topic_inner(topic_name, config, auto_created).await?;

        for assignment in assignments {
            self.assign_partition(assignment).await?;
        }

        // Offsets are already initialised to (0, 0) by the TopicCreated apply;
        // this only matters when the caller supplied different values.
        for (partition, high_watermark, log_start_offset) in offsets {
            if high_watermark != 0 || log_start_offset != 0 {
                self.update_partition_offset(topic_name, partition, high_watermark, log_start_offset).await?;
            }
        }

        Ok(topic_metadata)
    }

    /// Both create paths, differing only in whether the config was asked for.
    async fn create_topic_inner(
        &self,
        name: &str,
        config: TopicConfig,
        auto_created: bool,
    ) -> Result<TopicMetadata> {
        {
            let topics = self.state.topics.read().await;
            if topics.contains_key(name) {
                return Err(MetadataError::AlreadyExists(format!("Topic {} already exists", name)));
            }
        }

        let event = MetadataEvent::new_with_node(
            MetadataEventPayload::TopicCreated {
                name: name.to_string(),
                config,
                auto_created,
            },
            self.node_id,
        );

        self.write_and_apply(event).await?;

        let topics = self.state.topics.read().await;
        topics.get(name).cloned()
            .ok_or_else(|| MetadataError::NotFound(format!("Topic {} not found after creation", name)))
    }
}

#[async_trait]
impl MetadataStore for WalMetadataStore {
    async fn create_acl(&self, binding: AclBindingRecord) -> Result<()> {
        // Written through the event log, so it replicates to followers and is
        // replayed on recovery exactly like a topic.
        let event = MetadataEvent::new_with_node(
            MetadataEventPayload::AclCreated { binding },
            self.node_id,
        );
        self.write_and_apply(event).await
    }

    async fn delete_acl(&self, binding: AclBindingRecord) -> Result<()> {
        let event = MetadataEvent::new_with_node(
            MetadataEventPayload::AclDeleted { binding },
            self.node_id,
        );
        self.write_and_apply(event).await
    }

    async fn list_acls(&self) -> Result<Vec<AclBindingRecord>> {
        Ok(self.state.acls.read().await.clone())
    }

    async fn upsert_scram_credential(&self, credential: ScramCredentialRecord) -> Result<()> {
        let event = MetadataEvent::new_with_node(
            MetadataEventPayload::ScramCredentialUpserted { credential },
            self.node_id,
        );
        self.write_and_apply(event).await
    }

    async fn delete_scram_credential(&self, username: &str, mechanism: i8) -> Result<()> {
        let event = MetadataEvent::new_with_node(
            MetadataEventPayload::ScramCredentialDeleted {
                username: username.to_string(),
                mechanism,
            },
            self.node_id,
        );
        self.write_and_apply(event).await
    }

    async fn list_scram_credentials(&self) -> Result<Vec<ScramCredentialRecord>> {
        Ok(self
            .state
            .scram_credentials
            .read()
            .await
            .values()
            .cloned()
            .collect())
    }

    async fn create_topic(&self, name: &str, config: TopicConfig) -> Result<TopicMetadata> {
        self.create_topic_inner(name, config, false).await
    }

    async fn auto_create_topic(&self, name: &str, config: TopicConfig) -> Result<TopicMetadata> {
        self.create_topic_inner(name, config, true).await
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

    async fn assign_partition(&self, mut assignment: PartitionAssignment) -> Result<()> {
        // RP-3: the leader epoch is derived here, never supplied by the caller.
        // There are a dozen construction sites for PartitionAssignment across
        // the tree — rebalancer, admin API, auto-create, protocol handler — and
        // each would be a chance to skip the bump or reuse a value. Owning it in
        // one place makes "increments if and only if the leader changed" a
        // property of the store rather than a convention.
        //
        // A leader that has not changed keeps its epoch: re-asserting the same
        // assignment (anti-entropy re-broadcast, an idempotent rebalance pass)
        // must not manufacture leadership changes, or every follower would think
        // it had to truncate.
        let key = (assignment.topic.clone(), assignment.partition);
        let previous_seen = self
            .state
            .partition_assignments
            .get(&key)
            .map(|p| (p.leader_id, p.leader_epoch, p.isr.clone()));

        // An empty `isr` means "I do not know", not "nobody is in sync".
        //
        // Most callers — the rebalancer, auto-create, the admin API, failover
        // itself — construct an assignment to say something about *placement*
        // and have no idea which replicas are caught up. Letting their silence
        // erase a published in-sync set would leave failover choosing from
        // nothing moments after a leader change, which is precisely when it must
        // choose well. Only a caller that actually measured the set overwrites it.
        if assignment.isr.is_empty() {
            if let Some((_, _, previous_isr)) = &previous_seen {
                assignment.isr = previous_isr.clone();
            }
        }

        let previous_seen = previous_seen.map(|(leader_id, epoch, _)| (leader_id, epoch));
        assignment.leader_epoch = match previous_seen {
            Some((leader_id, epoch)) if leader_id == assignment.leader_id => epoch,
            Some((_, epoch)) => epoch.saturating_add(1),
            None => 0,
        };

        // A leader change is rare and consequential — it is the event that tells
        // every follower to re-run the RP-3.3 epoch handshake — so say it out
        // loud, including the case where this node had no previous assignment
        // and therefore starts the epoch at 0. That case is indistinguishable
        // from "no leader change" in the data, and it silently disarms epoch
        // truncation for the partition: every record ends up carrying epoch 0,
        // so `end_offset_for_epoch` always answers "current epoch, log end" and
        // no follower ever truncates.
        match previous_seen {
            Some((leader_id, _)) if leader_id != assignment.leader_id => {
                tracing::info!(
                    "{}-{}: leader {} → {}, epoch {}",
                    assignment.topic, assignment.partition,
                    leader_id, assignment.leader_id, assignment.leader_epoch
                );
            }
            None => {
                tracing::info!(
                    "{}-{}: first assignment on this node — leader {} at epoch 0",
                    assignment.topic, assignment.partition, assignment.leader_id
                );
            }
            _ => {}
        }

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

    async fn get_partition_leader_epoch(&self, topic: &str, partition: u32) -> Result<Option<i32>> {
        // Lock-free O(1) get — this is called once per produced batch.
        let key = (topic.to_string(), partition);
        Ok(self.state.partition_assignments.get(&key).map(|a| a.leader_epoch))
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

    async fn list_consumer_groups(&self) -> Result<Vec<ConsumerGroupMetadata>> {
        let groups = self.state.consumer_groups.read().await;
        Ok(groups.values().cloned().collect())
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
        // This is the path production takes: followers hold the store as
        // `Arc<dyn MetadataStore>`, so this trait method runs, not the inherent
        // one. It delegates rather than duplicating, because the two drifted and
        // the difference was the bug.
        //
        // It used to apply to in-memory state only — "WITHOUT writing to WAL" —
        // while the inherent method it shadowed carried a doc comment promising
        // that replicated metadata "survives pod restarts". It did not: a
        // follower lost every topic and partition assignment it had learned by
        // replication on every restart, keeping only what it wrote itself, and
        // then depended entirely on the leader's anti-entropy pass to refill it.
        //
        // Measured on a 3-node cluster: a restarted node reported 17 of 5,612
        // partition assignments and sat there for nine minutes. It was the Raft
        // leader, and the leader is the only node that re-broadcasts, so nothing
        // could refill it until leadership moved — at which point it went to
        // 5,612 within one pass.
        self.apply_replicated_event_durable(event).await
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
        self.with_assignments_inner(topic_name, config, assignments, offsets, false).await
    }

    async fn auto_create_topic_with_assignments(&self,
        topic_name: &str,
        config: TopicConfig,
        assignments: Vec<PartitionAssignment>,
        offsets: Vec<(u32, i64, i64)>
    ) -> Result<TopicMetadata> {
        self.with_assignments_inner(topic_name, config, assignments, offsets, true).await
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
            auto_created: false,
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

#[cfg(test)]
mod catalog_healing_tests {
    use super::*;
    use std::sync::Mutex;


    /// A follower that missed a `TopicDeleted` while it was down keeps the topic
    /// forever: the anti-entropy pass re-publishes `TopicCreated`, which can add
    /// what is missing but never removes what should be gone.
    ///
    /// Measured on a 3-node cluster: one node held 886 topics the other two had
    /// deleted, 871 of them real, for months.
    #[tokio::test]
    async fn a_catalog_snapshot_prunes_topics_the_leader_no_longer_has() {
        let wal_append: WalAppendFn = Arc::new(|_bytes| Box::pin(async { Ok(0i64) }));
        let store = WalMetadataStore::new(1, wal_append);

        // Ten topics, one stale: 10%, inside the mass-prune ceiling. A larger
        // fraction is treated as a partial snapshot - see
        // `a_partial_snapshot_does_not_prune_the_catalog`.
        let mut keep: Vec<String> = Vec::new();
        for i in 0..9 {
            let name = format!("kept-{}", i);
            store.create_topic(&name, TopicConfig::default()).await.unwrap();
            keep.push(name);
        }
        store
            .create_topic("deleted-while-i-was-down", TopicConfig::default())
            .await
            .unwrap();
        assert_eq!(store.list_topics().await.unwrap().len(), 10);

        store
            .apply_replicated_event(MetadataEvent::new_with_node(
                MetadataEventPayload::CatalogSnapshot { topics: keep },
                2,
            ))
            .await
            .unwrap();

        let names: Vec<String> = store
            .list_topics()
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.name)
            .collect();
        assert_eq!(names.len(), 9, "the stale topic was not pruned: {:?}", names);
        assert!(!names.contains(&"deleted-while-i-was-down".to_string()));
        assert!(names.contains(&"kept-0".to_string()));
    }



    /// Replicated metadata must be persisted, not just applied in memory.
    ///
    /// A follower learns topics and partition assignments by replication. If it
    /// only applies them to memory, every restart throws them away and the node
    /// depends entirely on the leader's anti-entropy pass to be told again —
    /// and the leader is the only node that re-broadcasts, so a restarted
    /// *leader* has nobody to refill it.
    ///
    /// Measured on a 3-node cluster: a restarted node reported 17 of 5,612
    /// partition assignments and stayed there for nine minutes, recovering only
    /// once leadership moved to a node with a full view.
    ///
    /// This drives the `MetadataStore` trait method deliberately, because that
    /// is the one followers reach through `Arc<dyn MetadataStore>`.
    #[tokio::test]
    async fn replicated_metadata_is_persisted_for_restart() {
        let appended: Arc<Mutex<Vec<MetadataEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&appended);
        let wal_append: WalAppendFn = Arc::new(move |bytes: Vec<u8>| {
            let sink = Arc::clone(&sink);
            Box::pin(async move {
                if let Ok(event) = MetadataEvent::from_bytes(&bytes) {
                    sink.lock().unwrap().push(event);
                }
                Ok(0i64)
            })
        });
        let follower = WalMetadataStore::new(2, wal_append);

        // Exactly what a follower receives from the leader.
        let store: &dyn MetadataStore = &follower;
        store
            .apply_replicated_event(MetadataEvent::new_with_node(
                MetadataEventPayload::TopicCreated {
                    name: "replicated".to_string(),
                    config: TopicConfig::default(),
                    auto_created: false,
                },
                1,
            ))
            .await
            .unwrap();
        store
            .apply_replicated_event(MetadataEvent::new_with_node(
                MetadataEventPayload::PartitionAssigned {
                    assignment: PartitionAssignment {
                        topic: "replicated".to_string(),
                        partition: 0,
                        broker_id: 1,
                        is_leader: true,
                        replicas: vec![1, 2, 3],
                        leader_id: 1,
                        leader_epoch: 1,
                        isr: vec![1, 2, 3],
                    },
                },
                1,
            ))
            .await
            .unwrap();

        let written = appended.lock().unwrap().clone();
        assert!(
            written.iter().any(|e| matches!(
                &e.payload,
                MetadataEventPayload::TopicCreated { name, .. } if name == "replicated"
            )),
            "a replicated topic was not persisted - it will vanish on restart"
        );
        assert!(
            written.iter().any(|e| matches!(
                &e.payload,
                MetadataEventPayload::PartitionAssigned { assignment }
                    if assignment.topic == "replicated"
            )),
            "a replicated partition assignment was not persisted - the node will \
             come back not knowing who leads this partition, and under follower-pull \
             it cannot replicate it at all"
        );

        // And it survives the restart.
        let noop: WalAppendFn = Arc::new(|_b| Box::pin(async { Ok(0i64) }));
        let restarted = WalMetadataStore::new(2, noop);
        restarted.replay_events(written).await.unwrap();
        assert_eq!(
            restarted.list_topics().await.unwrap().len(),
            1,
            "the replicated topic did not survive replay"
        );
        assert!(
            restarted
                .get_partition_assignments("replicated")
                .await
                .unwrap()
                .iter()
                .any(|a| a.partition == 0),
            "the replicated assignment did not survive replay"
        );
    }

    /// A deleted topic must stay deleted across a restart.
    ///
    /// Recovery replays the metadata WAL, so a delete's durability depends
    /// entirely on the `TopicDeleted` event being in that log and being applied
    /// after the `TopicCreated` it cancels. If it is not, every restart
    /// resurrects every topic ever deleted — which is what a 3-node cluster
    /// showed: 886 topics that had been deleted were back on all three nodes
    /// after a roll.
    #[tokio::test]
    async fn a_deleted_topic_stays_deleted_across_replay() {
        let appended: Arc<Mutex<Vec<MetadataEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&appended);
        let wal_append: WalAppendFn = Arc::new(move |bytes: Vec<u8>| {
            let sink = Arc::clone(&sink);
            Box::pin(async move {
                if let Ok(event) = MetadataEvent::from_bytes(&bytes) {
                    sink.lock().unwrap().push(event);
                }
                Ok(0i64)
            })
        });

        let store = WalMetadataStore::new(1, wal_append);
        store.create_topic("survivor", TopicConfig::default()).await.unwrap();
        store.create_topic("doomed", TopicConfig::default()).await.unwrap();
        store.delete_topic("doomed").await.unwrap();
        assert_eq!(store.list_topics().await.unwrap().len(), 1, "delete did not apply live");

        let replayed = appended.lock().unwrap().clone();
        assert!(
            replayed.iter().any(|e| matches!(
                &e.payload,
                MetadataEventPayload::TopicDeleted { name } if name == "doomed"
            )),
            "TopicDeleted was never written to the WAL - a delete cannot survive a restart"
        );

        // Restart: a fresh store replaying exactly what the WAL holds.
        let noop: WalAppendFn = Arc::new(|_b| Box::pin(async { Ok(0i64) }));
        let recovered = WalMetadataStore::new(1, noop);
        recovered.replay_events(replayed).await.unwrap();

        let names: Vec<String> = recovered
            .list_topics()
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.name)
            .collect();
        assert!(
            !names.contains(&"doomed".to_string()),
            "a deleted topic came back after replay: {:?}",
            names
        );
        assert!(names.contains(&"survivor".to_string()));
    }

    /// The prune must be durable.
    ///
    /// A follower's WAL still contains `TopicCreated` for a phantom topic. If the
    /// prune only changed in-memory state, the next restart would replay that
    /// creation and the phantom would come back — the bug would heal every 5
    /// minutes and return on every restart, forever.
    #[tokio::test]
    async fn pruning_writes_topic_deleted_to_the_wal() {
        let appended: Arc<Mutex<Vec<MetadataEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&appended);
        let wal_append: WalAppendFn = Arc::new(move |bytes: Vec<u8>| {
            let sink = Arc::clone(&sink);
            Box::pin(async move {
                if let Ok(event) = MetadataEvent::from_bytes(&bytes) {
                    sink.lock().unwrap().push(event);
                }
                Ok(0i64)
            })
        });
        let store = WalMetadataStore::new(1, wal_append);

        let mut keep: Vec<String> = Vec::new();
        for i in 0..9 {
            let name = format!("kept-{}", i);
            store.create_topic(&name, TopicConfig::default()).await.unwrap();
            keep.push(name);
        }
        store.create_topic("phantom", TopicConfig::default()).await.unwrap();
        appended.lock().unwrap().clear();

        store
            .apply_replicated_event(MetadataEvent::new_with_node(
                MetadataEventPayload::CatalogSnapshot { topics: keep },
                2,
            ))
            .await
            .unwrap();

        let written = appended.lock().unwrap().clone();
        let deleted: Vec<&String> = written
            .iter()
            .filter_map(|e| match &e.payload {
                MetadataEventPayload::TopicDeleted { name } => Some(name),
                _ => None,
            })
            .collect();
        assert_eq!(deleted, vec![&"phantom".to_string()], "prune was not persisted");

        // And the snapshot itself must NOT be written: it is a reconciliation
        // signal, and persisting thousands of topic names every anti-entropy
        // pass would bloat the metadata WAL.
        assert!(
            !written.iter().any(|e| matches!(
                e.payload,
                MetadataEventPayload::CatalogSnapshot { .. }
            )),
            "the snapshot event was persisted to the WAL"
        );
    }



    /// End to end: a follower that was down during a delete converges.
    ///
    /// This is the whole bug in one test. Two stores, the leader's event bus
    /// wired into the follower exactly as `MetadataWalReplicator` wires it in
    /// production. The follower misses a `TopicDeleted` because it is down, and
    /// then the anti-entropy pass runs. Before `CatalogSnapshot` the pass was
    /// additive and the follower kept the phantom forever; the assertion at the
    /// end is the one that failed.
    #[tokio::test]
    async fn a_follower_that_missed_a_delete_converges_on_the_next_pass() {
        fn store() -> WalMetadataStore {
            let noop: WalAppendFn = Arc::new(|_b| Box::pin(async { Ok(0i64) }));
            WalMetadataStore::new(1, noop)
        }

        let mut leader = store();
        let follower = Arc::new(store());

        // The leader's event bus delivers to the follower, as in production —
        // except while the follower is "down", when sends are dropped. That drop
        // is the real transport's behaviour: a metadata send to a follower with
        // no live connection is lost, not queued.
        let follower_up = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let wire: Arc<Mutex<Vec<MetadataEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let on_wire = Arc::clone(&wire);
        let up = Arc::clone(&follower_up);
        leader.set_event_bus(Arc::new(move |event: MetadataEvent| {
            if !up.load(std::sync::atomic::Ordering::SeqCst) {
                return 0; // dropped: no live connection, as in production
            }
            on_wire.lock().unwrap().push(event);
            1
        }));

        // Deliver whatever is on the wire to the follower.
        async fn deliver(wire: &Arc<Mutex<Vec<MetadataEvent>>>, follower: &WalMetadataStore) {
            let batch: Vec<MetadataEvent> = wire.lock().unwrap().drain(..).collect();
            for event in batch {
                follower.apply_replicated_event(event).await.unwrap();
            }
        }

        // Both nodes know the same ten topics.
        for i in 0..10 {
            leader
                .create_topic(&format!("topic-{}", i), TopicConfig::default())
                .await
                .unwrap();
        }
        deliver(&wire, &follower).await;
        assert_eq!(follower.list_topics().await.unwrap().len(), 10, "setup");

        // The follower goes down, and a topic is deleted while it is away.
        follower_up.store(false, std::sync::atomic::Ordering::SeqCst);
        leader.delete_topic("topic-7").await.unwrap();
        follower_up.store(true, std::sync::atomic::Ordering::SeqCst);
        deliver(&wire, &follower).await;

        assert_eq!(leader.list_topics().await.unwrap().len(), 9);
        assert_eq!(
            follower.list_topics().await.unwrap().len(),
            10,
            "precondition: the follower should still hold the phantom"
        );

        // Anti-entropy, in the order the loop runs it: re-assert what exists,
        // then state the complete set. Two passes, because the publisher will
        // not assert a snapshot until its own catalog has settled.
        for _ in 0..2 {
            leader.broadcast_all_topics().await;
            leader.broadcast_catalog_snapshot().await;
            deliver(&wire, &follower).await;
        }

        let names: Vec<String> = follower
            .list_topics()
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.name)
            .collect();
        assert!(
            !names.contains(&"topic-7".to_string()),
            "the follower kept a topic deleted while it was down: {:?}",
            names
        );
        assert_eq!(names.len(), 9, "converged to the wrong set: {:?}", names);
    }

    /// A snapshot from a node that has not finished recovering must not prune.
    ///
    /// Observed directly on a cluster: a node 60s into a restart reported 16
    /// topics of 5,611. The empty-snapshot guard does not catch that — 16 is not
    /// zero — and obeying it would have deleted the catalog on every follower.
    #[tokio::test]
    async fn a_partial_snapshot_does_not_prune_the_catalog() {
        let wal_append: WalAppendFn = Arc::new(|_bytes| Box::pin(async { Ok(0i64) }));
        let store = WalMetadataStore::new(1, wal_append);
        for i in 0..20 {
            store
                .create_topic(&format!("topic-{}", i), TopicConfig::default())
                .await
                .unwrap();
        }

        // A leader mid-recovery claiming only two topics exist.
        store
            .apply_replicated_event(MetadataEvent::new_with_node(
                MetadataEventPayload::CatalogSnapshot {
                    topics: vec!["topic-0".to_string(), "topic-1".to_string()],
                },
                2,
            ))
            .await
            .unwrap();

        assert_eq!(
            store.list_topics().await.unwrap().len(),
            20,
            "a partial snapshot pruned the catalog"
        );
    }

    /// The publisher must not assert a snapshot until its own catalog has
    /// stopped changing, so the first pass after a restart only arms the check.
    #[tokio::test]
    async fn no_snapshot_is_published_until_the_catalog_settles() {
        let wal_append: WalAppendFn = Arc::new(|_bytes| Box::pin(async { Ok(0i64) }));
        let mut store = WalMetadataStore::new(1, wal_append);
        let published: Arc<Mutex<Vec<MetadataEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&published);
        store.set_event_bus(Arc::new(move |event: MetadataEvent| {
            sink.lock().unwrap().push(event);
            1
        }));

        store.create_topic("a", TopicConfig::default()).await.unwrap();
        assert_eq!(store.broadcast_catalog_snapshot().await, 0, "first pass must only arm");

        // Still recovering: the count changed, so still no assertion.
        store.create_topic("b", TopicConfig::default()).await.unwrap();
        assert_eq!(store.broadcast_catalog_snapshot().await, 0, "a changed count must re-arm");

        // Settled: two passes agree.
        assert_eq!(store.broadcast_catalog_snapshot().await, 2, "a settled catalog must publish");
    }

    /// An empty snapshot must never wipe the catalog.
    ///
    /// A node that has not finished recovering, or a truncated read, produces an
    /// empty list far more often than a cluster genuinely holds no topics.
    /// Pruning is destructive, so the ambiguous case has to fail towards keeping
    /// data.
    #[tokio::test]
    async fn an_empty_catalog_snapshot_prunes_nothing() {
        let wal_append: WalAppendFn = Arc::new(|_bytes| Box::pin(async { Ok(0i64) }));
        let store = WalMetadataStore::new(1, wal_append);
        store.create_topic("real", TopicConfig::default()).await.unwrap();

        store
            .apply_replicated_event(MetadataEvent::new_with_node(
                MetadataEventPayload::CatalogSnapshot { topics: Vec::new() },
                2,
            ))
            .await
            .unwrap();

        assert_eq!(
            store.list_topics().await.unwrap().len(),
            1,
            "an empty snapshot deleted a real topic"
        );
    }

    /// A snapshot older than one already applied must be ignored, or a delayed
    /// or replayed message would resurrect what a newer pass pruned.
    #[tokio::test]
    async fn an_older_catalog_snapshot_is_ignored() {
        let wal_append: WalAppendFn = Arc::new(|_bytes| Box::pin(async { Ok(0i64) }));
        let store = WalMetadataStore::new(1, wal_append);
        // Ten topics so pruning one stays inside the mass-prune ceiling.
        let mut nine: Vec<String> = Vec::new();
        for i in 0..9 {
            let name = format!("a{}", i);
            store.create_topic(&name, TopicConfig::default()).await.unwrap();
            nine.push(name);
        }
        store.create_topic("b", TopicConfig::default()).await.unwrap();

        let newer = MetadataEvent::new_with_node(
            MetadataEventPayload::CatalogSnapshot { topics: nine.clone() },
            2,
        );
        let mut with_b = nine.clone();
        with_b.push("b".to_string());
        let mut older = MetadataEvent::new_with_node(
            MetadataEventPayload::CatalogSnapshot { topics: with_b },
            2,
        );
        older.timestamp = newer.timestamp - chrono::Duration::seconds(60);

        store.apply_replicated_event(newer).await.unwrap();
        assert_eq!(store.list_topics().await.unwrap().len(), 9, "prune did not happen");

        // The older snapshot still lists "b"; applying it must not bring it back.
        store.apply_replicated_event(older).await.unwrap();
        assert_eq!(
            store.list_topics().await.unwrap().len(),
            9,
            "a stale snapshot resurrected a pruned topic"
        );
    }

    /// Internal `__`-prefixed topics are per-node and are excluded from the
    /// snapshot, so pruning must exempt them — otherwise every pass would delete
    /// a node's own internal state.
    #[tokio::test]
    async fn a_catalog_snapshot_never_prunes_internal_topics() {
        let wal_append: WalAppendFn = Arc::new(|_bytes| Box::pin(async { Ok(0i64) }));
        let store = WalMetadataStore::new(1, wal_append);
        store.create_topic("__chronik_internal", TopicConfig::default()).await.unwrap();
        store.create_topic("real", TopicConfig::default()).await.unwrap();

        store
            .apply_replicated_event(MetadataEvent::new_with_node(
                MetadataEventPayload::CatalogSnapshot { topics: vec!["real".to_string()] },
                2,
            ))
            .await
            .unwrap();

        let names: Vec<String> = store
            .list_topics()
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.name)
            .collect();
        assert!(
            names.contains(&"__chronik_internal".to_string()),
            "pruned an internal topic: {:?}",
            names
        );
    }

    /// The publisher must state the whole set, excluding internal topics, so the
    /// two sides agree on what absence means.
    #[tokio::test]
    async fn the_published_snapshot_lists_every_non_internal_topic() {
        let wal_append: WalAppendFn = Arc::new(|_bytes| Box::pin(async { Ok(0i64) }));
        let mut store = WalMetadataStore::new(1, wal_append);
        let published: Arc<Mutex<Vec<MetadataEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&published);
        store.set_event_bus(Arc::new(move |event: MetadataEvent| {
            sink.lock().unwrap().push(event);
            1
        }));

        store.create_topic("orders", TopicConfig::default()).await.unwrap();
        store.create_topic("events", TopicConfig::default()).await.unwrap();
        store.create_topic("__internal", TopicConfig::default()).await.unwrap();
        published.lock().unwrap().clear();

        // First pass only arms the settling check; the second asserts.
        assert_eq!(store.broadcast_catalog_snapshot().await, 0);
        let count = store.broadcast_catalog_snapshot().await;
        assert_eq!(count, 2, "internal topics must be excluded");

        let events = published.lock().unwrap().clone();
        let snapshot = events
            .iter()
            .find_map(|e| match &e.payload {
                MetadataEventPayload::CatalogSnapshot { topics } => Some(topics.clone()),
                _ => None,
            })
            .expect("no CatalogSnapshot was published");
        assert!(snapshot.contains(&"orders".to_string()));
        assert!(snapshot.contains(&"events".to_string()));
        assert!(!snapshot.contains(&"__internal".to_string()));
    }

    /// The leader's startup re-broadcast is how a follower that restarted, joined
    /// late, or missed an event gets its catalog back. It must carry partition
    /// assignments, not just topics.
    ///
    /// Topics alone leave a follower knowing every topic and no leader for any
    /// partition. Under push that was survivable — the leader drives replication,
    /// so only its view had to be right. Under follower-pull the follower must
    /// know who to fetch from, so an assignment-less heal means it replicates
    /// nothing at all, silently, while the leader still reports isr:[1,2,3].
    ///
    /// Measured on a 3-node cluster before this: node 1 knew a leader for 66 of
    /// 69 partitions, node 3 for 21, node 2 for zero.
    #[tokio::test]
    async fn the_startup_rebroadcast_carries_assignments_not_just_topics() {
        let wal_append: WalAppendFn = Arc::new(|_bytes| Box::pin(async { Ok(0i64) }));
        let mut store = WalMetadataStore::new(1, wal_append);

        let published: Arc<Mutex<Vec<MetadataEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&published);
        store.set_event_bus(Arc::new(move |event: MetadataEvent| {
            sink.lock().unwrap().push(event);
            1
        }));

        let mut config = TopicConfig::default();
        config.partition_count = 3;
        store.create_topic("orders", config).await.unwrap();

        for partition in 0..3u32 {
            store
                .assign_partition(PartitionAssignment {
                    topic: "orders".to_string(),
                    partition,
                    broker_id: 1,
                    is_leader: true,
                    replicas: vec![1, 2, 3],
                    leader_id: (partition as u64 % 3) + 1,
                    leader_epoch: 0, // assigned by the metadata store
                    isr: Vec::new(),
                })
                .await
                .unwrap();
        }

        published.lock().unwrap().clear();
        store.broadcast_all_topics().await;

        let events = published.lock().unwrap().clone();
        let topics: Vec<&MetadataEvent> = events
            .iter()
            .filter(|e| matches!(e.payload, MetadataEventPayload::TopicCreated { .. }))
            .collect();
        let assignments: Vec<&MetadataEvent> = events
            .iter()
            .filter(|e| matches!(e.payload, MetadataEventPayload::PartitionAssigned { .. }))
            .collect();

        assert_eq!(topics.len(), 1, "the topic itself must still be re-broadcast");
        assert_eq!(
            assignments.len(),
            3,
            "every partition's assignment must be re-broadcast, or a pulling follower \
             cannot find a leader and replicates nothing"
        );

        // The leader id is the field that matters — it is what the follower fetches from.
        let mut leaders: Vec<u64> = assignments
            .iter()
            .map(|e| match &e.payload {
                MetadataEventPayload::PartitionAssigned { assignment } => assignment.leader_id,
                _ => unreachable!(),
            })
            .collect();
        leaders.sort();
        assert_eq!(leaders, vec![1, 2, 3], "each partition's leader must survive the heal");
    }

    /// Topics must be broadcast before the assignments that reference them, so a
    /// follower applying in order never sees an assignment for a topic it does
    /// not yet know.
    #[tokio::test]
    async fn topics_are_broadcast_before_their_assignments() {
        let wal_append: WalAppendFn = Arc::new(|_bytes| Box::pin(async { Ok(0i64) }));
        let mut store = WalMetadataStore::new(1, wal_append);

        let published: Arc<Mutex<Vec<MetadataEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&published);
        store.set_event_bus(Arc::new(move |event: MetadataEvent| {
            sink.lock().unwrap().push(event);
            1
        }));

        let mut config = TopicConfig::default();
        config.partition_count = 1;
        store.create_topic("orders", config).await.unwrap();
        store
            .assign_partition(PartitionAssignment {
                topic: "orders".to_string(),
                partition: 0,
                broker_id: 1,
                is_leader: true,
                replicas: vec![1, 2, 3],
                leader_id: 1,
                leader_epoch: 0, // assigned by the metadata store
                isr: Vec::new(),
            })
            .await
            .unwrap();

        published.lock().unwrap().clear();
        store.broadcast_all_topics().await;

        let events = published.lock().unwrap().clone();
        let first_assignment = events
            .iter()
            .position(|e| matches!(e.payload, MetadataEventPayload::PartitionAssigned { .. }))
            .expect("an assignment must be broadcast");
        let last_topic = events
            .iter()
            .rposition(|e| matches!(e.payload, MetadataEventPayload::TopicCreated { .. }))
            .expect("a topic must be broadcast");

        assert!(
            last_topic < first_assignment,
            "assignments must follow the topics they belong to"
        );
    }
}

#[cfg(test)]
mod leader_epoch_tests {
    use super::*;

    fn store() -> WalMetadataStore {
        let wal_append: WalAppendFn = Arc::new(|_bytes| Box::pin(async { Ok(0i64) }));
        WalMetadataStore::new(1, wal_append)
    }

    fn assignment(topic: &str, partition: u32, leader: u64) -> PartitionAssignment {
        PartitionAssignment {
            topic: topic.to_string(),
            partition,
            broker_id: leader as i32,
            is_leader: true,
            replicas: vec![1, 2, 3],
            leader_id: leader,
            leader_epoch: 999, // deliberately wrong: the store must overwrite it
            isr: Vec::new(),
        }
    }

    async fn epoch_of(store: &WalMetadataStore, topic: &str, partition: u32) -> i32 {
        store
            .get_partition_assignments(topic)
            .await
            .unwrap()
            .into_iter()
            .find(|a| a.partition == partition)
            .expect("assignment exists")
            .leader_epoch
    }

    /// Apply an assignment the way a replicated event does — carrying its own
    /// epoch, rather than having one derived. This is the path a re-broadcast
    /// or a returning node's replay takes.
    async fn apply_event(store: &WalMetadataStore, assignment: PartitionAssignment) {
        let event = MetadataEvent::new_with_node(
            MetadataEventPayload::PartitionAssigned { assignment },
            2,
        );
        store.apply_replicated_event(event).await.unwrap();
    }

    fn assignment_at(topic: &str, partition: u32, leader: u64, epoch: i32) -> PartitionAssignment {
        let mut a = assignment(topic, partition, leader);
        a.leader_epoch = epoch;
        a
    }

    /// The bug this guard exists for, reproduced on a cluster: node 1 died, the
    /// partition failed over to node 2, node 2 took 400 acknowledged records —
    /// then node 1 came back, replayed its own metadata WAL (stale by exactly
    /// the change that demoted it) and its assignment overwrote the newer one
    /// everywhere. All three nodes agreed the leader was node 1, which held no
    /// data, and consumers read **zero of 400** acknowledged records.
    #[tokio::test]
    async fn a_returning_node_cannot_undo_a_failover() {
        let store = store();

        // The cluster failed the partition over to node 2.
        apply_event(&store, assignment_at("orders", 0, 2, 1)).await;

        // Node 1 comes back and replays what it knew before it died.
        apply_event(&store, assignment_at("orders", 0, 1, 0)).await;

        let assignments = store.get_partition_assignments("orders").await.unwrap();
        let current = assignments.iter().find(|a| a.partition == 0).unwrap();
        assert_eq!(
            current.leader_id, 2,
            "a stale assignment must not move the partition back to a node that lost it"
        );
        assert_eq!(current.leader_epoch, 1);
    }

    /// Anti-entropy re-asserts unchanged assignments constantly. Those carry the
    /// same epoch and must still apply, or a node that missed the original event
    /// could never be healed by a re-broadcast.
    #[tokio::test]
    async fn an_equal_epoch_still_applies() {
        let store = store();
        apply_event(&store, assignment_at("orders", 0, 2, 3)).await;

        let mut replacement = assignment_at("orders", 0, 2, 3);
        replacement.replicas = vec![2, 3, 1];
        apply_event(&store, replacement).await;

        let assignments = store.get_partition_assignments("orders").await.unwrap();
        let current = assignments.iter().find(|a| a.partition == 0).unwrap();
        assert_eq!(current.replicas, vec![2, 3, 1], "a same-epoch re-assertion must apply");
    }

    /// Newer leadership must always win, which is the ordinary failover path.
    #[tokio::test]
    async fn a_newer_epoch_wins() {
        let store = store();
        apply_event(&store, assignment_at("orders", 0, 1, 0)).await;
        apply_event(&store, assignment_at("orders", 0, 3, 1)).await;

        let assignments = store.get_partition_assignments("orders").await.unwrap();
        let current = assignments.iter().find(|a| a.partition == 0).unwrap();
        assert_eq!(current.leader_id, 3);
        assert_eq!(current.leader_epoch, 1);
    }

    /// A partition nobody has seen before is accepted whatever its epoch —
    /// there is nothing to supersede.
    #[tokio::test]
    async fn a_first_assignment_is_always_accepted() {
        let store = store();
        apply_event(&store, assignment_at("orders", 0, 2, 7)).await;

        let assignments = store.get_partition_assignments("orders").await.unwrap();
        assert_eq!(assignments.iter().find(|a| a.partition == 0).unwrap().leader_id, 2);
    }

    /// The epoch is derived by the store, never taken from the caller. There are
    /// a dozen construction sites across the tree and each would otherwise be a
    /// chance to pass a stale or invented value.
    #[tokio::test]
    async fn the_store_owns_the_epoch_and_ignores_what_callers_pass() {
        let store = store();
        store.assign_partition(assignment("orders", 0, 1)).await.unwrap();

        assert_eq!(epoch_of(&store, "orders", 0).await, 0, "a first assignment starts at 0");
    }

    /// Every leadership change bumps the epoch — that is the whole signal a
    /// follower uses to know its log may have diverged.
    #[tokio::test]
    async fn a_leadership_change_bumps_the_epoch() {
        let store = store();
        store.assign_partition(assignment("orders", 0, 1)).await.unwrap();
        store.assign_partition(assignment("orders", 0, 2)).await.unwrap();
        store.assign_partition(assignment("orders", 0, 3)).await.unwrap();

        assert_eq!(epoch_of(&store, "orders", 0).await, 2);
    }

    /// Re-asserting the SAME leader must not bump. The anti-entropy loop
    /// re-broadcasts every assignment every few minutes, and an idempotent
    /// rebalance pass re-writes them too — if either manufactured a leadership
    /// change, every follower would think it had to truncate, repeatedly, on a
    /// perfectly healthy cluster.
    #[tokio::test]
    async fn re_asserting_the_same_leader_does_not_bump() {
        let store = store();
        store.assign_partition(assignment("orders", 0, 1)).await.unwrap();
        for _ in 0..10 {
            store.assign_partition(assignment("orders", 0, 1)).await.unwrap();
        }

        assert_eq!(epoch_of(&store, "orders", 0).await, 0);
    }

    /// Leadership moving away and back is two changes, not zero. Reusing the
    /// earlier epoch would make two different logs claim the same leadership
    /// period, and the truncation query would then answer with the wrong offset.
    #[tokio::test]
    async fn leadership_returning_to_a_previous_node_still_bumps() {
        let store = store();
        store.assign_partition(assignment("orders", 0, 1)).await.unwrap();
        store.assign_partition(assignment("orders", 0, 2)).await.unwrap();
        store.assign_partition(assignment("orders", 0, 1)).await.unwrap();

        assert_eq!(epoch_of(&store, "orders", 0).await, 2);
    }

    /// Epochs are per partition. A busy partition changing leaders must not
    /// advance a quiet one, or the quiet partition's followers would truncate
    /// against a history they never had.
    #[tokio::test]
    async fn epochs_are_tracked_per_partition() {
        let store = store();
        store.assign_partition(assignment("orders", 0, 1)).await.unwrap();
        store.assign_partition(assignment("orders", 1, 1)).await.unwrap();

        store.assign_partition(assignment("orders", 0, 2)).await.unwrap();
        store.assign_partition(assignment("orders", 0, 3)).await.unwrap();

        assert_eq!(epoch_of(&store, "orders", 0).await, 2);
        assert_eq!(epoch_of(&store, "orders", 1).await, 0);
    }

    /// Metadata events are JSON, and the field is `#[serde(default)]`, so events
    /// written before RP-3 must still decode — with epoch 0 rather than an
    /// error. A metadata WAL that fails to replay is an unrecoverable cluster.
    #[test]
    fn assignments_written_before_epochs_existed_still_decode() {
        let legacy = r#"{
            "topic": "orders",
            "partition": 0,
            "broker_id": 1,
            "is_leader": true,
            "replicas": [1, 2, 3],
            "leader_id": 1
        }"#;

        let decoded: PartitionAssignment =
            serde_json::from_str(legacy).expect("pre-RP-3 assignments must still decode");
        assert_eq!(decoded.leader_epoch, 0);
        assert_eq!(decoded.leader_id, 1);
    }
}

#[cfg(test)]
mod topic_provenance_tests {
    use super::*;

    fn store() -> WalMetadataStore {
        let wal_append: WalAppendFn = Arc::new(|_bytes| Box::pin(async { Ok(0i64) }));
        WalMetadataStore::new(1, wal_append)
    }

    fn config_with(partition_count: u32) -> TopicConfig {
        TopicConfig { partition_count, ..Default::default() }
    }

    async fn partitions_of(store: &WalMetadataStore, topic: &str) -> u32 {
        store.get_topic(topic).await.unwrap().expect("topic exists").config.partition_count
    }

    /// An auto-create must not widen a topic somebody created explicitly.
    ///
    /// This is the reported bug: `TopicConfig::default()` carries 3 partitions,
    /// the apply path kept whichever config had more, and a topic created with
    /// `--partitions 1` silently became 3. `/admin/status` then reported three
    /// partitions with real leaders while every record sat in partition 0,
    /// because the producer had seen one partition when it wrote.
    #[tokio::test]
    async fn an_auto_create_does_not_widen_an_explicit_topic() {
        let store = store();
        store.create_topic("orders", config_with(1)).await.unwrap();

        // The racing auto-create, carrying the default 3.
        store
            .apply_replicated_event(MetadataEvent::new_with_node(
                MetadataEventPayload::TopicCreated {
                    name: "orders".to_string(),
                    config: config_with(3),
                    auto_created: true,
                },
                2,
            ))
            .await
            .unwrap();

        assert_eq!(
            partitions_of(&store, "orders").await,
            1,
            "an auto-create silently widened a topic that was asked for with 1 partition"
        );
    }

    /// The other direction: an explicit create replaces an auto-created config
    /// even when it has FEWER partitions. Provenance decides, not size.
    #[tokio::test]
    async fn an_explicit_create_narrows_an_auto_created_topic() {
        let store = store();
        store.auto_create_topic("logs", config_with(3)).await.unwrap();

        store
            .apply_replicated_event(MetadataEvent::new_with_node(
                MetadataEventPayload::TopicCreated {
                    name: "logs".to_string(),
                    config: config_with(1),
                    auto_created: false,
                },
                2,
            ))
            .await
            .unwrap();

        assert_eq!(partitions_of(&store, "logs").await, 1);
        assert!(
            !store.get_topic("logs").await.unwrap().unwrap().auto_created,
            "the topic is no longer a guess once someone has asked for it"
        );
    }

    /// Two explicit events still ratchet upward — a genuine expansion is not
    /// blocked by the provenance rule.
    #[tokio::test]
    async fn explicit_expansion_still_applies() {
        let store = store();
        store.create_topic("events", config_with(1)).await.unwrap();

        store
            .apply_replicated_event(MetadataEvent::new_with_node(
                MetadataEventPayload::TopicCreated {
                    name: "events".to_string(),
                    config: config_with(6),
                    auto_created: false,
                },
                2,
            ))
            .await
            .unwrap();

        assert_eq!(partitions_of(&store, "events").await, 6);
    }

    /// Provenance is recorded, so a follower applying the replicated event
    /// reaches the same decision the leader did.
    #[tokio::test]
    async fn provenance_is_recorded_on_the_topic() {
        let store = store();
        store.auto_create_topic("guessed", config_with(3)).await.unwrap();
        store.create_topic("asked-for", config_with(3)).await.unwrap();

        assert!(store.get_topic("guessed").await.unwrap().unwrap().auto_created);
        assert!(!store.get_topic("asked-for").await.unwrap().unwrap().auto_created);
    }

    /// Events written before the field existed decode as explicit — the side
    /// that is protected — so an upgrade cannot start discarding configs.
    #[test]
    fn a_legacy_topic_created_event_decodes_as_explicit() {
        let legacy = r#"{"type":"TopicCreated","name":"old","config":{"partition_count":1,"replication_factor":1,"retention_ms":null,"segment_bytes":1073741824,"config":{}}}"#;
        let decoded: MetadataEventPayload =
            serde_json::from_str(legacy).expect("pre-provenance events must still decode");
        match decoded {
            MetadataEventPayload::TopicCreated { auto_created, .. } => {
                assert!(!auto_created, "an unmarked event must read as explicit");
            }
            _ => panic!("wrong variant"),
        }
    }
}
