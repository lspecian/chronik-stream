//! ISR (In-Sync Replica) Tracker (v2.2.7 Phase 3)
//!
//! Tracks follower lag per partition to determine which replicas are in-sync.
//! A replica is considered in-sync if:
//! 1. Its lag is below max_lag_entries (default: 10,000 messages)
//! 2. Its last update was within max_lag_ms (default: 10 seconds)
//!
//! Usage:
//! ```rust
//! let tracker = IsrTracker::new(10_000, 10_000);
//!
//! // Update follower offset after replication
//! tracker.update_follower_offset(2, "orders", 0, 12345);
//!
//! // Check if follower is in-sync
//! if tracker.is_in_sync(2, "orders", 0, 12350) {
//!     println!("Node 2 is in-sync for orders-0");
//! }
//! ```

use dashmap::DashMap;
use std::time::{SystemTime, UNIX_EPOCH};

/// Partition key (topic, partition)
type PartitionKey = (String, i32);

/// Follower state for a partition
#[derive(Debug, Clone)]
struct FollowerState {
    /// Last acknowledged offset
    last_offset: i64,
    /// Last update timestamp (milliseconds since epoch)
    last_update_ms: u64,
}

/// Whether a follower counts as in-sync, and — critically — whether we know at all.
///
/// `Unknown` exists to keep "we have never heard from this replica" separate from
/// "this replica is behind". Callers previously could not tell those apart, and
/// `/admin/status` resolved an all-empty ISR by reporting *every* replica as
/// in-sync. That inverted the signal precisely when it mattered: a partition
/// replicating to nobody reported perfect health.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncState {
    /// Caught up, or lagging within both bounds.
    InSync,
    /// Known to this tracker and outside the lag bounds.
    Lagging,
    /// Never acknowledged anything for this partition.
    Unknown,
}

/// ISR Tracker - Tracks which replicas are in-sync
pub struct IsrTracker {
    /// Follower offsets per partition: (node_id, partition) -> state
    follower_offsets: DashMap<(u64, PartitionKey), FollowerState>,

    /// Last time each node was heard from at all, in ms since epoch.
    ///
    /// Separate from per-partition offsets because liveness is a property of the
    /// node, not the partition. A caught-up replica produces no partition ACKs
    /// when its partitions are idle, so without a node-level beat there is no way
    /// to tell "caught up and healthy" from "caught up and dead" — a node killed
    /// for 60s kept reporting in-sync. Fed by heartbeat ACKs.
    ///
    /// Connection state is NOT usable for this: a TCP write succeeds into the
    /// local send buffer long after the peer is gone, so a failed write detects
    /// death minutes late, if at all.
    node_last_seen_ms: DashMap<u64, u64>,

    /// Maximum lag in number of entries before marking out-of-sync
    max_lag_entries: u64,

    /// Maximum lag in milliseconds before marking out-of-sync
    max_lag_ms: u64,

    /// How long a node may go unheard before it counts as dead.
    ///
    /// Deliberately wider than `max_lag_ms`: liveness is proven by heartbeat
    /// replies which arrive on the heartbeat interval, so a bound equal to that
    /// interval would flap on ordinary jitter. Three intervals absorbs a missed
    /// beat without holding a genuinely dead node in ISR for long.
    node_liveness_ms: u64,
}

impl IsrTracker {
    /// Create a new ISR tracker
    ///
    /// # Arguments
    /// - `max_lag_entries`: Max entries a follower can be behind (default: 10,000)
    /// - `max_lag_ms`: Max time a follower can be silent (default: 10,000ms = 10s)
    pub fn new(max_lag_entries: u64, max_lag_ms: u64) -> Self {
        Self {
            follower_offsets: DashMap::new(),
            node_last_seen_ms: DashMap::new(),
            max_lag_entries,
            max_lag_ms,
            node_liveness_ms: max_lag_ms.saturating_mul(3),
        }
    }

    /// Record that `node_id` is alive right now.
    ///
    /// Called on every ACK, including the liveness ACK a follower sends in reply
    /// to a heartbeat. That reply is what keeps an idle-but-healthy replica in
    /// ISR while still evicting a dead one.
    pub fn record_node_alive(&self, node_id: u64) {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        self.node_last_seen_ms.insert(node_id, now_ms);
    }

