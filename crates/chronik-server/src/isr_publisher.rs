//! Publish each partition's in-sync replica set into metadata.
//!
//! # Why this exists
//!
//! The in-sync set is measured from follower fetch positions, which only the
//! partition's leader sees. Keeping it solely in that leader's memory means it
//! disappears at exactly the moment it is needed: when that node dies and
//! someone else has to decide which replica may take over.
//!
//! Without it, failover chose on liveness alone. Measured on a three-node
//! cluster: a replica that had never replicated a single record of a partition
//! was elected its leader, started writing at offset 0, and destroyed 100
//! records that had been acknowledged at `acks=all` — acknowledged, by
//! definition, only because the in-sync set held them.
//!
//! So the leader republishes the set whenever it changes, and failover reads it
//! from the assignment rather than from a node that may no longer exist.
//!
//! # Why it does not disturb leadership
//!
//! An update re-asserts the assignment with the **same** `leader_id`, and
//! `assign_partition` bumps the leader epoch if and only if the leader changed.
//! An ISR update therefore cannot masquerade as a leadership change and cannot
//! set followers truncating.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chronik_common::metadata::catalog::IsrUpdate;
use chronik_common::metadata::traits::MetadataStore;
use tracing::{debug, info, warn};

use crate::isr_tracker::IsrTracker;

/// How often the set is re-measured. Cheap: in-memory reads, and a metadata
/// write only when the answer changed.
const DEFAULT_INTERVAL: Duration = Duration::from_secs(5);

/// The in-sync replica set for one partition, as its leader sees it.
///
/// The single definition of "in sync" in this codebase. `/admin/status` reports
/// what this returns and failover elects from what this returns; if the two ever
/// disagreed, an operator would be reading one set while the cluster acted on
/// another.
///
/// Returns `None` when the tracker has heard nothing at all for this partition,
/// which is not the same as "nobody is in sync": a freshly started cluster, or a
/// partition that has never been written to, has no positions to judge. Callers
/// treat `None` as "unknown" and fall back to the full replica set — the
/// pre-existing behaviour, kept because refusing to serve a brand-new partition
/// would be worse than the exposure it leaves.
///
/// An empty ISR that is *known* — every follower measurably behind — is
/// returned as `Some(vec![leader])`, because the leader is in sync with itself
/// by definition and never reports to itself.
pub fn in_sync_replicas(
    tracker: &IsrTracker,
    topic: &str,
    partition: i32,
    leader_offset: i64,
    replicas: &[u64],
    leader_id: u64,
) -> Option<Vec<u64>> {
    if tracker.is_unknown_for_all(topic, partition, replicas, leader_id) {
        return None;
    }

    // "Caught up" means reaching the leader's LOG END. Callers hold a high
    // watermark, which trails the in-sync followers by definition, so a stuck
    // follower measured against it can look caught up; the tracker has seen
    // the log end on every follower fetch, and the larger of the two wins.
    let leader_offset = tracker
        .leader_end_seen(topic, partition)
        .map_or(leader_offset, |end| end.max(leader_offset));

    let mut isr = tracker.get_isr(topic, partition, leader_offset, replicas);
    if !isr.contains(&leader_id) {
        isr.insert(0, leader_id);
    }

    // Report in replica order, so the published set is stable rather than
    // reordering with measurement noise — a set that churns would republish on
    // every tick and make the change log useless.
    let ordered: Vec<u64> = replicas
        .iter()
        .copied()
        .filter(|id| isr.contains(id))
        .collect();

    Some(ordered)
}

/// Keeps every partition this node leads publishing its in-sync set.
pub struct IsrPublisher {
    node_id: u64,
    metadata_store: Arc<dyn MetadataStore>,
    isr_tracker: Arc<IsrTracker>,
    produce_handler: Arc<crate::produce_handler::ProduceHandler>,
    interval: Duration,
    shutdown: Arc<AtomicBool>,
    published: Arc<AtomicU64>,
}

