//! The follower fetch loop (RP-2.4).
//!
//! One task per *leader*, not per partition — a follower batches every
//! partition it replicates from the same leader into a single long-polled
//! Fetch, the way Kafka's `ReplicaFetcherThread` does. A hundred partitions on
//! three leaders costs three in-flight requests, not a hundred.
//!
//! The offset a follower asks from is simultaneously its progress report, its
//! liveness signal and its resume position. That is the structural win over
//! push, which needed three separate mechanisms — an ACK channel, heartbeat
//! replies, connection pruning — to reconstruct the same three facts, and
//! shipped a bug in each.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chronik_common::metadata::traits::MetadataStore;
use dashmap::DashMap;
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

use super::apply::{apply_fetched_records, ApplyRefusal};
use super::connection::LeaderConnection;
use super::protocol::{FetchPartitionRequest, FetchRequestSpec, FetchTopicRequest};

/// Pause after a fetch that returned nothing, so the loop cannot spin through
/// the brief window where the leader's log end is ahead of what it can serve.
/// Deliberately far below `max_wait_ms`: this is a spin guard, not a poll
/// interval, and it must not add latency to replication.
const EMPTY_FETCH_BACKOFF: Duration = Duration::from_millis(5);

/// Longest pause between attempts to repair a partition that keeps re-detecting
/// divergence. Capped so a partition that becomes repairable is picked up again
/// within a few seconds rather than sitting out a long backoff.
const MAX_REPAIR_BACKOFF: Duration = Duration::from_secs(5);

/// How long to wait after the Nth consecutive failed repair round.
///
/// Doubling from 10ms: a repair that succeeds on the second or third attempt —
/// the normal case, where reconciliation needs one round trip to settle — is
/// delayed imperceptibly, while one that never succeeds stops consuming a core.
fn repair_backoff(round: u32) -> Duration {
    let millis = 10u64.saturating_mul(1u64 << round.min(9));
    Duration::from_millis(millis).min(MAX_REPAIR_BACKOFF)
}

/// Tuning for the fetch loop.
#[derive(Debug, Clone)]
pub struct ReplicaFetcherConfig {
    /// How long a leader may hold a fetch open waiting for data. This is the
    /// steady-state cost of an idle partition: one request per this interval.
    pub max_wait_ms: i32,
    /// Bytes the leader waits to accumulate before answering early. 1 means
    /// "answer as soon as anything lands", which is what replication wants —
    /// batching here would add latency to the ISR without saving round trips,
    /// since the long poll already collapses idle time.
    pub min_bytes: i32,
    /// Response cap across all partitions in one fetch.
    pub max_bytes: i32,
    /// Per-partition response cap.
    pub partition_max_bytes: i32,
    /// How often to re-read partition assignments from metadata.
    pub refresh_interval: Duration,
    /// Backoff after a failed fetch, before retrying the same leader.
    pub retry_backoff: Duration,
    /// How many independent fetch tasks to run per leader, each with its own
    /// connection and its own in-flight request (Kafka's `num.replica.fetchers`).
    ///
    /// One means a strictly serial fetch → apply → fetch loop, and since
    /// `acks=all` completes only when the *next* fetch reports the new position,
    /// that loop's period is the floor on replicated write latency. Measured at
    /// ~60 cycles/s while the brokers sat at ~570% of 1600% available CPU: the
    /// machine had capacity, the protocol had no concurrency (RP-9).
    pub fetcher_count: usize,
}

impl Default for ReplicaFetcherConfig {
    fn default() -> Self {
        Self {
            max_wait_ms: 500,
            min_bytes: 1,
            max_bytes: 50 * 1024 * 1024,
            partition_max_bytes: 10 * 1024 * 1024,
            refresh_interval: Duration::from_secs(10),
            retry_backoff: Duration::from_millis(500),
            fetcher_count: 4,
        }
    }
}

impl ReplicaFetcherConfig {
    pub fn from_env() -> Self {
        let mut config = Self::default();
        if let Some(v) = env_i32("CHRONIK_REPLICA_FETCH_MAX_WAIT_MS") {
            config.max_wait_ms = v;
        }
        if let Some(v) = env_i32("CHRONIK_REPLICA_FETCH_MIN_BYTES") {
            config.min_bytes = v;
        }
        if let Some(v) = env_i32("CHRONIK_REPLICA_FETCH_MAX_BYTES") {
            config.max_bytes = v;
        }
        if let Some(v) = env_i32("CHRONIK_REPLICA_FETCH_PARTITION_MAX_BYTES") {
            config.partition_max_bytes = v;
        }
        if let Some(v) = env_i32("CHRONIK_REPLICA_FETCH_REFRESH_SECS") {
            if v > 0 {
                config.refresh_interval = Duration::from_secs(v as u64);
            }
        }
        if let Some(v) = env_i32("CHRONIK_REPLICA_FETCH_RETRY_BACKOFF_MS") {
            if v >= 0 {
                config.retry_backoff = Duration::from_millis(v as u64);
            }
        }
        if let Some(v) = env_i32("CHRONIK_REPLICA_FETCHERS") {
            if v > 0 {
                config.fetcher_count = v as usize;
            }
        }
        config
    }
}

fn env_i32(key: &str) -> Option<i32> {
    std::env::var(key).ok().and_then(|v| v.trim().parse().ok())
}

/// Deal one leader's partitions across up to `fetchers` fetch tasks.
///
/// Round-robin, so a leader with four partitions and four fetchers gives each
/// one partition rather than three getting one and the fourth getting nothing.
/// Never returns an empty group — a task with no partitions would loop building
/// requests it cannot send — so the result is at most `partitions.len()` groups.
///
/// Pure, because the alternative is discovering the distribution by reading log
/// lines on a running cluster.
pub fn split_for_fetchers(
    partitions: &[FollowedPartition],
    fetchers: usize,
) -> Vec<Vec<FollowedPartition>> {
    let groups = fetchers.max(1).min(partitions.len().max(1));
    let mut out: Vec<Vec<FollowedPartition>> = vec![Vec::new(); groups];
    for (index, partition) in partitions.iter().enumerate() {
        out[index % groups].push(partition.clone());
    }
    out.retain(|group| !group.is_empty());
    out
}

/// The local partition state a follower needs, narrowed to three questions.
///
/// The fetcher used to hold an `Arc<ProduceHandler>` for this. That is a large
/// object with a dozen production construction sites and **none in any test**,
/// which is precisely why the truncation path below went untested: you could not
/// build one to drive it. Six attempts to prove that path on a real cluster
/// failed in the test rig rather than the product, and the cheapest of those
/// attempts cost more than this trait.
///
/// Three methods is the whole surface. `ProduceHandler` implements it; a test
/// can implement it in a dozen lines.
#[async_trait::async_trait]
pub trait FollowerState: Send + Sync {
    /// Leader-epoch history for this node's own log.
    fn leader_epochs(&self) -> Arc<crate::replication::leader_epoch::LeaderEpochStore>;

    /// Where this node's log currently ends for a partition.
    async fn local_log_end(&self, topic: &str, partition: i32) -> i64;

    /// Move the partition's offsets *down* after its log was truncated.
    async fn reset_after_truncation(
        &self,
        topic: &str,
        partition: i32,
        new_end: i64,
    ) -> chronik_common::Result<()>;
}

#[async_trait::async_trait]
impl FollowerState for crate::produce_handler::ProduceHandler {
    fn leader_epochs(&self) -> Arc<crate::replication::leader_epoch::LeaderEpochStore> {
        crate::produce_handler::ProduceHandler::leader_epochs(self)
    }

