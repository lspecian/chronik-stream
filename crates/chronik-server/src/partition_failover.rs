//! Partition leader failover (RP-5).
//!
//! When the node leading a partition dies, something has to give that partition
//! to a replica that is still alive. Until this module existed, nothing did:
//! `LeaderElector::elect_leader_from_isr` ignored ISR (its own comment said so),
//! returned `replicas[0]` — the incumbent, by construction — never checked
//! whether that node was alive, and never persisted the result, because the
//! Raft proposal was commented out in favour of "let the system self-heal via
//! produce requests". It logged `✅ Elected new leader` either way.
//!
//! Measured consequence, on a 3-node cluster with the leader's node cordoned for
//! 150 seconds: leadership never moved, the dead node stayed in ISR, the
//! partition reported `under_replicated: false`, and `acks=all` produce failed
//! outright. Reproduced identically under push, so this was never a
//! pull-replication regression — it is why "automatic leader election" and
//! "survives a minority failure" were not true of partition leadership.
//!
//! # Why liveness has to come from Raft
//!
//! The data path cannot supply it. Followers report their position to their
//! leader, so a *leader's* death is precisely the case with nobody left to
//! observe it — which is also why ISR never shrank in the run above. Raft
//! heartbeats run between all members irrespective of who leads which
//! partition, so the Raft leader always knows who is reachable. This is the
//! same reason Kafka's controller tracks broker liveness through its own
//! heartbeats rather than through replication traffic.
//!
//! # Why only the Raft leader acts
//!
//! Two nodes electing independently would hand the same partition to two
//! different leaders, which is the divergence RP-3.3 exists to clean up after.
//! Causing it here to fix an availability bug would be a poor trade.

use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chronik_common::metadata::traits::MetadataStore;
use chronik_common::metadata::PartitionAssignment;
use dashmap::DashMap;
use tracing::{debug, error, info, warn};

use crate::raft_cluster::RaftCluster;

/// How often peer liveness is sampled and assignments are checked.
const DEFAULT_TICK: Duration = Duration::from_secs(2);

/// How long a node may go unheard-from before it is treated as dead.
///
/// Must be comfortably larger than the tick and than Raft's election timeout,
/// because `recent_active` is cleared once per election-timeout cycle: a live
/// peer reads inactive for part of every cycle, and a window this side of that
/// would declare healthy nodes dead on a timer. Kafka's equivalent
/// (`replica.lag.time.max.ms`, and the broker session timeout) sits in the same
/// range for the same reason.
const DEFAULT_LIVENESS_WINDOW: Duration = Duration::from_secs(30);

/// One partition that must change leader, and to whom.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failover {
    pub topic: String,
    pub partition: u32,
    /// The leader that is no longer reachable.
    pub from: u64,
    /// The replica taking over. Always live, always a replica, never `from`.
    pub to: u64,
    /// The partition's replica set, **unchanged**.
    ///
    /// Failover moves leadership; it is not a reassignment. Dropping the dead
    /// node here would shrink the replica set permanently — the partition would
    /// come back from a transient failure at RF=2, then RF=1 after the next one,
    /// silently eroding the durability the cluster was configured for. It also
    /// leaves the returning node no longer a replica, so it never resumes
    /// replicating and never runs the RP-3.3 truncation handshake.
    ///
    /// What legitimately shrinks on failure is ISR, and `IsrTracker` already
    /// does that from its own liveness (RP-2.1).
    pub replicas: Vec<u64>,
    /// Replicas currently alive. Reported, not written — diagnostics only.
    pub live_replicas: Vec<u64>,
}

/// A partition whose leader is gone and which has no live replica to move to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stranded {
    pub topic: String,
    pub partition: u32,
    pub leader: u64,
}

/// The outcome of one planning pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FailoverPlan {
    pub failovers: Vec<Failover>,
    /// Partitions that cannot be rescued. Reported, never silently skipped:
    /// this is unavailable data, and it is exactly what an operator needs told.
    pub stranded: Vec<Stranded>,
}