impl IsrPublisher {
    pub fn new(
        node_id: u64,
        metadata_store: Arc<dyn MetadataStore>,
        isr_tracker: Arc<IsrTracker>,
        produce_handler: Arc<crate::produce_handler::ProduceHandler>,
    ) -> Arc<Self> {
        let interval = std::env::var("CHRONIK_ISR_PUBLISH_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|s| *s > 0)
            .map(Duration::from_secs)
            .unwrap_or(DEFAULT_INTERVAL);

        Arc::new(Self {
            node_id,
            metadata_store,
            isr_tracker,
            produce_handler,
            interval,
            shutdown: Arc::new(AtomicBool::new(false)),
            published: Arc::new(AtomicU64::new(0)),
        })
    }

    #[cfg(test)]
    pub fn published(&self) -> u64 {
        self.published.load(Ordering::Relaxed)
    }

    pub fn start(self: &Arc<Self>) {
        let this = Arc::clone(self);
        tokio::spawn(async move { this.run().await });
    }

    async fn run(self: Arc<Self>) {
        info!(
            "ISR publisher started on node {} (every {:?}) — failover elects from what this writes",
            self.node_id, self.interval
        );

        while !self.shutdown.load(Ordering::Relaxed) {
            tokio::time::sleep(self.interval).await;
            if let Err(e) = self.publish_once().await {
                warn!("Could not publish in-sync sets: {}", e);
            }
        }
    }

    /// One pass: publish every led partition whose stored set differs from the
    /// measured one.
    ///
    /// The store is the only reference. A per-process "last published" map used
    /// to sit in front of it, and because it starts empty, every boot rewrote the
    /// in-sync set of every partition the node led — about 5,600 metadata writes,
    /// each replicated to every peer, on a restart that changed nothing. A peer
    /// that was down catches up by reading the catalog log, not through this
    /// loop. Changed sets go out as one batch per pass, fenced by leader epoch.
    async fn publish_once(&self) -> chronik_common::Result<()> {
        let topics = self.metadata_store.list_topics().await?;

        // Partitions led this pass. Feeds the tracker's leadership clock, which
        // is what lets a follower that never fetches fall out of the set.
        let mut led: std::collections::HashSet<(String, i32)> = std::collections::HashSet::new();
        // Changed sets, published together at the end of the pass, each fenced
        // by the leader epoch it was measured under.
        let mut updates: Vec<IsrUpdate> = Vec::new();
        let mut previous: Vec<(Vec<u64>, Vec<u64>)> = Vec::new();

        for topic in topics {
            // Internal topics are not replicated through this path.
            if topic.name.starts_with("__") {
                continue;
            }

            let assignments = match self
                .metadata_store
                .get_partition_assignments(&topic.name)
                .await
            {
                Ok(assignments) => assignments,
                Err(e) => {
                    debug!("No assignments for {}: {}", topic.name, e);
                    continue;
                }
            };

            for assignment in assignments {
                // Only the leader can measure this. A follower publishing its
                // own guess would overwrite the one node with the evidence.
                if assignment.leader_id != self.node_id {
                    continue;
                }

                let partition = assignment.partition as i32;
                self.isr_tracker.note_leading(&topic.name, partition);
                led.insert((topic.name.clone(), partition));

                let leader_offset = self
                    .produce_handler
                    .get_high_watermark(&topic.name, partition)
                    .await
                    .unwrap_or(0);

                // Where this node's own copy starts; see `repair_missing_history`.
                let held_start = self.produce_handler.held_log_start(&topic.name, partition).await;

                let Some(isr) = in_sync_replicas(
                    &self.isr_tracker,
                    &topic.name,
                    partition,
                    leader_offset,
                    &assignment.replicas,
                    assignment.leader_id,
                ) else {
                    continue; // nothing measured yet — publishing a guess is worse
                };

                if let Some(start) = held_start {
                    if self.repair_missing_history(&topic.name, &assignment, start, &isr).await {
                        continue; // leadership handed away: the new leader publishes
                    }
                }

                if assignment.isr == isr {
                    continue; // already what the cluster holds
                }

                previous.push((assignment.isr.clone(), assignment.replicas.clone()));
                updates.push(IsrUpdate {
                    topic: topic.name.clone(),
                    partition: assignment.partition,
                    leader_id: assignment.leader_id,
                    leader_epoch: assignment.leader_epoch,
                    isr,
                });
            }
        }

        // A partition skipped this pass (moved away, or its assignments could
        // not be read) restarts its grace period when next led. Erring that way
        // only delays a shrink; keeping a stale clock could shrink too early.
        self.isr_tracker.retain_leading(&led);

        if !updates.is_empty() {
            let outcomes = self.metadata_store.update_isr(updates.clone()).await?;
            for ((u, (was, replicas)), outcome) in updates.into_iter().zip(previous).zip(outcomes) {
                match outcome {
                    Ok(()) => {
                        info!(
                            "{}-{}: in-sync set is now {:?} (was {:?}, of replicas {:?})",
                            u.topic, u.partition, u.isr, was, replicas
                        );
                        self.published.fetch_add(1, Ordering::Relaxed);
                    }
                    // Leadership moved since the measurement: the new leader publishes.
                    Err(e) => debug!("{}-{}: in-sync set {:?} not published: {}", u.topic, u.partition, u.isr, e),
                }
            }
        }

        Ok(())
    }

    /// A leader whose copy starts above the partition's log start is missing
    /// records it claims to have: consumers asking for them get the next record
    /// it holds instead. What to do is decided by `plan_history_repair` from
    /// what every replica reported on its last fetch.
    ///
    /// Returns whether leadership was handed away.
    async fn repair_missing_history(
        &self,
        topic: &str,
        assignment: &chronik_common::metadata::traits::PartitionAssignment,
        held_start: i64,
        isr: &[u64],
    ) -> bool {
        let partition = assignment.partition as i32;
        let log_start = self.produce_handler.get_log_start_offset(topic, partition).await;
        if held_start <= log_start {
            return false;
        }
        // Give followers a full lag window from leadership to report.
        if !self.isr_tracker.leadership_grace_expired(topic, partition) {
            return false;
        }
        let reports: Vec<FollowerReport> = assignment
            .replicas
            .iter()
            .copied()
            .filter(|&id| id != self.node_id)
            .map(|id| FollowerReport {
                node: id,
                copy: self.isr_tracker.follower_log_start(id, topic, partition),
                current: self.isr_tracker.heard_from_recently(id),
            })
            .collect();

        match plan_history_repair(held_start, log_start, &reports, isr) {
            HistoryRepair::Nothing => false,
            HistoryRepair::HandOff { to, from, isr: new_isr } => {
                warn!(
                    "{}-{}: this node's copy starts at {} but the partition's log starts at {}; node {} \
                     holds from {}. Handing it the leadership (in-sync set {:?}) so offsets {}..{} are \
                     readable again.",
                    topic, partition, held_start, log_start, to, from, new_isr, from, held_start
                );
                let change = chronik_common::metadata::catalog::AssignmentChange {
                    topic: topic.to_string(),
                    partition: assignment.partition,
                    replicas: None,
                    leader: Some(to),
                    isr: Some(new_isr),
                    expected_epoch: Some(assignment.leader_epoch),
                };
                match self.metadata_store.set_assignments(vec![change]).await {
                    Ok(outcomes) => match outcomes.into_iter().next() {
                        Some(Ok(())) => true,
                        Some(Err(e)) => {
                            debug!("{}-{}: leadership not handed to {}: {}", topic, partition, to, e);
                            false
                        }
                        None => false,
                    },
                    Err(e) => {
                        warn!("{}-{}: could not hand leadership to {}: {}", topic, partition, to, e);
                        false
                    }
                }
            }
            HistoryRepair::MoveLogStart => {
                warn!(
                    "{}-{}: offsets {}..{} are on no replica (this node holds from {}, and every other \
                     replica reported holding no more). Moving the log start to {} so consumers start \
                     where records exist.",
                    topic, partition, log_start, held_start, held_start, held_start
                );
                if let Err(code) = self.produce_handler.delete_records(topic, partition, held_start).await {
                    warn!("{}-{}: could not move the log start to {} (error {})", topic, partition, held_start, code);
                }
                false
            }
        }
    }
}

/// What a follower last reported about its copy of a partition.
#[derive(Debug, Clone, Copy)]
pub struct FollowerReport {
    pub node: u64,
    /// (where its copy starts, its fetch offset), from its last fetch.
    pub copy: Option<(i64, i64)>,
    /// Heard from within the liveness window.
    pub current: bool,
}

/// What a leader missing the start of its copy does about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryRepair {
    Nothing,
    /// An in-sync follower holds more of the start: hand it the leadership,
    /// with an in-sync set leaving out every replica that holds less (this one
    /// included), so nothing elects them back before they have rebuilt.
    HandOff { to: u64, from: i64, isr: Vec<u64> },
    /// No replica holds more, and every one has reported: the records below
    /// the leader's copy exist nowhere. Move the log start up to its first
    /// record, so the earliest offset tells the truth and followers copy the
    /// partition from where it really starts.
    MoveLogStart,
}

