//! ISR (In-Sync Replica) Tracker (v2.5.0 Phase 3)
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
        let key = (node_id, (topic.to_string(), partition));

        match self.follower_offsets.get(&key) {
            Some(state) => {
                // Check offset lag
                let offset_lag = leader_offset - state.last_offset;
                if offset_lag > self.max_lag_entries as i64 {
                    return false;
                }

                // Check time lag
                let now_ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64;
                let time_lag_ms = now_ms - state.last_update_ms;
                if time_lag_ms > self.max_lag_ms {
                    return false;
                }

                true
            }
            None => {
                // No state recorded - not in-sync
                false
            }
        }
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
}