/// Decide which partitions change leader, given who is alive.
///
/// Pure, because this decides where writes go. The rules:
///
/// - a partition whose leader is alive is left alone — this must be a no-op in
///   the steady state, or it would rewrite assignments continuously and bump a
///   leader epoch every pass, which would have every follower truncating on a
///   timer;
/// - internal (`__`) topics are skipped: they are Raft's own, and are led by
///   the Raft leader rather than by partition assignment;
/// - the new leader is the first *live* replica in the assignment's replica
///   order, which keeps the choice deterministic and preferring the
///   conventional preferred-replica ordering;
/// - a partition with no live replica is reported stranded, not "fixed" by
///   handing it to a dead node, which is precisely the bug this replaces.
pub fn plan_failover(
    assignments: &[PartitionAssignment],
    live: &HashSet<u64>,
) -> FailoverPlan {
    let mut plan = FailoverPlan::default();

    for assignment in assignments {
        if assignment.topic.starts_with("__") {
            continue;
        }
        if live.contains(&assignment.leader_id) {
            continue;
        }

        let live_replicas: Vec<u64> = assignment
            .replicas
            .iter()
            .copied()
            .filter(|id| live.contains(id))
            .collect();

        match live_replicas.first().copied() {
            Some(to) => plan.failovers.push(Failover {
                topic: assignment.topic.clone(),
                partition: assignment.partition,
                from: assignment.leader_id,
                to,
                replicas: assignment.replicas.clone(),
                live_replicas,
            }),
            None => plan.stranded.push(Stranded {
                topic: assignment.topic.clone(),
                partition: assignment.partition,
                leader: assignment.leader_id,
            }),
        }
    }

    plan
}

/// Tracks which nodes have been heard from, and moves partitions off the ones
/// that have not.
pub struct PartitionFailoverController {
    node_id: u64,
    raft: Arc<RaftCluster>,
    metadata_store: Arc<dyn MetadataStore>,
    /// Last moment each node was observed active by Raft.
    last_active: DashMap<u64, Instant>,
    /// When this node last became the Raft leader, or `None` if it is not.
    /// Failover is suppressed until a full liveness window has passed since
    /// then — see `grace_expired`.
    leader_since: parking_lot::Mutex<Option<Instant>>,
    /// Last live set logged, so the line above prints on change rather than
    /// every tick.
    last_reported_live: parking_lot::Mutex<Option<Vec<u64>>>,
    tick: Duration,
    liveness_window: Duration,
    shutdown: Arc<AtomicBool>,
    failovers_performed: Arc<AtomicU64>,
}

impl PartitionFailoverController {
    pub fn new(
        node_id: u64,
        raft: Arc<RaftCluster>,
        metadata_store: Arc<dyn MetadataStore>,
    ) -> Arc<Self> {
        Arc::new(Self {
            node_id,
            raft,
            metadata_store,
            last_active: DashMap::new(),
            leader_since: parking_lot::Mutex::new(None),
            last_reported_live: parking_lot::Mutex::new(None),
            tick: env_secs("CHRONIK_FAILOVER_TICK_SECS").unwrap_or(DEFAULT_TICK),
            liveness_window: env_secs("CHRONIK_FAILOVER_LIVENESS_SECS")
                .unwrap_or(DEFAULT_LIVENESS_WINDOW),
            shutdown: Arc::new(AtomicBool::new(false)),
            failovers_performed: Arc::new(AtomicU64::new(0)),
        })
    }

