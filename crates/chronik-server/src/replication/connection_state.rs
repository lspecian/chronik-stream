//! Connection state and setup
//!
//! Extracted from `handle_connection()` to reduce complexity.
//! Handles connection initialization with timeout monitoring and election workers.

use bytes::BytesMut;
use dashmap::DashMap;
use std::sync::{atomic::AtomicBool, Arc};
use tokio::sync::mpsc;
use tracing::info;

/// Connection state
///
/// Manages connection buffer and monitoring setup.
pub struct ConnectionState;

impl ConnectionState {
    /// Initialize connection buffer
    ///
    /// Complexity: < 5 (simple buffer creation)
    pub fn init_buffer() -> BytesMut {
        BytesMut::with_capacity(64 * 1024) // 64KB buffer
    }

    /// Setup timeout monitoring with channel-based elections
    ///
    /// Complexity: < 20 (spawns 2 background tasks with channel communication)
    pub fn setup_timeout_monitoring(
        leader_elector: &Option<Arc<crate::leader_election::LeaderElector>>,
        last_heartbeat: Arc<DashMap<(String, i32), std::time::Instant>>,
        shutdown: Arc<AtomicBool>,
    ) {
        // v2.2.7 DEADLOCK FIX: Spawn timeout monitor with channel-based elections
        // The monitor sends election requests to a channel instead of calling directly,
        // preventing deadlocks with raft_node lock
        if let Some(ref elector) = leader_elector {
            // Create election trigger channel
            let (election_tx, election_rx) = mpsc::unbounded_channel();

            // Spawn election worker that can safely lock raft_node
            let elector_clone = Arc::clone(elector);
            tokio::spawn(async move {
                crate::wal_replication::WalReceiver::run_election_worker(
                    election_rx,
                    elector_clone,
                )
                .await;
            });

            // Spawn timeout monitor that sends to channel (non-blocking)
            let last_heartbeat_clone = Arc::clone(&last_heartbeat);
            let shutdown_clone = Arc::clone(&shutdown);
            tokio::spawn(async move {
                crate::wal_replication::WalReceiver::monitor_timeouts(
                    election_tx,
                    last_heartbeat_clone,
                    shutdown_clone,
                )
                .await;
            });

            info!("WAL timeout monitoring ENABLED with channel-based elections (deadlock-free)");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_init_buffer() {
        let buffer = ConnectionState::init_buffer();
        assert_eq!(buffer.capacity(), 64 * 1024);
        assert_eq!(buffer.len(), 0);
    }
}
