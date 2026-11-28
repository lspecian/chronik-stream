//! Fetch request handler for serving data to Kafka consumers.

use chronik_common::{Result, Error};
use chronik_common::metadata::traits::MetadataStore;
use chronik_monitoring::MetricsRecorder;
use chronik_protocol::{FetchRequest, FetchResponse, FetchResponseTopic, FetchResponsePartition};
use chronik_storage::{Segment, ObjectStoreTrait, SegmentIndex};
use chronik_storage::kafka_records::{KafkaRecordBatch, RecordHeader as KafkaRecordHeader, CompressionType};
use chronik_storage::tantivy_segment::TantivySegmentReader;
use chronik_wal::WalManager;
use dashmap::DashMap;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tokio::time::timeout;
use tracing::{debug, error, info, trace, warn};

/// Replica fetches served, process-wide. Only ever used to sample the timing
/// breakdown in `handle_fetch` rather than log every round trip (RP-9).
static REPLICA_FETCH_SAMPLES: AtomicU64 = AtomicU64::new(0);

/// In-memory buffer for recent records
/// CRITICAL v1.3.32: Store RAW Kafka batch bytes to preserve CRC
#[derive(Debug)]
struct PartitionBuffer {
    /// Raw Kafka RecordBatch bytes in wire format (preserves original CRC)
    raw_batches: Vec<bytes::Bytes>,
    /// Metadata about batches for offset tracking
    batch_metadata: Vec<BatchMetadata>,
    base_offset: i64,
    high_watermark: i64,
    /// Minimum offset actually present in buffer after trimming (v1.3.48)
    /// Used to detect gaps and trigger WAL fallback when old batches are trimmed
    min_offset_in_buffer: i64,
}

/// Metadata about a batch in the buffer
#[derive(Debug, Clone)]
struct BatchMetadata {
    base_offset: i64,
    last_offset: i64,
}

/// Fetch handler state
struct FetchState {
    /// In-memory buffers for recent data
    buffers: HashMap<(String, i32), PartitionBuffer>,
}

/// Segment metadata for fetch operations
#[derive(Clone, Debug)]
struct SegmentInfo {
    segment_id: String,
    base_offset: i64,
    last_offset: i64,
    object_key: String,
}

/// Configuration for FetchHandler behavior
///
/// There is no follower fetching (KIP-392): every node serves fetches from its
/// own log and clients route to the partition leader, which applies COMMIT/ABORT
/// markers synchronously, so the Last Stable Offset is correct the instant
/// `commitTransaction` returns. Follower fetching would reintroduce a
/// replication-lag visibility window on read_committed, so any future wiring
/// must keep read_committed on the leader (or make the follower wait for the
/// marker).
#[derive(Debug, Clone, Default)]
pub struct FetchHandlerConfig {
    /// Node ID (for preferred_read_replica)
    pub node_id: i32,
}

/// Fetch request handler
/// v1.3.47+: Uses Arc<WalManager> directly (no RwLock - WalManager uses DashMap internally)
/// v1.3.66+: Raft-aware with follower read support using RaftReplicaManager
/// v2.0.0+: Read-your-writes consistency with ReadIndex protocol
pub struct FetchHandler {
    metadata_store: Arc<dyn MetadataStore>,
    object_store: Arc<dyn ObjectStoreTrait>,
    wal_manager: Option<Arc<WalManager>>,
    segment_index: Option<Arc<SegmentIndex>>,
    produce_handler: Option<Arc<crate::produce_handler::ProduceHandler>>,
    state: Arc<RwLock<FetchState>>,
    /// Configuration for fetch behavior
    config: FetchHandlerConfig,
    /// RP-2.1: where follower fetch positions are recorded.
    ///
    /// A follower's fetch offset is its log end offset, so the fetch itself is
    /// both a progress report and a liveness signal — replacing the ACK channel,
    /// heartbeat replies and connection probing that the push model needed to
    /// approximate the same information.
    isr_tracker: Option<Arc<crate::isr_tracker::IsrTracker>>,
    isr_ack_tracker: Option<Arc<crate::isr_ack_tracker::IsrAckTracker>>,

    /// Woken when a partition is appended to, so a parked fetch learns about new
    /// records instead of discovering them on a timer.
    ///
    /// The long poll used to check every 10ms. That is not a small cost, because
    /// each check asks the metadata store for the partition's segments — so
    /// polling faster does not help: at 1ms the checks cost about as much as the
    /// interval saves, which is exactly what measurement showed (3,887 vs 3,935
    /// msg/s, indistinguishable).
    ///
    /// The interval is also directly in the `acks=all` critical path under load.
    /// Every producer is blocked waiting for the follower, so no new record
    /// exists until the previous batch is acknowledged — meaning the follower's
    /// fetch almost always arrives at an idle partition and parks. Its wake-up
    /// latency is therefore the loop period, and the loop period is the cap on
    /// replicated throughput (RP-9).
    append_notify: Arc<DashMap<(String, i32), Arc<tokio::sync::Notify>>>,
    /// RP-2.3: cap consumer reads at the in-sync watermark. Off unless pull
    /// replication is active — see `consumer_visible_watermark`.
    hw_from_isr: bool,
}

impl FetchHandler {
    /// Create a new fetch handler
    #[cfg(test)]
    pub fn new(
        metadata_store: Arc<dyn MetadataStore>,
        object_store: Arc<dyn ObjectStoreTrait>,
    ) -> Self {
        Self {
            metadata_store,
            object_store,
            wal_manager: None,
            segment_index: None,
            produce_handler: None,
            state: Arc::new(RwLock::new(FetchState {
                buffers: HashMap::new(),
            })),
            config: FetchHandlerConfig::default(),
            isr_tracker: None,
            isr_ack_tracker: None,
            append_notify: Arc::new(DashMap::new()),
            hw_from_isr: false,
        }
    }

    /// Create a new fetch handler with WAL and ProduceHandler integration (v1.3.39+)
    /// v1.3.47+: Accepts Arc<WalManager> directly (no RwLock - WalManager uses DashMap internally)
    pub fn new_with_wal(
        metadata_store: Arc<dyn MetadataStore>,
        object_store: Arc<dyn ObjectStoreTrait>,
        wal_manager: Arc<WalManager>,
        produce_handler: Arc<crate::produce_handler::ProduceHandler>,
    ) -> Self {
        Self {
            metadata_store,
            object_store,
            wal_manager: Some(wal_manager),
            segment_index: None,
            produce_handler: Some(produce_handler),
            state: Arc::new(RwLock::new(FetchState {
                buffers: HashMap::new(),
            })),
            config: FetchHandlerConfig::default(),
            isr_tracker: None,
            isr_ack_tracker: None,
            append_notify: Arc::new(DashMap::new()),
            hw_from_isr: false,
        }
    }

    /// RP-2.1: attach the ISR tracker so follower fetches record their position.
    ///
    /// Cluster mode only. Without it, follower fetches are served exactly as
    /// before and nothing is recorded, so single-node is unaffected.
    pub fn set_isr_tracker(&mut self, tracker: Arc<crate::isr_tracker::IsrTracker>) {
        self.isr_tracker = Some(tracker);
        info!("ISR tracker wired to FetchHandler — follower fetches now report replication progress");
    }

    /// RP-2.3: attach the ACK tracker so a follower's fetch offset settles
    /// `acks=all` waits, the way an ACK frame does under push.
    ///
    /// Cluster mode only; without it, follower fetches record ISR progress but
    /// do not release producers, which is correct for push where the ACK frame
    /// already does that job.
    pub fn set_isr_ack_tracker(&mut self, tracker: Arc<crate::isr_ack_tracker::IsrAckTracker>) {
        self.isr_ack_tracker = Some(tracker);
        info!("ISR ACK tracker wired to FetchHandler — follower fetches now settle acks=all");
    }

    /// RP-2.3: bound consumer reads by `min(LEO across ISR)` instead of the
    /// leader's own write position.
    pub fn set_hw_from_isr(&mut self, enabled: bool) {
        self.hw_from_isr = enabled;
        if enabled {
            info!("Consumer high watermark now follows the in-sync set, not the leader's write position");
        }
    }

    /// Handle a fetch request
    pub async fn handle_fetch(
        &self,
        request: FetchRequest,
        correlation_id: i32,
    ) -> Result<FetchResponse> {
        // `max_wait_ms` bounds the REQUEST, and the wait belongs to the request
        // as a whole — not to each partition in turn.
        //
        // Serving partitions serially, each free to park on its own, made a
        // 10-partition fetch take 10x max_wait_ms. Sharing one deadline across
        // them fixed the total but not the shape: the FIRST partition examined
        // could still spend the entire budget waiting, and a partition is
        // examined in request order, not in order of who has data. One idle
        // partition ahead of an active one in the same request therefore delayed
        // every record on that active partition by the full max_wait_ms.
        //
        // Measured: `acks=all` cost a flat 505ms per produce on a three-node
        // cluster with three topics, because the follower replicating them sends
        // ONE request covering all three (RP-2.4) and the idle ones were reached
        // first. The producer waits for the follower, the follower is parked
        // behind an unrelated idle partition, and nothing is wrong with either
        // (#36).
        //
        // So: serve everything without waiting, and only if the whole request
        // came back empty, wait once — for ANY partition to get data — and serve
        // everything again. That is also what Kafka's contract says the wait is:
        // a property of the request, satisfied by the first partition to have
        // something to send.
        let started = Instant::now();
        let wait_deadline =
            started + Duration::from_millis(request.max_wait_ms.max(0) as u64);

        let mut response_topics = self.serve_all_partitions(&request).await?;
        let first_serve = started.elapsed();
        let mut waited = Duration::ZERO;

        if request.max_wait_ms > 0 && !any_records(&response_topics)
            && self.wait_for_any_partition(&request, wait_deadline).await {
                waited = started.elapsed() - first_serve;
                response_topics = self.serve_all_partitions(&request).await?;
            }

        // A replica fetch's cost is the floor on `acks=all` latency and the cap
        // on its throughput: the producer cannot be acknowledged until the
        // follower's NEXT fetch reports a position past the record. The
        // follower's own view of this call (RP-9) is the request as a whole, so
        // split it here into the part spent serving and the part spent parked.
        if request.replica_id >= 0 {
            let n = REPLICA_FETCH_SAMPLES.fetch_add(1, Ordering::Relaxed) + 1;
            if n.is_multiple_of(200) {
                debug!(
                    "replica fetch {} from node {}: serve {:?}, wait {:?}, total {:?}",
                    n,
                    request.replica_id,
                    first_serve,
                    waited,
                    started.elapsed()
                );
            }
        }


        // Record fetch metrics
        let mut fetched_bytes: u64 = 0;
        for t in &response_topics {
            for p in &t.partitions {
                fetched_bytes += p.records.len() as u64;
            }
        }
        MetricsRecorder::record_fetch(true, fetched_bytes);

        Ok(FetchResponse {
            header: chronik_protocol::parser::ResponseHeader { correlation_id },
            throttle_time_ms: 0,
            error_code: 0,
            session_id: 0,
            topics: response_topics,
        })
    }

    /// Serve every partition in the request, without waiting for any of them.
    ///
    /// Waiting is the caller's job (`wait_for_any_partition`) precisely so that
    /// one partition cannot spend the request's budget on behalf of the others.
    ///
    /// Partitions are served one at a time, and an attempt to serve them
    /// concurrently was **reverted**.
    ///
    /// The reasoning for concurrency was sound on paper — independent reads
    /// against independent logs, and serving one partition measures 2.5–4ms, so
    /// a 3-partition fetch spends ~10ms here. It made no difference to
    /// replicated throughput (2,369 vs 2,210 msg/s, inside the run-to-run
    /// spread), because the follower's cycle is dominated by waiting on a leader
    /// that is also serving producers, not by this loop.
    ///
    /// And it broke RP-3.3: the divergence test went from passing consistently
    /// to 2 runs in 3, failing with committed records unreadable after a
    /// follower returned. `fetch_partition` is not a pure read — it records the
    /// follower's position with the ISR trackers — so overlapping those side
    /// effects across partitions is not free. No measured benefit and a
    /// reproducible correctness cost is an easy call.
    ///
    /// If this is revisited, the entry price is understanding what those side
    /// effects do when interleaved, not just that the reads are independent.
    async fn serve_all_partitions(
        &self,
        request: &FetchRequest,
    ) -> Result<Vec<FetchResponseTopic>> {
        let mut response_topics = Vec::with_capacity(request.topics.len());

        for topic_request in &request.topics {
            let mut served = Vec::with_capacity(topic_request.partitions.len());
            for pr in &topic_request.partitions {
                // Where the follower's copy starts (Fetch v5+). Decoded and
                // dropped until v2.20.6; see `in_sync_replicas` for why the
                // leader needs it.
                if request.replica_id >= 0 && pr.log_start_offset >= 0 {
                    if let Some(ref tracker) = self.isr_tracker {
                        tracker.record_follower_log_start(
                            request.replica_id as u64,
                            &topic_request.name,
                            pr.partition,
                            pr.log_start_offset,
                            pr.fetch_offset,
                        );
                    }
                }
                served.push(
                    self.fetch_partition(
                        &topic_request.name,
                        pr.partition,
                        pr.fetch_offset,
                        pr.partition_max_bytes,
                        request.max_wait_ms,
                        request.replica_id,
                    )
                    .await,
                );
            }

            let mut response_partitions = Vec::with_capacity(topic_request.partitions.len());

            for partition_response in served {
                let mut partition_response = partition_response?;

                // EOS layer 6: for read_committed (isolation_level == 1) report the real
                // Last Stable Offset and the aborted-transactions list from the per-
                // partition transaction index. The consumer uses the LSO as its read
                // boundary and the aborted list (with the ABORT control markers in the
                // log) to drop aborted records. read_uncommitted (0) is left untouched.
                if request.isolation_level == 1 {
                    if let Some(ref ph) = self.produce_handler {
                        let idx = ph.transaction_index();
                        let hwm = partition_response.high_watermark;
                        let lso = idx.last_stable_offset(
                            &topic_request.name, partition_response.partition, hwm);
                        partition_response.last_stable_offset = lso;
                        let aborted: Vec<chronik_protocol::AbortedTransaction> =
                            idx.aborted_below(&topic_request.name, partition_response.partition, hwm)
                                .into_iter()
                                .map(|a| chronik_protocol::AbortedTransaction {
                                    producer_id: a.producer_id,
                                    first_offset: a.first_offset,
                                })
                                .collect();
                        partition_response.aborted = Some(aborted);

                        // A read_committed consumer must not receive records at or
                        // beyond the LSO — they belong to a still-open transaction.
                        // Setting the LSO field alone is not enough: the Java client
                        // delivers whatever record batches the broker returns, so the
                        // broker must also truncate the returned batches at the LSO
                        // (a transaction's first batch starts exactly on the LSO, so
                        // this drops on a batch boundary). Without this, an open (or
                        // crash-recovered open) transaction's records leak to
                        // read_committed as if committed.
                        if lso < hwm && !partition_response.records.is_empty() {
                            let before = partition_response.records.len();
                            let keep = keep_len_below_offset(&partition_response.records, lso);
                            if keep < before {
                                tracing::debug!("read_committed truncate {}-{}: lso={} hwm={} bytes {}->{}",
                                    topic_request.name, partition_response.partition, lso, hwm, before, keep);
                                partition_response.records.truncate(keep);
                            }
                        }
                    }
                }

                response_partitions.push(partition_response);
            }

            response_topics.push(FetchResponseTopic {
                name: topic_request.name.clone(),
                partitions: response_partitions,
            });
        }

        Ok(response_topics)
    }