    /// Whether `node_id` has been heard from within the liveness bound.
    ///
    /// Unknown nodes count as alive: at startup nothing has been heard from
    /// anyone, and the caller (`is_unknown_for_all`) handles that case
    /// separately. Treating unknown as dead here would wrongly empty ISR before
    /// the first heartbeat.
    pub fn is_node_alive(&self, node_id: u64) -> bool {
        self.node_is_alive(node_id)
    }

    fn node_is_alive(&self, node_id: u64) -> bool {
        let Some(last) = self.node_last_seen_ms.get(&node_id) else {
            return true;
        };
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        now_ms.saturating_sub(*last) <= self.node_liveness_ms
    }

    /// Check if a follower is in-sync for a partition
    ///
    /// # Arguments
    /// - `node_id`: Follower node ID
    /// - `topic`: Topic name
    /// - `partition`: Partition ID
    /// - `leader_offset`: Current leader's high watermark
    ///
    /// # Returns
    /// true if follower is in-sync, false otherwise
    pub fn is_in_sync(
        &self,
        node_id: u64,
        topic: &str,
        partition: i32,
        leader_offset: i64,
    ) -> bool {
        self.sync_state(node_id, topic, partition, leader_offset) == SyncState::InSync
    }

    /// Classify a follower as in-sync, lagging, or unknown.
    ///
    /// Two behaviours worth stating explicitly, because the naive version of
    /// this function is wrong in both:
    ///
    /// 1. **A caught-up follower never ages out.** The time bound measures how
    ///    long a follower has been *behind*, not how long since it last spoke.
    ///    Applying it unconditionally drops every replica of an idle partition
    ///    out of ISR after `max_lag_ms` even though they hold exactly the
    ///    leader's data — a false alarm on any topic that stops receiving
    ///    writes. Kafka's `replica.lag.time.max.ms` has the same semantics.
    ///
    /// 2. **Clocks move backwards.** `now - last_update` underflows on an NTP
    ///    step, and on u64 that wraps to a colossal lag rather than panicking in
    ///    release, silently ejecting healthy replicas. Saturating subtraction.
    pub fn sync_state(
        &self,
        node_id: u64,
        topic: &str,
        partition: i32,
        leader_offset: i64,
    ) -> SyncState {
        let key = (node_id, (topic.to_string(), partition));

        let Some(state) = self.follower_offsets.get(&key) else {
            return SyncState::Unknown;
        };

        // Liveness first: a node we have stopped hearing from is out, however
        // far along its last reported offset was. Without this a replica that
        // died while caught up stays in-sync forever, since the lag bound below
        // never fires for a caught-up replica and it will never ACK again.
        if !self.node_is_alive(node_id) {
            return SyncState::Lagging;
        }

        // Caught up (or ahead) and alive — in-sync regardless of elapsed time.
        if state.last_offset >= leader_offset {
            return SyncState::InSync;
        }

        let offset_lag = leader_offset - state.last_offset;
        if offset_lag > self.max_lag_entries as i64 {
            return SyncState::Lagging;
        }

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        if now_ms.saturating_sub(state.last_update_ms) > self.max_lag_ms {
            return SyncState::Lagging;
        }

        SyncState::InSync
    }

    /// True when nothing is known about any of `replicas` for this partition.
    ///
    /// Lets a caller distinguish "cluster just started, no ACKs yet" — where
    /// treating the assignment as ISR is reasonable — from "we have data and
    /// every follower is behind", where it is a lie.
    pub fn is_unknown_for_all(
        &self,
        topic: &str,
        partition: i32,
        replicas: &[u64],
        leader_id: u64,
    ) -> bool {
        replicas
            .iter()
            .filter(|&&node_id| node_id != leader_id) // the leader never ACKs to itself
            .all(|&node_id| {
                !self
                    .follower_offsets
                    .contains_key(&(node_id, (topic.to_string(), partition)))
            })
    }

    /// Update follower offset after successful replication
    ///
    /// # Arguments
    /// - `node_id`: Follower node ID
    /// - `topic`: Topic name
    /// - `partition`: Partition ID
    /// - `offset`: New acknowledged offset
    pub fn update_follower_offset(
        &self,
        node_id: u64,
        topic: &str,
        partition: i32,
        offset: i64,
    ) {
        let key = (node_id, (topic.to_string(), partition));
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        self.follower_offsets.insert(
            key,
            FollowerState {
                last_offset: offset,
                last_update_ms: now_ms,
            },
        );
    }

