//! ISR quorum tracking for `acks=all` produce requests.
//!
//! A producer using `acks=all` blocks until enough replicas hold its batch.
//! This is where that wait is registered and released.
//!
//! ## Replica progress is a watermark, not an event
//!
//! A replica reporting offset N has durably written *everything below N* — it
//! could not have asked for N otherwise. So a report at 500 satisfies a wait
//! at 437, and any wait below it.
//!
//! The original implementation keyed waits on an exact `(topic, partition,
//! offset)` and only matched an ACK naming that same offset. Under push that
//! held together because the leader emitted one ACK per pushed batch. It does
//! not survive follower-pull (RP-2.4), where a follower's fetch offset jumps
//! across many batch boundaries at once and rarely lands on a registered
//! offset — every `acks=all` produce would have waited out the full 30s
//! replication timeout.
//!
//! Monotonic tracking is also strictly more correct under push: a follower
//! demonstrably at offset 500 satisfies a wait at 437 even if the individual
//! ACK for 437 was lost or coalesced. Exact matching made a dropped ACK stall
//! a producer that was already durable.

use anyhow::Result;
use dashmap::DashMap;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::oneshot;
use tracing::{debug, info, warn};

/// How long an `acks=all` wait may sit before it is failed and released.
/// Matches Kafka's default `request.timeout.ms`.
pub const DEFAULT_ACK_TIMEOUT: Duration = Duration::from_secs(30);

/// How often the reaper runs.
const CLEANUP_INTERVAL: Duration = Duration::from_secs(5);

/// Tracks `acks=all` requests waiting for ISR quorum.
pub struct IsrAckTracker {
    partitions: DashMap<(String, i32), PartitionState>,
}

#[derive(Default)]
struct PartitionState {
    /// node id → highest offset that node has confirmed durable. Under pull
    /// this is the follower's fetch offset; under push, its ACKed LEO.
    node_offsets: HashMap<u64, i64>,
    /// Waits keyed by the offset they need, ascending. Ordering matters: once
    /// an offset has too few replicas, every higher offset does too, so the
    /// scan can stop.
    waiters: BTreeMap<i64, Vec<WaitEntry>>,
}

struct WaitEntry {
    tx: Option<oneshot::Sender<Result<()>>>,
    quorum_size: usize,
    registered_at: Instant,
}