    /// The LOG END, not the high watermark — the difference is the whole point.
    ///
    /// The two coincide on a node that has only ever followed, because the apply
    /// path moves both together. They part on a node that was a LEADER and
    /// accepted writes which never reached quorum: those records sit above its
    /// high watermark, and they are exactly the divergent tail this fetcher
    /// exists to cut.
    ///
    /// Reading the high watermark here reported a log end BELOW the records that
    /// need truncating, so `plan_reconciliation` would conclude "my log is a
    /// prefix of the leader's" and resume — leaving the uncommitted tail in place
    /// forever, silently, which is the failure RP-3.3 is for.
    async fn local_log_end(&self, topic: &str, partition: i32) -> i64 {
        let log_end = self.get_log_end_offset(topic, partition).await;
        if log_end > 0 {
            return log_end;
        }
        // No in-memory state yet (nothing produced since start): fall back to the
        // recovered watermark rather than claiming an empty log.
        self.get_high_watermark(topic, partition)
            .await
            .unwrap_or(0)
            .max(0)
    }

    async fn reset_after_truncation(
        &self,
        topic: &str,
        partition: i32,
        new_end: i64,
    ) -> chronik_common::Result<()> {
        self.reset_offsets_after_truncation(topic, partition, new_end).await
    }
}

/// One partition this node replicates, and who to get it from.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct FollowedPartition {
    pub topic: String,
    pub partition: i32,
    pub leader: u64,
}

/// Decide which partitions this node must fetch, grouped by leader.
///
/// Pure so the placement rules are testable without metadata or a cluster:
/// replicate only what you are assigned, never fetch from yourself, and never
/// fetch a partition whose leader is unknown or unreachable.
pub fn plan_assignments(
    node_id: u64,
    assignments: &[(String, i32, Option<u64>, Vec<u64>)],
    known_peers: &HashMap<u64, String>,
) -> BTreeMap<u64, Vec<FollowedPartition>> {
    let mut by_leader: BTreeMap<u64, Vec<FollowedPartition>> = BTreeMap::new();

    for (topic, partition, leader, replicas) in assignments {
        // Only replicate partitions assigned to this node.
        if !replicas.contains(&node_id) {
            continue;
        }

        let Some(leader) = *leader else {
            debug!("Skipping {}-{}: no leader elected yet", topic, partition);
            continue;
        };

        // The leader serves its own log; it does not fetch it.
        if leader == node_id {
            continue;
        }

        if !known_peers.contains_key(&leader) {
            warn!(
                "Cannot replicate {}-{}: leader {} has no address in the cluster config",
                topic, partition, leader
            );
            continue;
        }

        by_leader.entry(leader).or_default().push(FollowedPartition {
            topic: topic.clone(),
            partition: *partition,
            leader,
        });
    }

    for partitions in by_leader.values_mut() {
        partitions.sort();
    }

    by_leader
}

/// Runs a node's follower-side replication.
pub struct ReplicaFetcher {
    node_id: u64,
    config: ReplicaFetcherConfig,
    metadata_store: Arc<dyn MetadataStore>,
    wal_manager: Arc<chronik_wal::WalManager>,
    /// Kept concrete because the apply path hands it to `apply_fetched_records`,
    /// which updates the transaction index as well as offsets.
    produce_handler: Option<Arc<crate::produce_handler::ProduceHandler>>,
    /// The same handler in production, narrowed to what reconciliation needs —
    /// and substitutable in tests. See [`FollowerState`].
    follower_state: Option<Arc<dyn FollowerState>>,
    /// node id → Kafka address, from the cluster config.
    peers: HashMap<u64, String>,
    /// Wakes the supervisor the moment assignments change, instead of leaving it
    /// to the next `refresh_interval` tick.
    ///
    /// Without this, a partition created at t=0 is not replicated until the tick
    /// at t=10s, and an `acks=all` produce to it blocks for that whole time —
    /// waiting for a follower ack from a follower that does not yet know the
    /// partition exists. Measured: 7s for the first write to a new topic, which
    /// is past the default `socket.timeout.ms` of clients that lower it, and a
    /// timed-out produce that the broker did append is retried and appended
    /// twice (#36).
    metadata_events: Option<Arc<crate::metadata_events::MetadataEventBus>>,
    /// Follower LEO per partition, the offset the next fetch asks from.
    positions: Arc<DashMap<(String, i32), i64>>,
    shutdown: Arc<AtomicBool>,
    batches_applied: Arc<AtomicU64>,
    fetch_errors: Arc<AtomicU64>,
}

impl ReplicaFetcher {
    pub fn new(
        node_id: u64,
        config: ReplicaFetcherConfig,
        metadata_store: Arc<dyn MetadataStore>,
        wal_manager: Arc<chronik_wal::WalManager>,
        peers: HashMap<u64, String>,
    ) -> Arc<Self> {
        Arc::new(Self {
            node_id,
            config,
            metadata_store,
            wal_manager,
            produce_handler: None,
            follower_state: None,
            peers,
            metadata_events: None,
            positions: Arc::new(DashMap::new()),
            shutdown: Arc::new(AtomicBool::new(false)),
            batches_applied: Arc::new(AtomicU64::new(0)),
            fetch_errors: Arc::new(AtomicU64::new(0)),
        })
    }

    pub fn with_produce_handler(
        mut self: Arc<Self>,
        handler: Arc<crate::produce_handler::ProduceHandler>,
    ) -> Arc<Self> {
        if let Some(inner) = Arc::get_mut(&mut self) {
            inner.follower_state = Some(handler.clone() as Arc<dyn FollowerState>);
            inner.produce_handler = Some(handler);
        } else {
            warn!("ReplicaFetcher already shared; produce handler not attached");
        }
        self
    }

    /// Subscribe the supervisor to metadata changes, so a new or reassigned
    /// partition starts replicating immediately rather than at the next tick.
    pub fn with_metadata_events(
        mut self: Arc<Self>,
        bus: Arc<crate::metadata_events::MetadataEventBus>,
    ) -> Arc<Self> {
        if let Some(inner) = Arc::get_mut(&mut self) {
            inner.metadata_events = Some(bus);
        } else {
            warn!("ReplicaFetcher already shared; metadata events not attached");
        }
        self
    }

    /// Attach local state without a `ProduceHandler` — the seam that makes the
    /// reconciliation path drivable from a test.
    #[cfg(test)]
    fn with_follower_state(mut self: Arc<Self>, state: Arc<dyn FollowerState>) -> Arc<Self> {
        if let Some(inner) = Arc::get_mut(&mut self) {
            inner.follower_state = Some(state);
        }
        self
    }

    pub fn batches_applied(&self) -> u64 {
        self.batches_applied.load(Ordering::Relaxed)
    }

