//! ISR (In-Sync Replica) Tracker (v2.2.7 Phase 3)
//!
//! Tracks follower lag per partition to determine which replicas are in-sync.
//! A replica is considered in-sync if:
//! 1. It has been caught up to the leader within max_lag_ms (default: 10 s)
//! 2. Optionally, its lag is below max_lag_entries (off unless
//!    CHRONIK_REPLICA_LAG_MAX_ENTRIES is set)
//!
//! Usage:
//! ```rust
//! let tracker = IsrTracker::new(10_000, 10_000);
//!
//! // Record a follower fetch (fetch offset, leader log end)
//! tracker.record_follower_fetch(2, "orders", 0, 12345, 12345);
//!
//! // Check if follower is in-sync
//! if tracker.is_in_sync(2, "orders", 0, 12350) {
//!     println!("Node 2 is in-sync for orders-0");
//! }
//! ```

use dashmap::DashMap;
use std::time::{SystemTime, UNIX_EPOCH};

/// Reported as the replicated position when a partition has followers but none
/// are live: below every real offset, so any retention check that compares
/// against it holds rather than deletes.
const UNREPLICATED: i64 = -1;

/// Partition key (topic, partition)
type PartitionKey = (String, i32);

/// Follower state for a partition
#[derive(Debug, Clone)]
struct FollowerState {
    /// Last acknowledged offset
    last_offset: i64,
    /// When the follower last fetched at all (milliseconds since epoch). Says it
    /// is replicating, NOT that it is keeping up — the retention interlock uses it.
    last_update_ms: u64,
    /// When the follower was last caught up to the leader's log end — Kafka's
    /// `lastCaughtUpTimeMs`, and what the `max_lag_ms` bound measures.
    ///
    /// The bound used to measure `last_update_ms` instead, which every fetch
    /// refreshes. A follower that fetched the same offset forever therefore
    /// never aged out: measured, two followers stuck at offset 100 of a 1,000-
    /// record partition stayed in ISR indefinitely, one was elected when the
    /// leader died, and every acknowledged `acks=all` record was gone.
    last_caught_up_ms: u64,
    /// The leader's log end when this follower last fetched. Lets a follower
    /// count as caught up under continuous writes, where the log end always
    /// moves between its fetches (Kafka's `lastFetchLeaderLogEndOffset`).
    last_fetch_leader_end: i64,
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

    /// When this node started leading each partition, in ms since epoch, as
    /// observed by the ISR publisher.
    ///
    /// The clock that turns "never heard from" into "not in sync". A follower
    /// proves it is replicating by fetching; one that never fetches leaves no
    /// trace in `follower_offsets` at all, and without a start time there is no
    /// way to tell "the cluster has just started" from "this follower has not
    /// fetched once in hours". Measured: with SASL required, the replica fetcher
    /// could not authenticate, no follower ever fetched, and every partition
    /// reported a full in-sync set indefinitely while followers held nothing.
    ///
    /// Kafka does the same: a new leader gives each follower
    /// `replica.lag.time.max.ms` from the moment it took over, then shrinks the
    /// set if the follower has not caught up.
    leading_since_ms: DashMap<PartitionKey, u64>,

    /// Optional record-count bound on lag; `None` (the default) leaves time
    /// as the only bound, as in Kafka since 0.9. See `from_env`.
    max_lag_entries: Option<u64>,

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
            leading_since_ms: DashMap::new(),
            max_lag_entries: Some(max_lag_entries),
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
        if self.max_lag_entries.is_some_and(|max| offset_lag > max as i64) {
            return SyncState::Lagging;
        }

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        // Behind: in sync only if it was caught up recently. Fetching without
        // catching up does not count — see `last_caught_up_ms`.
        if now_ms.saturating_sub(state.last_caught_up_ms) > self.max_lag_ms {
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
        // Past the grace period, silence is evidence: a follower that has not
        // fetched once in `max_lag_ms` of our leadership is not replicating.
        if self.leadership_grace_expired(topic, partition) {
            return false;
        }
        replicas
            .iter()
            .filter(|&&node_id| node_id != leader_id) // the leader never ACKs to itself
            .all(|&node_id| {
                !self
                    .follower_offsets
                    .contains_key(&(node_id, (topic.to_string(), partition)))
            })
    }