    /// Replace the append notifier with one shared with the produce path.
    pub fn set_append_notify(
        &mut self,
        notify: Arc<DashMap<(String, i32), Arc<tokio::sync::Notify>>>,
    ) {
        self.append_notify = notify;
    }

    /// The wake-up handle for a partition, created on first use.
    ///
    /// Only a *waiter* creates one, so a partition nobody is parked on costs
    /// nothing — and `notify_appended` above skips partitions with no entry
    /// rather than allocating one per append.
    fn notify_for(&self, topic: &str, partition: i32) -> Arc<tokio::sync::Notify> {
        self.append_notify
            .entry((topic.to_string(), partition))
            .or_insert_with(|| Arc::new(tokio::sync::Notify::new()))
            .clone()
    }

    /// Wait until ANY partition in the request has something past its fetch
    /// offset, or the request's deadline passes. Returns whether it found any.
    ///
    /// Detection only — no records are read here. The one reader is
    /// `fetch_data_available_path`, reached through `serve_all_partitions`. Two
    /// readers is exactly what went wrong before: the copy that lived inside the
    /// long poll used `fetch_records` while the direct path used
    /// `fetch_raw_bytes` first, so the poll could sit through its whole budget
    /// failing to read a record the very next request returned immediately.
    async fn wait_for_any_partition(
        &self,
        request: &FetchRequest,
        wait_deadline: Instant,
    ) -> bool {
        // A backstop, not the mechanism. An append wakes this directly; the
        // timer only covers a wake-up that never arrives — a record made visible
        // by a background segment flush rather than by the produce path, or a
        // notify lost to a race this code has not thought of. It is deliberately
        // slow, because being the fallback is the whole of its job.
        const IDLE_RECHECK: Duration = Duration::from_millis(100);

        loop {
            // Register for the wake-up BEFORE looking. `Notify::notified()`
            // captures notifications from the moment the future is created, so
            // an append landing between the check below and the await is not
            // lost. Checking first and registering after is the classic missed
            // wake-up, and here it would cost a producer the full recheck
            // interval.
            let waits: Vec<_> = request
                .topics
                .iter()
                .flat_map(|t| {
                    t.partitions
                        .iter()
                        .map(move |p| self.notify_for(&t.name, p.partition))
                })
                .collect();
            let notified: Vec<_> = waits.iter().map(|n| n.notified()).collect();

            for topic_request in &request.topics {
                for partition_request in &topic_request.partitions {
                    if self
                        .has_data_beyond(
                            &topic_request.name,
                            partition_request.partition,
                            partition_request.fetch_offset,
                            request.replica_id,
                        )
                        .await
                    {
                        return true;
                    }
                }
            }

            let remaining = wait_deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }

            // Whichever partition grows first wins; `select_all` needs a
            // non-empty set, and a request with no partitions has nothing to
            // wait for anyway.
            if notified.is_empty() {
                return false;
            }
            let any_append = futures::future::select_all(
                notified.into_iter().map(Box::pin).collect::<Vec<_>>(),
            );
            let _ = tokio::time::timeout(remaining.min(IDLE_RECHECK), any_append).await;
        }
    }

    /// Is there anything to send for this partition past `fetch_offset`?
    ///
    /// Consults the live log by the rule that governs this reader (follower →
    /// log end, consumer → in-sync position) AND the segment index, because
    /// neither subsumes the other: the live path has no in-memory state for a
    /// partition that has not been produced to since startup, and the segment
    /// index is written by a background flusher some time after the fact.
    ///
    /// Consulting only the segment index — which is what the old long poll did —
    /// meant a parked fetch could not see a record until it had been flushed,
    /// however long it had already been durable.
    async fn has_data_beyond(
        &self,
        topic: &str,
        partition: i32,
        fetch_offset: i64,
        replica_id: i32,
    ) -> bool {
        if let Ok(live_end) = self
            .readable_end_offset(topic, partition, fetch_offset, replica_id)
            .await
        {
            if live_end > fetch_offset {
                return true;
            }
        }

        let Ok(segments) = self
            .metadata_store
            .list_segments(topic, Some(partition as u32))
            .await
        else {
            return false;
        };
        let segment_end = segments.iter().map(|s| s.end_offset + 1).max().unwrap_or(0);
        if segment_end <= fetch_offset {
            return false;
        }

        // The segment index knows nothing about replication, so a consumer must
        // not be woken by records the in-sync set does not hold — the exact
        // visibility RP-2.3 caps, reached by a different route.
        if replica_id >= 0 {
            return true;
        }
        self.consumer_visible_watermark(topic, partition, segment_end)
            .await
            > fetch_offset
    }

    // ============================================================================
    // Phase 2.13 Helper Methods - fetch_partition() Extraction
    // ============================================================================

    /// Validate topic and partition existence
    ///
    /// Returns None if valid, Some(error_response) if invalid
    ///
    /// Complexity: < 15 (two metadata checks + error construction)
    /// NOT_LEADER_OR_FOLLOWER when metadata says another node leads this
    /// partition (RP-7).
    ///
    /// Narrow on purpose. It rejects only when an assignment exists *and* names
    /// a different node: no assignment, an assignment naming this node, or an
    /// unset node id all serve as before, so single-node deployments and
    /// not-yet-assigned topics are unaffected.
    ///
    /// The alternative — serving an empty log because our catalog is stale — is
    /// indistinguishable from "you are caught up", and that is what let a
    /// returning node answer 17,259 fetches for a partition it no longer led
    /// while the cluster reported itself healthy.
    async fn reject_if_not_leader(
        &self,
        topic: &str,
        partition: i32,
    ) -> Option<FetchResponsePartition> {
        let this_node = self.config.node_id as u64;
        if this_node == 0 {
            return None; // no cluster identity — nothing to compare against
        }

        let assignments = self.metadata_store.get_partition_assignments(topic).await.ok()?;
        let assignment = assignments.iter().find(|a| a.partition == partition as u32)?;

        if assignment.leader_id == this_node {
            return None;
        }

        debug!(
            "{}-{}: refusing to serve — metadata says node {} leads this partition, not node {}",
            topic, partition, assignment.leader_id, this_node
        );

        Some(FetchResponsePartition {
            partition,
            error_code: 6, // NOT_LEADER_OR_FOLLOWER
            high_watermark: -1,
            last_stable_offset: -1,
            log_start_offset: -1,
            aborted: None,
            preferred_read_replica: -1,
            records: vec![],
        })
    }

    async fn validate_topic_and_partition(
        &self,
        topic: &str,
        partition: i32,
    ) -> Result<Option<FetchResponsePartition>> {
        // Check topic exists
        let topic_metadata = match self.metadata_store.get_topic(topic).await? {
            Some(meta) => meta,
            None => {
                return Ok(Some(FetchResponsePartition {
                    partition,
                    error_code: 3, // UNKNOWN_TOPIC_OR_PARTITION
                    high_watermark: -1,
                    last_stable_offset: -1,
                    log_start_offset: -1,
                    aborted: None,
                    preferred_read_replica: -1,
                    records: vec![],
                }));
            }
        };

        // Check partition in range
        if partition < 0 || partition >= topic_metadata.config.partition_count as i32 {
            return Ok(Some(FetchResponsePartition {
                partition,
                error_code: 3, // UNKNOWN_TOPIC_OR_PARTITION
                high_watermark: -1,
                last_stable_offset: -1,
                log_start_offset: -1,
                aborted: None,
                preferred_read_replica: -1,
                records: vec![],
            }));
        }

        Ok(None) // Valid
    }

    /// Get high watermark for partition with ProduceHandler + metadata_store fallback
    ///
    /// v2.2.9 FIX: Fallback to metadata_store on followers where ProduceHandler is empty
    ///
    /// Complexity: < 20 (ProduceHandler query + fallback path + logging)
    async fn get_high_watermark_for_fetch(
        &self,
        topic: &str,
        partition: i32,
        fetch_offset: i64,
    ) -> Result<i64> {
        // Try ProduceHandler first (source of truth on leaders)
        let mut high_watermark = if let Some(ref produce_handler) = self.produce_handler {
            produce_handler.get_high_watermark(topic, partition).await
                .unwrap_or_else(|e| {
                    tracing::warn!(
                        "Failed to get high watermark from ProduceHandler for {}-{}: {}, defaulting to 0",
                        topic, partition, e
                    );
                    0
                })
        } else {
            0
        };

        // v2.2.9 FIX: Fallback to metadata_store on followers
        if high_watermark == 0 {
            if let Ok(Some((meta_hwm, _log_start))) = self.metadata_store.get_partition_offset(topic, partition as u32).await {
                if meta_hwm > 0 {
                    tracing::debug!(
                        "📊 WATERMARK FALLBACK: Using metadata_store for {}-{}: {} (ProduceHandler returned 0)",
                        topic, partition, meta_hwm
                    );
                    high_watermark = meta_hwm;
                }
            }
        }

        // Per-fetch, and the long poll now re-evaluates it every 10ms, so this
        // cannot be `info!` — it would emit a hundred lines per second for every
        // parked consumer and every follower.
        debug!(
            "📊 WATERMARK: topic={}, partition={}, high_watermark={}, fetch_offset={}, gap={} (from ProduceHandler)",
            topic, partition, high_watermark, fetch_offset, high_watermark - fetch_offset
        );

        Ok(high_watermark)
    }

    /// RP-2.3: cap a consumer's view at what the in-sync set actually holds.
    ///
    /// Gated on pull replication. Under push, follower positions come from ACK
    /// frames whose delivery this roadmap has already had to fix three times;
    /// bounding consumer visibility on that data would convert a reporting bug
    /// into a stall. Under pull the position is the follower's own fetch offset,
    /// which cannot be stale without the follower having stopped — in which case
    /// it leaves ISR and stops constraining the watermark.
    ///
    /// Single-node is unaffected: there is no ISR tracker and no replicas.
    async fn consumer_visible_watermark(
        &self,
        topic: &str,
        partition: i32,
        leader_leo: i64,
    ) -> i64 {
        if !self.hw_from_isr {
            return leader_leo;
        }
        let Some(ref tracker) = self.isr_tracker else {
            return leader_leo;
        };

        let assignments = match self.metadata_store.get_partition_assignments(topic).await {
            Ok(a) => a,
            Err(e) => {
                debug!("No assignments for {} ({}); serving the leader's position", topic, e);
                return leader_leo;
            }
        };
        let Some(assignment) = assignments.iter().find(|a| a.partition == partition as u32) else {
            return leader_leo;
        };

        let replicas: Vec<u64> = assignment.replicas.to_vec();
        let watermark = tracker.replicated_watermark(
            topic,
            partition,
            leader_leo,
            &replicas,
            assignment.leader_id,
        );

        if watermark < leader_leo {
            debug!(
                "{}-{}: consumers capped at {} (leader is at {}) — the in-sync set is behind",
                topic, partition, watermark, leader_leo
            );
        }
        watermark
    }

    /// How far this fetch may read, from the live log.
    ///
    /// The two callers ask the same question at different moments — once when
    /// the request arrives, and again on every tick of the long poll — and they
    /// must agree, or a request parks waiting for a number that a different rule
    /// already moved past.
    ///
    /// A follower is bounded by the leader's LOG END; a consumer by what the
    /// in-sync set holds. Serving a follower the high watermark instead is what
    /// made `acks=all` cost ~700ms per request (#36): the producer waits for the
    /// follower to acknowledge offset N, and the follower is told the log ends
    /// below N, so neither side can move. The high watermark is a *result* of
    /// replication and cannot also be its input.
    async fn readable_end_offset(
        &self,
        topic: &str,
        partition: i32,
        fetch_offset: i64,
        replica_id: i32,
    ) -> Result<i64> {
        if replica_id < 0 {
            let leader_leo = self
                .get_high_watermark_for_fetch(topic, partition, fetch_offset)
                .await?;
            return Ok(self
                .consumer_visible_watermark(topic, partition, leader_leo)
                .await);
        }

        let log_end = match self.produce_handler {
            Some(ref handler) => handler.get_log_end_offset(topic, partition).await,
            None => 0,
        };

        // A node that has not produced since starting has no in-memory partition
        // state and reports 0. That is a "don't know", not "empty" — fall back to
        // the watermark path, which reads persisted metadata.
        if log_end == 0 {
            return self
                .get_high_watermark_for_fetch(topic, partition, fetch_offset)
                .await;
        }

        // Bounded by what this leader can actually serve, not by what it has
        // assigned.
        //
        // Offsets are handed out before the WAL writes them, so for a moment the
        // log end runs ahead of the log. A follower told the assigned end asks
        // for an offset inside that gap and gets an empty response — it cannot
        // tell "not yet written" from "nothing there" — so it backs off and asks
        // again. Measured after the read path was fixed: 1-2% of fetch cycles
        // came back empty, each costing `EMPTY_FETCH_BACKOFF`.
        //
        // Reporting the durable end instead turns that into a park: the request
        // waits inside the long poll and the commit worker wakes it the moment
        // the record is on disk. Nothing is withheld that could have been sent —
        // the records in the gap were not servable either way.
        //
        // `None` means nothing has committed under this process yet, which is a
        // "don't know" and leaves the assigned end as the best answer available.
        if let Some(ref wal) = self.wal_manager {
            if let Some(durable_end) = wal.durable_end_offset(topic, partition) {
                return Ok(log_end.min(durable_end));
            }
        }

        Ok(log_end)
    }

    /// Get the partition's log start offset (low watermark) for a fetch.
    ///
    /// Advanced by the Kafka DeleteRecords API; records below it were deleted.
    /// ProduceHandler is authoritative (and itself falls back to persisted
    /// metadata); if there is no ProduceHandler we read metadata directly.
    async fn get_log_start_offset_for_fetch(&self, topic: &str, partition: i32) -> i64 {
        if let Some(ref produce_handler) = self.produce_handler {
            return produce_handler.get_log_start_offset(topic, partition).await;
        }
        if let Ok(Some((_hwm, log_start))) = self.metadata_store.get_partition_offset(topic, partition as u32).await {
            return log_start.max(0);
        }
        0
    }

    /// Validate fetch_offset is within valid range
    ///
    /// Returns None if valid, Some(error_response) if out of range
    ///
    /// Complexity: < 10 (simple range check + error construction)
    fn check_offset_range(
        &self,
        partition: i32,
        fetch_offset: i64,
        log_start_offset: i64,
        high_watermark: i64,
    ) -> Option<FetchResponsePartition> {
        if fetch_offset < log_start_offset {
            Some(FetchResponsePartition {
                partition,
                error_code: 1, // OFFSET_OUT_OF_RANGE
                high_watermark,
                last_stable_offset: high_watermark,
                log_start_offset,
                aborted: None,
                preferred_read_replica: -1,
                records: vec![],
            })
        } else {
            None
        }
    }

    /// Fetch data when available (fetch_offset < high_watermark)
    ///
    /// Tries fetch_raw_bytes() first (CRC-preserving), falls back to fetch_records()
    ///
    /// Complexity: < 25 (timeout handling + raw bytes path + fallback path + encoding)
    #[allow(clippy::too_many_arguments)] // internal call that threads its caller's state through
    async fn fetch_data_available_path(
        &self,
        topic: &str,
        partition: i32,
        fetch_offset: i64,
        high_watermark: i64,
        log_start_offset: i64,
        max_bytes: i32,
        _max_wait_ms: i32,
        fetch_start: Instant,
    ) -> Result<FetchResponsePartition> {
        // v2.2.7.2: Log data available path
        debug!(
            "DATA AVAILABLE: topic={}, partition={}, fetch_offset={}, high_watermark={}, available={}",
            topic, partition, fetch_offset, high_watermark, high_watermark - fetch_offset
        );

        // Data is available, fetch it.
        //
        // `max_wait_ms` is NOT the budget for this read. In Kafka it bounds how
        // long the broker waits for `min_bytes` to *become* available; once data
        // exists the broker returns it. Using it as a deadline on the read itself
        // meant that when a busy leader took longer than the client's poll budget
        // to serve records that were already there, the read was thrown away and
        // an EMPTY response returned — so the follower immediately re-fetched and
        // the leader redid the same work.
        //
        // That is a positive feedback loop, and it made cluster throughput
        // bimodal: a run settled at either ~85,000 msg/s or ~8,000, never in
        // between, on `acks=1` and `acks=all` but never `acks=0` or single-node.
        // Raising the follower's window tenfold
        // (CHRONIK_REPLICA_FETCH_MAX_WAIT_MS=5000) removed the collapse
        // completely — 8 runs of 8 clean at 89,291-97,316 against 2-in-8
        // collapsing at the 500ms default — which is what identified this line.
        // See RP-12 in docs/ROADMAP_REPLICATION.md.
        //
        // What remains is a safety net against a genuinely stuck read, which is
        // what the old `else` branch already used, applied to both branches.
        const READ_SAFETY_DEADLINE: Duration = Duration::from_secs(30);
        let fetch_timeout = READ_SAFETY_DEADLINE;

        // CRITICAL CRC FIX v1.3.32: Try to fetch raw Kafka bytes first to preserve CRC
        debug!("FETCH RAW: trying raw bytes (CRC-preserving) for {}-{}", topic, partition);
        let raw_bytes_result = timeout(fetch_timeout, async {
            self.fetch_raw_bytes(
                topic,
                partition,
                fetch_offset,
                high_watermark,
                max_bytes,
            ).await
        }).await;

        let records_bytes = match raw_bytes_result {
            Ok(Ok(Some(raw_bytes))) => {
                tracing::debug!("CRC-PRESERVED: fetched {} bytes of raw Kafka data for {}-{}",
                    raw_bytes.len(), topic, partition);

                // HEX DUMP: Outgoing raw bytes to consumer
                if raw_bytes.len() >= 61 {
                    let hex_first_64: String = raw_bytes.iter().take(64).map(|b| format!("{:02x}", b)).collect::<Vec<_>>().join(" ");
                    tracing::debug!("FETCH OUTGOING (first 64 bytes): {}", hex_first_64);
                }

                raw_bytes
            }
            Err(_) => {
                // The parsed-records fallback below reads the same WAL range,
                // so running it after a timeout only doubled the time a stuck
                // read held the request — 60s per fetch instead of 30.
                tracing::warn!(
                    "Fetch read exceeded the {}s safety deadline for {}-{} at offset {} — returning empty",
                    READ_SAFETY_DEADLINE.as_secs(), topic, partition, fetch_offset
                );
                Vec::new()
            }
            Ok(Ok(None)) | Ok(Err(_)) => {
                // Fall back to parsed records (will recompute CRC)
                tracing::debug!("CRC-RECOMPUTED: no raw bytes available, falling back to parsed records for {}-{}",
                    topic, partition);

                let fetch_result = timeout(fetch_timeout, async {
                    self.fetch_records(
                        topic,
                        partition,
                        fetch_offset,
                        high_watermark,
                        max_bytes,
                    ).await
                }).await;

                let records = match fetch_result {
                    Ok(Ok(recs)) => {
                        tracing::trace!("Fetched {} records from {}-{}", recs.len(), topic, partition);
                        recs
                    },
                    Ok(Err(e)) => {
                        tracing::warn!("Error fetching records from {}-{}: {:?}", topic, partition, e);
                        vec![]
                    }
                    Err(_) => {
                        // 30s safety deadline, not the client's max_wait — see the
                        // READ_SAFETY_DEADLINE comment above. Reaching this means a read
                        // is genuinely stuck, not merely slow.
                        tracing::warn!(
                            "Fetch read exceeded the {}s safety deadline for {}-{} — returning empty",
                            READ_SAFETY_DEADLINE.as_secs(), topic, partition
                        );
                        vec![]
                    }
                };

                // Encode the records - will recompute CRC
                self.encode_kafka_records(&records, 0)?
            }
        };

        // v2.2.7.2: Log successful fetch completion
        let fetch_elapsed = fetch_start.elapsed();
        debug!(
            "✅ FETCH SUCCESS: topic={}, partition={}, offset={}, bytes={}, elapsed={:?}",
            topic, partition, fetch_offset, records_bytes.len(), fetch_elapsed
        );

        Ok(self.build_success_response(partition, high_watermark, log_start_offset, records_bytes))
    }

    /// Build success FetchResponsePartition
    ///
    /// Complexity: < 5 (simple struct construction)
    fn build_success_response(
        &self,
        partition: i32,
        high_watermark: i64,
        log_start_offset: i64,
        mut records: Vec<u8>,
    ) -> FetchResponsePartition {
        // EOS: every response path funnels through here, so sanitize control-batch CRCs
        // in one place (see sanitize_batch_crcs). This makes transaction COMMIT/ABORT
        // markers self-consistent on the wire regardless of which serving path assembled
        // them; producer data batches are left byte-identical.
        sanitize_batch_crcs(&mut records);
        FetchResponsePartition {
            partition,
            error_code: 0,
            high_watermark,
            last_stable_offset: high_watermark,
            log_start_offset,
            aborted: None,
            preferred_read_replica: -1,
            records,
        }
    }

    // ============================================================================
    // Main fetch_partition() Function (Refactored)
    // ============================================================================

    /// Fetch data from a specific partition
    ///
    /// Refactored in Phase 2.13 to reduce from 299 lines to 51 lines (82.9% reduction)
    /// Orchestrates 5 helper methods for clean separation of concerns
    ///
    /// Complexity: < 20 (validation calls + routing logic)
    async fn fetch_partition(
        &self,
        topic: &str,
        partition: i32,
        fetch_offset: i64,
        max_bytes: i32,
        max_wait_ms: i32,
        replica_id: i32,
    ) -> Result<FetchResponsePartition> {
        // RP-2.1: a fetch carrying a replica id is a *follower* replicating, not a
        // consumer reading. Kafka clients send -1 here; only a broker sets it to
        // its own node id. The field has always been decoded and thrown away.
        //
        // Recording the follower's position is the whole point: its fetch offset
        // IS its log end offset — it cannot ask for offset N without having
        // durably written everything below N. That gives the leader progress
        // tracking for free, where the push model needed a separate ACK channel,
        // a liveness heartbeat, and a reconnect probe to approximate the same
        // thing (see the RP-1 cluster findings).
        if replica_id >= 0 {
            // The follower's position is recorded below, once the leader's log
            // end is known: whether it is keeping up is relative to that.
            if let Some(ref tracker) = self.isr_tracker {
                tracker.record_node_alive(replica_id as u64);
                debug!(
                    "Follower fetch: node {} at offset {} for {}-{}",
                    replica_id, fetch_offset, topic, partition
                );
            }

            // RP-2.3: the same offset also settles `acks=all`. A follower asking
            // for N holds everything below N, so this is the pull equivalent of
            // the ACK frame the push path sends — and it is why the ack tracker
            // had to become offset-monotonic: a fetch offset skips across many
            // batch boundaries and rarely equals a registered wait exactly.
            if let Some(ref tracker) = self.isr_ack_tracker {
                tracker.record_ack(topic, partition, fetch_offset, replica_id as u64);
            }
        }

        // v2.2.7.2: Enhanced tracing to debug large batch consumption stalls
        let fetch_start = Instant::now();
        debug!(
            "🔍 FETCH START: topic={}, partition={}, offset={}, max_bytes={}, max_wait_ms={}",
            topic, partition, fetch_offset, max_bytes, max_wait_ms
        );

        // Phase 1: Validate topic and partition existence
        if let Some(error_response) = self.validate_topic_and_partition(topic, partition).await? {
            return Ok(error_response);
        }

        // RP-7: refuse to serve a partition this node does not lead.
        //
        // A node whose catalog is stale can believe it still leads a partition
        // that failed over while it was away. Measured: a returning node served
        // 17,259 fetches as leader of a partition whose log was empty — every
        // consumer routed there saw an empty topic, and a follower pointed at it
        // replicated nothing, while all three nodes reported
        // `under_replicated: false`. Answering "no records" is indistinguishable
        // from "caught up", which is what made it silent.
        //
        // NOT_LEADER_OR_FOLLOWER is the Kafka-correct answer: clients refresh
        // metadata and retry against the real leader, and a replica fetcher
        // treats it as a signal to re-read assignments rather than as data.
        //
        // Deliberately narrow — this rejects only when metadata *positively*
        // names a different node. No assignment, or one naming this node, still
        // serves, so single-node and not-yet-assigned topics are untouched.
        if let Some(error_response) = self.reject_if_not_leader(topic, partition).await {
            return Ok(error_response);
        }

        // Phase 2: how far this fetch may read.
        //
        // RP-2.3: a follower reads up to the leader's log end; a consumer only up
        // to what the in-sync set holds. Serving a consumer past that would show
        // it a record that disappears if the leader is lost — the leader's write
        // position is not a durability statement.
        //
        // A follower must NOT be capped this way, or replication deadlocks: the
        // watermark cannot advance until followers fetch, and they cannot fetch
        // past a watermark that is waiting on them. Both branches live in
        // `readable_end_offset`, which the long poll below re-evaluates.
        let high_watermark = self
            .readable_end_offset(topic, partition, fetch_offset, replica_id)
            .await?;

        // For a follower that bound IS the leader's log end, so this is where
        // its fetch can be judged: caught up, or merely still fetching.
        if replica_id >= 0 {
            if let Some(ref tracker) = self.isr_tracker {
                tracker.record_follower_fetch(
                    replica_id as u64,
                    topic,
                    partition,
                    fetch_offset,
                    high_watermark,
                );
            }
        }

        // Log start offset (low watermark). Advanced by the Kafka DeleteRecords API;
        // records below it have been deleted and must not be served. Sourced from
        // ProduceHandler (authoritative), falling back to persisted metadata.
        let log_start_offset = self.get_log_start_offset_for_fetch(topic, partition).await;

        // Phase 3: Validate fetch_offset is within valid range. A fetch below the
        // low watermark (deleted range) returns OFFSET_OUT_OF_RANGE.
        if let Some(error_response) = self.check_offset_range(partition, fetch_offset, log_start_offset, high_watermark) {
            return Ok(error_response);
        }

        // Phase 4-5: Route to data available or no data path
        if fetch_offset < high_watermark {
            // Data available - fetch and return
            self.fetch_data_available_path(
                topic,
                partition,
                fetch_offset,
                high_watermark,
                log_start_offset,
                max_bytes,
                max_wait_ms,
                fetch_start,
            )
            .await
        } else {
            // Nothing past this offset. Returning empty is the whole of it —
            // waiting for something to arrive belongs to the request, not to one
            // partition of it, and happens in `wait_for_any_partition`.
            let empty_records = self.encode_kafka_records(&[], 0)?;
            Ok(self.build_success_response(
                partition,
                high_watermark,
                log_start_offset,
                empty_records,
            ))
        }
    }

    /// Try fetching raw bytes from Tantivy (Phase 4 for fetch_raw_bytes)
    ///
    /// Complexity: < 20 (Tantivy fetch delegation)
    ///
    /// Returns: Optional raw batch bytes from Tantivy
    async fn try_fetch_raw_from_tantivy(
        &self,
        topic: &str,
        partition: i32,
        fetch_offset: i64,
        high_watermark: i64,
        max_bytes: i32,
    ) -> Result<Option<Vec<u8>>> {
        let segment_index = match &self.segment_index {
            Some(idx) => idx,
            None => return Ok(None),
        };

        tracing::debug!("RAW→TANTIVY: checking Tantivy segment index for {}-{}", topic, partition);

        match self.fetch_from_tantivy(
            segment_index,
            topic,
            partition,
            fetch_offset,
            high_watermark,
            max_bytes,
        ).await {
            Ok(Some(bytes)) => {
                tracing::debug!(
                    "RAW→TANTIVY: returning {} bytes from Tantivy segments",
                    bytes.len()
                );
                Ok(Some(bytes))
            }
            Ok(None) => {
                tracing::debug!("RAW→TANTIVY: no matching Tantivy segments found");
                Ok(None)
            }
            Err(e) => {
                tracing::warn!("RAW→TANTIVY: Error fetching from Tantivy: {}", e);
                Ok(None)
            }
        }
    }

    /// Try fetching raw bytes from segments (Phase 3 for fetch_raw_bytes)
    ///
    /// Complexity: < 25 (segment iteration + batch header parsing + filtering)
    ///
    /// Returns: Optional raw batch bytes from segments
    async fn try_fetch_raw_from_segments(
        &self,
        topic: &str,
        partition: i32,
        fetch_offset: i64,
        high_watermark: i64,
        max_bytes: i32,
    ) -> Result<Option<Vec<u8>>> {
        tracing::debug!("RAW→SEGMENTS: buffer and WAL empty or no match, trying segments");
        let segments = self.get_segments_for_range(topic, partition, fetch_offset, high_watermark).await?;

        if segments.is_empty() {
            tracing::debug!("RAW→SEGMENTS: no segments found for range");
            return Ok(None);
        }

        let mut combined_bytes = Vec::new();
        // Labelled: a full budget must end the scan, not just this segment —
        // otherwise every remaining segment up to the high watermark is still
        // downloaded and deserialized, only to contribute nothing.
        'segments: for segment_info in &segments {
            // A segment the catalog names may not be in this node's store; it
            // is one fewer source, not a failed fetch.
            let data = match self.object_store.get(&segment_info.object_key).await {
                Ok(data) => data,
                Err(e) => {
                    tracing::debug!("RAW→SEGMENT: {} unavailable: {}", segment_info.object_key, e);
                    continue 'segments;
                }
            };

            // The WalIndexer writes raw segments as bincode `Vec<CanonicalRecord>`
            // — the WAL's own batches, with their original wire bytes. This tier
            // only understood the older `Segment` container, so every read of an
            // indexer-written segment failed to deserialize, the error ended the
            // raw read, and archived records could only be served re-encoded by
            // the parsed fallback (which the CRC rules above exist to avoid).
            let segment = match Segment::deserialize(data.clone()) {
                Ok(segment) => segment,
                Err(_) => {
                    let mut canonicals: Vec<chronik_storage::canonical_record::CanonicalRecord> =
                        match chronik_storage::archive::decode_raw_segment(&data) {
                            Ok(c) => c,
                            Err(e) => {
                                tracing::warn!(
                                    "RAW→SEGMENT: {} is neither a Segment nor canonical records: {}",
                                    segment_info.object_key, e
                                );
                                continue 'segments;
                            }
                        };
                    canonicals.sort_by_key(|c| c.base_offset);
                    for c in &canonicals {
                        if c.last_offset() < fetch_offset || c.base_offset >= high_watermark {
                            continue;
                        }
                        let batch = c.to_kafka_batch().map_err(|e| {
                            Error::Internal(format!("rebuilding batch from {}: {}", segment_info.object_key, e))
                        })?;
                        if !combined_bytes.is_empty() && combined_bytes.len() + batch.len() > max_bytes as usize {
                            break 'segments;
                        }
                        combined_bytes.extend_from_slice(&batch);
                    }
                    continue 'segments;
                }
            };

            tracing::debug!(
                "RAW→SEGMENT: segment {} has {} bytes of raw_kafka_batches",
                segment_info.segment_id, segment.raw_kafka_batches.len()
            );

            if segment.raw_kafka_batches.is_empty() {
                // Segment doesn't have raw bytes (v1 format or indexed-only)
                // Cannot preserve CRC, need to fall back to parsed records
                tracing::warn!(
                    "RAW→SEGMENT: Segment {} has no raw_kafka_batches, cannot preserve CRC",
                    segment_info.segment_id
                );
                // Serve what was gathered; the next fetch starts here and falls
                // back to the parsed path for this segment.
                if combined_bytes.is_empty() {
                    return Ok(None);
                }
                break 'segments;
            }

            // CRITICAL FIX: Parse batch headers to filter by offset range
            // We must ONLY include batches that overlap [fetch_offset, high_watermark)
            // Otherwise we return wrong batches and clients see CRC errors!
            let mut cursor_pos = 0;

            while cursor_pos < segment.raw_kafka_batches.len() {
                let batch_start = cursor_pos;

                // Read batch header (minimum 61 bytes for v2 format)
                if (segment.raw_kafka_batches.len() - batch_start) < 61 {
                    // Not enough bytes for a valid batch
                    break;
                }

                // Parse JUST the header to get offsets (without decoding records)
                // Use manual big-endian byte parsing to avoid any trait complications
                let base_offset = i64::from_be_bytes([
                    segment.raw_kafka_batches[batch_start],
                    segment.raw_kafka_batches[batch_start + 1],
                    segment.raw_kafka_batches[batch_start + 2],
                    segment.raw_kafka_batches[batch_start + 3],
                    segment.raw_kafka_batches[batch_start + 4],
                    segment.raw_kafka_batches[batch_start + 5],
                    segment.raw_kafka_batches[batch_start + 6],
                    segment.raw_kafka_batches[batch_start + 7],
                ]);

                let batch_length = i32::from_be_bytes([
                    segment.raw_kafka_batches[batch_start + 8],
                    segment.raw_kafka_batches[batch_start + 9],
                    segment.raw_kafka_batches[batch_start + 10],
                    segment.raw_kafka_batches[batch_start + 11],
                ]);

                // Read last_offset_delta at offset 23 (after base_offset, batch_length, partition_leader_epoch, magic, crc, attributes)
                let last_offset_delta = i32::from_be_bytes([
                    segment.raw_kafka_batches[batch_start + 23],
                    segment.raw_kafka_batches[batch_start + 24],
                    segment.raw_kafka_batches[batch_start + 25],
                    segment.raw_kafka_batches[batch_start + 26],
                ]);
                let last_offset = base_offset + last_offset_delta as i64;

                // Total batch size is: 12 bytes (base_offset + batch_length) + batch_length
                let total_batch_size = 12 + batch_length as usize;

                tracing::debug!(
                    "RAW→BATCH: Found batch at offset {}, base_offset={}, last_offset={}, size={}",
                    batch_start, base_offset, last_offset, total_batch_size
                );

                // Check if this batch overlaps with requested range [fetch_offset, high_watermark)
                if last_offset >= fetch_offset && base_offset < high_watermark {
                    // This batch is in range, include it
                    if combined_bytes.len() + total_batch_size > max_bytes as usize && !combined_bytes.is_empty() {
                        // Would exceed max_bytes, stop here
                        break 'segments;
                    }

                    let batch_bytes = &segment.raw_kafka_batches[batch_start..batch_start + total_batch_size];
                    combined_bytes.extend_from_slice(batch_bytes);

                    tracing::debug!(
                        "RAW→BATCH: Including {} bytes from batch {}-{}",
                        total_batch_size, base_offset, last_offset
                    );
                } else {
                    tracing::debug!(
                        "RAW→BATCH: Skipping batch {}-{} (outside range {}-{})",
                        base_offset, last_offset, fetch_offset, high_watermark
                    );
                }

                // Move to next batch
                cursor_pos = batch_start + total_batch_size;
            }
        }

        if !combined_bytes.is_empty() {
            tracing::debug!(
                "RAW→SEGMENTS: Returning {} bytes of raw Kafka data from {} segments",
                combined_bytes.len(), segments.len()
            );
            Ok(Some(combined_bytes))
        } else {
            Ok(None)
        }
    }

    /// Try fetching raw bytes from WAL (Phase 2 for fetch_raw_bytes)
    ///
    /// Complexity: < 15 (WAL fetch delegation)
    ///
    /// Returns: Optional raw batch bytes from WAL
    async fn try_fetch_raw_from_wal(
        &self,
        topic: &str,
        partition: i32,
        fetch_offset: i64,
        high_watermark: i64,
        max_bytes: i32,
    ) -> Result<Option<Vec<u8>>> {
        tracing::debug!("RAW→WAL: Buffer empty or no match, trying WAL");

        if let Some(wal_manager) = self.wal_manager.as_ref() {
            let raw_bytes_from_wal = self.fetch_raw_bytes_from_wal(
                wal_manager,
                topic,
                partition,
                fetch_offset,
                high_watermark,
                max_bytes,
            ).await?;

            if let Some(bytes) = raw_bytes_from_wal {
                tracing::debug!(
                    "RAW→WAL: Returning {} bytes of raw Kafka data from WAL",
                    bytes.len()
                );
                return Ok(Some(bytes));
            }
            tracing::debug!("RAW→WAL: No raw bytes found in WAL");
        }

        Ok(None)
    }

    /// Try fetching raw bytes from buffer (Phase 1 for fetch_raw_bytes)
    ///
    /// Complexity: < 20 (buffer lookup + byte concatenation)
    ///
    /// Returns: Optional raw batch bytes from buffer
    async fn try_fetch_raw_from_buffer(
        &self,
        topic: &str,
        partition: i32,
        fetch_offset: i64,
        high_watermark: i64,
        max_bytes: i32,
    ) -> Option<Vec<u8>> {
        let state = self.state.read().await;
        let buffer = state.buffers.get(&(topic.to_string(), partition))?;

        tracing::debug!(
            "RAW→BUFFER: Checking buffer for {}-{}, buffer has {} batches",
            topic, partition, buffer.batch_metadata.len()
        );

        let mut combined_bytes = Vec::new();
        for (batch_idx, metadata) in buffer.batch_metadata.iter().enumerate() {
            // Check if this batch overlaps with requested range
            if metadata.last_offset >= fetch_offset && metadata.base_offset < high_watermark {
                let raw_batch = &buffer.raw_batches[batch_idx];

                if combined_bytes.len() + raw_batch.len() > max_bytes as usize && !combined_bytes.is_empty() {
                    break;
                }

                tracing::debug!(
                    "RAW→BUFFER: Adding {} bytes from batch at offsets {}-{}",
                    raw_batch.len(), metadata.base_offset, metadata.last_offset
                );
                combined_bytes.extend_from_slice(raw_batch);
            }
        }

        if !combined_bytes.is_empty() {
            tracing::debug!(
                "RAW→BUFFER: Returning {} bytes of raw Kafka data from buffer",
                combined_bytes.len()
            );
            Some(combined_bytes)
        } else {
            None
        }
    }

    /// Try fetching records from Tantivy archives (Phase 4 - cold storage)
    ///
    /// Complexity: < 25 (Tantivy fetch + merge + sort)
    ///
    /// Returns: updated records vector with Tantivy records merged in
    async fn try_fetch_from_tantivy_phase(
        &self,
        topic: &str,
        partition: i32,
        fetch_offset: i64,
        high_watermark: i64,
        max_bytes: i32,
        mut records: Vec<chronik_storage::Record>,
    ) -> Result<Vec<chronik_storage::Record>> {
        if self.segment_index.is_none() {
            return Ok(records);
        }

        debug!(
            "FETCH→TANTIVY: Trying Tantivy archives for {}-{} (have {} records so far)",
            topic, partition, records.len()
        );

        // Try to get records via Tantivy fetch (will download tar.gz, search index, return results)
        // This is a fallback for very old archived data
        match self.fetch_from_tantivy_for_records(
            topic,
            partition,
            fetch_offset,
            high_watermark,
            max_bytes
        ).await {
            Ok(tantivy_records) if !tantivy_records.is_empty() => {
                debug!(
                    "FETCH→TANTIVY: Successfully fetched {} records from Tantivy archives for {}-{}",
                    tantivy_records.len(), topic, partition
                );

                merge_by_offset(&mut records, tantivy_records);
            }
            Ok(_) => {
                debug!(
                    "FETCH→TANTIVY: No records found in Tantivy archives for {}-{} at offset {}",
                    topic, partition, fetch_offset
                );
            }
            Err(e) => {
                warn!(
                    "FETCH→TANTIVY: Failed to fetch from Tantivy archives for {}-{}: {}",
                    topic, partition, e
                );
            }
        }

        Ok(records)
    }

    /// Try fetching records from S3 raw segments (Phase 3 - warm storage)
    ///
    /// Complexity: < 25 (S3 fetch + merge + sort)
    ///
    /// Returns: updated records vector with S3 records merged in
    async fn try_fetch_from_s3_phase(
        &self,
        topic: &str,
        partition: i32,
        fetch_offset: i64,
        high_watermark: i64,
        max_bytes: i32,
        mut records: Vec<chronik_storage::Record>,
    ) -> Result<Vec<chronik_storage::Record>> {
        debug!(
            "FETCH→S3_RAW_SEGMENTS: Trying to download raw segment from S3 for {}-{} (have {} records so far)",
            topic, partition, records.len()
        );

        match self.fetch_from_s3_raw_segments(
            topic,
            partition,
            fetch_offset,
            high_watermark,
            max_bytes
        ).await {
            Ok(s3_records) if !s3_records.is_empty() => {
                debug!(
                    "FETCH→S3_RAW_SEGMENTS: Successfully fetched {} records from S3 for {}-{}",
                    s3_records.len(), topic, partition
                );

                merge_by_offset(&mut records, s3_records);
            }
            Ok(_) => {
                debug!(
                    "FETCH→S3_RAW_SEGMENTS: No records found in S3 raw segments for {}-{} at offset {}",
                    topic, partition, fetch_offset
                );
            }
            Err(e) => {
                warn!(
                    "FETCH→S3_RAW_SEGMENTS: Failed to fetch from S3 for {}-{}: {} - will try legacy segments",
                    topic, partition, e
                );
            }
        }

        Ok(records)
    }

    /// Try fetching records from WAL (Phase 2 - fallback for trimmed buffer)
    ///
    /// Complexity: < 25 (gap detection + WAL fetch + merge)
    ///
    /// Returns: updated records vector with WAL records merged in
    #[allow(clippy::too_many_arguments)] // internal call that threads its caller's state through
    async fn try_fetch_from_wal_phase(
        &self,
        topic: &str,
        partition: i32,
        fetch_offset: i64,
        max_bytes: i32,
        mut records: Vec<chronik_storage::Record>,
        buffer_highest_offset: i64,
        buffer_min_offset: i64,
    ) -> Result<Vec<chronik_storage::Record>> {
        // v1.3.48: Improved WAL fallback logic to detect buffer gaps from trimming
        // Check if we need to read from WAL for missing data
        let need_earlier_records = records.is_empty() ||
            (buffer_highest_offset >= 0 && fetch_offset < buffer_highest_offset);

        // NEW (v1.3.48): Detect if fetch_offset is before buffer's min_offset (trimmed region)
        let fetch_in_trimmed_region = buffer_min_offset >= 0 && fetch_offset < buffer_min_offset;

        // Try WAL for any missing records or to continue the fetch
        // WAL should have records that were trimmed from buffer
        if let Some(wal_manager) = &self.wal_manager {
            // Try WAL if:
            // 1. Buffer is empty, OR
            // 2. Need earlier records than buffer has, OR
            // 3. Fetch offset is in trimmed region (gap in buffer)
            let should_try_wal = records.is_empty() || need_earlier_records || fetch_in_trimmed_region;

            if should_try_wal {
                if fetch_in_trimmed_region {
                    debug!(
                        "FETCH→WAL_FALLBACK: fetch_offset={} is before buffer min_offset={} (trimmed), reading from WAL",
                        fetch_offset, buffer_min_offset
                    );
                }
                match self.fetch_from_wal(wal_manager, topic, partition, fetch_offset, max_bytes).await {
                    Ok(wal_records) => {
                        if !wal_records.is_empty() {
                            debug!(
                                "FETCH→WAL: Successfully fetched {} records from WAL for {}-{}",
                                wal_records.len(), topic, partition
                            );
                            merge_by_offset(&mut records, wal_records);
                        } else {
                            debug!(
                                "FETCH→WAL: No records found in WAL for {}-{} at offset {}",
                                topic, partition, fetch_offset
                            );
                        }
                    }
                    Err(e) => {
                        // The indexer deletes a WAL segment once it is in the segment
                        // tier, so a missing segment is the normal hand-off to the
                        // next tier, on every fetch of older data. Anything else is a
                        // real read failure.
                        let msg = e.to_string();
                        if msg.contains("Segment not found") {
                            debug!(
                                "FETCH→WAL: WAL segment gone for {}-{} ({}), trying segments",
                                topic, partition, msg
                            );
                        } else {
                            warn!(
                                "FETCH→WAL: Failed to fetch from WAL for {}-{}: {} - will try segments",
                                topic, partition, msg
                            );
                        }
                    }
                }
            }
        }

        Ok(records)
    }

    /// Try fetching records from in-memory buffer (Phase 1 - hottest path)
    ///
    /// Complexity: < 25 (buffer lookup + batch decode loop)
    ///
    /// Returns: (records, bytes_fetched, buffer_highest_offset, buffer_min_offset)
    async fn fetch_from_buffer(
        &self,
        topic: &str,
        partition: i32,
        fetch_offset: i64,
        high_watermark: i64,
        max_bytes: i32,
    ) -> Result<(Vec<chronik_storage::Record>, usize, i64, i64)> {
        let state = self.state.read().await;
        let buffer = match state.buffers.get(&(topic.to_string(), partition)) {
            Some(b) => b,
            None => return Ok((vec![], 0, -1, -1)),
        };

        debug!(
            "FETCH→BUFFER: Checking buffer for {}-{}, buffer has {} batches, min_offset={}",
            topic, partition, buffer.batch_metadata.len(), buffer.min_offset_in_buffer
        );

        let mut records = Vec::new();
        let mut bytes_fetched = 0usize;
        let mut buffer_max_offset = -1i64;
        let buffer_min = buffer.min_offset_in_buffer;

        for (batch_idx, metadata) in buffer.batch_metadata.iter().enumerate() {
            if metadata.last_offset >= fetch_offset && metadata.base_offset < high_watermark {
                // Decode records from raw batch
                let raw_batch = &buffer.raw_batches[batch_idx];
                match self.decode_records_from_raw_batch(raw_batch, fetch_offset, high_watermark) {
                    Ok(batch_records) => {
                        for record in batch_records {
                            let record_size = record.value.len() +
                                record.key.as_ref().map(|k| k.len()).unwrap_or(0) + 24;

                            if bytes_fetched + record_size > max_bytes as usize && !records.is_empty() {
                                break;
                            }

                            debug!(
                                "FETCH→BUFFER: Found record at offset {} in buffer",
                                record.offset
                            );

                            records.push(record.clone());
                            bytes_fetched += record_size;
                            buffer_max_offset = buffer_max_offset.max(record.offset);
                        }
                    }
                    Err(e) => {
                        tracing::error!("Failed to decode batch from buffer: {}", e);
                        continue;
                    }
                }
            }
        }

        if !records.is_empty() {
            debug!(
                "FETCH→BUFFER: Fetched {} records from buffer for {}-{}, highest offset: {}",
                records.len(), topic, partition, buffer_max_offset
            );
        }

        Ok((records, bytes_fetched, buffer_max_offset, buffer_min))
    }

    /// Fetch records with proper priority: Buffer → WAL → Segments, cut to the
    /// fetch's byte budget.
    async fn fetch_records(
        &self,
        topic: &str,
        partition: i32,
        fetch_offset: i64,
        high_watermark: i64,
        max_bytes: i32,
    ) -> Result<Vec<chronik_storage::Record>> {
        let mut records = self
            .fetch_records_uncapped(topic, partition, fetch_offset, high_watermark, max_bytes)
            .await?;
        records.sort_by_key(|r| r.offset);
        cap_to_byte_budget(&mut records, max_bytes);
        Ok(records)
    }

    async fn fetch_records_uncapped(
        &self,
        topic: &str,
        partition: i32,
        fetch_offset: i64,
        high_watermark: i64,
        max_bytes: i32,
    ) -> Result<Vec<chronik_storage::Record>> {
        debug!(
            "fetch_records called - topic: {}, partition: {}, fetch_offset: {}, high_watermark: {}",
            topic, partition, fetch_offset, high_watermark
        );

        // PHASE 1: Try in-memory buffer first (fastest path - extracted helper)
        let (mut records, bytes_fetched, buffer_highest_offset, buffer_min_offset) =
            self.fetch_from_buffer(topic, partition, fetch_offset, high_watermark, max_bytes).await?;

        // If we got records from buffer, check if we need more from WAL/segments
        if !records.is_empty() && bytes_fetched >= max_bytes as usize {
            // We have enough data from buffer alone
            return Ok(records);
        }

        // PHASE 2: Try WAL for missing records or trimmed buffer gaps (extracted helper)
        records = self.try_fetch_from_wal_phase(
            topic,
            partition,
            fetch_offset,
            max_bytes,
            records,
            buffer_highest_offset,
            buffer_min_offset
        ).await?;

        // If we now have enough data after WAL, return
        if !records.is_empty() && bytes_fetched >= max_bytes as usize {
            return Ok(records);
        }

        // PHASE 3: Try downloading raw segments from S3 (Tier 2: warm storage - extracted helper)
        // This is the NEW v1.3.64 flow where sealed WAL segments are uploaded to S3
        // Colder tiers are needed when nothing was found, or when what was found
        // starts above the fetch offset: that gap may be in the segment tier
        // (see `fetch_raw_bytes`). Asking whenever the buffer merely reached
        // higher sent nearly every fallback read to the archives.
        let need_earlier_records = records.iter().map(|r| r.offset).min().is_none_or(|lowest| lowest > fetch_offset);

        if records.is_empty() || need_earlier_records {
            records = self.try_fetch_from_s3_phase(
                topic,
                partition,
                fetch_offset,
                high_watermark,
                max_bytes,
                records
            ).await?;

            // If we now have enough data, return
            if !records.is_empty() {
                return Ok(records);
            }
        }

        // PHASE 4: Try Tantivy archives (cold storage - extracted helper)
        // This provides searchable indexed archives for very old data
        if records.is_empty() || need_earlier_records {
            records = self.try_fetch_from_tantivy_phase(
                topic,
                partition,
                fetch_offset,
                high_watermark,
                max_bytes,
                records
            ).await?;
        }

        Ok(records)
    }

    /// Fetch records from WAL manager
    ///
    /// NEW (v1.3.36): Handle WAL V2 CanonicalRecord format
    /// v1.3.47+: Direct call to WalManager (no RwLock - uses DashMap internally)
    async fn fetch_from_wal(
        &self,
        wal_manager: &Arc<WalManager>,
        topic: &str,
        partition: i32,
        fetch_offset: i64,
        max_bytes: i32,
    ) -> Result<Vec<chronik_storage::Record>> {
        use chronik_storage::canonical_record::CanonicalRecord;

        // CRITICAL FIX (v1.3.55): Match fetch_raw_bytes_from_wal limit increase
        // Old limit (max_bytes / 100) caused consumer timeouts at ~75K messages
        let max_records = std::cmp::max(10000, max_bytes as usize / 10);

        let wal_records = wal_manager.read_from_bounded(topic, partition, fetch_offset, max_records, fetch_byte_budget(max_bytes)).await
            .map_err(|e| Error::Internal(format!("WAL read failed: {}", e)))?;

        // Convert WalRecord to chronik_storage::Record
        let mut records = Vec::new();
        for wal_record in wal_records {
            // Process WAL V2 records (CanonicalRecord batches)
            if let chronik_wal::record::WalRecord::V2 { canonical_data, .. } = wal_record {
                // Deserialize CanonicalRecord from WAL
                match bincode::deserialize::<CanonicalRecord>(&canonical_data) {
                    Ok(canonical_record) => {
                        // Extract individual records from the batch
                        for entry in &canonical_record.records {
                            // Filter by offset range
                            if entry.offset >= fetch_offset {
                                let storage_record = chronik_storage::Record {
                                    offset: entry.offset,
                                    timestamp: entry.timestamp,
                                    key: entry.key.clone(),
                                    value: entry.value.clone().unwrap_or_default(),
                                    headers: entry.headers.iter()
                                        .filter_map(|h| h.value.as_ref().map(|v| (h.key.clone(), v.clone())))
                                        .collect(),
                                };
                                records.push(storage_record);
                            }
                        }
                    }
                    Err(e) => {
                        warn!("Failed to deserialize CanonicalRecord from WAL V2: {}", e);
                    }
                }
            }
            // V1 records are skipped (legacy format from pre-v1.3.36)
        }

        tracing::trace!("WAL returned {} records starting from offset {} for {}-{}",
            records.len(), fetch_offset, topic, partition);

        Ok(records)
    }

    /// Fetch records from S3 raw segments (Tier 2: warm storage)
    ///
    /// NEW (v1.3.64): Download and deserialize bincode Vec<CanonicalRecord> from S3
    /// Path: segments/{topic}/{partition}/{min_offset}-{max_offset}.segment
    async fn fetch_from_s3_raw_segments(
        &self,
        topic: &str,
        partition: i32,
        fetch_offset: i64,
        high_watermark: i64,
        max_bytes: i32,
    ) -> Result<Vec<chronik_storage::Record>> {
        use chronik_storage::canonical_record::CanonicalRecord;

        // Same lookup as the raw path: what this node's store holds, in offset order.
        let matching_segments: Vec<(String, i64, i64)> = self
            .get_segments_for_range(topic, partition, fetch_offset, high_watermark)
            .await?
            .into_iter()
            .map(|s| (s.object_key, s.base_offset, s.last_offset))
            .collect();
        if matching_segments.is_empty() {
            debug!(
                "S3→NO_MATCH: No segments overlap with fetch range {}-{} for {}-{}",
                fetch_offset, high_watermark, topic, partition
            );
            return Ok(vec![]);
        }

        let mut all_records = Vec::new();
        let mut bytes_fetched = 0usize;

        'segments: for (object_key, segment_min, segment_max) in matching_segments {
            if bytes_fetched >= max_bytes as usize && !all_records.is_empty() {
                break;
            }

            debug!(
                "S3→DOWNLOAD: Downloading raw segment {} (offsets {}-{})",
                object_key, segment_min, segment_max
            );

            // Download segment from S3
            let segment_data = match self.object_store.get(&object_key).await {
                Ok(data) => data,
                Err(e) => {
                    warn!(
                        "S3→DOWNLOAD: Failed to download segment {}: {}",
                        object_key, e
                    );
                    continue; // Skip this segment, try others
                }
            };

            debug!(
                "S3→DOWNLOAD: Downloaded {} bytes from {}",
                segment_data.len(), object_key
            );

            // The WalIndexer's raw segment: the WAL's CanonicalRecords,
            // zstd-compressed (or plain, if written before compression)
            let canonical_records: Vec<CanonicalRecord> = match chronik_storage::archive::decode_raw_segment(&segment_data) {
                Ok(records) => records,
                Err(e) => {
                    error!(
                        "S3→DESERIALIZE: Failed to deserialize canonical records from {}: {}",
                        object_key, e
                    );
                    continue;
                }
            };

            debug!(
                "S3→DESERIALIZE: Segment {} contains {} canonical record batches",
                object_key, canonical_records.len()
            );

            // Extract individual records from CanonicalRecords
            for canonical_record in canonical_records {
                for entry in &canonical_record.records {
                    // Filter by offset range
                    if entry.offset >= fetch_offset && entry.offset < high_watermark {
                        let record_size = entry.value.as_ref().map(|v| v.len()).unwrap_or(0)
                            + entry.key.as_ref().map(|k| k.len()).unwrap_or(0)
                            + 24; // Estimated overhead

                        // The whole scan stops: breaking only this loop left
                        // `bytes_fetched` just under the budget, so every remaining
                        // segment to the high watermark was still downloaded.
                        if bytes_fetched + record_size > max_bytes as usize && !all_records.is_empty() {
                            break 'segments;
                        }

                        let storage_record = chronik_storage::Record {
                            offset: entry.offset,
                            timestamp: entry.timestamp,
                            key: entry.key.clone(),
                            value: entry.value.clone().unwrap_or_default(),
                            headers: entry.headers.iter()
                                .filter_map(|h| h.value.as_ref().map(|v| (h.key.clone(), v.clone())))
                                .collect(),
                        };

                        all_records.push(storage_record);
                        bytes_fetched += record_size;
                    }
                }
            }
        }

        debug!(
            "S3→COMPLETE: Fetched {} records ({} bytes) from S3 raw segments for {}-{}",
            all_records.len(), bytes_fetched, topic, partition
        );

        Ok(all_records)
    }

    /// Fetch raw RecordBatch bytes from WAL (compressed_records_wire_bytes)
    ///
    /// CRITICAL FIX (v1.3.59): Return ORIGINAL batches AS-IS by concatenation!
    /// Each batch has its ORIGINAL CRC which is only valid for that specific batch.
    /// Java Kafka clients validate CRC and will reject batches with modified CRCs.
    ///
    /// The Kafka protocol ALLOWS concatenating multiple RecordBatches in a Fetch response.
    /// This is the CORRECT approach - return the original batches exactly as stored.
    ///
    /// v1.3.47+: Direct call to WalManager (no RwLock - uses DashMap internally)
    async fn fetch_raw_bytes_from_wal(
        &self,
        wal_manager: &Arc<WalManager>,
        topic: &str,
        partition: i32,
        fetch_offset: i64,
        high_watermark: i64,
        max_bytes: i32,
    ) -> Result<Option<Vec<u8>>> {
        use chronik_storage::canonical_record::CanonicalRecord;

        let max_records = std::cmp::max(10000, max_bytes as usize / 10);

        let read_start = Instant::now();
        let wal_records = wal_manager.read_from_bounded(topic, partition, fetch_offset, max_records, fetch_byte_budget(max_bytes)).await
            .map_err(|e| Error::Internal(format!("WAL read failed: {}", e)))?;
        let read_took = read_start.elapsed();
        if read_took > Duration::from_millis(1) {
            debug!(
                "RAW→WAL: read_from({}-{}, offset {}) took {:?} for {} records",
                topic, partition, fetch_offset, read_took, wal_records.len()
            );
        }

        if wal_records.is_empty() {
            return Ok(None);
        }

        // CRITICAL FIX (v1.3.59): Concatenate ORIGINAL batch bytes without modification
        // Each batch's CRC is valid for its own bytes - we cannot re-encode or combine
        let mut concatenated_bytes = Vec::new();
        let mut batches_concatenated = 0;

        for wal_record in wal_records {
            if let chronik_wal::record::WalRecord::V2 { canonical_data, .. } = wal_record {
                match bincode::deserialize::<CanonicalRecord>(&canonical_data) {
                    Ok(canonical_record) => {
                        // Verify this batch's records are in the requested range
                        let base_offset = canonical_record.base_offset;
                        let last_offset = canonical_record.last_offset();

                        if last_offset >= fetch_offset && base_offset < high_watermark {
                            // CRITICAL: Call to_kafka_batch() to reconstruct the full RecordBatch
                            // with 61-byte header + compressed records payload
                            // compressed_records_wire_bytes alone is NOT a valid RecordBatch!
                            match canonical_record.to_kafka_batch() {
                                Ok(kafka_batch_bytes) => {
                                    concatenated_bytes.extend_from_slice(&kafka_batch_bytes);
                                    batches_concatenated += 1;

                                    // Per batch, on every fetch. At `warn` this
                                    // wrote 23MB of logs per node in a 25-second
                                    // replication run — the logging itself was a
                                    // measurable share of fetch latency (RP-9).
                                    trace!(
                                        "RAW→WAL: appended reconstructed batch offsets {}-{} ({} bytes)",
                                        base_offset, last_offset, kafka_batch_bytes.len()
                                    );
                                }
                                Err(e) => {
                                    warn!("Failed to convert CanonicalRecord to Kafka batch: {}", e);
                                    continue;
                                }
                            }
                        } else {
                            trace!(
                                "RAW→WAL: skipped batch offsets {}-{} (last_offset >= fetch_offset: {}, base_offset < high_watermark: {})",
                                base_offset, last_offset,
                                last_offset >= fetch_offset,
                                base_offset < high_watermark
                            );
                        }
                    }
                    Err(e) => {
                        warn!("Failed to deserialize CanonicalRecord from WAL V2: {}", e);
                        continue;
                    }
                }
            }
        }

        if concatenated_bytes.is_empty() {
            return Ok(None);
        }

        debug!(
            "RAW→WAL: concatenated {} original batches, total {} bytes for {}-{}",
            batches_concatenated, concatenated_bytes.len(), topic, partition
        );

        Ok(Some(concatenated_bytes))
    }

    /// Fetch raw Kafka batch bytes directly (preserves CRC) - try buffer first, then segments
    async fn fetch_raw_bytes(
        &self,
        topic: &str,
        partition: i32,
        fetch_offset: i64,
        high_watermark: i64,
        max_bytes: i32,
    ) -> Result<Option<Vec<u8>>> {
        tracing::debug!(
            "fetch_raw_bytes - topic: {}, partition: {}, fetch_offset: {}, high_watermark: {}",
            topic, partition, fetch_offset, high_watermark
        );

        // Tiers from hottest to coldest: buffer, WAL, segments, Tantivy. Each
        // answers "records from `fetch_offset` on", so a tier that has lost the
        // offset — the WAL after the indexer reclaims a segment, the buffer
        // after a trim — answers with whatever it holds NEXT. Taking the first
        // non-empty answer served that as if the offsets in between did not
        // exist: asked for 6105, the leader returned 67954, and 61,849 records
        // sitting in the segment tier were skipped by every consumer and
        // follower.
        //
        // So an answer that starts above the offset only bounds the search:
        // colder tiers are asked for the gap, [fetch_offset, its first base),
        // and the earliest answer wins. Only when no tier holds the gap is it
        // a genuine hole, stepped over as Kafka's consumers do.
        let mut upper = high_watermark;
        let mut later: Option<Vec<u8>> = None;
        for tier in 0..4 {
            let answer = match tier {
                0 => self.try_fetch_raw_from_buffer(topic, partition, fetch_offset, upper, max_bytes).await,
                // A partition can have no WAL left at all — reclaimed, or never
                // re-created after its directory went — while its records sit in
                // the segment tier. A WAL error is a miss for this tier, not the
                // end of the read: propagating it failed every fetch of such a
                // partition, so followers could never copy it. Live on
                // Thunderbird: 10 partitions held by the leader's segments only.
                1 => match self.try_fetch_raw_from_wal(topic, partition, fetch_offset, upper, max_bytes).await {
                    Ok(answer) => answer,
                    Err(e) => {
                        tracing::debug!("RAW→WAL: {}-{} unreadable from WAL ({}), trying segments", topic, partition, e);
                        None
                    }
                },
                2 => self.try_fetch_raw_from_segments(topic, partition, fetch_offset, upper, max_bytes).await?,
                _ => self.try_fetch_raw_from_tantivy(topic, partition, fetch_offset, upper, max_bytes).await?,
            };
            let Some(bytes) = answer else { continue };
            match first_batch_base(&bytes) {
                Some(base) if base > fetch_offset => {
                    tracing::debug!(
                        "RAW tier {} for {}-{} starts at {} above fetch offset {} — asking colder tiers for the gap",
                        tier, topic, partition, base, fetch_offset
                    );
                    upper = base;
                    later = Some(bytes);
                }
                _ => return Ok(Some(bytes)),
            }
        }
        Ok(later)
    }

    /// Fetch records from Tantivy archives (cold storage) - for consumption
    /// This is similar to fetch_from_tantivy but returns parsed Record objects instead of raw bytes
    async fn fetch_from_tantivy_for_records(
        &self,
        topic: &str,
        partition: i32,
        fetch_offset: i64,
        high_watermark: i64,
        max_bytes: i32,
    ) -> Result<Vec<chronik_storage::Record>> {
        let segment_index = match &self.segment_index {
            Some(idx) => idx,
            None => return Ok(vec![]),
        };

        // Query segment index for matching Tantivy segments
        let mut tantivy_segments = segment_index.find_segments_by_offset_range(
            topic,
            partition,
            fetch_offset,
            high_watermark,
        ).await?;
        // In offset order, so the byte budget below keeps the records nearest
        // the fetch offset and the rest are never downloaded.
        tantivy_segments.sort_by_key(|s| s.min_offset);
        let budget = fetch_byte_budget(max_bytes);
        let mut gathered = 0usize;

        if tantivy_segments.is_empty() {
            debug!("No Tantivy segments found for {}-{} range {}-{}",
                topic, partition, fetch_offset, high_watermark);
            return Ok(vec![]);
        }

        debug!(
            "Found {} Tantivy segments for {}-{} range {}-{}, downloading and reading",
            tantivy_segments.len(), topic, partition, fetch_offset, high_watermark
        );

        // Collect all entries from all matching segments
        let mut all_entries = Vec::new();

        for segment_metadata in tantivy_segments {
            if gathered >= budget && !all_entries.is_empty() {
                break;
            }
            // Download segment from object store
            let segment_data = match self.object_store.get(&segment_metadata.object_store_path).await {
                Ok(data) => data,
                Err(e) => {
                    warn!(
                        "Failed to download Tantivy segment {}: {}",
                        segment_metadata.segment_id, e
                    );
                    continue; // Skip this segment, try others
                }
            };

            // Deserialize Tantivy segment
            let reader = match TantivySegmentReader::from_archive_bytes(segment_data.clone()) {
                Ok(r) => r,
                Err(e) => {
                    warn!(
                        "Failed to deserialize Tantivy segment {}: {}",
                        segment_metadata.segment_id, e
                    );
                    continue;
                }
            };

            // Query for records in the offset range
            let entries = match reader.query_by_offset_range(fetch_offset, high_watermark) {
                Ok(e) => e,
                Err(e) => {
                    warn!(
                        "Failed to query Tantivy segment {}: {}",
                        segment_metadata.segment_id, e
                    );
                    continue;
                }
            };

            debug!(
                "Read {} entries from Tantivy segment {}",
                entries.len(), segment_metadata.segment_id
            );

            gathered += entries.iter().map(|e| e.value.as_ref().map_or(0, |v| v.len()) + e.key.as_ref().map_or(0, |k| k.len())).sum::<usize>();
            all_entries.extend(entries);
        }

        if all_entries.is_empty() {
            debug!("No entries found in Tantivy segments for {}-{} range {}-{}",
                topic, partition, fetch_offset, high_watermark);
            return Ok(vec![]);
        }

        // Sort entries by offset, each offset once: two archives must never
        // cover one offset (merges swap atomically), but a consumer must not
        // receive a record twice if they ever did.
        all_entries.sort_by_key(|e| e.offset);
        all_entries.dedup_by_key(|e| e.offset);

        // Convert entries to chronik_storage::Record
        let records: Vec<chronik_storage::Record> = all_entries.into_iter()
            .map(|entry| chronik_storage::Record {
                offset: entry.offset,
                timestamp: entry.timestamp,
                key: entry.key,
                value: entry.value.unwrap_or_default(),
                headers: entry.headers.iter()
                    .filter_map(|h| h.value.as_ref().map(|v| (h.key.clone(), v.clone())))
                    .collect(),
            })
            .collect();

        debug!(
            "Returning {} records from Tantivy segments for {}-{} range {}-{}",
            records.len(), topic, partition, fetch_offset, high_watermark
        );

        Ok(records)
    }

    /// Fetch data from Tantivy segments (warm storage) - returns raw bytes
    async fn fetch_from_tantivy(
        &self,
        segment_index: &Arc<SegmentIndex>,
        topic: &str,
        partition: i32,
        fetch_offset: i64,
        high_watermark: i64,
        _max_bytes: i32,
    ) -> Result<Option<Vec<u8>>> {
        use chronik_storage::canonical_record::{CanonicalRecord, CompressionType, TimestampType};

        // Query segment index for matching Tantivy segments
        let tantivy_segments = segment_index.find_segments_by_offset_range(
            topic,
            partition,
            fetch_offset,
            high_watermark,
        ).await?;

        if tantivy_segments.is_empty() {
            tracing::debug!("No Tantivy segments found for {}-{} range {}-{}",
                topic, partition, fetch_offset, high_watermark);
            return Ok(None);
        }

        tracing::debug!(
            "Found {} Tantivy segments for {}-{} range {}-{}, downloading and reading",
            tantivy_segments.len(), topic, partition, fetch_offset, high_watermark
        );

        // Collect all entries from all matching segments
        let mut all_entries = Vec::new();

        for segment_metadata in tantivy_segments {
            // Download segment from object store
            let segment_data = match self.object_store.get(&segment_metadata.object_store_path).await {
                Ok(data) => data,
                Err(e) => {
                    tracing::warn!(
                        "Failed to download Tantivy segment {}: {}",
                        segment_metadata.segment_id, e
                    );
                    continue; // Skip this segment, try others
                }
            };

            // Deserialize Tantivy segment
            let reader = match TantivySegmentReader::from_archive_bytes(segment_data.clone()) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(
                        "Failed to deserialize Tantivy segment {}: {}",
                        segment_metadata.segment_id, e
                    );
                    continue;
                }
            };

            // Query for records in the offset range
            let entries = match reader.query_by_offset_range(fetch_offset, high_watermark) {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(
                        "Failed to query Tantivy segment {}: {}",
                        segment_metadata.segment_id, e
                    );
                    continue;
                }
            };

            tracing::debug!(
                "Read {} entries from Tantivy segment {}",
                entries.len(), segment_metadata.segment_id
            );

            all_entries.extend(entries);
        }

        if all_entries.is_empty() {
            tracing::debug!("No entries found in Tantivy segments for {}-{} range {}-{}",
                topic, partition, fetch_offset, high_watermark);
            return Ok(None);
        }

        // Sort entries by offset (should already be sorted, but ensure correctness)
        all_entries.sort_by_key(|e| e.offset);
        all_entries.dedup_by_key(|e| e.offset);

        // Reconstruct CanonicalRecord from entries
        // Note: We use default compression (None) since we're serving the data uncompressed
        let canonical_record = CanonicalRecord::from_entries(
            all_entries,
            CompressionType::None,
            TimestampType::CreateTime,
        )?;

        // Convert to Kafka wire format
        let kafka_batch = canonical_record.to_kafka_batch()?;

        tracing::debug!(
            "Returning {} bytes from Tantivy segments for {}-{} range {}-{}",
            kafka_batch.len(), topic, partition, fetch_offset, high_watermark
        );

        Ok(Some(kafka_batch.to_vec()))
    }

    /// Get segments that contain data in the given offset range
    async fn get_segments_for_range(
        &self,
        topic: &str,
        partition: i32,
        start_offset: i64,
        end_offset: i64,
    ) -> Result<Vec<SegmentInfo>> {
        // What THIS node's object store holds. The indexer writes raw segments
        // as `segments/<topic>/<partition>/<min>-<max>.segment`, so the key is
        // the index.
        //
        // This used to ask the metadata catalog, which the indexer writes only
        // on the metadata leader ("followers don't own __chronik_metadata"):
        // a partition led by any other node had its archived segments
        // registered nowhere, and the tier answered "no segments" for records
        // sitting in its own store. On a local cluster, 1,599 archived segments
        // were unreachable through Fetch. The catalog is still consulted when
        // the store lists nothing, for stores that cannot list.
        let mut infos: Vec<SegmentInfo> =
            match chronik_storage::archive::list_archived_segments(self.object_store.as_ref(), topic, partition).await {
                Ok(segments) => segments
                    .into_iter()
                    .map(|s| SegmentInfo {
                        segment_id: format!("{}-{}", s.base_offset, s.last_offset),
                        base_offset: s.base_offset,
                        last_offset: s.last_offset,
                        object_key: s.object_key,
                    })
                    .collect(),
                Err(e) => {
                    tracing::debug!("SEGMENT→LIST: cannot list the archive of {}-{}: {}", topic, partition, e);
                    Vec::new()
                }
            };

        if infos.is_empty() {
            infos = self
                .metadata_store
                .list_segments(topic, Some(partition as u32))
                .await?
                .into_iter()
                .map(|s| SegmentInfo {
                    segment_id: s.segment_id,
                    base_offset: s.start_offset,
                    last_offset: s.end_offset,
                    object_key: s.path,
                })
                .collect();
        }

        infos.retain(|s| s.last_offset >= start_offset && s.base_offset < end_offset);
        infos.sort_by_key(|s| s.base_offset);
        tracing::debug!(
            "get_segments_for_range {}-{} [{}, {}): {} segments",
            topic, partition, start_offset, end_offset, infos.len()
        );
        Ok(infos)
    }
    
    /// The record batches this node holds for `topic`/`partition` from
    /// `from_offset`, up to about `max_bytes`, bounded by the log start and
    /// the readable end — whether or not this node leads the partition.
    ///
    /// For maintenance only (re-archiving records the indexer missed), never
    /// for clients: clients must be refused by a non-leader (RP-7), but a
    /// follower's local copy is exactly what a node re-archives from. Empty
    /// when the offset is outside the log (a deleted topic, an emptied or
    /// recreated partition, records past retention).
    pub async fn read_local_range(
        &self,
        topic: &str,
        partition: i32,
        from_offset: i64,
        max_bytes: i32,
    ) -> Result<Vec<u8>> {
        let high_watermark = self.readable_end_offset(topic, partition, from_offset, -1).await?;
        let log_start_offset = self.get_log_start_offset_for_fetch(topic, partition).await;
        if from_offset < log_start_offset.max(0) || from_offset >= high_watermark {
            return Ok(Vec::new());
        }
        let served = self
            .fetch_data_available_path(
                topic,
                partition,
                from_offset,
                high_watermark,
                log_start_offset,
                max_bytes,
                0,
                Instant::now(),
            )
            .await?;
        if served.error_code != 0 {
            return Ok(Vec::new());
        }
        Ok(served.records)
    }

    /// ListOffsets by timestamp: the earliest offset whose record timestamp is
    /// at or after `target_ms`, with that record's timestamp, or `None` when no
    /// record is that recent (Kafka answers offset -1).
    ///
    /// This used to be a placeholder answering offset 500 for every timestamp,
    /// so `offsetsForTimes`, `--reset-offsets --to-datetime`, and every
    /// "start from time T" consumer silently began at an arbitrary record —
    /// past the end of a small partition, deep inside a large one.
    ///
    /// Binary search over offsets, probing the first record at each midpoint,
    /// finds the boundary in O(log n) reads when timestamps rise with offsets —
    /// the normal case, and the case in which this equals Kafka's answer
    /// exactly. Producer-assigned CreateTime need not be monotonic, so a final
    /// forward scan starts `TIMESTAMP_SCAN_BACK` records before the boundary
    /// and returns the earliest qualifying record it meets. Kafka's own answer
    /// is approximate in the same way: its time index records the running
    /// maximum, not every record.
    pub async fn offset_for_timestamp(
        &self,
        topic: &str,
        partition: i32,
        target_ms: i64,
        log_start: i64,
        high_watermark: i64,
    ) -> Result<Option<(i64, i64)>> {
        const PROBE_BYTES: i32 = 64 * 1024;
        const SCAN_BYTES: i32 = 1024 * 1024;
        const TIMESTAMP_SCAN_BACK: i64 = 1000;

        if log_start >= high_watermark {
            return Ok(None);
        }

        let (mut lo, mut hi) = (log_start, high_watermark);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let first = self
                .fetch_records(topic, partition, mid, high_watermark, PROBE_BYTES)
                .await?
                .into_iter()
                .filter(|r| r.offset >= mid)
                .min_by_key(|r| r.offset);
            match first {
                Some(r) if r.timestamp >= target_ms => hi = mid,
                // Step past holes too: the first record at or after `mid` is
                // the one that answered, wherever it sits.
                Some(r) => lo = (r.offset + 1).max(mid + 1),
                None => hi = mid,
            }
        }

        let mut cursor = lo.saturating_sub(TIMESTAMP_SCAN_BACK).max(log_start);
        while cursor < high_watermark {
            let mut records = self
                .fetch_records(topic, partition, cursor, high_watermark, SCAN_BYTES)
                .await?;
            records.retain(|r| r.offset >= cursor);
            if records.is_empty() {
                break;
            }
            records.sort_by_key(|r| r.offset);
            if let Some(hit) = records.iter().find(|r| r.timestamp >= target_ms) {
                return Ok(Some((hit.offset, hit.timestamp)));
            }
            cursor = records.last().map(|r| r.offset + 1).unwrap_or(high_watermark);
        }
        Ok(None)
    }

    /// Update in-memory buffer with raw Kafka batch bytes (v1.3.32 FIX)
    /// CRITICAL: Stores original wire-format bytes to preserve CRC
    #[allow(clippy::too_many_arguments)] // internal call that threads its caller's state through
    pub async fn update_buffer_with_raw_batch(
        &self,
        topic: &str,
        partition: i32,
        raw_bytes: &[u8],
        base_offset: i64,
        last_offset: i64,
        record_count: i32,
        high_watermark: i64,
    ) -> Result<()> {
        if raw_bytes.is_empty() {
            return Ok(());
        }

        tracing::debug!(
            "BUFFER→RAW_UPDATE: Storing {} bytes for {}-{}, offset_range=[{}-{}], count={}, high_watermark={}",
            raw_bytes.len(), topic, partition, base_offset, last_offset, record_count, high_watermark
        );

        let mut state = self.state.write().await;
        let key = (topic.to_string(), partition);

        let buffer = state.buffers.entry(key.clone()).or_insert(PartitionBuffer {
            raw_batches: Vec::new(),
            batch_metadata: Vec::new(),
            base_offset,
            high_watermark,
            min_offset_in_buffer: base_offset,  // v1.3.48: Track actual minimum offset in buffer
        });

        // Check for duplicate batches (same base_offset)
        if !buffer.batch_metadata.iter().any(|m| m.base_offset == base_offset) {
            // Store raw bytes
            buffer.raw_batches.push(bytes::Bytes::copy_from_slice(raw_bytes));

            // Store metadata
            buffer.batch_metadata.push(BatchMetadata {
                base_offset,
                last_offset,
            });

            tracing::debug!(
                "BUFFER→RAW_STORED: Added batch to {}-{}, now has {} batches",
                topic, partition, buffer.raw_batches.len()
            );
        } else {
            tracing::debug!(
                "BUFFER→RAW_SKIP: Skipping duplicate batch at offset {} for {}-{}",
                base_offset, topic, partition
            );
        }

        // Update high watermark
        buffer.high_watermark = high_watermark;

        // Update base_offset if this is the first batch or earlier
        if buffer.batch_metadata.is_empty() || base_offset < buffer.base_offset {
            buffer.base_offset = base_offset;
        }

        // Trim old batches if buffer too large (keep last 100 batches)
        // v1.3.48: Track min_offset_in_buffer to detect gaps and trigger WAL fallback
        if buffer.raw_batches.len() > 100 {
            let trim_count = buffer.raw_batches.len() - 100;

            // Track what we're trimming for logging
            let first_trimmed = buffer.batch_metadata[0].base_offset;
            let last_trimmed = buffer.batch_metadata[trim_count - 1].last_offset;

            buffer.raw_batches.drain(0..trim_count);
            buffer.batch_metadata.drain(0..trim_count);

            if let Some(first_meta) = buffer.batch_metadata.first() {
                buffer.base_offset = first_meta.base_offset;
                buffer.min_offset_in_buffer = first_meta.base_offset;  // v1.3.48: Update min after trim
            }

            debug!(
                "BUFFER→TRIM: Trimmed {} old batches from {}-{} (offsets {}-{}), {} batches remain, min_offset_in_buffer={}, data still in WAL",
                trim_count, topic, partition, first_trimmed, last_trimmed,
                buffer.raw_batches.len(), buffer.min_offset_in_buffer
            );
        }

        Ok(())
    }

    /// Decode records from raw Kafka RecordBatch bytes
    /// CRITICAL v1.3.32: Decode original bytes instead of re-encoding
    fn decode_records_from_raw_batch(
        &self,
        raw_bytes: &[u8],
        min_offset: i64,
        max_offset: i64,
    ) -> Result<Vec<chronik_storage::Record>> {
        use bytes::Buf;

        let mut cursor = raw_bytes;
        let mut records = Vec::new();

        // Skip RecordBatch header to get to records
        // RecordBatch format v2:
        // - base_offset (8 bytes)
        // - batch_length (4 bytes)
        // - partition_leader_epoch (4 bytes)
        // - magic (1 byte)
        // - crc (4 bytes)
        // - attributes (2 bytes)
        // - last_offset_delta (4 bytes)
        // - base_timestamp (8 bytes)
        // - max_timestamp (8 bytes)
        // - producer_id (8 bytes)
        // - producer_epoch (2 bytes)
        // - base_sequence (4 bytes)
        // - record_count (4 bytes)
        // Total header: 61 bytes

        if raw_bytes.len() < 61 {
            return Err(Error::Protocol("RecordBatch too small".into()));
        }

        let base_offset = cursor.get_i64();
        cursor.advance(4); // batch_length
        cursor.advance(4); // partition_leader_epoch
        cursor.advance(1); // magic
        cursor.advance(4); // crc
        cursor.advance(2); // attributes
        cursor.advance(4); // last_offset_delta
        let base_timestamp = cursor.get_i64();
        cursor.advance(8); // max_timestamp
        cursor.advance(8); // producer_id
        cursor.advance(2); // producer_epoch
        cursor.advance(4); // base_sequence
        let record_count = cursor.get_i32();

        // Parse individual records
        for _ in 0..record_count {
            if cursor.remaining() == 0 {
                break;
            }

            // Read record length (varint)
            let length = self.read_varint(&mut cursor)?;
            if cursor.remaining() < length as usize {
                break;
            }

            // Read attributes (1 byte)
            cursor.advance(1);

            // Read timestamp delta (varint)
            let timestamp_delta = self.read_varlong(&mut cursor)?;

            // Read offset delta (varint)
            let offset_delta = self.read_varint(&mut cursor)?;
            let record_offset = base_offset + offset_delta as i64;

            // Check if record is in requested range
            if record_offset < min_offset || record_offset >= max_offset {
                // Skip this record
                let key_len = self.read_varint(&mut cursor)?;
                if key_len >= 0 {
                    cursor.advance(key_len as usize);
                }
                let value_len = self.read_varint(&mut cursor)?;
                if value_len >= 0 {
                    cursor.advance(value_len as usize);
                }
                let header_count = self.read_varint(&mut cursor)?;
                for _ in 0..header_count {
                    let key_len = self.read_varint(&mut cursor)?;
                    cursor.advance(key_len as usize);
                    let val_len = self.read_varint(&mut cursor)?;
                    cursor.advance(val_len as usize);
                }
                continue;
            }

            // Read key (varint length + bytes)
            let key_len = self.read_varint(&mut cursor)?;
            let key = if key_len >= 0 {
                let mut key_bytes = vec![0u8; key_len as usize];
                cursor.copy_to_slice(&mut key_bytes);
                Some(key_bytes)
            } else {
                None
            };

            // Read value (varint length + bytes)
            let value_len = self.read_varint(&mut cursor)?;
            let value = if value_len >= 0 {
                let mut value_bytes = vec![0u8; value_len as usize];
                cursor.copy_to_slice(&mut value_bytes);
                value_bytes
            } else {
                vec![]
            };

            // Read headers (varint count, then key-value pairs)
            let header_count = self.read_varint(&mut cursor)?;
            let mut headers = std::collections::HashMap::new();
            for _ in 0..header_count {
                let key_len = self.read_varint(&mut cursor)?;
                let mut key_bytes = vec![0u8; key_len as usize];
                cursor.copy_to_slice(&mut key_bytes);
                let key_str = String::from_utf8_lossy(&key_bytes).to_string();

                let val_len = self.read_varint(&mut cursor)?;
                let mut val_bytes = vec![0u8; val_len as usize];
                cursor.copy_to_slice(&mut val_bytes);

                headers.insert(key_str, val_bytes);
            }

            records.push(chronik_storage::Record {
                offset: record_offset,
                timestamp: base_timestamp + timestamp_delta,
                key,
                value,
                headers,
            });
        }

        Ok(records)
    }

    /// Read varint from byte slice (zigzag encoded)
    fn read_varint(&self, cursor: &mut &[u8]) -> Result<i32> {
        use bytes::Buf;
        let mut result: i32 = 0;
        let mut shift = 0;
        loop {
            if cursor.remaining() == 0 {
                return Err(Error::Protocol("Unexpected end of varint".into()));
            }
            let byte = cursor.get_u8();
            result |= ((byte & 0x7F) as i32) << shift;
            if byte & 0x80 == 0 {
                break;
            }
            shift += 7;
        }
        // Zigzag decode
        Ok((result >> 1) ^ -(result & 1))
    }

    /// Read varlong from byte slice (zigzag encoded)
    fn read_varlong(&self, cursor: &mut &[u8]) -> Result<i64> {
        use bytes::Buf;
        let mut result: i64 = 0;
        let mut shift = 0;
        loop {
            if cursor.remaining() == 0 {
                return Err(Error::Protocol("Unexpected end of varlong".into()));
            }
            let byte = cursor.get_u8();
            result |= ((byte & 0x7F) as i64) << shift;
            if byte & 0x80 == 0 {
                break;
            }
            shift += 7;
        }
        // Zigzag decode
        Ok((result >> 1) ^ -(result & 1))
    }

    /// Encode records in Kafka RecordBatch format
    fn encode_kafka_records(
        &self,
        records: &[chronik_storage::Record],
        _leader_epoch: i32,
    ) -> Result<Vec<u8>> {
        use bytes::Bytes;
        
        // For empty record sets, return an empty vector
        // The protocol handler will write this as a 0-length bytes field
        if records.is_empty() {
            return Ok(vec![]);
        }
        
        // Group records into batches (simple approach: one batch for all)
        let base_offset = records[0].offset;
        let base_timestamp = records[0].timestamp;
        
        let mut batch = KafkaRecordBatch::new(
            base_offset,
            base_timestamp,
            -1, // No producer ID for fetched records
            -1, // No producer epoch
            -1, // No base sequence
            CompressionType::None, // No compression for now
            false, // Not transactional
        );
        
        // Add all records to the batch
        for record in records {
            let headers: Vec<KafkaRecordHeader> = record.headers.iter()
                .map(|(k, v)| KafkaRecordHeader {
                    key: k.clone(),
                    value: Some(Bytes::from(v.clone())),
                })
                .collect();
            
            batch.add_record(
                record.key.as_ref().map(|k| Bytes::from(k.clone())),
                Some(Bytes::from(record.value.clone())),
                headers,
                record.timestamp,
            );
        }
        
        // Encode the batch
        let encoded = batch.encode()?;
        Ok(encoded.to_vec())
    }
}