    pub fn fetch_errors(&self) -> u64 {
        self.fetch_errors.load(Ordering::Relaxed)
    }

    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }

    /// Start the supervisor. It owns one fetch task per leader and rebuilds
    /// them when assignments change.
    pub fn start(self: &Arc<Self>) {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            this.run_supervisor().await;
        });
    }

    async fn run_supervisor(self: Arc<Self>) {
        info!(
            "Follower-pull replication active on node {} ({} peers known)",
            self.node_id,
            self.peers.len()
        );

        let mut running: HashMap<(u64, usize), tokio::task::JoinHandle<()>> = HashMap::new();
        let mut current: BTreeMap<u64, Vec<FollowedPartition>> = BTreeMap::new();
        let mut events = self.metadata_events.as_ref().map(|bus| bus.subscribe());

        while !self.shutdown.load(Ordering::Relaxed) {
            let desired = match self.read_assignments().await {
                Ok(assignments) => {
                    let plan = plan_assignments(self.node_id, &assignments, &self.peers);
                    self.warn_if_replicating_nothing(&assignments, &plan);
                    plan
                }
                Err(e) => {
                    warn!("Could not read partition assignments: {}", e);
                    self.await_assignment_change(&mut events).await;
                    continue;
                }
            };

            if desired != current {
                // Assignments changed: stop every task and rebuild. A follower
                // has at most a handful of leaders, so a full rebuild costs one
                // reconnect and avoids the state machine a diff would need.
                for ((leader, index), handle) in running.drain() {
                    debug!("Stopping fetch task {} for leader {}", index, leader);
                    handle.abort();
                }

                for (leader, partitions) in &desired {
                    let addr = match self.peers.get(leader) {
                        Some(addr) => addr.clone(),
                        None => continue,
                    };

                    // Split this leader's partitions across several fetch tasks,
                    // each with its own connection and its own in-flight request.
                    //
                    // One task per leader makes replication a strictly serial
                    // loop — fetch, apply, fetch — and `acks=all` cannot complete
                    // until the *next* fetch reports the new position, so that
                    // loop's period is the floor on replicated write latency.
                    // Measured: ~60 cycles/s, and the cycle is almost entirely
                    // the fetch waiting on a leader that is busy serving
                    // producers (RP-9). The brokers were at ~570% CPU of 1600%
                    // available, so the machine had the capacity; what it lacked
                    // was concurrent requests.
                    //
                    // This is Kafka's `num.replica.fetchers`, and it splits by
                    // partition for the same reason Kafka does: a single
                    // partition's log is a sequential stream whose next fetch
                    // offset is only known after the previous response is
                    // applied, so it cannot be pipelined — but different
                    // partitions are independent and can be fetched at once.
                    let groups = split_for_fetchers(partitions, self.config.fetcher_count);
                    info!(
                        "Replicating {} partition(s) from leader {} at {} across {} fetcher(s)",
                        partitions.len(),
                        leader,
                        addr,
                        groups.len()
                    );
                    for (index, group) in groups.into_iter().enumerate() {
                        let this = Arc::clone(&self);
                        let addr = addr.clone();
                        let leader = *leader;
                        running.insert(
                            (leader, index),
                            tokio::spawn(async move {
                                this.run_leader_loop(leader, addr, group).await;
                            }),
                        );
                    }
                }

                current = desired;
            }

            self.await_assignment_change(&mut events).await;
        }

        for (_, handle) in running.drain() {
            handle.abort();
        }
        info!("Follower-pull replication stopped on node {}", self.node_id);
    }

    /// Wait before re-reading assignments: until metadata says something
    /// changed, or the refresh interval elapses, whichever comes first.
    ///
    /// The interval alone is a poor answer to "when should a follower notice a
    /// new partition?". At the default 10s, a partition created at t=0 is not
    /// replicated until t=10s, and every `acks=all` produce to it blocks for the
    /// whole gap — the producer is waiting on a follower that has not been told
    /// the partition exists. That is not a slow path, it is a stall, and clients
    /// that lower `socket.timeout.ms` below it time out, retry, and get their
    /// records appended twice (#36).
    ///
    /// The interval stays as a floor for the case where no bus is attached and
    /// as a backstop if an event is ever missed — a lagged broadcast receiver
    /// drops messages under load, and this must not depend on catching every one.
    async fn await_assignment_change(
        &self,
        events: &mut Option<tokio::sync::broadcast::Receiver<crate::metadata_events::MetadataEvent>>,
    ) {
        let Some(rx) = events.as_mut() else {
            sleep(self.config.refresh_interval).await;
            return;
        };

        match tokio::time::timeout(self.config.refresh_interval, rx.recv()).await {
            Ok(Ok(event)) => {
                debug!("Re-reading assignments: metadata changed ({:?})", event);
            }
            // Lagged: events were dropped, so assignments may well have changed.
            // Re-reading is the correct response to not knowing.
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(n))) => {
                debug!("Re-reading assignments: missed {} metadata event(s)", n);
            }
            Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                *events = None;
                sleep(self.config.refresh_interval).await;
            }
            Err(_) => {} // the interval elapsed; re-read on the tick as before
        }
    }

    /// Say so, loudly, when this node ends up replicating nothing.
    ///
    /// Pull moves a dependency that push did not have: the *follower* must know
    /// who leads each partition. If its metadata is missing or has no leader,
    /// `plan_assignments` correctly plans nothing — and the node then sits there
    /// replicating nothing at all, while the leader's `/admin/status` reports
    /// `isr:[1,2,3]` because its own view is fine.
    ///
    /// That is the founding bug of this roadmap reproduced exactly, so it must
    /// never be silent. Observed on the test cluster: after a restart one node
    /// held zero partition assignments and replicated nothing, with no log line
    /// to say so.
    fn warn_if_replicating_nothing(
        &self,
        assignments: &[(String, i32, Option<u64>, Vec<u64>)],
        plan: &BTreeMap<u64, Vec<FollowedPartition>>,
    ) {
        if !plan.is_empty() || assignments.is_empty() {
            return;
        }

        let assigned_here = assignments
            .iter()
            .filter(|(_, _, _, replicas)| replicas.contains(&self.node_id))
            .count();
        let leaderless = assignments
            .iter()
            .filter(|(_, _, leader, replicas)| leader.is_none() && replicas.contains(&self.node_id))
            .count();

        if assigned_here == 0 {
            warn!(
                "Node {} replicates nothing: it is not listed as a replica for any of the {} known partitions. \
                 If this cluster has RF>1, this node's partition assignments are missing or stale.",
                self.node_id,
                assignments.len()
            );
        } else {
            warn!(
                "Node {} replicates nothing despite being a replica for {} partition(s) — {} of them have no leader \
                 in this node's metadata. Data is NOT being replicated here, whatever the leader's /admin/status says.",
                self.node_id, assigned_here, leaderless
            );
        }
    }

    /// Read every partition's leader and replica set from metadata.
    ///
    /// Sourced from `get_partition_assignments`, the same call `/admin/status`
    /// uses, and deliberately NOT from `get_partition_leader` /
    /// `get_partition_replicas`. Those diverge: after a restart the per-partition
    /// leader lookup returned `None` for topics whose assignments still carried a
    /// leader, so a follower silently replicated only the handful of topics that
    /// had been written to recently — 3 of ~30 on the test cluster — while
    /// `/admin/status` reported every one of them `isr:[1,2,3]`.
    ///
    /// A partition that is quietly not replicated while metadata claims it is, is
    /// this roadmap's founding bug wearing a different hat. One source of truth.
    ///
    /// It is also one metadata call per topic instead of two per partition.
    async fn read_assignments(&self) -> chronik_common::Result<Vec<(String, i32, Option<u64>, Vec<u64>)>> {
        let topics = self.metadata_store.list_topics().await?;
        let mut out = Vec::new();

        for topic in topics {
            let assignments = match self.metadata_store.get_partition_assignments(&topic.name).await {
                Ok(assignments) => assignments,
                Err(e) => {
                    warn!("Cannot read assignments for {}: {}", topic.name, e);
                    continue;
                }
            };

            for assignment in assignments {
                out.push((
                    topic.name.clone(),
                    assignment.partition as i32,
                    Some(assignment.leader_id),
                    assignment.replicas.clone(),
                ));
            }
        }

        Ok(out)
    }

    /// The fetch loop for one leader.
    async fn run_leader_loop(
        self: Arc<Self>,
        leader: u64,
        addr: String,
        partitions: Vec<FollowedPartition>,
    ) {
        let mut connection = LeaderConnection::new(addr);

        // Reconcile before the first fetch. This task exists because the
        // supervisor saw an assignment change, which includes a leader change —
        // the one moment a follower can be holding records the new leader never
        // committed. Appending on top of those interleaves two histories.
        let mut needs_reconcile = true;

        // Consecutive fetches that found divergence again. Reset by any fetch
        // that does not — so a partition repaired on the second attempt pays
        // nothing, and one that never repairs backs off instead of spinning.
        let mut repair_rounds: u32 = 0;

        // Cycle counter, so the timing breakdown above is sampled rather than
        // logged on every round trip.
        let mut cycles: u64 = 0;

        // Cycles that came back carrying nothing. A high share of these under
        // sustained load means the leader is waking us for records it cannot yet
        // serve, and each one costs `EMPTY_FETCH_BACKOFF` (RP-9).
        let mut empties: u64 = 0;

        while !self.shutdown.load(Ordering::Relaxed) {
            if needs_reconcile {
                match self.reconcile_with_leader(&mut connection, &partitions).await {
                    Ok(()) => needs_reconcile = false,
                    Err(e) => {
                        self.fetch_errors.fetch_add(1, Ordering::Relaxed);
                        warn!(
                            "Could not reconcile with leader {} at {}: {}. \
                             Not fetching until this succeeds — appending first could interleave two histories.",
                            leader,
                            connection.addr(),
                            e
                        );
                        sleep(self.config.retry_backoff).await;
                        continue;
                    }
                }
            }

            // Where the replication cycle's time goes.
            //
            // `acks=all` cannot complete until the follower's NEXT fetch reports
            // a position past the record, so the cycle time here is the floor on
            // replicated write latency and the cap on replicated throughput.
            // Measured at ~60 cycles/s (16ms each) while fsync, the leader's poll
            // interval, serial partition serving and partition count were all
            // ruled out one at a time — so the breakdown is logged rather than
            // guessed at again (RP-9).
            let cycle_start = std::time::Instant::now();

            let spec = match self.build_request(&partitions).await {
                Some(spec) => spec,
                None => {
                    sleep(self.config.retry_backoff).await;
                    continue;
                }
            };
            let built = cycle_start.elapsed();

            match connection.fetch(spec).await {
                Ok(response) => {
                    let fetched = cycle_start.elapsed();
                    let mut got_records = false;
                    let mut diverged = false;
                    for topic in response.topics {
                        for partition in topic.partitions {
                            got_records |= !partition.records.is_empty();
                            if self.handle_partition_response(&topic.name, partition).await
                                == PartitionOutcome::Diverged
                            {
                                needs_reconcile = true;
                                diverged = true;
                            }
                        }
                    }

                    let applied = cycle_start.elapsed();
                    cycles = cycles.wrapping_add(1);
                    if !got_records {
                        empties = empties.wrapping_add(1);
                    }
                    if cycles % 200 == 0 {
                        debug!(
                            "replication cycle {}: build {:?}, fetch {:?}, apply {:?}, total {:?}, empty {}/{}",
                            cycles,
                            built,
                            fetched - built,
                            applied - fetched,
                            applied,
                            empties,
                            cycles
                        );
                    }

                    // Detect → reconcile → resume → detect the same thing again
                    // is a cycle that makes no progress, and it ran at network
                    // speed: 37,166 iterations in a single run, ~1,600/second,
                    // indefinitely, burning a core while repairing nothing.
                    //
                    // The causes are fixed elsewhere; this is the guard that
                    // should have made them survivable. A repair loop that
                    // cannot advance must slow down whatever the reason —
                    // including reasons not yet found.
                    if diverged {
                        let round = repair_rounds.saturating_add(1);
                        repair_rounds = round;
                        if round % 20 == 0 {
                            warn!(
                                "Replication from leader {} has re-detected divergence {} times \
                                 without making progress — the repair is not converging.",
                                leader, round
                            );
                        }
                        sleep(repair_backoff(round)).await;
                    } else {
                        repair_rounds = 0;
                    }

                    // A response carrying nothing normally means the leader held
                    // this request for its full `max_wait_ms` and there was still
                    // no data — already paced, nothing to add.
                    //
                    // It can also come back instantly: the leader assigns a
                    // batch's offsets before it writes them, so for a moment its
                    // log end is past what it can actually serve. Without a pause
                    // this loop would spin at network speed through that window.
                    // It is short, so the backoff only has to be non-zero.
                    if !got_records {
                        sleep(EMPTY_FETCH_BACKOFF).await;
                    }
                }
                Err(e) => {
                    self.fetch_errors.fetch_add(1, Ordering::Relaxed);
                    warn!(
                        "Fetch from leader {} at {} failed: {}",
                        leader,
                        connection.addr(),
                        e
                    );
                    sleep(self.config.retry_backoff).await;
                }
            }
        }
    }

    /// Ask the leader where this follower's epochs ended, and truncate to the
    /// answer (RP-3.3).
    ///
    /// This is the whole point of leader epochs. Two logs can agree on every
    /// offset and disagree on the records at them — a follower that accepted
    /// writes from a leader which then lost the election holds records that were
    /// never committed. Offsets alone cannot find where they parted; the epoch
    /// history can, because it records which leader wrote which range.
    ///
    /// Errors propagate: the caller does not fetch until this succeeds. An
    /// unreconciled fetch is how divergence becomes permanent.
    async fn reconcile_with_leader(
        &self,
        connection: &mut LeaderConnection,
        partitions: &[FollowedPartition],
    ) -> chronik_common::Result<()> {
        use chronik_protocol::offset_for_leader_epoch_types::{
            OffsetForLeaderEpochRequest, OffsetForLeaderPartition, OffsetForLeaderTopic,
        };

        let Some(state) = self.follower_state.as_ref() else {
            // No local state to reconcile against.
            return Ok(());
        };
        let epochs = state.leader_epochs();

        let mut by_topic: BTreeMap<String, Vec<OffsetForLeaderPartition>> = BTreeMap::new();
        for followed in partitions {
            // No history means nothing to reconcile: either the log is empty, or
            // it predates epoch stamping. Truncating on that basis would be a
            // guess, and a guess here destroys data.
            let Some(epoch) = epochs.latest_epoch(&followed.topic, followed.partition) else {
                continue;
            };
            by_topic
                .entry(followed.topic.clone())
                .or_default()
                .push(OffsetForLeaderPartition {
                    partition: followed.partition,
                    leader_epoch: epoch,
                });
        }

        if by_topic.is_empty() {
            // Worth saying out loud: this is the state in which a follower can
            // never truncate. It is legitimate for an empty or pre-RP-3 log,
            // and it is also what a bug looks like — the epoch warm-up running
            // after the fetcher started produced exactly this, silently.
            debug!(
                "Nothing to reconcile with this leader: none of the {} followed partition(s) \
                 have leader-epoch history",
                partitions.len()
            );
            return Ok(());
        }

        let asked: usize = by_topic.values().map(|p| p.len()).sum();
        info!(
            "Reconciling {} partition(s) with the leader before fetching (leader-epoch handshake)",
            asked
        );

        let request = OffsetForLeaderEpochRequest {
            topics: by_topic
                .into_iter()
                .map(|(name, partitions)| OffsetForLeaderTopic { name, partitions })
                .collect(),
        };

        let response = connection.offset_for_leader_epoch(&request).await?;

        for topic in response.topics {
            for answer in topic.partitions {
                self.apply_epoch_answer(&topic.name, answer).await;
            }
        }

        Ok(())
    }

    /// Act on one partition's "your epoch ended here".
    async fn apply_epoch_answer(
        &self,
        topic: &str,
        answer: chronik_protocol::offset_for_leader_epoch_types::OffsetForLeaderPartitionResponse,
    ) {
        let partition = answer.partition;
        let local_end = self.position_of(topic, partition).await;

        match plan_reconciliation(answer.error_code, answer.end_offset, local_end) {
            ReconcileAction::Resume => debug!(
                "{}-{}: log is a prefix of the leader's (ours ends at {}, the epoch ran to {}) — nothing to truncate",
                topic, partition, local_end, answer.end_offset
            ),
            ReconcileAction::Abstain { reason } => warn!(
                "{}-{}: not truncating — {}. Replication will resolve this through the fetch \
                 offset instead if it can.",
                topic, partition, reason
            ),
            ReconcileAction::Truncate { to } => {
                self.truncate_partition(topic, partition, to, local_end).await
            }
        }
    }

    /// Discard this replica's divergent tail and reset everything that tracked it.
    async fn truncate_partition(&self, topic: &str, partition: i32, target: i64, local_end: i64) {
        warn!(
            "{}-{}: diverged from the leader. Local log ends at {}, but the leader's history \
             for our epoch ends at {} — discarding offsets {}..{}.",
            topic, partition, local_end, target, target, local_end
        );

        let outcome = match self.wal_manager.truncate_to(topic, partition, target).await {
            Ok(outcome) => outcome,
            Err(e) => {
                // The divergent records are still on disk. Refuse to fetch past
                // them rather than append on top of a log known to be wrong.
                error!(
                    "{}-{}: could not truncate to {}: {}. Replication of this partition is \
                     stalled — its log diverges from the leader's and cannot be repaired here.",
                    topic, partition, target, e
                );
                self.fetch_errors.fetch_add(1, Ordering::Relaxed);
                return;
            }
        };

        // The log may end below the target: a target inside a batch takes that
        // whole batch. Resume from where it actually ends.
        //
        // `None` means the log holds no records at all, which is a real outcome
        // — everything above the target, nothing below it — and only then is 0
        // right. It must not be reached by a truncation that simply had no work
        // to do; that path now reports the real log end, because collapsing "I
        // do not know" into 0 threw away a healthy log and re-replicated the
        // whole partition.
        let new_end = outcome.new_log_end_offset.unwrap_or(0);
        if outcome.new_log_end_offset.is_none() && !outcome.touched_disk() {
            warn!(
                "{}-{}: truncation to {} changed nothing and could not say where the log ends. \
                 Treating it as empty and restarting replication from 0 — if this partition had \
                 records, they are about to be re-fetched.",
                topic, partition, target
            );
        }
        self.positions.insert((topic.to_string(), partition), new_end);

        if let Some(state) = &self.follower_state {
            state
                .leader_epochs()
                .truncate_from_end(topic, partition, new_end);
            if let Err(e) = state.reset_after_truncation(topic, partition, new_end).await {
                warn!(
                    "{}-{}: truncated the log to {} but could not reset the watermark: {}",
                    topic, partition, new_end, e
                );
            }
        }

        info!(
            "{}-{}: truncated to {} ({} segment(s) removed, {} bytes discarded); resuming replication",
            topic, partition, new_end, outcome.segments_deleted, outcome.bytes_discarded
        );
    }

    /// Build one Fetch covering every partition this leader owns.
    async fn build_request(&self, partitions: &[FollowedPartition]) -> Option<FetchRequestSpec> {
        let mut by_topic: BTreeMap<String, Vec<FetchPartitionRequest>> = BTreeMap::new();

        for followed in partitions {
            let offset = self
                .position_of(&followed.topic, followed.partition)
                .await;

            by_topic
                .entry(followed.topic.clone())
                .or_default()
                .push(FetchPartitionRequest {
                    partition: followed.partition,
                    fetch_offset: offset,
                    log_start_offset: 0,
                    // Left unset: this broker parses `current_leader_epoch` on
                    // the leader side but never validates it, so populating it
                    // would cost a metadata read per fetch and fence nothing.
                    // Divergence is caught by the epoch handshake in
                    // `reconcile_with_leader` instead. Wiring real fencing means
                    // the leader rejecting stale epochs first; see
                    // docs/ROADMAP_REPLICATION.md.
                    current_leader_epoch: -1,
                    max_bytes: self.config.partition_max_bytes,
                });
        }

        if by_topic.is_empty() {
            return None;
        }

        Some(FetchRequestSpec {
            correlation_id: 0, // the connection assigns it
            replica_id: self.node_id as i32,
            max_wait_ms: self.config.max_wait_ms,
            min_bytes: self.config.min_bytes,
            max_bytes: self.config.max_bytes,
            topics: by_topic
                .into_iter()
                .map(|(name, partitions)| FetchTopicRequest { name, partitions })
                .collect(),
        })
    }

    /// The offset this follower's log next accepts — the ONE answer.
    ///
    /// Every caller that needs "where does our log end" goes through here,
    /// including `reconcile_with_leader`, which used to ask `local_log_end`
    /// instead. Two sources for one fact drifted apart, and the broker logged
    /// both, one line apart, for the same partition at the same instant:
    ///
    /// ```text
    /// log is a prefix of the leader's (ours ends at 91, the epoch ran to 91) — nothing to truncate
    /// fetched batch spans [91, 130] across the local log end 100 — logs have diverged
    /// ```
    ///
    /// Reconcile compared 91 and correctly resumed; the apply path measured
    /// against 100 and rejected the very batch that fetch returned. Neither was
    /// wrong about its own number, and the loop could not terminate because each
    /// kept being right.
    ///
    /// The authority is this map, advanced by the apply path as batches land,
    /// and NOT `ProduceHandler`'s offsets. Those are maintained by the produce
    /// path; on a follower they are updated as a side effect of applying, and
    /// reading them here instead — tried, measured — leaves the fetch position
    /// stuck at 0, so the follower re-fetches offset 0 forever, never reports
    /// progress, and consumers capped at the in-sync watermark see almost
    /// nothing (1 of 100 records). The seeding read below is where local state
    /// legitimately comes in: after a restart it is the only thing that knows.
    async fn position_of(&self, topic: &str, partition: i32) -> i64 {
        let key = (topic.to_string(), partition);
        if let Some(offset) = self.positions.get(&key) {
            return *offset;
        }

        let offset = match &self.follower_state {
            Some(state) => state.local_log_end(topic, partition).await,
            None => 0,
        };

        debug!(
            "Resuming replication of {}-{} from local offset {}",
            topic, partition, offset
        );
        self.positions.insert(key, offset);
        offset
    }

    async fn handle_partition_response(
        &self,
        topic: &str,
        partition: super::protocol::FetchedPartition,
    ) -> PartitionOutcome {
        let index = partition.partition;

        if partition.error_code != 0 {
            return self
                .handle_partition_error(topic, index, partition.error_code)
                .await;
        }

        if partition.records.is_empty() {
            return PartitionOutcome::Fine; // caught up
        }

        let expected = self.position_of(topic, index).await;

        match apply_fetched_records(
            &self.wal_manager,
            topic,
            index,
            &partition.records,
            expected,
            self.produce_handler.as_ref(),
            None,
        )
        .await
        {
            Ok(leo) => {
                if leo != expected {
                    self.positions.insert((topic.to_string(), index), leo);
                    self.batches_applied.fetch_add(1, Ordering::Relaxed);
                    debug!(
                        "Replicated {}-{} to offset {} (leader hw {})",
                        topic, index, leo, partition.high_watermark
                    );
                }
                PartitionOutcome::Fine
            }
            Err(ApplyRefusal::Gap { expected, found }) => {
                // The leader's log starts past where this replica is: retention
                // moved on while it was away. Resetting to the leader's start
                // is the only way forward, and losing the intervening offsets is
                // real — say so rather than logging a debug line.
                error!(
                    "{}-{}: local log ends at {} but the leader's data starts at {}. \
                     Resetting to {}; offsets {}..{} are not recoverable on this replica.",
                    topic, index, expected, found, found, expected, found
                );
                self.positions.insert((topic.to_string(), index), found);
                PartitionOutcome::Fine
            }
            Err(refusal @ ApplyRefusal::Straddle { .. }) => {
                // Divergence caught mid-stream: the leader sent a batch that
                // starts below our LEO and ends above it, so the two logs hold
                // different records at the same offsets. Appending would
                // interleave two histories. Re-run the epoch handshake and
                // truncate to whatever the leader says.
                warn!(
                    "{}-{}: {} — reconciling with the leader before fetching again.",
                    topic, index, refusal
                );
                PartitionOutcome::Diverged
            }
            Err(refusal) => {
                self.fetch_errors.fetch_add(1, Ordering::Relaxed);
                warn!("{}-{}: could not apply fetched records — {}", topic, index, refusal);
                PartitionOutcome::Fine
            }
        }
    }

    async fn handle_partition_error(
        &self,
        topic: &str,
        partition: i32,
        error_code: i16,
    ) -> PartitionOutcome {
        match error_code {
            // The leader moved. The supervisor re-reads assignments on its own
            // schedule and will repoint or stop this task.
            5 | 6 => {
                debug!(
                    "{}-{}: leader has moved (error {}), waiting for the assignment refresh",
                    topic, partition, error_code
                );
                PartitionOutcome::Fine
            }
            // Our fetch offset is not in the leader's log at all. That is either
            // retention having moved past us or a divergence — the epoch
            // handshake distinguishes them, so ask rather than guess.
            1 => {
                warn!(
                    "{}-{}: offset out of range on the leader; reconciling to find out where our logs part",
                    topic, partition
                );
                PartitionOutcome::Diverged
            }
            // Fencing: our epoch is stale or ahead of the leader's.
            74 | 75 => {
                warn!(
                    "{}-{}: leader rejected our epoch (error {}); reconciling",
                    topic, partition, error_code
                );
                PartitionOutcome::Diverged
            }
            3 => {
                debug!("{}-{}: leader does not know this partition yet", topic, partition);
                PartitionOutcome::Fine
            }
            other => {
                warn!("{}-{}: leader returned error {}", topic, partition, other);
                PartitionOutcome::Fine
            }
        }
    }
}