    /// Get all in-sync replicas for a partition
    ///
    /// # Arguments
    /// - `topic`: Topic name
    /// - `partition`: Partition ID
    /// - `leader_offset`: Current leader's high watermark
    /// - `all_replicas`: All replicas for this partition
    ///
    /// # Returns
    /// List of node IDs that are in-sync
    pub fn get_isr(
        &self,
        topic: &str,
        partition: i32,
        leader_offset: i64,
        all_replicas: &[u64],
    ) -> Vec<u64> {
        all_replicas
            .iter()
            .filter(|&&node_id| self.is_in_sync(node_id, topic, partition, leader_offset))
            .copied()
            .collect()
    }

    /// Lowest offset acknowledged by every follower still keeping up, or `None`
    /// when no follower is tracked for this partition.
    ///
    /// This is the retention interlock's input — Postgres's replication slot in
    /// miniature. WAL at or below this offset has reached every live follower and
    /// is safe to discard; above it, discarding would strand a replica with no
    /// way to obtain the data, because nothing in the system re-sends it.
    ///
    /// Followers silent beyond `max_lag_ms` are deliberately excluded. They have
    /// fallen out of ISR and must resync; letting them pin WAL forever would let
    /// one dead node fill the disk. This matches Kafka, where retention is
    /// independent of a follower that has dropped out.
    ///
    /// `None` means "no replication in play" (single node, or nothing acked yet)
    /// and callers must treat it as *no* interlock, preserving prior behaviour.
    pub fn min_acked_offset_of_live_followers(
        &self,
        topic: &str,
        partition: i32,
    ) -> Option<i64> {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        self.follower_offsets
            .iter()
            .filter(|entry| {
                let (_, (t, p)) = entry.key();
                t == topic && *p == partition
            })
            .filter(|entry| {
                now_ms.saturating_sub(entry.value().last_update_ms) <= self.max_lag_ms
            })
            .map(|entry| entry.value().last_offset)
            .min()
    }

    /// Drop everything known about a node, across all partitions.
    ///
    /// Called when the leader loses its connection to a follower. Without this, a
    /// follower that was caught up when it died stays in ISR indefinitely: it is
    /// caught up (so the lag bound never fires) and it will never ACK again (so
    /// nothing else can evict it). Observed live — a node killed for 60s still
    /// reported `isr=[1,2,3]`.
    ///
    /// Kafka does not need this because a follower proves liveness by continuing
    /// to fetch. In the push model the only equivalent signal is the connection
    /// itself, refreshed every heartbeat interval. RP-2 makes this unnecessary
    /// again by moving to fetch.
    pub fn remove_node(&self, node_id: u64) {
        self.follower_offsets
            .retain(|(nid, _), _| *nid != node_id);
    }

    /// Remove follower state (e.g., when node leaves cluster)
    #[allow(dead_code)]
    pub fn remove_follower(&self, node_id: u64, topic: &str, partition: i32) {
        let key = (node_id, (topic.to_string(), partition));
        self.follower_offsets.remove(&key);
    }

    /// Get follower lag for debugging/monitoring
    pub fn get_follower_lag(
        &self,
        node_id: u64,
        topic: &str,
        partition: i32,
        leader_offset: i64,
    ) -> Option<i64> {
        let key = (node_id, (topic.to_string(), partition));
        let last_offset = self.follower_offsets.get(&key).map(|state| state.last_offset)?;

        // A replica that has stopped reporting is frozen at whatever offset it
        // last reached, so the arithmetic says lag 0 for a replica that is gone.
        // Reporting that next to `under_replicated: true` invites the reader to
        // conclude the alert is spurious — the same "metadata looks healthy
        // while replication is not happening" failure this tracker exists to
        // end. Its distance is unknown, not zero, so report nothing for it and
        // let ISR carry the signal.
        if !self.node_is_alive(node_id) {
            return None;
        }

        Some(leader_offset - last_offset)
    }

