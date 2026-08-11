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

/// Which replication mechanism moves partition data.
///
/// These are mutually exclusive by construction: running both would deliver
/// every record twice and the follower would refuse the duplicates as gaps.
/// There is no coexistence mode, and there is no external consumer of the
/// replication protocol, so switching is an image redeploy rather than a
/// runtime toggle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplicationMode {
    /// Leader pushes to followers over the WAL replication port (the mechanism
    /// RP-1 hardened). Default until pull is soaked.
    Push,
    /// Followers fetch from their leader over the Kafka port (RP-2.4).
    Pull,
}

impl ReplicationMode {
    /// Read `CHRONIK_REPLICATION_MODE`. Anything unrecognised falls back to
    /// push with a warning — an unfamiliar value must not silently disable
    /// replication.
    pub fn from_env() -> Self {
        match std::env::var("CHRONIK_REPLICATION_MODE") {
            Ok(v) => Self::parse(&v),
            Err(_) => ReplicationMode::Push,
        }
    }

    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "pull" | "fetch" | "follower-pull" => ReplicationMode::Pull,
            "push" | "" => ReplicationMode::Push,
            other => {
                warn!(
                    "CHRONIK_REPLICATION_MODE='{}' is not recognised; using push. Valid values: push, pull",
                    other
                );
                ReplicationMode::Push
            }
        }
    }

    pub fn is_pull(&self) -> bool {
        matches!(self, ReplicationMode::Pull)
    }
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
        config
    }
}