/// What a follower does with a leader's answer to "where did my epoch end?".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconcileAction {
    /// The local log is a prefix of the leader's. Keep fetching.
    Resume,
    /// The answer cannot be acted on. Leave the log alone — every alternative
    /// here destroys data on a guess.
    Abstain { reason: String },
    /// Discard everything at and above this offset.
    Truncate { to: i64 },
}

/// Decide what to do with one epoch answer.
///
/// Pure, because this is the decision that deletes data. Every branch that
/// *does not* truncate is as important as the one that does: acting on an
/// error code, or on the `-1` that means "I cannot answer", would discard a
/// correct log — which is the failure leader epochs exist to prevent, arrived
/// at through the machinery meant to prevent it.
pub fn plan_reconciliation(error_code: i16, end_offset: i64, local_log_end: i64) -> ReconcileAction {
    if error_code != 0 {
        return ReconcileAction::Abstain {
            reason: format!("the leader answered the epoch query with error {error_code}"),
        };
    }

    // -1 is the leader saying it cannot speak to our epoch: newer than anything
    // it holds, or aged out of its history. It is not an offset.
    if end_offset < 0 {
        return ReconcileAction::Abstain {
            reason: "the leader cannot say where our epoch ended".to_string(),
        };
    }

    if end_offset >= local_log_end {
        return ReconcileAction::Resume;
    }

    ReconcileAction::Truncate { to: end_offset }
}