    /// RP-2.3: the high watermark a *consumer* may read up to.
    ///
    /// Kafka's rule: `HW = min(LEO across the in-sync set)`. A record is only
    /// visible once every in-sync replica holds it, so a consumer can never read
    /// a record that would vanish if the leader were lost. Today's HW is the
    /// leader's own write position, which over-reports exactly that.
    ///
    /// Two exclusions matter, and getting either wrong breaks the cluster in a
    /// way that looks like a hang:
    ///
    /// - **Replicas outside ISR do not hold the watermark back.** A dead replica
    ///   is frozen at its last offset; letting it bound the HW would stall every
    ///   consumer on the partition until an operator intervened. That is the
    ///   scenario `min.insync.replicas` exists to police, not the HW.
    /// - **Knowing nothing means no constraint.** Before any follower has
    ///   reported — a freshly started cluster — bounding the HW at 0 would hide
    ///   the entire log. Return the leader's position and let ISR reporting catch
    ///   up.
    pub fn replicated_watermark(
        &self,
        topic: &str,
        partition: i32,
        leader_leo: i64,
        replicas: &[u64],
        leader_id: u64,
    ) -> i64 {
        let mut watermark = leader_leo;
        let mut any_in_sync_follower = false;

        for &node_id in replicas.iter().filter(|&&id| id != leader_id) {
            if self.sync_state(node_id, topic, partition, leader_leo) != SyncState::InSync {
                continue;
            }
            let Some(lag) = self.get_follower_lag(node_id, topic, partition, leader_leo) else {
                continue;
            };
            any_in_sync_follower = true;
            watermark = watermark.min(leader_leo - lag);
        }

        if !any_in_sync_follower {
            return leader_leo;
        }
        watermark.clamp(0, leader_leo)
    }
}

impl Default for IsrTracker {
    fn default() -> Self {
        Self::new(10_000, 10_000) // Default: 10K entries, 10s timeout
    }
}