fn env_i32(key: &str) -> Option<i32> {
    std::env::var(key).ok().and_then(|v| v.trim().parse().ok())
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
    produce_handler: Option<Arc<crate::produce_handler::ProduceHandler>>,
    /// node id → Kafka address, from the cluster config.
    peers: HashMap<u64, String>,
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
            peers,
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
            inner.produce_handler = Some(handler);
        } else {
            warn!("ReplicaFetcher already shared; produce handler not attached");
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

        let mut running: HashMap<u64, tokio::task::JoinHandle<()>> = HashMap::new();
        let mut current: BTreeMap<u64, Vec<FollowedPartition>> = BTreeMap::new();

        while !self.shutdown.load(Ordering::Relaxed) {
            let desired = match self.read_assignments().await {
                Ok(assignments) => plan_assignments(self.node_id, &assignments, &self.peers),
                Err(e) => {
                    warn!("Could not read partition assignments: {}", e);
                    sleep(self.config.refresh_interval).await;
                    continue;
                }
            };

            if desired != current {
                // Assignments changed: stop every task and rebuild. A follower
                // has at most a handful of leaders, so a full rebuild costs one
                // reconnect and avoids the state machine a diff would need.
                for (leader, handle) in running.drain() {
                    debug!("Stopping fetch task for leader {}", leader);
                    handle.abort();
                }

                for (leader, partitions) in &desired {
                    let addr = match self.peers.get(leader) {
                        Some(addr) => addr.clone(),
                        None => continue,
                    };
                    info!(
                        "Replicating {} partition(s) from leader {} at {}",
                        partitions.len(),
                        leader,
                        addr
                    );
                    let this = Arc::clone(&self);
                    let partitions = partitions.clone();
                    let leader = *leader;
                    running.insert(
                        leader,
                        tokio::spawn(async move {
                            this.run_leader_loop(leader, addr, partitions).await;
                        }),
                    );
                }

                current = desired;
            }

            sleep(self.config.refresh_interval).await;
        }

        for (_, handle) in running.drain() {
            handle.abort();
        }
        info!("Follower-pull replication stopped on node {}", self.node_id);
    }

    /// Read every partition's leader and replica set from metadata.
    async fn read_assignments(&self) -> chronik_common::Result<Vec<(String, i32, Option<u64>, Vec<u64>)>> {
        let topics = self.metadata_store.list_topics().await?;
        let mut out = Vec::new();

        for topic in topics {
            for partition in 0..topic.config.partition_count {
                let partition = partition as i32;
                let leader = self
                    .metadata_store
                    .get_partition_leader(&topic.name, partition as u32)
                    .await
                    .ok()
                    .flatten()
                    .map(|id| id as u64);
                let replicas = self
                    .metadata_store
                    .get_partition_replicas(&topic.name, partition as u32)
                    .await
                    .ok()
                    .flatten()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|id| id as u64)
                    .collect();

                out.push((topic.name.clone(), partition, leader, replicas));
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

        while !self.shutdown.load(Ordering::Relaxed) {
            let spec = match self.build_request(&partitions).await {
                Some(spec) => spec,
                None => {
                    sleep(self.config.retry_backoff).await;
                    continue;
                }
            };

            match connection.fetch(spec).await {
                Ok(response) => {
                    for topic in response.topics {
                        for partition in topic.partitions {
                            self.handle_partition_response(&topic.name, partition).await;
                        }
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
                    // RP-3 will populate this so a stale leader can fence us.
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

    /// The offset this follower's log next accepts.
    ///
    /// Cached after the first read, then advanced by the apply path. The first
    /// read comes from the local watermark, which WAL recovery restores on
    /// startup — that is what makes catch-up after a restart free rather than a
    /// mechanism of its own.
    async fn position_of(&self, topic: &str, partition: i32) -> i64 {
        let key = (topic.to_string(), partition);
        if let Some(offset) = self.positions.get(&key) {
            return *offset;
        }

        let offset = match &self.produce_handler {
            Some(handler) => handler
                .get_high_watermark(topic, partition)
                .await
                .unwrap_or(0)
                .max(0),
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
    ) {
        let index = partition.partition;

        if partition.error_code != 0 {
            self.handle_partition_error(topic, index, partition.error_code)
                .await;
            return;
        }

        if partition.records.is_empty() {
            return; // caught up
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
            }
            Err(refusal @ ApplyRefusal::Straddle { .. }) => {
                // Divergence. Blindly appending would interleave two histories;
                // resolving it needs the epoch history RP-3 adds. Stop this
                // partition rather than corrupt it.
                error!(
                    "{}-{}: replication halted — {}. This needs leader-epoch truncation (RP-3).",
                    topic, index, refusal
                );
                self.fetch_errors.fetch_add(1, Ordering::Relaxed);
            }
            Err(refusal) => {
                self.fetch_errors.fetch_add(1, Ordering::Relaxed);
                warn!("{}-{}: could not apply fetched records — {}", topic, index, refusal);
            }
        }
    }

    async fn handle_partition_error(&self, topic: &str, partition: i32, error_code: i16) {
        match error_code {
            // The leader moved. The supervisor re-reads assignments on its own
            // schedule and will repoint or stop this task.
            5 | 6 => debug!(
                "{}-{}: leader has moved (error {}), waiting for the assignment refresh",
                topic, partition, error_code
            ),
            // Our fetch offset is not in the leader's log at all.
            1 => warn!(
                "{}-{}: offset out of range on the leader; will resync on the next assignment refresh",
                topic, partition
            ),
            3 => debug!("{}-{}: leader does not know this partition yet", topic, partition),
            other => warn!("{}-{}: leader returned error {}", topic, partition, other),
        }
    }
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

    #[test]
    fn replication_mode_defaults_to_push() {
        assert_eq!(ReplicationMode::parse("push"), ReplicationMode::Push);
        assert_eq!(ReplicationMode::parse(""), ReplicationMode::Push);
        assert_eq!(ReplicationMode::parse("pull"), ReplicationMode::Pull);
        assert_eq!(ReplicationMode::parse("  PULL  "), ReplicationMode::Pull);
    }

    /// A typo must not silently disable replication — that is the failure this
    /// whole roadmap exists because of.
    #[test]
    fn an_unknown_replication_mode_falls_back_to_push() {
        assert_eq!(ReplicationMode::parse("pulll"), ReplicationMode::Push);
        assert_eq!(ReplicationMode::parse("off"), ReplicationMode::Push);
    }
}
