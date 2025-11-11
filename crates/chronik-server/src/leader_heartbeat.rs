//! Leader heartbeat infrastructure for Phase 3: Fast Follower Reads
//!
//! This module implements the heartbeat protocol between leader and followers:
//! - **HeartbeatSender**: Runs on leader, sends periodic heartbeats to followers
//! - **HeartbeatReceiver**: Runs on followers, receives heartbeats and updates leases
//!
//! **Protocol**:
//! 1. Leader broadcasts `LeaderHeartbeat` message every 1-2 seconds
//! 2. Followers receive heartbeat and update their LeaseManager
//! 3. Followers can now read from local state while lease is valid

use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::interval;
use crate::leader_lease::LeaseManager;

/// Heartbeat message sent from leader to followers
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaderHeartbeat {
    /// ID of the leader sending this heartbeat
    pub leader_id: u64,

    /// Current Raft term
    pub term: u64,

    /// Timestamp when heartbeat was sent (for monitoring only)
    pub timestamp: u64,
}

impl LeaderHeartbeat {
    /// Create a new heartbeat message
    pub fn new(leader_id: u64, term: u64) -> Self {
        Self {
            leader_id,
            term,
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        }
    }
}

/// Heartbeat sender (runs on leader)
///
/// Periodically sends heartbeats to all followers to maintain their leases.
pub struct HeartbeatSender {
    /// ID of this node (the leader)
    node_id: u64,

    /// How often to send heartbeats (default: 1 second)
    interval: Duration,
}

impl HeartbeatSender {
    /// Create a new HeartbeatSender
    ///
    /// # Arguments
    /// * `node_id` - ID of this node (the leader)
    /// * `interval` - How often to send heartbeats (recommended: 1s)
    pub fn new(node_id: u64, interval: Duration) -> Self {
        Self { node_id, interval }
    }

    /// Start the heartbeat loop (runs forever)
    ///
    /// This should be spawned as a Tokio task on the leader.
    ///
    /// # Arguments
    /// * `raft_cluster` - Reference to RaftCluster for broadcasting heartbeats
    ///
    /// # Example
    /// ```no_run
    /// let sender = HeartbeatSender::new(1, Duration::from_secs(1));
    /// tokio::spawn(async move {
    ///     sender.run(raft_cluster).await;
    /// });
    /// ```
    pub async fn run(
        self,
        raft_cluster: Arc<crate::raft_cluster::RaftCluster>,
    ) {
        let mut tick = interval(self.interval);
        let mut heartbeat_count = 0u64;

        tracing::info!(
            "🫀 Started heartbeat sender: node_id={}, interval={}ms",
            self.node_id,
            self.interval.as_millis()
        );

        loop {
            tick.tick().await;

            // Only send heartbeats if we're the leader
            if !raft_cluster.am_i_leader() {
                tracing::debug!("Not leader anymore, pausing heartbeats");
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }

            let term = raft_cluster.current_term();
            let heartbeat = LeaderHeartbeat::new(self.node_id, term);

            heartbeat_count += 1;
            tracing::trace!(
                "Sending heartbeat #{}: leader={}, term={}",
                heartbeat_count,
                self.node_id,
                term
            );

            // Broadcast to all followers
            if let Err(e) = raft_cluster.broadcast_heartbeat(heartbeat).await {
                tracing::warn!("Failed to broadcast heartbeat: {:?}", e);
            }
        }
    }
}

/// Heartbeat receiver (runs on followers)
///
/// Receives heartbeats from leader and updates the LeaseManager.
pub struct HeartbeatReceiver {
    /// Lease manager to update when heartbeats arrive
    lease_manager: Arc<LeaseManager>,

    /// ID of this node (for logging)
    node_id: u64,
}

impl HeartbeatReceiver {
    /// Create a new HeartbeatReceiver
    ///
    /// # Arguments
    /// * `node_id` - ID of this node (the follower)
    /// * `lease_manager` - LeaseManager to update on heartbeats
    pub fn new(node_id: u64, lease_manager: Arc<LeaseManager>) -> Self {
        Self {
            lease_manager,
            node_id,
        }
    }

    /// Handle an incoming heartbeat message
    ///
    /// Updates the lease manager with the new heartbeat information.
    ///
    /// # Arguments
    /// * `heartbeat` - The heartbeat message from the leader
    pub async fn handle_heartbeat(&self, heartbeat: LeaderHeartbeat) {
        tracing::trace!(
            "Received heartbeat: from_leader={}, term={}, node={}",
            heartbeat.leader_id,
            heartbeat.term,
            self.node_id
        );

        // Update lease manager
        self.lease_manager
            .update_lease(heartbeat.leader_id, heartbeat.term)
            .await;
    }

    /// Start the heartbeat receiver loop
    ///
    /// This polls for incoming heartbeat messages and processes them.
    /// Should be spawned as a Tokio task on followers.
    ///
    /// # Arguments
    /// * `raft_cluster` - Reference to RaftCluster for receiving heartbeats
    ///
    /// # Example
    /// ```no_run
    /// let receiver = HeartbeatReceiver::new(2, lease_manager);
    /// tokio::spawn(async move {
    ///     receiver.run(raft_cluster).await;
    /// });
    /// ```
    pub async fn run(
        self,
        raft_cluster: Arc<crate::raft_cluster::RaftCluster>,
    ) {
        tracing::info!(
            "🫀 Started heartbeat receiver: node_id={}",
            self.node_id
        );

        // Subscribe to heartbeat messages from RaftCluster
        let mut rx = raft_cluster.subscribe_heartbeats();

        loop {
            match rx.recv().await {
                Ok(heartbeat) => {
                    self.handle_heartbeat(heartbeat).await;
                }
                Err(e) => {
                    tracing::error!("Heartbeat receiver error: {:?}", e);
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_heartbeat_creation() {
        let hb = LeaderHeartbeat::new(1, 10);
        assert_eq!(hb.leader_id, 1);
        assert_eq!(hb.term, 10);
        assert!(hb.timestamp > 0);
    }

    #[test]
    fn test_heartbeat_serialization() {
        let hb = LeaderHeartbeat::new(1, 10);
        let bytes = bincode::serialize(&hb).unwrap();
        let deserialized: LeaderHeartbeat = bincode::deserialize(&bytes).unwrap();

        assert_eq!(deserialized.leader_id, hb.leader_id);
        assert_eq!(deserialized.term, hb.term);
        assert_eq!(deserialized.timestamp, hb.timestamp);
    }

    #[tokio::test]
    async fn test_heartbeat_receiver() {
        let lease_manager = Arc::new(LeaseManager::new(Duration::from_secs(5)));
        let receiver = HeartbeatReceiver::new(2, lease_manager.clone());

        // No lease initially
        assert!(!lease_manager.has_valid_lease().await);

        // Receive heartbeat
        let hb = LeaderHeartbeat::new(1, 10);
        receiver.handle_heartbeat(hb).await;

        // Lease should now be valid
        assert!(lease_manager.has_valid_lease().await);
        assert_eq!(lease_manager.get_leader_id().await, Some(1));
    }
}
