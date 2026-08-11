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

    /// Maximum lag in number of entries before marking out-of-sync
    max_lag_entries: u64,

    /// Maximum lag in milliseconds before marking out-of-sync
    max_lag_ms: u64,
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
            max_lag_entries,
            max_lag_ms,
        }
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

        // Caught up (or ahead) — in-sync regardless of elapsed time.
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

    /// Remove follower state (e.g., when node leaves cluster)
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
        self.follower_offsets
            .get(&key)
            .map(|state| leader_offset - state.last_offset)
    }
}

impl Default for IsrTracker {
    fn default() -> Self {
        Self::new(10_000, 10_000) // Default: 10K entries, 10s timeout
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
}