/// Recompute the CRC-32C of every v2 RecordBatch in a concatenated fetch payload,
/// over the Kafka-spec range (attributes .. end of batch), and write it into each
/// batch's CRC field. Makes every outgoing batch self-consistent regardless of which
/// serving path produced it. The batch content is left byte-identical; only the CRC
/// field (bytes 17-20 of each batch, little-endian) is (re)written. Non-v2 or partial
/// trailing bytes are left untouched.
/// The byte budget a partition fetch may read from the WAL.
///
/// Kafka's `partition_max_bytes`: the first batch is returned whatever its
/// size (the WAL read guarantees that), so a non-positive value still makes
/// progress one batch at a time.
fn fetch_byte_budget(max_bytes: i32) -> usize {
    max_bytes.max(1) as usize
}

/// Base offset of the first record batch in raw fetch bytes (big-endian i64
/// at the start of a v2 batch), if there is a whole header.
fn first_batch_base(bytes: &[u8]) -> Option<i64> {
    let head: [u8; 8] = bytes.get(..8)?.try_into().ok()?;
    Some(i64::from_be_bytes(head))
}

/// Add `incoming` to `records`, skipping offsets already present, and leave
/// the result ordered by offset.
///
/// Each storage tier's records used to be merged with a linear search of the
/// existing ones per record. The archive tiers return everything from the
/// fetch offset to the high watermark, so on a 1.8M-record partition that was
/// ~10^11 comparisons — in a loop with no await, which the fetch's 30-second
/// timeout cannot interrupt. Measured live on signal-stream v2.16.8: one tokio
/// worker at 100% in this loop for as long as followers kept fetching, every
/// follower fetch of the partition timing out, consumers reading nothing.
fn merge_by_offset(records: &mut Vec<chronik_storage::Record>, incoming: Vec<chronik_storage::Record>) {
    let mut held: std::collections::HashSet<i64> = records.iter().map(|r| r.offset).collect();
    records.extend(incoming.into_iter().filter(|r| held.insert(r.offset)));
    records.sort_by_key(|r| r.offset);
}