impl IsrAckTracker {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            partitions: DashMap::new(),
        })
    }

    /// Start the background reaper.
    ///
    /// Without this, a wait that never reaches quorum stays in the map forever.
    /// The producer's own `timeout()` releases the *caller*, but the entry it
    /// registered is left behind — so before replication was fixed, when no
    /// follower ever ACKed, every `acks=all` produce leaked one entry for the
    /// life of the process.
    pub fn start_cleanup_task(self: &Arc<Self>) {
        let tracker = Arc::clone(self);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(CLEANUP_INTERVAL);
            loop {
                ticker.tick().await;
                tracker.cleanup_expired(DEFAULT_ACK_TIMEOUT);
            }
        });
        debug!("IsrAckTracker cleanup task started");
    }

    /// Register an `acks=all` request waiting for quorum.
    ///
    /// `offset` is the batch's log end offset — the position a replica must
    /// have reached for this batch to count as replicated to it.
    pub fn register_wait(
        &self,
        topic: String,
        partition: i32,
        offset: i64,
        quorum_size: usize,
        tx: oneshot::Sender<Result<()>>,
    ) {
        let mut state = self
            .partitions
            .entry((topic.clone(), partition))
            .or_default();

        // A replica may already be past this offset — it reports continuously,
        // not only in response to a write. Checking now avoids waiting for the
        // next report to notice something that is already true.
        let already_reached = state
            .node_offsets
            .values()
            .filter(|&&reached| reached >= offset)
            .count();

        if already_reached >= quorum_size {
            drop(state);
            debug!(
                "acks=all {}-{} offset {} already satisfied by {} replica(s)",
                topic, partition, offset, already_reached
            );
            let _ = tx.send(Ok(()));
            return;
        }

        state.waiters.entry(offset).or_default().push(WaitEntry {
            tx: Some(tx),
            quorum_size,
            registered_at: Instant::now(),
        });

        debug!(
            "acks=all {}-{} offset {} waiting for quorum {} ({} replica(s) there already)",
            topic, partition, offset, quorum_size, already_reached
        );
    }

    /// Record how far a replica has durably progressed.
    ///
    /// Hot path: called on every follower fetch and every leader self-ack.
    /// `offset` is that replica's log end offset — everything below it is
    /// durable there.
    pub fn record_ack(&self, topic: &str, partition: i32, offset: i64, node_id: u64) {
        let mut state = self
            .partitions
            .entry((topic.to_string(), partition))
            .or_default();

        // Never move a replica's position backwards. Reports can arrive out of
        // order — two in-flight batches, or a fetch retried after a reconnect —
        // and regressing would un-satisfy waits that were already released.
        let slot = state.node_offsets.entry(node_id).or_insert(i64::MIN);
        if offset > *slot {
            *slot = offset;
        } else {
            return; // no progress, nothing can newly qualify
        }

        let highest = state.node_offsets.values().copied().max().unwrap_or(i64::MIN);

        // Only waits at or below the furthest replica can possibly qualify.
        let candidates: Vec<i64> = state
            .waiters
            .range(..=highest)
            .map(|(&offset, _)| offset)
            .collect();

        let mut to_notify: Vec<oneshot::Sender<Result<()>>> = Vec::new();

        for wait_offset in candidates {
            let reached = state
                .node_offsets
                .values()
                .filter(|&&reached| reached >= wait_offset)
                .count();

            let Some(entries) = state.waiters.get_mut(&wait_offset) else {
                continue;
            };

            // Entries at the same offset can carry different quorum sizes if
            // min.insync.replicas changed between produces, so each is judged
            // on its own rather than the group.
            let mut still_waiting = Vec::new();
            for mut entry in entries.drain(..) {
                if reached >= entry.quorum_size {
                    if let Some(tx) = entry.tx.take() {
                        to_notify.push(tx);
                    }
                } else {
                    still_waiting.push(entry);
                }
            }

            if still_waiting.is_empty() {
                state.waiters.remove(&wait_offset);
            } else {
                *entries = still_waiting;
            }
        }

        // Release the map guard before notifying: a producer woken here resumes
        // immediately and may register its next wait on this same partition.
        drop(state);

        let notified = to_notify.len();
        for tx in to_notify {
            let _ = tx.send(Ok(()));
        }

        if notified > 0 {
            debug!(
                "Node {} reached {}-{} offset {}, releasing {} acks=all waiter(s)",
                node_id, topic, partition, offset, notified
            );
        }
    }

    /// Fail and release waits older than `timeout`.
    pub fn cleanup_expired(&self, timeout: Duration) {
        let now = Instant::now();
        let mut expired = Vec::new();

        for mut partition in self.partitions.iter_mut() {
            let (topic, index) = partition.key().clone();
            let state = partition.value_mut();

            let mut drained_offsets = Vec::new();
            for (&offset, entries) in state.waiters.iter_mut() {
                let mut still_waiting = Vec::new();
                for mut entry in entries.drain(..) {
                    if now.duration_since(entry.registered_at) > timeout {
                        if let Some(tx) = entry.tx.take() {
                            expired.push((topic.clone(), index, offset, entry.quorum_size, tx));
                        }
                    } else {
                        still_waiting.push(entry);
                    }
                }
                if still_waiting.is_empty() {
                    drained_offsets.push(offset);
                } else {
                    *entries = still_waiting;
                }
            }

            for offset in drained_offsets {
                state.waiters.remove(&offset);
            }
        }

        for (topic, partition, offset, quorum, tx) in expired {
            warn!(
                "acks=all timed out for {}-{} offset {} (quorum {} not reached in {:?})",
                topic, partition, offset, quorum, timeout
            );
            let _ = tx.send(Err(anyhow::anyhow!(
                "ISR quorum timeout for {}-{} offset {} (quorum {})",
                topic,
                partition,
                offset,
                quorum
            )));
        }
    }

    /// Number of `acks=all` requests currently waiting.
    pub fn pending_count(&self) -> usize {
        self.partitions
            .iter()
            .map(|p| p.waiters.values().map(|v| v.len()).sum::<usize>())
            .sum()
    }

    /// Highest offset a given replica has confirmed for a partition.
    pub fn replica_offset(&self, topic: &str, partition: i32, node_id: u64) -> Option<i64> {
        self.partitions
            .get(&(topic.to_string(), partition))
            .and_then(|state| state.node_offsets.get(&node_id).copied())
            .filter(|&offset| offset != i64::MIN)
    }

    pub fn stats(&self) -> IsrAckStats {
        let mut pending_requests = 0;
        let mut oldest_age_ms = 0u64;
        let mut tracked_replicas = 0;

        for partition in self.partitions.iter() {
            tracked_replicas += partition.node_offsets.len();
            for entries in partition.waiters.values() {
                pending_requests += entries.len();
                for entry in entries {
                    let age_ms = entry.registered_at.elapsed().as_millis() as u64;
                    if age_ms > oldest_age_ms {
                        oldest_age_ms = age_ms;
                    }
                }
            }
        }

        IsrAckStats {
            pending_requests,
            tracked_replicas,
            waiting_for_quorum: pending_requests,
            oldest_request_age_ms: oldest_age_ms,
        }
    }
}