/// RP-1.1: lets the WalIndexer hold WAL retention until followers have the data.
///
/// The indexer only learns "has every live follower got up to offset N?" — it
/// deliberately knows nothing about ISR, ACK frames or node ids.
impl chronik_storage::wal_indexer::ReplicationProgress for IsrTracker {
    fn min_replicated_offset(&self, topic: &str, partition: i32) -> Option<i64> {
        self.min_acked_offset_of_live_followers(topic, partition)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_isr_tracker_basic() {
        let tracker = IsrTracker::new(1000, 5000);

        // Initially not in-sync (no state)
        assert!(!tracker.is_in_sync(2, "orders", 0, 1000));

        // Update follower offset
        tracker.update_follower_offset(2, "orders", 0, 950);

        // Now in-sync (lag = 50)
        assert!(tracker.is_in_sync(2, "orders", 0, 1000));

        // Out of sync if lag > max_lag_entries
        assert!(!tracker.is_in_sync(2, "orders", 0, 2000)); // lag = 1050 > 1000
    }

    #[test]
    fn test_get_isr() {
        let tracker = IsrTracker::new(100, 5000);
        let all_replicas = vec![1, 2, 3];

        tracker.update_follower_offset(2, "test", 0, 990);
        tracker.update_follower_offset(3, "test", 0, 800); // Out of sync

        let isr = tracker.get_isr("test", 0, 1000, &all_replicas);
        assert_eq!(isr, vec![2]); // Only node 2 is in-sync
    }

    /// A follower that has caught up must stay in ISR no matter how long the
    /// partition then sits idle.
    ///
    /// The time bound measures how long a replica has been *behind*. Applying it
    /// unconditionally ejects every replica of a quiet topic once `max_lag_ms`
    /// elapses, even though they hold exactly the leader's data.
    #[test]
    fn caught_up_follower_does_not_age_out_of_isr() {
        let tracker = IsrTracker::new(1000, 10_000);
        let long_ago_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            - 60_000; // silent for a minute, well past max_lag_ms

        tracker.follower_offsets.insert(
            (2, ("idle".to_string(), 0)),
            FollowerState { last_offset: 500, last_update_ms: long_ago_ms },
        );

        // Caught up to the leader → in-sync however long the topic has been quiet.
        assert_eq!(tracker.sync_state(2, "idle", 0, 500), SyncState::InSync);
        assert!(tracker.is_in_sync(2, "idle", 0, 500));

        // Genuinely behind AND silent past the bound → lagging.
        assert_eq!(tracker.sync_state(2, "idle", 0, 501), SyncState::Lagging);
    }

    /// "Never heard from" and "known to be behind" must be distinguishable.
    ///
    /// `/admin/status` reports the assignment as ISR when nothing is known, which
    /// is right at startup and a lie afterwards. Without this distinction a
    /// partition replicating to nobody reported a full, healthy ISR — exactly how
    /// the outage in #29 stayed invisible.
    #[test]
    fn unknown_is_distinct_from_lagging() {
        let tracker = IsrTracker::new(10, 60_000);
        let replicas = vec![1, 2, 3];

        // Nothing recorded yet: unknown for every follower (leader 1 excluded).
        assert_eq!(tracker.sync_state(2, "t", 0, 100), SyncState::Unknown);
        assert!(tracker.is_unknown_for_all("t", 0, &replicas, 1));

        // One follower reports in, far behind. Now something IS known, so the
        // caller must not fall back to "ISR = all replicas".
        tracker.update_follower_offset(2, "t", 0, 1);
        assert_eq!(tracker.sync_state(2, "t", 0, 100), SyncState::Lagging);
        assert!(!tracker.is_unknown_for_all("t", 0, &replicas, 1));
        assert!(tracker.get_isr("t", 0, 100, &replicas).is_empty());
    }

    /// A follower that dies while caught up must still leave ISR.
    ///
    /// Removing the time bound for caught-up replicas (so idle partitions keep
    /// their ISR) creates the opposite hazard: a node killed while caught up is
    /// in-sync forever, because the lag bound never fires and it will never ACK
    /// again. Observed live — a node killed for 60s still reported isr=[1,2,3].
    ///
    /// Liveness is what evicts it, proven by heartbeat replies. Connection state
    /// cannot do this job: a TCP write lands in the local send buffer long after
    /// the peer is gone, so a failed write detects death minutes late — which is
    /// exactly why the first attempt at this fix did nothing on a real cluster.
    #[test]
    fn dead_but_caught_up_follower_leaves_isr_when_it_stops_answering() {
        let tracker = IsrTracker::new(1000, 10_000);
        let replicas = vec![1, 2, 3];

        tracker.update_follower_offset(2, "t", 0, 100);
        tracker.update_follower_offset(3, "t", 0, 100);
        tracker.record_node_alive(2);
        tracker.record_node_alive(3);
        assert_eq!(tracker.get_isr("t", 0, 100, &replicas), vec![2, 3]);

        // Node 3 stops answering heartbeats. Backdate its last-seen past the
        // liveness window; its offset still says "caught up".
        let stale = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            - (10_000 * 3 + 1_000);
        tracker.node_last_seen_ms.insert(3, stale);

        assert_eq!(tracker.sync_state(3, "t", 0, 100), SyncState::Lagging);
        assert_eq!(tracker.get_isr("t", 0, 100, &replicas), vec![2]);
    }

    /// The liveness window must be wider than the heartbeat interval or ISR
    /// flaps on ordinary jitter — a node one heartbeat late is not dead.
    #[test]
    fn liveness_window_tolerates_a_missed_heartbeat() {
        let tracker = IsrTracker::new(1000, 10_000);
        tracker.update_follower_offset(2, "t", 0, 100);

        let one_beat_late = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            - 15_000; // 1.5 heartbeat intervals
        tracker.node_last_seen_ms.insert(2, one_beat_late);

        assert_eq!(
            tracker.sync_state(2, "t", 0, 100),
            SyncState::InSync,
            "a single missed heartbeat must not eject a healthy replica"
        );
    }

    /// Explicit eviction still works for a node genuinely removed from the cluster.
    #[test]
    fn dead_but_caught_up_follower_is_evicted_on_connection_loss() {
        let tracker = IsrTracker::new(1000, 10_000);
        let replicas = vec![1, 2, 3];

        tracker.update_follower_offset(2, "t", 0, 100);
        tracker.update_follower_offset(3, "t", 0, 100);
        assert_eq!(tracker.get_isr("t", 0, 100, &replicas), vec![2, 3]);

        tracker.remove_node(3);

        assert_eq!(tracker.get_isr("t", 0, 100, &replicas), vec![2]);
        assert_eq!(tracker.sync_state(3, "t", 0, 100), SyncState::Unknown);
    }

    /// Eviction is per node, across every partition it replicated.
    #[test]
    fn remove_node_clears_all_partitions() {
        let tracker = IsrTracker::new(1000, 10_000);

        tracker.update_follower_offset(3, "a", 0, 10);
        tracker.update_follower_offset(3, "b", 7, 20);
        tracker.update_follower_offset(2, "a", 0, 10);

        tracker.remove_node(3);

        assert_eq!(tracker.sync_state(3, "a", 0, 10), SyncState::Unknown);
        assert_eq!(tracker.sync_state(3, "b", 7, 20), SyncState::Unknown);
        assert_eq!(tracker.sync_state(2, "a", 0, 10), SyncState::InSync, "other nodes untouched");
    }

    /// A backwards clock step must not eject healthy replicas.
    ///
    /// `now - last_update` on u64 wraps rather than panicking in release, turning
    /// a small NTP correction into an enormous apparent lag.
    #[test]
    fn backwards_clock_does_not_eject_replica() {
        let tracker = IsrTracker::new(1000, 10_000);
        let future_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            + 60_000;

        tracker.follower_offsets.insert(
            (2, ("t".to_string(), 0)),
            FollowerState { last_offset: 10, last_update_ms: future_ms },
        );

        // Behind by 5, timestamp in the future: saturating_sub yields 0 elapsed,
        // so this is in-sync rather than wrapped to a colossal lag.
        assert_eq!(tracker.sync_state(2, "t", 0, 15), SyncState::InSync);
    }

    /// A replica excluded from ISR for liveness must not also be reported at
    /// lag 0. Its last offset is frozen where it died, so the subtraction says
    /// "caught up" for a replica that is gone — printed next to
    /// `under_replicated: true`, that reads as a false alarm. This is the same
    /// shape as the outage that started this work: the metadata looked healthy
    /// precisely when replication was not happening.
    #[test]
    fn a_dead_replica_reports_no_lag_rather_than_zero_lag() {
        let tracker = IsrTracker::new(1000, 10_000);

        tracker.update_follower_offset(2, "t", 0, 100);
        tracker.record_node_alive(2);
        assert_eq!(tracker.get_follower_lag(2, "t", 0, 100), Some(0));

        // Backdate the liveness stamp well past the window.
        let stale = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            - 10_000 * 3 - 5_000;
        tracker.node_last_seen_ms.insert(2, stale);

        assert_eq!(
            tracker.get_follower_lag(2, "t", 0, 100),
            None,
            "a replica that stopped reporting has unknown distance, not zero"
        );
        assert_eq!(tracker.sync_state(2, "t", 0, 100), SyncState::Lagging);
    }

    /// RP-2.3: a consumer must not see a record that only the leader holds.
    #[test]
    fn watermark_is_bounded_by_the_slowest_in_sync_follower() {
        let tracker = IsrTracker::new(1000, 10_000);

        tracker.update_follower_offset(2, "t", 0, 90);
        tracker.record_node_alive(2);
        tracker.update_follower_offset(3, "t", 0, 75);
        tracker.record_node_alive(3);

        assert_eq!(
            tracker.replicated_watermark("t", 0, 100, &[1, 2, 3], 1),
            75,
            "the watermark follows the furthest-behind in-sync replica"
        );
    }

    /// A replica that has dropped out of ISR must NOT pin the watermark. If it
    /// did, one dead node would stall every consumer on the partition until an
    /// operator intervened — turning a survivable failure into an outage.
    #[test]
    fn a_replica_outside_isr_does_not_hold_the_watermark_back() {
        let tracker = IsrTracker::new(1000, 10_000);

        tracker.update_follower_offset(2, "t", 0, 100);
        tracker.record_node_alive(2);
        tracker.update_follower_offset(3, "t", 0, 10);
        tracker.record_node_alive(3);

        // Node 3 stops answering entirely.
        let stale = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            - 10_000 * 3 - 5_000;
        tracker.node_last_seen_ms.insert(3, stale);

        assert_eq!(tracker.sync_state(3, "t", 0, 100), SyncState::Lagging);
        assert_eq!(
            tracker.replicated_watermark("t", 0, 100, &[1, 2, 3], 1),
            100,
            "a dead replica must not stall consumers"
        );
    }

    /// Before any follower has reported, bounding the watermark at 0 would hide
    /// the whole log. Nothing known means no constraint.
    #[test]
    fn an_unreported_partition_is_not_bounded_to_zero() {
        let tracker = IsrTracker::new(1000, 10_000);

        assert_eq!(
            tracker.replicated_watermark("t", 0, 500, &[1, 2, 3], 1),
            500,
            "no follower data must not be read as 'nothing is replicated'"
        );
    }

    /// A single-node partition has no followers to wait for.
    #[test]
    fn a_lone_leader_is_its_own_watermark() {
        let tracker = IsrTracker::new(1000, 10_000);
        assert_eq!(tracker.replicated_watermark("t", 0, 42, &[1], 1), 42);
    }

    /// A follower reporting ahead of the leader (an in-flight write the leader
    /// has not yet counted) must not push the watermark past the leader's log.
    #[test]
    fn the_watermark_never_exceeds_the_leader() {
        let tracker = IsrTracker::new(1000, 10_000);

        tracker.update_follower_offset(2, "t", 0, 150);
        tracker.record_node_alive(2);

        assert_eq!(tracker.replicated_watermark("t", 0, 100, &[1, 2], 1), 100);
    }
}