/// Cut offset-ordered `records` to roughly `max_bytes` of payload — the first
/// record always, so a fetch makes progress whatever the budget.
///
/// The archive tiers read to the high watermark; without this the whole
/// remainder of the partition was encoded into one response.
fn cap_to_byte_budget(records: &mut Vec<chronik_storage::Record>, max_bytes: i32) {
    let budget = fetch_byte_budget(max_bytes);
    let mut used = 0usize;
    let mut keep = records.len();
    for (i, r) in records.iter().enumerate() {
        let size = r.value.len()
            + r.key.as_ref().map_or(0, |k| k.len())
            + r.headers.iter().map(|(k, v)| k.len() + v.len()).sum::<usize>()
            + RECORD_OVERHEAD_BYTES;
        if i > 0 && used + size > budget {
            keep = i;
            break;
        }
        used += size;
    }
    records.truncate(keep);
}

/// Per-record framing in a v2 batch (length, attributes, deltas, counts) —
/// an estimate; the budget is approximate, as Kafka's own is per batch.
const RECORD_OVERHEAD_BYTES: usize = 16;

/// Does this response carry anything at all?
///
/// The question a fetch's wait is really asking: Kafka holds a request open
/// until *some* partition has data, not until a particular one does.
fn any_records(topics: &[FetchResponseTopic]) -> bool {
    topics
        .iter()
        .any(|t| t.partitions.iter().any(|p| !p.records.is_empty()))
}