    /// Record that this node leads `topic`-`partition`, starting the grace
    /// period on the first call. Idempotent: later calls keep the original time.
    pub fn note_leading(&self, topic: &str, partition: i32) {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        self.leading_since_ms
            .entry((topic.to_string(), partition))
            .or_insert(now_ms);
    }

    /// Forget leadership of every partition not in `led`, so that leading a
    /// partition again later starts a fresh grace period instead of inheriting
    /// a stale one that would shrink the set before followers had a chance.
    pub fn retain_leading(&self, led: &std::collections::HashSet<PartitionKey>) {
        self.leading_since_ms.retain(|key, _| led.contains(key));
    }

    /// True once this node has led the partition for longer than `max_lag_ms`.
    /// A partition never marked as led has no grace period to expire.
    fn leadership_grace_expired(&self, topic: &str, partition: i32) -> bool {
        let Some(since) = self
            .leading_since_ms
            .get(&(topic.to_string(), partition))
            .map(|v| *v)
        else {
            return false;
        };
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        now_ms.saturating_sub(since) > self.max_lag_ms
    }

    /// Update follower offset after successful replication (tests)
    ///
    /// # Arguments
    /// - `node_id`: Follower node ID
    /// - `topic`: Topic name
    /// - `partition`: Partition ID
    /// - `offset`: New acknowledged offset
    #[cfg(test)]
    pub fn update_follower_offset(
        &self,
        node_id: u64,
        topic: &str,
        partition: i32,
        offset: i64,
    ) {
        // A bare offset report carries no leader position, so it asserts the
        // follower is caught up as of now. The fetch path, which knows the
        // leader's log end, uses `record_follower_fetch`.
        self.record_follower_fetch(node_id, topic, partition, offset, offset);
    }

    /// Record a follower's fetch at `fetch_offset` while the leader's log ended
    /// at `leader_end` — Kafka's `Replica.updateFetchState`.
    ///
    /// The follower is caught up as of now if it asked for the leader's end. If
    /// it reached where the leader's end stood at its previous fetch, it was
    /// caught up as of that fetch: under continuous writes the end moves between
    /// fetches, and a healthy follower is always a few records behind it.
    /// Otherwise its caught-up time does not move, and after `max_lag_ms` it
    /// leaves the in-sync set. A follower seen for the first time starts with a
    /// full grace period, as Kafka grants one when it becomes leader.
    pub fn record_follower_fetch(
        &self,
        node_id: u64,
        topic: &str,
        partition: i32,
        fetch_offset: i64,
        leader_end: i64,
    ) {
        let key = (node_id, (topic.to_string(), partition));
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let previous = self.follower_offsets.get(&key).map(|s| s.value().clone());
        let last_caught_up_ms = match previous {
            _ if fetch_offset >= leader_end => now_ms,
            Some(p) if fetch_offset >= p.last_fetch_leader_end => p.last_update_ms,
            Some(p) => p.last_caught_up_ms,
            None => now_ms,
        };

        self.follower_offsets.insert(
            key,
            FollowerState {
                last_offset: fetch_offset,
                last_update_ms: now_ms,
                last_caught_up_ms,
                last_fetch_leader_end: leader_end,
            },
        );
    }

