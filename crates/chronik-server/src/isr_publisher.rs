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

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chronik_common::metadata::traits::MetadataStore;
use chronik_common::metadata::PartitionAssignment;
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

    pub fn published(&self) -> u64 {
        self.published.load(Ordering::Relaxed)
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
            "ISR publisher started on node {} (every {:?}) — failover elects from what this writes",
            self.node_id, self.interval
        );

        // Last set published per partition, so a metadata write happens on
        // change rather than on every tick.
        let mut last: HashMap<(String, i32), Vec<u64>> = HashMap::new();

        while !self.shutdown.load(Ordering::Relaxed) {
            tokio::time::sleep(self.interval).await;
            if let Err(e) = self.publish_once(&mut last).await {
                warn!("Could not publish in-sync sets: {}", e);
            }
        }
    }

    async fn publish_once(
        &self,
        last: &mut HashMap<(String, i32), Vec<u64>>,
    ) -> chronik_common::Result<()> {
        let topics = self.metadata_store.list_topics().await?;

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
                let leader_offset = self
                    .produce_handler
                    .get_high_watermark(&topic.name, partition)
                    .await
                    .unwrap_or(0);

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

                let key = (topic.name.clone(), partition);
                if last.get(&key) == Some(&isr) && assignment.isr == isr {
                    continue;
                }

                // Same leader_id, so `assign_partition` keeps the epoch: this
                // must not read as a leadership change.
                let update = PartitionAssignment {
                    isr: isr.clone(),
                    ..assignment.clone()
                };

                match self.metadata_store.assign_partition(update).await {
                    Ok(()) => {
                        if last.insert(key, isr.clone()).as_ref() != Some(&isr) {
                            info!(
                                "{}-{}: in-sync set is now {:?} (of replicas {:?})",
                                topic.name, partition, isr, assignment.replicas
                            );
                        }
                        self.published.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => warn!(
                        "{}-{}: could not publish in-sync set {:?}: {}",
                        topic.name, partition, isr, e
                    ),
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