/// Decide `HistoryRepair` for a leader whose copy starts at `held_start`,
/// above the partition's log start `log_start`, from its followers' reports
/// and the in-sync set. Pure: this is the decision that moves leadership or
/// declares records gone.
pub fn plan_history_repair(held_start: i64, log_start: i64, reports: &[FollowerReport], isr: &[u64]) -> HistoryRepair {
    if held_start <= log_start {
        return HistoryRepair::Nothing;
    }
    // An in-sync follower holding records below the leader's start; the one
    // holding the most.
    let best = reports
        .iter()
        .filter(|r| isr.contains(&r.node))
        .filter_map(|r| r.copy.and_then(|(start, fetched)| (start < held_start && start < fetched).then_some((start, r.node))))
        .min();
    if let Some((from, to)) = best {
        let isr = isr
            .iter()
            .copied()
            .filter(|&id| {
                id == to
                    || reports
                        .iter()
                        .any(|r| r.node == id && r.copy.is_some_and(|(start, _)| start <= from))
            })
            .collect();
        return HistoryRepair::HandOff { to, from, isr };
    }
    // A follower that holds nothing (its copy starts at its fetch offset) or
    // starts at or above ours has nothing to offer. Anyone not heard from, or
    // who has not reported (a node before v2.20.6), might: wait.
    let nobody_holds_more = reports.iter().all(|r| {
        r.current && r.copy.is_some_and(|(start, fetched)| start >= held_start || start >= fetched)
    });
    if nobody_holds_more {
        HistoryRepair::MoveLogStart
    } else {
        HistoryRepair::Nothing
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use chronik_common::metadata::PartitionAssignment;

    fn report(node: u64, start: i64, fetched: i64) -> FollowerReport {
        FollowerReport { node, copy: Some((start, fetched)), current: true }
    }

    /// The Thunderbird case: node 1 leads holding from 20,185 of a partition
    /// whose log starts at 0, node 2 holds all of it, node 3 is like node 1.
    /// Node 2 takes over, and the in-sync set leaves out the two that hold
    /// less until they have rebuilt.
    #[test]
    fn a_leader_missing_its_start_hands_over_to_a_follower_that_holds_it() {
        let reports = [report(2, 0, 21_862), report(3, 20_185, 21_862)];
        assert_eq!(
            plan_history_repair(20_185, 0, &reports, &[1, 2, 3]),
            HistoryRepair::HandOff { to: 2, from: 0, isr: vec![2] }
        );
        // Not in sync: not a candidate, whatever it holds.
        assert_eq!(plan_history_repair(20_185, 0, &reports, &[1, 3]), HistoryRepair::Nothing);
    }

    /// Records no replica holds are gone: the log start moves up to the first
    /// one that exists. Followers holding nothing (rebuilding) or less do not
    /// stand in the way.
    #[test]
    fn records_on_no_replica_move_the_log_start() {
        let reports = [report(2, 25_415, 29_449), report(3, 1, 1)];
        assert_eq!(plan_history_repair(1, 0, &reports, &[1, 2]), HistoryRepair::MoveLogStart);
    }

    /// Anyone not heard from, or not reporting, might hold the records: wait.
    #[test]
    fn records_are_not_declared_gone_while_a_replica_is_unaccounted_for() {
        let silent = FollowerReport { node: 3, copy: Some((25_415, 29_449)), current: false };
        assert_eq!(plan_history_repair(1, 0, &[report(2, 25_415, 29_449), silent], &[1, 2]), HistoryRepair::Nothing);
        let unreported = FollowerReport { node: 3, copy: None, current: true };
        assert_eq!(plan_history_repair(1, 0, &[report(2, 25_415, 29_449), unreported], &[1, 2]), HistoryRepair::Nothing);
    }

    /// A leader holding from its log start (retention trimmed both) is whole.
    #[test]
    fn a_leader_holding_from_its_log_start_does_nothing() {
        assert_eq!(plan_history_repair(7_500, 7_500, &[report(2, 0, 9_000)], &[1, 2]), HistoryRepair::Nothing);
    }

    /// A partition nobody has reported on is "unknown", not "empty".
    ///
    /// The distinction matters at the other end: failover reads an empty
    /// published set as "not reported yet" and falls back to the replica set,
    /// so publishing an empty set here would silently re-enable the unclean
    /// election this exists to prevent.
    #[test]
    fn unmeasured_partition_reports_unknown() {
        let tracker = IsrTracker::new(1000, 10_000);
        assert_eq!(
            in_sync_replicas(&tracker, "orders", 0, 100, &[1, 2, 3], 1),
            None
        );
    }

    /// The leader is in sync with itself even though it never reports to itself.
    #[test]
    fn leader_is_always_in_its_own_set() {
        let tracker = IsrTracker::new(1000, 10_000);
        tracker.update_follower_offset(2, "orders", 0, 100);
        tracker.update_follower_offset(3, "orders", 0, 100);

        assert_eq!(
            in_sync_replicas(&tracker, "orders", 0, 100, &[1, 2, 3], 1),
            Some(vec![1, 2, 3])
        );
    }

    /// A follower that is measurably behind is excluded — this is the whole
    /// point. Node 3 here is the replica that must never be elected.
    #[test]
    fn a_replica_that_is_behind_is_not_in_sync() {
        let tracker = IsrTracker::new(100, 10_000);
        tracker.update_follower_offset(2, "orders", 0, 1_000);
        tracker.update_follower_offset(3, "orders", 0, 10); // 990 behind

        assert_eq!(
            in_sync_replicas(&tracker, "orders", 0, 1_000, &[1, 2, 3], 1),
            Some(vec![1, 2])
        );
    }

    /// Reported in replica order regardless of measurement order, so the
    /// published value does not churn and trigger pointless republishing.
    #[test]
    fn the_set_is_ordered_by_replica_list() {
        let tracker = IsrTracker::new(1000, 10_000);
        tracker.update_follower_offset(3, "orders", 0, 100);
        tracker.update_follower_offset(1, "orders", 0, 100);

        assert_eq!(
            in_sync_replicas(&tracker, "orders", 0, 100, &[3, 1, 2], 2),
            Some(vec![3, 1, 2])
        );
    }

    /// A restart that changed nothing must write nothing.
    ///
    /// The publisher used to gate writes on an in-memory "last published" map
    /// that starts empty, so every boot rewrote the in-sync set of every led
    /// partition — ~5,600 replicated metadata writes per node restart on a
    /// cluster where none of them changed. The store is the reference now.
    #[tokio::test]
    async fn a_set_the_store_already_holds_is_not_rewritten() {
        use chronik_common::metadata::{InMemoryMetadataStore, TopicConfig};

        let temp_dir = tempfile::tempdir().unwrap();
        let store = Arc::new(InMemoryMetadataStore::new());
        let handler = Arc::new(
            crate::produce_handler::tests::handler_with_pipeline(&temp_dir, store.clone()).await,
        );

        let config = TopicConfig {
            partition_count: 2,
            ..Default::default()
        };
        store.create_topic("orders", config).await.unwrap();
        for (partition, isr) in [(0u32, vec![1, 2]), (1u32, vec![1])] {
            store
                .assign_partition(PartitionAssignment {
                    topic: "orders".into(),
                    partition,
                    broker_id: 1,
                    is_leader: true,
                    replicas: vec![1, 2],
                    leader_id: 1,
                    leader_epoch: 0, // derived by the store, not taken from here
                    isr,
                })
                .await
                .unwrap();
        }

        // Follower 2 is caught up on both partitions, so both measure [1, 2].
        let tracker = Arc::new(IsrTracker::new(100, 10_000));
        tracker.update_follower_offset(2, "orders", 0, 0);
        tracker.update_follower_offset(2, "orders", 1, 0);

        let epochs_before: std::collections::HashMap<u32, i32> = store
            .get_partition_assignments("orders")
            .await
            .unwrap()
            .into_iter()
            .map(|a| (a.partition, a.leader_epoch))
            .collect();

        let publisher = IsrPublisher::new(1, store.clone(), tracker, handler);

        // Fresh process: only partition 1, whose stored set is stale, is written.
        publisher.publish_once().await.unwrap();
        assert_eq!(publisher.published(), 1, "partition 0 already held [1, 2]");

        let stored = store.get_partition_assignments("orders").await.unwrap();
        for a in &stored {
            assert_eq!(a.isr, vec![1, 2], "partition {} converged", a.partition);
            assert_eq!(
                a.leader_epoch, epochs_before[&a.partition],
                "an ISR update is not a leadership change");
        }

        // Nothing changed since: the next pass writes nothing at all.
        publisher.publish_once().await.unwrap();
        assert_eq!(publisher.published(), 1);
    }
}