    /// The highest leader log end seen on any follower's fetch of this
    /// partition. Lets a caller holding only a high watermark judge "caught up"
    /// against the log end, as the fetch path does.
    pub fn leader_end_seen(&self, topic: &str, partition: i32) -> Option<i64> {
        self.follower_offsets
            .iter()
            .filter(|e| e.key().1 .0 == topic && e.key().1 .1 == partition)
            .map(|e| e.value().last_fetch_leader_end)
            .max()
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

        let mut known_follower = false;
        let mut min_live: Option<i64> = None;

        for entry in self.follower_offsets.iter() {
            let (_, (t, p)) = entry.key();
            if t != topic || *p != partition {
                continue;
            }
            known_follower = true;

            if now_ms.saturating_sub(entry.value().last_update_ms) <= self.max_lag_ms {
                let offset = entry.value().last_offset;
                min_live = Some(min_live.map_or(offset, |m: i64| m.min(offset)));
            }
        }

        match (known_follower, min_live) {
            // This partition HAS followers and not one of them is live.
            //
            // Nothing is provably replicated, so report a position below every
            // real offset rather than `None`. The caller reads `None` as "no
            // interlock" and goes on to delete WAL segments — so returning it
            // here switched the retention guard off at the exact moment it was
            // needed: when the followers are gone is precisely when the leader's
            // copy is the only copy.
            //
            // Measured: with both followers frozen, the indexer archived and
            // deleted a WAL segment whose tail no follower had ever received.
            (true, None) => Some(UNREPLICATED),
            // No follower has ever reported for this partition: RF=1, or a
            // cluster that has not replicated it yet. Unchanged — `None` means
            // "no interlock", which is what single-node deployments rely on.
            _ => min_live,
        }
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

        // Never negative. A follower replicates the leader's LOG END, while the
        // offset it is compared against here is a high watermark — which by
        // definition trails until the followers acknowledge. So a perfectly
        // healthy replica is routinely *ahead* of the number it is measured
        // against, and the raw subtraction reports that as lag -7.
        //
        // "How far behind" has no negative values. Both callers want that
        // question answered: `/admin/status` renders it for an operator, and
        // `replicated_watermark` subtracts it and clamps at the leader anyway.
        Some((leader_offset - last_offset).max(0))
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

/// How long a replica may stay measurably behind before it leaves ISR.
///
/// Kafka's `replica.lag.time.max.ms` defaults to 30s. This is deliberately
/// tighter: ISR is the signal an operator reads to learn that acknowledged data
/// is at risk, and a 30s window means a third of a minute of writes land looking
/// fully replicated when they are not. The cluster conformance suite is
/// validated at this value — raise it toward Kafka's default if your followers
/// are on a link where 10s of lag is routine, and re-run that suite.
const DEFAULT_REPLICA_LAG_TIME_MAX_MS: u64 = 10_000;

// A record-count bound on follower lag is OFF unless
// CHRONIK_REPLICA_LAG_MAX_ENTRIES is set. Kafka removed the equivalent
// (`replica.lag.max.messages`) in 0.9: a fixed count ejects healthy replicas on
// a busy partition. Measured here: it defaulted to 10,000, which at ~130K msg/s
// per partition is 77 ms of ordinary fetch lag, so followers left ISR in the
// middle of every sustained acks=all run and the producer backed off on
// NOT_ENOUGH_REPLICAS — 130K msg/s instead of 460K on a 3-node cluster, on
// v2.16.11 too. The time bound already catches a follower that cannot keep up.

impl IsrTracker {
    /// Build from the environment, so the bound is operator-tunable.
    ///
    /// It was `IsrTracker::default()` at the one call site that matters, which
    /// left the bound hardcoded with no way to change it — Kafka exposes exactly
    /// this knob, and a cluster whose followers sit on a slower link had no
    /// answer short of a rebuild.
    pub fn from_env() -> Self {
        fn env_u64(key: &str, default: u64) -> u64 {
            match std::env::var(key) {
                Ok(raw) => match raw.parse::<u64>() {
                    Ok(v) if v > 0 => v,
                    _ => {
                        tracing::warn!(
                            "{}={:?} is not a positive integer — using {}",
                            key, raw, default
                        );
                        default
                    }
                },
                Err(_) => default,
            }
        }

        let max_lag_ms = env_u64(
            "CHRONIK_REPLICA_LAG_TIME_MAX_MS",
            DEFAULT_REPLICA_LAG_TIME_MAX_MS,
        );
        let max_lag_entries = std::env::var("CHRONIK_REPLICA_LAG_MAX_ENTRIES")
            .ok()
            .map(|_| env_u64("CHRONIK_REPLICA_LAG_MAX_ENTRIES", u64::MAX));

        tracing::info!(
            max_lag_ms,
            max_lag_entries = ?max_lag_entries,
            "ISR lag bounds (CHRONIK_REPLICA_LAG_TIME_MAX_MS / _MAX_ENTRIES)"
        );

        let mut tracker = Self::new(0, max_lag_ms);
        tracker.max_lag_entries = max_lag_entries;
        tracker
    }
}

impl Default for IsrTracker {
    fn default() -> Self {
        let mut tracker = Self::new(0, DEFAULT_REPLICA_LAG_TIME_MAX_MS);
        tracker.max_lag_entries = None;
        tracker
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

    /// A follower far behind in records but caught up moments ago stays in
    /// sync by default: on a busy partition 10,000 records is tens of
    /// milliseconds of ordinary fetch lag. The default count bound ejected such
    /// followers in the middle of sustained acks=all runs, and throughput fell
    /// from ~460K to ~130K msg/s as the producer backed off on
    /// NOT_ENOUGH_REPLICAS.
    #[test]
    fn a_recently_caught_up_follower_stays_in_sync_however_many_records_behind() {
        let tracker = IsrTracker::default();
        tracker.record_follower_fetch(2, "busy", 0, 1_000_000, 1_000_000);
        // The leader has written 50,000 more since the follower's last fetch.
        assert!(tracker.is_in_sync(2, "busy", 0, 1_050_000));
    }

    /// An operator who sets CHRONIK_REPLICA_LAG_MAX_ENTRIES still gets it.
    #[test]
    fn an_explicit_count_bound_still_applies() {
        let tracker = IsrTracker::new(10_000, 10_000);
        tracker.record_follower_fetch(2, "busy", 0, 1_000_000, 1_000_000);
        assert!(!tracker.is_in_sync(2, "busy", 0, 1_050_000));
    }

    /// A partition whose followers have all gone silent must report that
    /// NOTHING is provably replicated, not "no constraint".
    ///
    /// The WAL retention interlock treats `None` as "no followers to wait for"
    /// and deletes indexed segments. Returning `None` here when the followers
    /// merely stopped answering therefore switched the guard off exactly when
    /// the leader's copy was the only copy — measured with both followers
    /// frozen, where the indexer archived and deleted a segment whose tail no
    /// follower had ever received.
    #[test]
    fn a_partition_whose_followers_are_all_silent_reports_nothing_replicated() {
        let tracker = IsrTracker::new(1000, 10_000);
        let long_ago_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            - 60_000; // silent for a minute, well past max_lag_ms

        tracker.follower_offsets.insert(
            (2, ("orders".to_string(), 0)),
            FollowerState { last_offset: 500, last_update_ms: long_ago_ms, last_caught_up_ms: long_ago_ms, last_fetch_leader_end: 500 },
        );

        assert_eq!(
            tracker.min_acked_offset_of_live_followers("orders", 0),
            Some(-1),
            "a silent follower must not read as 'nothing to wait for'"
        );
    }

    /// A partition nobody has ever reported on keeps the old meaning: no
    /// interlock. Single-node deployments have no followers and must not have
    /// their WAL pinned forever.
    #[test]
    fn a_partition_with_no_followers_at_all_imposes_no_interlock() {
        let tracker = IsrTracker::new(1000, 10_000);
        assert_eq!(tracker.min_acked_offset_of_live_followers("orders", 0), None);
    }

    /// With live followers it is still the minimum of their positions.
    #[test]
    fn live_followers_report_their_slowest() {
        let tracker = IsrTracker::new(1000, 10_000);
        tracker.update_follower_offset(2, "orders", 0, 900);
        tracker.update_follower_offset(3, "orders", 0, 750);

        assert_eq!(
            tracker.min_acked_offset_of_live_followers("orders", 0),
            Some(750)
        );
    }

    /// A follower ahead of the offset it is compared against reports lag 0, not
    /// a negative number.
    ///
    /// This is the normal case, not an edge case: a follower replicates the
    /// leader's log end, while `/admin/status` measures it against the high
    /// watermark, which trails until that very follower acknowledges. Before
    /// `acks=all` was fixed the two could not diverge this way, because the
    /// follower was never shown records above the watermark (#36).
    #[test]
    fn follower_ahead_of_the_watermark_reports_no_lag() {
        let tracker = IsrTracker::new(1000, 10_000);
        tracker.update_follower_offset(2, "orders", 0, 507);

        // Leader's watermark is 500; the follower already holds 507.
        assert_eq!(tracker.get_follower_lag(2, "orders", 0, 500), Some(0));

        // Genuinely behind still reports the real distance.
        tracker.update_follower_offset(3, "orders", 0, 460);
        assert_eq!(tracker.get_follower_lag(3, "orders", 0, 500), Some(40));
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
            FollowerState { last_offset: 500, last_update_ms: long_ago_ms, last_caught_up_ms: long_ago_ms, last_fetch_leader_end: 500 },
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
            FollowerState { last_offset: 10, last_update_ms: future_ms, last_caught_up_ms: future_ms, last_fetch_leader_end: 10 },
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

    /// OQ3: the lag bound is time-based and operator-tunable.
    ///
    /// Kafka exposes `replica.lag.time.max.ms`; this had no equivalent — the
    /// bound was compiled in, so a cluster whose followers sit on a slower link
    /// could not widen it. A bad value must fall back to the default rather than
    /// panic or, worse, parse to 0 and eject every replica instantly.
    #[test]
    fn lag_bound_reads_the_environment_and_rejects_nonsense() {
        // Defaults when unset.
        std::env::remove_var("CHRONIK_REPLICA_LAG_TIME_MAX_MS");
        std::env::remove_var("CHRONIK_REPLICA_LAG_MAX_ENTRIES");
        let tracker = IsrTracker::from_env();
        assert_eq!(tracker.max_lag_ms, DEFAULT_REPLICA_LAG_TIME_MAX_MS);

        // Honours a valid override, and liveness stays a multiple of it.
        std::env::set_var("CHRONIK_REPLICA_LAG_TIME_MAX_MS", "30000");
        let tracker = IsrTracker::from_env();
        assert_eq!(tracker.max_lag_ms, 30_000);
        assert_eq!(tracker.node_liveness_ms, 90_000);

        // Zero would eject every replica the instant it fell one record behind.
        std::env::set_var("CHRONIK_REPLICA_LAG_TIME_MAX_MS", "0");
        assert_eq!(
            IsrTracker::from_env().max_lag_ms,
            DEFAULT_REPLICA_LAG_TIME_MAX_MS
        );

        std::env::set_var("CHRONIK_REPLICA_LAG_TIME_MAX_MS", "soon");
        assert_eq!(
            IsrTracker::from_env().max_lag_ms,
            DEFAULT_REPLICA_LAG_TIME_MAX_MS
        );

        std::env::remove_var("CHRONIK_REPLICA_LAG_TIME_MAX_MS");
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

    fn a_minute_ago_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            - 60_000
    }

    /// A follower that never fetches leaves the in-sync set once the leader has
    /// led for longer than the lag bound.
    ///
    /// The case the tracker could not see: no fetch, so no entry, so "unknown",
    /// so the publisher kept the full replica set. Measured with SASL required
    /// and an unauthenticated replica fetcher: every partition kept a full
    /// in-sync set while no follower held any of its data.
    #[test]
    fn a_follower_that_never_fetches_is_not_unknown_forever() {
        let tracker = IsrTracker::new(1000, 10_000);
        let replicas = vec![1, 2, 3];

        // Just took over: followers get their chance.
        tracker.note_leading("t", 0);
        assert!(tracker.is_unknown_for_all("t", 0, &replicas, 1));

        // Led for a minute without one fetch: that is evidence.
        tracker
            .leading_since_ms
            .insert(("t".to_string(), 0), a_minute_ago_ms());
        assert!(!tracker.is_unknown_for_all("t", 0, &replicas, 1));
        assert!(tracker.get_isr("t", 0, 0, &[2, 3]).is_empty());
    }

    /// A partition this node does not lead never expires: only the leader can
    /// measure followers, so nobody else may conclude they are absent.
    #[test]
    fn grace_never_expires_for_a_partition_not_led() {
        let tracker = IsrTracker::new(1000, 10_000);
        assert!(tracker.is_unknown_for_all("t", 0, &[1, 2, 3], 1));
    }

    /// `note_leading` keeps the first time; losing and regaining leadership
    /// starts over.
    #[test]
    fn regaining_leadership_restarts_the_grace_period() {
        let tracker = IsrTracker::new(1000, 10_000);
        tracker
            .leading_since_ms
            .insert(("t".to_string(), 0), a_minute_ago_ms());

        tracker.note_leading("t", 0); // must not reset an ongoing leadership
        assert!(tracker.leadership_grace_expired("t", 0));

        tracker.retain_leading(&std::collections::HashSet::new()); // lost it
        tracker.note_leading("t", 0); // and got it back
        assert!(!tracker.leadership_grace_expired("t", 0));
    }

    fn backdate_caught_up(tracker: &IsrTracker, node: u64, topic: &str, partition: i32, ms_ago: u64) {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
        tracker
            .follower_offsets
            .get_mut(&(node, (topic.to_string(), partition)))
            .unwrap()
            .last_caught_up_ms = now - ms_ago;
    }

    /// Fetching is not keeping up. Measured: two followers stuck at offset 100
    /// of a 1,000-record partition (a straddling batch they could not apply)
    /// fetched every few hundred ms, so the old "time since last update" never
    /// grew; they stayed in ISR, one was elected when the leader died, and every
    /// acknowledged acks=all record was lost.
    #[test]
    fn a_follower_that_fetches_without_catching_up_leaves_isr() {
        let tracker = IsrTracker::new(10_000, 10_000);
        tracker.record_follower_fetch(2, "sd", 0, 100, 1_000);
        assert_eq!(tracker.sync_state(2, "sd", 0, 1_000), SyncState::InSync, "first sighting: grace period");

        backdate_caught_up(&tracker, 2, "sd", 0, 11_000);
        tracker.record_follower_fetch(2, "sd", 0, 100, 1_000); // still fetching, still at 100
        assert_eq!(
            tracker.sync_state(2, "sd", 0, 1_000),
            SyncState::Lagging,
            "900 behind (inside the entry bound) and not caught up for 11s"
        );
    }

    /// Under continuous writes the leader's end moves between fetches, so a
    /// healthy follower never asks for the CURRENT end. Reaching where the end
    /// stood at its previous fetch is caught up as of that fetch (Kafka's rule),
    /// or every busy partition would lose its followers.
    #[test]
    fn a_follower_keeping_pace_with_a_moving_log_end_stays_in_sync() {
        let tracker = IsrTracker::new(10_000, 10_000);
        tracker.record_follower_fetch(2, "busy", 0, 90, 100);
        backdate_caught_up(&tracker, 2, "busy", 0, 60_000);

        tracker.record_follower_fetch(2, "busy", 0, 100, 110); // reached the previous end
        assert_eq!(tracker.sync_state(2, "busy", 0, 110), SyncState::InSync);

        backdate_caught_up(&tracker, 2, "busy", 0, 60_000);
        tracker.record_follower_fetch(2, "busy", 0, 105, 120); // fell short of it (110)
        assert_eq!(tracker.sync_state(2, "busy", 0, 120), SyncState::Lagging);
    }

    /// Callers hold a high watermark; "caught up" is about the log end the
    /// follower saw. A stuck follower must not pass by matching the watermark.
    #[test]
    fn caught_up_is_judged_against_the_log_end_not_the_watermark() {
        let tracker = IsrTracker::new(10_000, 10_000);
        tracker.record_follower_fetch(2, "sd", 0, 100, 1_000);
        backdate_caught_up(&tracker, 2, "sd", 0, 11_000);

        assert_eq!(tracker.leader_end_seen("sd", 0), Some(1_000));
        assert_eq!(
            crate::isr_publisher::in_sync_replicas(&tracker, "sd", 0, 100, &[1, 2], 1),
            Some(vec![1]),
            "a watermark of 100 must not make the follower at 100 look caught up"
        );
    }
}