/// Length of the leading run of record batches whose base_offset is strictly below
/// `offset`. Batches are offset-ordered and self-delimiting (base_offset i64 at [0..8],
/// batch_length i32 at [8..12] counting everything after those 12 bytes). Used to
/// truncate a read_committed fetch response at the Last Stable Offset — the first batch
/// with base_offset >= offset marks the cut point. Returns the full length if no batch
/// reaches `offset`.
fn keep_len_below_offset(bytes: &[u8], offset: i64) -> usize {
    let mut pos = 0usize;
    let len = bytes.len();
    while pos + 12 <= len {
        let base_offset = i64::from_be_bytes([
            bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3],
            bytes[pos + 4], bytes[pos + 5], bytes[pos + 6], bytes[pos + 7],
        ]);
        let batch_length = i32::from_be_bytes([
            bytes[pos + 8], bytes[pos + 9], bytes[pos + 10], bytes[pos + 11],
        ]) as usize;
        let batch_end = pos + 12 + batch_length;
        if batch_length < 9 || batch_end > len {
            break; // malformed or partial trailing batch — keep what precedes it
        }
        if base_offset >= offset {
            return pos;
        }
        pos = batch_end;
    }
    len
}

fn sanitize_batch_crcs(bytes: &mut [u8]) {
    let mut pos = 0usize;
    let len = bytes.len();
    while pos + 21 <= len {
        // batch_length counts everything after the base_offset(8) + batch_length(4) fields.
        let batch_length = i32::from_be_bytes([
            bytes[pos + 8], bytes[pos + 9], bytes[pos + 10], bytes[pos + 11],
        ]) as usize;
        let batch_end = pos + 12 + batch_length;
        if batch_length < 9 || batch_end > len {
            break; // malformed or partial trailing batch — leave as-is
        }
        let magic = bytes[pos + 16] as i8;
        // Only re-CRC CONTROL batches (attributes bit 5). These are the batches Chronik
        // itself produces (transaction COMMIT/ABORT markers) and are the only ones that
        // can reach the wire with an inconsistent CRC. Producer data batches are served
        // with their own valid CRC and must NOT be touched (recomputing them risks
        // mismatches on compressed/multi-record batches).
        let is_control = (u16::from_be_bytes([bytes[pos + 21], bytes[pos + 22]]) & 0x20) != 0;
        if magic == 2 && is_control {
            // CRC covers attributes (pos+21) to end of batch.
            let crc = crc32c::crc32c(&bytes[pos + 21..batch_end]);
            bytes[pos + 17..pos + 21].copy_from_slice(&crc.to_be_bytes());
        }
        pos = batch_end;
    }
}