    pub fn failovers_performed(&self) -> u64 {
        self.failovers_performed.load(Ordering::Relaxed)
    }

    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }

    pub fn start(self: &Arc<Self>) {
        let this = Arc::clone(self);
        tokio::spawn(async move { this.run().await });
    }

    async fn run(self: Arc<Self>) {
        info!(
            "Partition failover controller started on node {} (tick {:?}, liveness window {:?})",
            self.node_id, self.tick, self.liveness_window
        );

        while !self.shutdown.load(Ordering::Relaxed) {
            tokio::time::sleep(self.tick).await;

            let active = match self.raft.sample_active_peers().await {
                Some(active) => active,
                None => {
                    // Not the Raft leader. Drop the samples: they age while we
                    // are not watching, and acting on stale ones the moment we
                    // are elected would fail partitions over on evidence
                    // gathered by nobody.
                    self.forget_liveness();
                    continue;
                }
            };

            let now = Instant::now();
            for node in active {
                self.last_active.insert(node, now);
            }

            if !self.grace_expired(now) {
                continue;
            }

            self.report_live_set(now);

            if let Err(e) = self.reconcile(now).await {
                warn!("Partition failover pass failed: {}", e);
            }
        }

        info!("Partition failover controller stopped on node {}", self.node_id);
    }

    /// Log the live set whenever it changes.
    ///
    /// The first build of this controller shipped as a silent no-op on a real
    /// cluster: the planner was correct and its liveness input was a constant,
    /// so nothing was ever planned and nothing was ever logged. "Who does this
    /// think is alive" must be answerable from a log rather than a rebuild.
    fn report_live_set(&self, now: Instant) {
        let mut sorted: Vec<u64> = self.live_nodes(now).into_iter().collect();
        sorted.sort_unstable();

        let mut last = self.last_reported_live.lock();
        if last.as_deref() != Some(sorted.as_slice()) {
            info!("Partition failover: live nodes {:?}", sorted);
            *last = Some(sorted);
        }
    }

    /// Forget everything observed while we were Raft leader.
    fn forget_liveness(&self) {
        if self.leader_since.lock().take().is_some() {
            self.last_active.clear();
            debug!("No longer Raft leader; discarding partition liveness samples");
        }
    }

    /// Whether we have been Raft leader long enough for silence to mean death.
    ///
    /// A newly elected leader has heard from nobody yet. Without this, its first
    /// pass would see every node as unheard-from and fail every partition in the
    /// cluster over at once — turning a leader election into a full reassignment
    /// storm, which is a far worse failure than the one being fixed.
    fn grace_expired(&self, now: Instant) -> bool {
        let mut since = self.leader_since.lock();
        let started = *since.get_or_insert(now);
        drop(since);

        if now.duration_since(started) < self.liveness_window {
            debug!("Failover suppressed: newly elected, still learning who is alive");
            return false;
        }
        true
    }

    /// Nodes heard from within the liveness window.
    fn live_nodes(&self, now: Instant) -> HashSet<u64> {
        self.last_active
            .iter()
            .filter(|entry| now.duration_since(*entry.value()) <= self.liveness_window)
            .map(|entry| *entry.key())
            .collect()
    }

    async fn reconcile(&self, now: Instant) -> chronik_common::Result<()> {
        let live = self.live_nodes(now);
        if live.is_empty() {
            // We are the Raft leader, so we are alive by definition; an empty
            // set means the sampler is broken, not that the cluster is gone.
            warn!("Partition failover: no node looks alive, including this one — not acting");
            return Ok(());
        }

        let assignments = self.all_assignments().await?;
        let plan = plan_failover(&assignments, &live);

        for stranded in &plan.stranded {
            warn!(
                "{}-{}: leader {} is unreachable and no replica is alive. This partition is \
                 unavailable until a replica returns — it cannot be failed over.",
                stranded.topic, stranded.partition, stranded.leader
            );
        }

        for failover in &plan.failovers {
            self.apply(failover).await;
        }

        Ok(())
    }

    /// Move one partition to its new leader.
    ///
    /// Persisted through `assign_partition`, which is what makes this real: it
    /// derives the leader epoch, so a genuine leader change bumps it exactly
    /// once. That bump is what tells followers to re-run the RP-3.3 epoch
    /// handshake and truncate anything the old leader never committed.
    async fn apply(&self, failover: &Failover) {
        let assignment = PartitionAssignment {
            topic: failover.topic.clone(),
            partition: failover.partition,
            broker_id: failover.to as i32,
            is_leader: true,
            // Unchanged — see `Failover::replicas`.
            replicas: failover.replicas.clone(),
            leader_id: failover.to,
            // Derived by the store, never by callers.
            leader_epoch: 0,
        };

        match self.metadata_store.assign_partition(assignment).await {
            Ok(()) => {
                self.failovers_performed.fetch_add(1, Ordering::Relaxed);
                info!(
                    "{}-{}: leader {} is unreachable — failed over to {} (live replicas {:?})",
                    failover.topic,
                    failover.partition,
                    failover.from,
                    failover.to,
                    failover.live_replicas
                );
            }
            Err(e) => error!(
                "{}-{}: could not fail over from unreachable leader {} to {}: {}",
                failover.topic, failover.partition, failover.from, failover.to, e
            ),
        }
    }

    /// Every partition assignment in the cluster.
    ///
    /// Read through `get_partition_assignments`, the same call `/admin/status`
    /// and the replica fetcher use. RP-2 learned this the hard way: the
    /// per-partition leader lookup diverges from the assignments after a
    /// restart, and a failover controller reading the divergent view would move
    /// leadership for partitions that never lost it.
    async fn all_assignments(&self) -> chronik_common::Result<Vec<PartitionAssignment>> {
        let topics = self.metadata_store.list_topics().await?;
        let mut out = Vec::new();
        let mut by_topic: BTreeMap<String, ()> = BTreeMap::new();

        for topic in topics {
            if by_topic.insert(topic.name.clone(), ()).is_some() {
                continue;
            }
            match self.metadata_store.get_partition_assignments(&topic.name).await {
                Ok(assignments) => out.extend(assignments),
                Err(e) => warn!("Failover: cannot read assignments for {}: {}", topic.name, e),
            }
        }

        Ok(out)
    }
}