/// Statistics for monitoring.
#[derive(Debug, Clone)]
pub struct IsrAckStats {
    /// `acks=all` requests still waiting.
    pub pending_requests: usize,
    /// How many (partition, replica) positions are being tracked.
    pub tracked_replicas: usize,
    /// Same as `pending_requests`; a registered wait is by definition unsatisfied.
    pub waiting_for_quorum: usize,
    /// Age of the oldest outstanding wait.
    pub oldest_request_age_ms: u64,
}

impl Default for IsrAckTracker {
    fn default() -> Self {
        Self {
            partitions: DashMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn quorum_releases_the_producer() {
        let tracker = IsrAckTracker::new();
        let (tx, rx) = oneshot::channel();

        tracker.register_wait("test".to_string(), 0, 100, 2, tx);

        tracker.record_ack("test", 0, 100, 1);
        assert_eq!(tracker.pending_count(), 1, "one replica is not quorum");

        tracker.record_ack("test", 0, 100, 2);
        assert_eq!(tracker.pending_count(), 0);
        assert!(rx.await.unwrap().is_ok());
    }

    /// The change that makes follower-pull work: a replica reporting offset 500
    /// has written everything below it, so it satisfies a wait at 437. Under
    /// pull a follower's fetch offset advances across many batches at once and
    /// almost never equals a registered offset — exact matching would have
    /// stalled every acks=all produce until the 30s timeout.
    #[tokio::test]
    async fn a_higher_report_satisfies_a_lower_wait() {
        let tracker = IsrAckTracker::new();
        let (tx, rx) = oneshot::channel();

        tracker.register_wait("test".to_string(), 0, 437, 2, tx);

        tracker.record_ack("test", 0, 500, 1);
        tracker.record_ack("test", 0, 512, 2);

        assert_eq!(tracker.pending_count(), 0);
        assert!(rx.await.unwrap().is_ok());
    }

    /// One report can release a backlog of waits at once — exactly what happens
    /// when a follower catches up after falling behind.
    #[tokio::test]
    async fn one_report_releases_every_wait_below_it() {
        let tracker = IsrAckTracker::new();
        let mut receivers = Vec::new();

        for offset in [10, 20, 30, 40] {
            let (tx, rx) = oneshot::channel();
            tracker.register_wait("test".to_string(), 0, offset, 2, tx);
            receivers.push((offset, rx));
        }
        assert_eq!(tracker.pending_count(), 4);

        tracker.record_ack("test", 0, 35, 1);
        tracker.record_ack("test", 0, 35, 2);

        assert_eq!(tracker.pending_count(), 1, "only the wait at 40 remains");
        for (offset, rx) in receivers {
            if offset <= 35 {
                assert!(rx.await.unwrap().is_ok(), "offset {offset} should be released");
            }
        }
    }

    /// A replica's position must never regress. Reports can arrive out of order
    /// — two in-flight batches, or a fetch retried after a reconnect — and
    /// going backwards would un-satisfy waits that were already released.
    #[tokio::test]
    async fn a_replica_position_never_goes_backwards() {
        let tracker = IsrAckTracker::new();

        tracker.record_ack("test", 0, 500, 1);
        tracker.record_ack("test", 0, 100, 1);

        assert_eq!(tracker.replica_offset("test", 0, 1), Some(500));

        let (tx, rx) = oneshot::channel();
        tracker.register_wait("test".to_string(), 0, 400, 1, tx);
        assert!(rx.await.unwrap().is_ok(), "the stale report must not have lowered node 1");
    }

    /// Replicas report continuously, not only in response to a write, so a
    /// batch can be replicated before its wait is even registered. Checking at
    /// registration avoids waiting for the next report to notice.
    #[tokio::test]
    async fn a_wait_already_satisfied_returns_immediately() {
        let tracker = IsrAckTracker::new();

        tracker.record_ack("test", 0, 900, 1);
        tracker.record_ack("test", 0, 900, 2);

        let (tx, rx) = oneshot::channel();
        tracker.register_wait("test".to_string(), 0, 100, 2, tx);

        assert_eq!(tracker.pending_count(), 0);
        assert!(rx.await.unwrap().is_ok());
    }

    /// The same replica reporting twice is one replica, not two. Otherwise a
    /// single chatty follower could satisfy a quorum of 2 by itself and
    /// acks=all would return on data that exists in one place.
    #[tokio::test]
    async fn one_replica_cannot_form_a_quorum_by_itself() {
        let tracker = IsrAckTracker::new();
        let (tx, _rx) = oneshot::channel();

        tracker.register_wait("test".to_string(), 0, 100, 2, tx);

        tracker.record_ack("test", 0, 100, 1);
        tracker.record_ack("test", 0, 200, 1);
        tracker.record_ack("test", 0, 300, 1);

        assert_eq!(tracker.pending_count(), 1, "node 1 alone is not a quorum of 2");
    }

    /// Waits are per partition. A busy partition must not release another's.
    #[tokio::test]
    async fn partitions_do_not_release_each_other() {
        let tracker = IsrAckTracker::new();
        let (tx0, _rx0) = oneshot::channel();
        let (tx1, rx1) = oneshot::channel();

        tracker.register_wait("test".to_string(), 0, 100, 2, tx0);
        tracker.register_wait("test".to_string(), 1, 100, 2, tx1);

        tracker.record_ack("test", 1, 100, 1);
        tracker.record_ack("test", 1, 100, 2);

        assert_eq!(tracker.pending_count(), 1, "partition 0 must still be waiting");
        assert!(rx1.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn an_unreached_quorum_times_out_with_an_error() {
        let tracker = IsrAckTracker::new();
        let (tx, rx) = oneshot::channel();

        tracker.register_wait("test".to_string(), 0, 100, 2, tx);
        tracker.record_ack("test", 0, 100, 1);

        tokio::time::sleep(Duration::from_millis(10)).await;
        tracker.cleanup_expired(Duration::from_millis(1));

        assert_eq!(tracker.pending_count(), 0);
        assert!(rx.await.unwrap().is_err());
    }

    /// The leak this rewrite closes. `cleanup_expired` existed but nothing ever
    /// called it, so an entry that never reached quorum stayed for the life of
    /// the process — the producer's own timeout released the caller but not the
    /// registration. While replication was silently not running, that was every
    /// acks=all produce the broker ever served.
    #[tokio::test]
    async fn waits_that_never_reach_quorum_do_not_accumulate() {
        let tracker = IsrAckTracker::new();
        let mut receivers = Vec::new();

        for offset in 0..500 {
            let (tx, rx) = oneshot::channel();
            tracker.register_wait("test".to_string(), 0, offset, 3, tx);
            receivers.push(rx);
        }
        assert_eq!(tracker.pending_count(), 500);

        tokio::time::sleep(Duration::from_millis(10)).await;
        tracker.cleanup_expired(Duration::from_millis(1));

        assert_eq!(tracker.pending_count(), 0, "expired waits must be reclaimed");
        for rx in receivers {
            assert!(rx.await.unwrap().is_err(), "each caller must be told it failed");
        }
    }

    #[test]
    fn stats_report_outstanding_waits_and_tracked_replicas() {
        let tracker = IsrAckTracker::new();
        let (tx1, _rx1) = oneshot::channel();
        let (tx2, _rx2) = oneshot::channel();

        tracker.register_wait("test".to_string(), 0, 100, 2, tx1);
        tracker.register_wait("test".to_string(), 1, 200, 2, tx2);

        tracker.record_ack("test", 0, 100, 1);
        tracker.record_ack("test", 1, 200, 1);
        tracker.record_ack("test", 1, 200, 2);

        let stats = tracker.stats();
        assert_eq!(stats.pending_requests, 1, "partition 1 reached quorum");
        assert_eq!(stats.tracked_replicas, 3, "1 replica on p0, 2 on p1");
    }
}