// TODO: Create fetch_handler_test.rs when tests are needed
// #[cfg(test)]
// #[path = "fetch_handler_test.rs"]
// mod fetch_handler_test;

#[cfg(test)]
mod replica_fetch_tests {
    use crate::isr_tracker::{IsrTracker, SyncState};
    use std::sync::Arc;

    /// RP-2.1: `replica_id` distinguishes a replicating follower from a consumer.
    ///
    /// Kafka clients send -1; only a broker sets it to its own node id. The field
    /// has always been decoded and discarded, so this pins the semantics the
    /// fetch path now depends on: a follower fetch records progress, a consumer
    /// fetch records nothing.
    ///
    /// This is the property that makes pull cheaper than push. The push model
    /// needed an ACK channel for progress, heartbeat replies for liveness, and a
    /// reconnect probe for restarts — three mechanisms, each of which had a bug
    /// only a live cluster exposed. A fetch offset is all three at once.
    #[test]
    fn follower_fetch_records_progress_consumer_fetch_does_not() {
        let tracker = Arc::new(IsrTracker::new(1000, 10_000));

        // Consumer fetch: replica_id < 0 → nothing recorded.
        let consumer_replica_id: i32 = -1;
        if consumer_replica_id >= 0 {
            tracker.update_follower_offset(consumer_replica_id as u64, "orders", 0, 500);
        }
        assert_eq!(
            tracker.sync_state(2, "orders", 0, 500),
            SyncState::Unknown,
            "a consumer fetch must not be mistaken for replication progress"
        );

        // Follower fetch from node 2 at offset 500: it cannot ask for 500 without
        // having durably written everything below it, so 500 is its LEO.
        let follower_replica_id: i32 = 2;
        if follower_replica_id >= 0 {
            tracker.update_follower_offset(follower_replica_id as u64, "orders", 0, 500);
            tracker.record_node_alive(follower_replica_id as u64);
        }

        assert_eq!(tracker.sync_state(2, "orders", 0, 500), SyncState::InSync);
        assert_eq!(tracker.get_isr("orders", 0, 500, &[1, 2]), vec![2]);
    }