fn env_secs(key: &str) -> Option<Duration> {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
        .map(Duration::from_secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assignment(topic: &str, partition: u32, leader: u64, replicas: &[u64]) -> PartitionAssignment {
        PartitionAssignment {
            topic: topic.to_string(),
            partition,
            broker_id: leader as i32,
            is_leader: true,
            replicas: replicas.to_vec(),
            leader_id: leader,
            leader_epoch: 0,
        }
    }

    fn live(ids: &[u64]) -> HashSet<u64> {
        ids.iter().copied().collect()
    }

    /// The steady state, and the most important case: a healthy cluster must
    /// plan nothing. Every failover bumps a leader epoch, and an epoch bump
    /// sends every follower into the truncation handshake — so a controller
    /// that churned here would have the cluster permanently reconciling.
    #[test]
    fn a_healthy_cluster_changes_nothing() {
        let assignments = vec![
            assignment("orders", 0, 1, &[1, 2, 3]),
            assignment("orders", 1, 2, &[2, 3, 1]),
            assignment("events", 0, 3, &[3, 1, 2]),
        ];

        assert_eq!(
            plan_failover(&assignments, &live(&[1, 2, 3])),
            FailoverPlan::default()
        );
    }

    /// The bug this module exists for: the old code returned `replicas[0]`,
    /// which is the incumbent, so a dead leader was re-elected forever.
    #[test]
    fn a_dead_leader_is_replaced_and_never_re_elected() {
        let assignments = vec![assignment("orders", 0, 1, &[1, 2, 3])];

        let plan = plan_failover(&assignments, &live(&[2, 3]));

        assert_eq!(plan.failovers.len(), 1);
        let f = &plan.failovers[0];
        assert_eq!(f.from, 1);
        assert_eq!(f.to, 2, "the first live replica takes over");
        assert_ne!(f.to, f.from, "the dead leader must never be re-elected");
        assert!(plan.stranded.is_empty());
    }

    /// The replica set survives a failover, so the node that died is still a
    /// replica when it returns.
    ///
    /// Caught on a real cluster: the first version wrote the *live* replicas
    /// back as the replica set, so a partition came back from a transient
    /// failure at RF=2 and the returning node was no longer a replica at all —
    /// it never resumed replicating, and so never ran the RP-3.3 handshake.
    /// Repeat the failure and RF reaches 1 without anything reporting it.
    #[test]
    fn the_replica_set_survives_so_the_dead_node_can_rejoin() {
        let assignments = vec![assignment("orders", 0, 3, &[3, 1, 2])];

        let plan = plan_failover(&assignments, &live(&[1, 2]));

        assert_eq!(
            plan.failovers[0].replicas,
            vec![3, 1, 2],
            "failover moves leadership; it is not a reassignment"
        );
        assert!(
            plan.failovers[0].replicas.contains(&3),
            "the node that died must still be a replica when it comes back"
        );
        // Liveness is still reported, just not written as the replica set.
        assert_eq!(plan.failovers[0].live_replicas, vec![1, 2]);
    }

    /// Replica order is the preferred-leader order, so the choice must follow it
    /// rather than whatever a set iterator happens to yield — otherwise two
    /// passes over the same input could pick different leaders.
    #[test]
    fn the_new_leader_is_deterministic_and_follows_replica_order() {
        let assignments = vec![assignment("orders", 0, 9, &[9, 3, 1, 2])];

        for _ in 0..20 {
            let plan = plan_failover(&assignments, &live(&[1, 2, 3]));
            assert_eq!(plan.failovers[0].to, 3, "first live replica in replica order");
        }
    }

    /// A partition with nothing alive is unavailable. Saying so is the point:
    /// the alternative is handing it to a dead node and reporting success,
    /// which is what the code being replaced did.
    #[test]
    fn a_partition_with_no_live_replica_is_reported_not_papered_over() {
        let assignments = vec![assignment("orders", 0, 1, &[1, 2])];

        let plan = plan_failover(&assignments, &live(&[3]));

        assert!(plan.failovers.is_empty(), "there is nowhere to fail over to");
        assert_eq!(
            plan.stranded,
            vec![Stranded {
                topic: "orders".to_string(),
                partition: 0,
                leader: 1
            }]
        );
    }

    /// Internal topics are Raft's own; their leadership follows Raft leadership,
    /// not partition assignment. Failing them over would fight the consensus
    /// layer for control of the thing that stores the assignments.
    #[test]
    fn internal_topics_are_left_to_raft() {
        let assignments = vec![
            assignment("__chronik_metadata", 0, 1, &[1, 2, 3]),
            assignment("__raft_metadata", 0, 1, &[1, 2, 3]),
        ];

        assert_eq!(plan_failover(&assignments, &live(&[2, 3])), FailoverPlan::default());
    }

    /// Only the partitions that lost their leader move.
    #[test]
    fn only_partitions_led_by_the_dead_node_are_touched() {
        let assignments = vec![
            assignment("orders", 0, 1, &[1, 2, 3]),
            assignment("orders", 1, 2, &[2, 3, 1]),
            assignment("orders", 2, 3, &[3, 1, 2]),
        ];

        let plan = plan_failover(&assignments, &live(&[2, 3]));

        assert_eq!(plan.failovers.len(), 1);
        assert_eq!(plan.failovers[0].topic, "orders");
        assert_eq!(plan.failovers[0].partition, 0);
    }

    /// Losing two of three nodes moves what it can and strands what it cannot,
    /// in the same pass. Neither outcome may mask the other.
    #[test]
    fn a_double_failure_moves_what_it_can_and_strands_the_rest() {
        let assignments = vec![
            assignment("a", 0, 1, &[1, 2, 3]), // 3 survives → failover
            assignment("b", 0, 1, &[1, 2]),    // nothing survives → stranded
        ];

        let plan = plan_failover(&assignments, &live(&[3]));

        assert_eq!(plan.failovers.len(), 1);
        assert_eq!(plan.failovers[0].to, 3);
        assert_eq!(plan.stranded.len(), 1);
        assert_eq!(plan.stranded[0].topic, "b");
    }

    /// Applying a plan must reach a fixed point: after the failover is written
    /// back, a second pass over the new state plans nothing. Otherwise the
    /// controller would bump the epoch every tick.
    #[test]
    fn planning_converges_after_one_pass() {
        let mut assignments = vec![assignment("orders", 0, 1, &[1, 2, 3])];
        let alive = live(&[2, 3]);

        let first = plan_failover(&assignments, &alive);
        assert_eq!(first.failovers.len(), 1);

        // Write the plan back the way the controller does.
        assignments[0].leader_id = first.failovers[0].to;
        assignments[0].replicas = first.failovers[0].replicas.clone();

        assert_eq!(
            plan_failover(&assignments, &alive),
            FailoverPlan::default(),
            "a second pass over the applied state must be a no-op"
        );
        assert_eq!(
            assignments[0].replicas,
            vec![1, 2, 3],
            "and the replica set is still RF=3"
        );
    }

    /// Repeated failures must not erode the replica set. This is the property
    /// that the first cluster run violated silently.
    #[test]
    fn repeated_failovers_do_not_erode_the_replica_set() {
        let mut assignments = vec![assignment("orders", 0, 1, &[1, 2, 3])];

        for (dead, alive_now) in [(1u64, vec![2u64, 3]), (2, vec![3])] {
            let plan = plan_failover(&assignments, &live(&alive_now));
            assert_eq!(plan.failovers.len(), 1, "leader {dead} should have moved");
            assignments[0].leader_id = plan.failovers[0].to;
            assignments[0].replicas = plan.failovers[0].replicas.clone();
        }

        assert_eq!(assignments[0].leader_id, 3);
        assert_eq!(
            assignments[0].replicas,
            vec![1, 2, 3],
            "two failures in a row must still leave RF=3"
        );
    }

    /// A replica that is not in the live set cannot be chosen even if it is
    /// listed first — the ordering preference must never override liveness.
    #[test]
    fn liveness_beats_preference() {
        let assignments = vec![assignment("orders", 0, 5, &[5, 4, 2])];

        let plan = plan_failover(&assignments, &live(&[2]));

        assert_eq!(plan.failovers[0].to, 2);
    }

    /// An empty live set means we know nothing, not that everything is dead.
    /// The controller guards this before calling, but the planner must not
    /// produce a cluster-wide reassignment if that guard is ever removed.
    #[test]
    fn knowing_nothing_strands_rather_than_reassigns() {
        let assignments = vec![assignment("orders", 0, 1, &[1, 2, 3])];

        let plan = plan_failover(&assignments, &live(&[]));

        assert!(plan.failovers.is_empty());
        assert_eq!(plan.stranded.len(), 1);
    }
}