/// What one partition's slice of a fetch response means for the loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PartitionOutcome {
    /// Applied, idle, or a condition that resolves itself.
    Fine,
    /// This follower's log disagrees with the leader's. It must run the epoch
    /// handshake and truncate before fetching this leader again.
    Diverged,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peers() -> HashMap<u64, String> {
        HashMap::from([
            (1, "node1:9092".to_string()),
            (2, "node2:9092".to_string()),
            (3, "node3:9092".to_string()),
        ])
    }

    fn followed(n: usize) -> Vec<FollowedPartition> {
        (0..n)
            .map(|i| FollowedPartition {
                topic: "orders".to_string(),
                partition: i as i32,
                leader: 1,
            })
            .collect()
    }

    /// Partitions are dealt round-robin, so four partitions across four fetchers
    /// is one each — not three on one task and an idle fourth.
    #[test]
    fn partitions_are_dealt_evenly_across_fetchers() {
        let groups = split_for_fetchers(&followed(4), 4);
        assert_eq!(groups.len(), 4);
        assert!(groups.iter().all(|g| g.len() == 1));
    }

    /// Every partition is fetched exactly once. Losing one here means a replica
    /// that silently never receives it — this roadmap's founding bug.
    #[test]
    fn every_partition_is_assigned_exactly_once() {
        for fetchers in 1..=6 {
            let groups = split_for_fetchers(&followed(5), fetchers);
            let mut seen: Vec<i32> = groups.iter().flatten().map(|p| p.partition).collect();
            seen.sort();
            assert_eq!(seen, vec![0, 1, 2, 3, 4], "with {} fetcher(s)", fetchers);
        }
    }

    /// Never an empty group: a fetch task with no partitions would loop building
    /// requests it cannot send.
    #[test]
    fn more_fetchers_than_partitions_does_not_make_idle_tasks() {
        let groups = split_for_fetchers(&followed(2), 8);
        assert_eq!(groups.len(), 2);
        assert!(groups.iter().all(|g| !g.is_empty()));
    }

    /// A single fetcher keeps the old shape exactly: one task, all partitions.
    #[test]
    fn one_fetcher_is_the_previous_behaviour() {
        let groups = split_for_fetchers(&followed(5), 1);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].len(), 5);
    }

    /// Zero is not a valid count and must not produce zero tasks — that would
    /// stop replication silently, which is the failure mode this whole effort
    /// exists to end.
    #[test]
    fn zero_fetchers_still_replicates() {
        let groups = split_for_fetchers(&followed(3), 0);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].len(), 3);
    }

    #[test]
    fn no_partitions_produces_no_tasks() {
        assert!(split_for_fetchers(&[], 4).is_empty());
    }

    /// The core placement rule: fetch what you are assigned, from whoever leads
    /// it. Grouping by leader is what keeps one long-poll per leader rather
    /// than one per partition.
    #[test]
    fn partitions_are_grouped_by_their_leader() {
        let assignments = vec![
            ("orders".to_string(), 0, Some(1), vec![1, 2, 3]),
            ("orders".to_string(), 1, Some(2), vec![2, 3, 1]),
            ("events".to_string(), 0, Some(1), vec![1, 3, 2]),
        ];

        let plan = plan_assignments(3, &assignments, &peers());

        assert_eq!(plan.len(), 2, "two leaders → two fetch tasks");
        assert_eq!(plan[&1].len(), 2, "both partitions led by node 1 share one request");
        assert_eq!(plan[&2].len(), 1);
    }

    /// A leader serves its own log. Fetching from yourself would be an infinite
    /// loop of re-appending your own records.
    #[test]
    fn a_node_never_fetches_partitions_it_leads() {
        let assignments = vec![
            ("orders".to_string(), 0, Some(2), vec![1, 2, 3]),
            ("orders".to_string(), 1, Some(2), vec![1, 2, 3]),
        ];

        let plan = plan_assignments(2, &assignments, &peers());

        assert!(plan.is_empty(), "node 2 leads both partitions, so it fetches nothing");
    }

    /// Replicating a partition you are not assigned would place data where the
    /// cluster does not expect it — and the retention interlock (RP-1.1) would
    /// then hold WAL segments for a replica nobody counts.
    #[test]
    fn unassigned_partitions_are_not_replicated() {
        let assignments = vec![
            ("orders".to_string(), 0, Some(1), vec![1, 2]),
            ("orders".to_string(), 1, Some(1), vec![1, 2, 3]),
        ];

        let plan = plan_assignments(3, &assignments, &peers());

        assert_eq!(plan.len(), 1);
        assert_eq!(plan[&1].len(), 1);
        assert_eq!(plan[&1][0].partition, 1, "only the partition node 3 replicates");
    }

    /// Between a topic being created and a leader being elected there is a
    /// window with no leader. Fetching from nobody is not an error state to
    /// retry loudly — it is a wait.
    #[test]
    fn partitions_without_a_leader_are_left_alone() {
        let assignments = vec![("orders".to_string(), 0, None, vec![1, 2, 3])];

        let plan = plan_assignments(3, &assignments, &peers());

        assert!(plan.is_empty());
    }

    /// A leader we have no address for cannot be fetched from. Silently
    /// dropping it would look identical to "caught up", so `plan_assignments`
    /// warns and the partition stays visibly unreplicated.
    #[test]
    fn a_leader_with_no_known_address_is_skipped() {
        let assignments = vec![("orders".to_string(), 0, Some(9), vec![1, 3, 9])];

        let plan = plan_assignments(3, &assignments, &peers());

        assert!(plan.is_empty());
    }

    /// Determinism matters: the supervisor rebuilds its tasks whenever the plan
    /// differs from the last one, so an unstable ordering would thrash the
    /// connections on every refresh.
    #[test]
    fn the_plan_is_stable_across_input_order() {
        let forward = vec![
            ("a".to_string(), 0, Some(1), vec![1, 3]),
            ("a".to_string(), 1, Some(1), vec![1, 3]),
            ("b".to_string(), 0, Some(2), vec![2, 3]),
        ];
        let mut reversed = forward.clone();
        reversed.reverse();

        assert_eq!(
            plan_assignments(3, &forward, &peers()),
            plan_assignments(3, &reversed, &peers()),
            "the same assignments must produce the same plan regardless of order"
        );
    }

    // ---- RP-3.3: the cut itself, end to end ----
    //
    // A real WAL on disk, a real socket, the broker's own API-23 codec on the
    // far end, and the fetcher's own reconciliation path driven start to finish.
    // This is what six cluster attempts were trying to establish; each failed in
    // the harness rather than the product, and none of them could assert *which*
    // records survived. These can.

    /// Minimal local state: an epoch history, a log end, and a record of the
    /// reset the fetcher asks for.
    struct FakeState {
        epochs: Arc<crate::replication::leader_epoch::LeaderEpochStore>,
        log_end: i64,
        reset_to: Arc<std::sync::Mutex<Option<(String, i32, i64)>>>,
    }

    #[async_trait::async_trait]
    impl FollowerState for FakeState {
        fn leader_epochs(&self) -> Arc<crate::replication::leader_epoch::LeaderEpochStore> {
            Arc::clone(&self.epochs)
        }
        async fn local_log_end(&self, _topic: &str, _partition: i32) -> i64 {
            self.log_end
        }
        async fn reset_after_truncation(
            &self,
            topic: &str,
            partition: i32,
            new_end: i64,
        ) -> chronik_common::Result<()> {
            *self.reset_to.lock().unwrap() = Some((topic.to_string(), partition, new_end));
            Ok(())
        }
    }

    /// A leader that answers "your epoch ended at `end_offset`", using the
    /// broker's own codec so this cannot drift from what a real leader sends.
    async fn spawn_epoch_leader(end_offset: i64) -> String {
        use chronik_protocol::offset_for_leader_epoch_types::{
            encode_response, parse_request, OffsetForLeaderEpochResponse,
            OffsetForLeaderPartitionResponse, OffsetForLeaderTopicResponse,
        };
        use chronik_protocol::parser::{parse_request_header, write_response_header, Decoder, ResponseHeader};
        use bytes::{Bytes, BytesMut};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();

        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                tokio::spawn(async move {
                    loop {
                        let mut len = [0u8; 4];
                        if socket.read_exact(&mut len).await.is_err() {
                            return;
                        }
                        let mut payload = vec![0u8; i32::from_be_bytes(len) as usize];
                        if socket.read_exact(&mut payload).await.is_err() {
                            return;
                        }

                        let mut wire = Bytes::from(payload);
                        let header = parse_request_header(&mut wire).unwrap();
                        let mut decoder = Decoder::new(&mut wire);
                        let request = parse_request(&mut decoder).unwrap();

                        let topics = request
                            .topics
                            .iter()
                            .map(|t| OffsetForLeaderTopicResponse {
                                name: t.name.clone(),
                                partitions: t
                                    .partitions
                                    .iter()
                                    .map(|p| OffsetForLeaderPartitionResponse {
                                        error_code: 0,
                                        partition: p.partition,
                                        end_offset,
                                    })
                                    .collect(),
                            })
                            .collect();

                        let mut body = BytesMut::new();
                        encode_response(&mut body, &OffsetForLeaderEpochResponse { topics });

                        let mut response = BytesMut::new();
                        write_response_header(
                            &mut response,
                            &ResponseHeader { correlation_id: header.correlation_id },
                        );
                        response.extend_from_slice(&body);

                        let mut framed = BytesMut::new();
                        framed.extend_from_slice(&(response.len() as i32).to_be_bytes());
                        framed.extend_from_slice(&response);
                        if socket.write_all(&framed).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });

        addr
    }

    /// Build a follower holding `records` single-record batches, all stamped
    /// with `epoch`, and point it at a leader on `addr`.
    async fn follower_with_log(
        dir: &std::path::Path,
        topic: &str,
        partition: i32,
        records: i64,
        epoch: i32,
        addr: &str,
    ) -> (Arc<ReplicaFetcher>, Arc<FakeState>, Arc<chronik_wal::WalManager>) {
        let mut config = chronik_wal::config::WalConfig::default();
        config.data_dir = dir.to_path_buf();
        let wal = Arc::new(chronik_wal::WalManager::new(config).await.unwrap());

        let epochs = Arc::new(crate::replication::leader_epoch::LeaderEpochStore::new());
        for offset in 0..records {
            wal.append_canonical(
                topic.to_string(),
                partition,
                format!("value-{offset}").into_bytes(),
                offset,
                offset,
                1,
            )
            .await
            .unwrap();
            epochs.observe_append(topic, partition, epoch, offset);
        }

        let state = Arc::new(FakeState {
            epochs,
            log_end: records,
            reset_to: Arc::new(std::sync::Mutex::new(None)),
        });

        let metadata: Arc<dyn MetadataStore> =
            Arc::new(chronik_common::metadata::InMemoryMetadataStore::new());
        let fetcher = ReplicaFetcher::new(
            2,
            ReplicaFetcherConfig::default(),
            metadata,
            Arc::clone(&wal),
            HashMap::from([(1u64, addr.to_string())]),
        )
        .with_follower_state(Arc::clone(&state) as Arc<dyn FollowerState>);

        (fetcher, state, wal)
    }

    async fn readable_offsets(wal: &chronik_wal::WalManager, topic: &str, partition: i32) -> Vec<i64> {
        wal.read_from(topic, partition, 0, usize::MAX)
            .await
            .unwrap()
            .iter()
            .filter_map(|r| match r {
                chronik_wal::WalRecord::V2 { base_offset, .. } => Some(*base_offset),
                _ => None,
            })
            .collect()
    }

    /// The case this whole phase exists for: our log runs past where the
    /// leader's history for our epoch ended, so the tail must go.
    #[tokio::test]
    async fn a_divergent_tail_is_cut_and_the_position_reset() {
        let dir = tempfile::tempdir().unwrap();
        let (topic, partition) = ("orders", 0);

        // The leader says epoch 0 ended at 60; we hold 100 records.
        let addr = spawn_epoch_leader(60).await;
        let (fetcher, state, wal) =
            follower_with_log(dir.path(), topic, partition, 100, 0, &addr).await;

        let mut connection = LeaderConnection::new(addr);
        let followed = vec![FollowedPartition {
            topic: topic.to_string(),
            partition,
            leader: 1,
        }];

        fetcher
            .reconcile_with_leader(&mut connection, &followed)
            .await
            .expect("reconciliation should succeed");

        let surviving = readable_offsets(&wal, topic, partition).await;
        assert_eq!(
            surviving,
            (0..60).collect::<Vec<_>>(),
            "everything at or above the divergence point must be gone, and nothing below it"
        );

        assert_eq!(
            fetcher.positions.get(&(topic.to_string(), partition)).map(|o| *o),
            Some(60),
            "the next fetch must resume exactly where the log now ends"
        );

        assert_eq!(
            *state.reset_to.lock().unwrap(),
            Some((topic.to_string(), partition, 60)),
            "the watermark must be walked back too, or this node advertises records it discarded"
        );

        assert_eq!(
            state.epochs.snapshot(topic, partition).len(),
            1,
            "epoch history must survive a cut that did not remove its only entry"
        );
    }

    /// The common case — caught up, or behind. Nothing may be touched.
    #[tokio::test]
    async fn a_log_that_is_a_prefix_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let (topic, partition) = ("orders", 0);

        // The leader's epoch ran to 500; we only hold 100.
        let addr = spawn_epoch_leader(500).await;
        let (fetcher, state, wal) =
            follower_with_log(dir.path(), topic, partition, 100, 0, &addr).await;

        let mut connection = LeaderConnection::new(addr);
        let followed = vec![FollowedPartition {
            topic: topic.to_string(),
            partition,
            leader: 1,
        }];

        fetcher
            .reconcile_with_leader(&mut connection, &followed)
            .await
            .unwrap();

        assert_eq!(
            readable_offsets(&wal, topic, partition).await,
            (0..100).collect::<Vec<_>>(),
            "a follower that is merely behind must not lose a single record"
        );
        assert_eq!(*state.reset_to.lock().unwrap(), None, "nothing to reset");
    }

    /// `-1` means "I cannot answer", not offset -1 and not "delete everything".
    /// Acting on it would destroy a correct log through the machinery built to
    /// protect it.
    #[tokio::test]
    async fn an_unanswerable_epoch_leaves_the_log_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let (topic, partition) = ("orders", 0);

        let addr = spawn_epoch_leader(-1).await;
        let (fetcher, state, wal) =
            follower_with_log(dir.path(), topic, partition, 100, 0, &addr).await;

        let mut connection = LeaderConnection::new(addr);
        let followed = vec![FollowedPartition {
            topic: topic.to_string(),
            partition,
            leader: 1,
        }];

        fetcher
            .reconcile_with_leader(&mut connection, &followed)
            .await
            .unwrap();

        assert_eq!(
            readable_offsets(&wal, topic, partition).await,
            (0..100).collect::<Vec<_>>(),
            "an abstaining leader must not cost us the log"
        );
        assert_eq!(*state.reset_to.lock().unwrap(), None);
    }

    /// A follower with no epoch history cannot reconcile, and must not guess.
    /// It also must not send a query it has nothing to ask about.
    #[tokio::test]
    async fn a_log_without_epoch_history_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let (topic, partition) = ("orders", 0);

        // Records stamped -1: pre-RP-3 data, so `observe_append` records nothing.
        let addr = spawn_epoch_leader(10).await;
        let (fetcher, state, wal) =
            follower_with_log(dir.path(), topic, partition, 100, -1, &addr).await;

        let mut connection = LeaderConnection::new(addr);
        let followed = vec![FollowedPartition {
            topic: topic.to_string(),
            partition,
            leader: 1,
        }];

        fetcher
            .reconcile_with_leader(&mut connection, &followed)
            .await
            .unwrap();

        assert_eq!(
            readable_offsets(&wal, topic, partition).await,
            (0..100).collect::<Vec<_>>(),
            "an unstamped log must not be truncated on a guess"
        );
        assert_eq!(*state.reset_to.lock().unwrap(), None);
    }

    // ---- RP-3.3: deciding whether to discard a divergent tail ----

    /// The case truncation exists for: the leader's history for our epoch ends
    /// before our log does, so we hold records it never committed.
    #[test]
    fn a_log_that_runs_past_the_leaders_epoch_is_truncated() {
        assert_eq!(
            plan_reconciliation(0, 100, 150),
            ReconcileAction::Truncate { to: 100 }
        );
    }

    /// The common case once a follower is caught up, and the case on every
    /// ordinary restart: our log is a prefix, so nothing is discarded.
    #[test]
    fn a_log_that_is_a_prefix_of_the_leaders_is_left_alone() {
        assert_eq!(plan_reconciliation(0, 150, 100), ReconcileAction::Resume);
        assert_eq!(
            plan_reconciliation(0, 100, 100),
            ReconcileAction::Resume,
            "equal is a prefix, not a divergence"
        );
    }

    /// `-1` means "I cannot answer", not offset -1 and not offset 0. Treating it
    /// as an offset would truncate a correct log to nothing — the exact data
    /// loss this machinery exists to prevent, reached through the machinery.
    #[test]
    fn an_unanswerable_epoch_never_truncates() {
        assert!(matches!(
            plan_reconciliation(0, -1, 500),
            ReconcileAction::Abstain { .. }
        ));
    }

    /// An error is not a truncation instruction either.
    #[test]
    fn an_error_never_truncates() {
        for error_code in [1i16, 6, 74, 75] {
            assert!(
                matches!(
                    plan_reconciliation(error_code, 0, 500),
                    ReconcileAction::Abstain { .. }
                ),
                "error {error_code} must not be read as 'truncate to 0'"
            );
        }
    }

    /// An empty local log has nothing to discard whatever the leader says.
    #[test]
    fn an_empty_log_is_never_truncated() {
        assert_eq!(plan_reconciliation(0, 0, 0), ReconcileAction::Resume);
        assert_eq!(plan_reconciliation(0, 90, 0), ReconcileAction::Resume);
    }

    /// Truncating to 0 is legitimate when the leader committed nothing in our
    /// epoch and we hold records — it must not be confused with the error cases
    /// above, which also carry a zero.
    #[test]
    fn a_genuine_zero_still_truncates() {
        assert_eq!(
            plan_reconciliation(0, 0, 40),
            ReconcileAction::Truncate { to: 0 }
        );
    }

    /// The property: the plan never discards anything the leader confirmed.
    #[test]
    fn truncation_never_cuts_below_what_the_leader_confirmed() {
        for end_offset in -1i64..40 {
            for local_end in 0i64..40 {
                if let ReconcileAction::Truncate { to } =
                    plan_reconciliation(0, end_offset, local_end)
                {
                    assert_eq!(to, end_offset, "the cut must be exactly where the epoch ended");
                    assert!(to >= 0, "a negative answer is not an offset");
                    assert!(to < local_end, "truncating at or above our end is a no-op, not a cut");
                }
            }
        }
    }
}