    /// A follower that keeps fetching stays in ISR without any separate heartbeat.
    ///
    /// Under push this required a dedicated liveness ACK; the first attempt used
    /// connection state and silently did nothing, because a TCP write succeeds
    /// into the local send buffer long after the peer is gone.
    #[test]
    fn fetching_is_its_own_liveness_proof() {
        let tracker = Arc::new(IsrTracker::new(1000, 10_000));

        tracker.update_follower_offset(2, "orders", 0, 100);
        tracker.record_node_alive(2);
        assert_eq!(tracker.sync_state(2, "orders", 0, 100), SyncState::InSync);

        // Next fetch arrives at a higher offset — progress and liveness together.
        tracker.update_follower_offset(2, "orders", 0, 250);
        tracker.record_node_alive(2);
        assert_eq!(tracker.sync_state(2, "orders", 0, 250), SyncState::InSync);
    }
}

#[cfg(test)]
mod fetch_wait_budget_tests {
    use super::*;
    use chronik_common::metadata::memory::InMemoryMetadataStore;
    use chronik_common::metadata::traits::TopicConfig;
    use chronik_protocol::{FetchRequest, FetchRequestPartition, FetchRequestTopic};
    use tempfile::TempDir;

    async fn handler_with_topic(partitions: u32) -> (FetchHandler, Arc<InMemoryMetadataStore>, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let metadata_store = Arc::new(InMemoryMetadataStore::new());

        let object_store_config = chronik_storage::object_store::ObjectStoreConfig {
            backend: chronik_storage::object_store::StorageBackend::Local {
                path: temp_dir.path().join("segments").to_str().unwrap().to_string(),
            },
            ..Default::default()
        };
        let object_store: Arc<dyn ObjectStoreTrait> = Arc::from(
            chronik_storage::object_store::ObjectStoreFactory::create(object_store_config)
                .await
                .unwrap(),
        );
        let topic_config = TopicConfig {
            partition_count: partitions,
            ..Default::default()
        };
        metadata_store
            .create_topic("orders", topic_config)
            .await
            .unwrap();

        let handler = FetchHandler::new(metadata_store.clone(), object_store);
        (handler, metadata_store, temp_dir)
    }

    fn idle_fetch(partitions: i32, max_wait_ms: i32) -> FetchRequest {
        FetchRequest {
            replica_id: -1,
            max_wait_ms,
            min_bytes: 1,
            max_bytes: 10 * 1024 * 1024,
            isolation_level: 0,
            session_id: 0,
            session_epoch: -1,
            topics: vec![FetchRequestTopic {
                name: "orders".to_string(),
                partitions: (0..partitions)
                    .map(|partition| FetchRequestPartition {
                        partition,
                        current_leader_epoch: -1,
                        fetch_offset: 0,
                        log_start_offset: 0,
                        partition_max_bytes: 1024 * 1024,
                    })
                    .collect(),
            }],
        }
    }

    /// `max_wait_ms` bounds the request, not each partition in it.
    ///
    /// Partitions are served serially, so before the shared deadline every idle
    /// partition spent the full budget and an N-partition fetch took N ×
    /// max_wait_ms to return — with Kafka's default 500ms, an 8-partition
    /// consumer waited 4 seconds for an empty response and would usually hit
    /// its own request timeout first.
    ///
    /// This is also what makes follower-pull viable: a follower batches every
    /// partition it replicates from one leader into a single request.
    #[tokio::test]
    async fn an_idle_multi_partition_fetch_stays_within_the_request_budget() {
        let (handler, _store, _dir) = handler_with_topic(8).await;

        let max_wait_ms = 300;
        let started = Instant::now();
        let response = handler
            .handle_fetch(idle_fetch(8, max_wait_ms), 1)
            .await
            .expect("an idle fetch still returns a response");
        let elapsed = started.elapsed();

        assert_eq!(response.topics[0].partitions.len(), 8);

        // Two-sided on purpose. The upper bound is the bug: serial per-partition
        // waiting would have taken 8 × 300ms = 2.4s. The lower bound stops the
        // test passing for the wrong reason — if the wait were dropped entirely
        // the response would be instant, which would also satisfy the ceiling
        // while turning every idle follower into a busy loop.
        assert!(
            elapsed >= Duration::from_millis(max_wait_ms as u64 / 2),
            "returned in {:?}: the long poll is not holding at all",
            elapsed
        );
        assert!(
            elapsed < Duration::from_millis(max_wait_ms as u64 * 3),
            "8 idle partitions took {:?}, which means the wait budget is still being spent per partition",
            elapsed
        );
    }

    /// The single-partition case must keep waiting the full budget — that long
    /// poll is what stops an idle consumer from busy-looping, and it is what
    /// keeps a caught-up follower cheap.
    #[tokio::test]
    async fn a_single_idle_partition_still_long_polls() {
        let (handler, _store, _dir) = handler_with_topic(1).await;

        let max_wait_ms = 200;
        let started = Instant::now();
        handler
            .handle_fetch(idle_fetch(1, max_wait_ms), 1)
            .await
            .unwrap();
        let elapsed = started.elapsed();

        assert!(
            elapsed >= Duration::from_millis(max_wait_ms as u64 / 2),
            "an idle single-partition fetch returned in {:?}; the long poll is not holding",
            elapsed
        );
    }

    /// `max_wait_ms = 0` means "answer now". It must not wait at all, whatever
    /// the deadline arithmetic does.
    #[tokio::test]
    async fn a_zero_wait_fetch_returns_immediately() {
        let (handler, _store, _dir) = handler_with_topic(4).await;

        let started = Instant::now();
        handler.handle_fetch(idle_fetch(4, 0), 1).await.unwrap();

        assert!(
            started.elapsed() < Duration::from_millis(150),
            "max_wait_ms=0 must not block"
        );
    }
}
#[cfg(test)]
mod tier_merge_tests {
    use super::{cap_to_byte_budget, merge_by_offset, RECORD_OVERHEAD_BYTES};
    use chronik_storage::Record;
    use std::time::{Duration, Instant};

    fn rec(offset: i64, value_len: usize) -> Record {
        Record {
            offset,
            timestamp: 0,
            key: None,
            value: vec![0u8; value_len],
            headers: Default::default(),
        }
    }

    fn offsets(records: &[Record]) -> Vec<i64> {
        records.iter().map(|r| r.offset).collect()
    }

    #[test]
    fn merging_keeps_one_copy_of_each_offset_in_order() {
        let mut held = vec![rec(5, 1), rec(1, 1)];
        merge_by_offset(&mut held, vec![rec(3, 1), rec(5, 1), rec(1, 1), rec(9, 1), rec(3, 1)]);
        assert_eq!(offsets(&held), vec![1, 3, 5, 9]);
    }

    /// The archive tiers hand over everything to the high watermark. Merging
    /// 300,000 records into 300,000 used to be ~10^11 comparisons with no
    /// await; it must now be a fraction of a second.
    #[test]
    fn merging_a_whole_partition_is_not_quadratic() {
        let mut held: Vec<Record> = (0..300_000).map(|o| rec(o, 0)).collect();
        let incoming: Vec<Record> = (150_000..450_000).map(|o| rec(o, 0)).collect();
        let started = Instant::now();
        merge_by_offset(&mut held, incoming);
        assert_eq!(held.len(), 450_000);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "merge took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn the_byte_budget_cuts_a_merged_result() {
        let mut records: Vec<Record> = (0..100).map(|o| rec(o, 100)).collect();
        cap_to_byte_budget(&mut records, (3 * (100 + RECORD_OVERHEAD_BYTES)) as i32);
        assert_eq!(offsets(&records), vec![0, 1, 2]);
    }

    #[test]
    fn a_record_larger_than_the_budget_is_still_returned() {
        let mut records = vec![rec(0, 10_000), rec(1, 10)];
        cap_to_byte_budget(&mut records, 1);
        assert_eq!(offsets(&records), vec![0]);
    }
}

/// The v2 record batches in a fetch's record bytes (`baseOffset` i64,
/// `batchLength` i32, then `batchLength` bytes; magic at byte 16). Older
/// message sets are skipped.
pub fn record_batches(bytes: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at + 17 <= bytes.len() {
        let len = i32::from_be_bytes(bytes[at + 8..at + 12].try_into().expect("4 bytes"));
        if len <= 0 {
            break;
        }
        let end = at + 12 + len as usize;
        if end > bytes.len() {
            break; // a partial batch at the end of a size-bounded read
        }
        if bytes[at + 16] == 2 {
            out.push(&bytes[at..end]);
        }
        at = end;
    }
    out
}
