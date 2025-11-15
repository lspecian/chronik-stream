/// ISR ACK Tracker for acks=-1 Support (v2.2.7 Phase 4)
///
/// This component tracks pending acks=-1 produce requests and notifies producers
/// when ISR quorum is reached. It bridges the gap between:
/// 1. ProduceHandler waiting for quorum (tokio::sync::oneshot::Receiver)
/// 2. Followers sending ACKs via WAL replication stream
/// 3. WalReplicationManager receiving ACKs and recording them here
///
/// Design:
/// - Lock-free for record_ack() (hot path) using DashMap
/// - Single background cleanup task for expired entries
/// - Timeout: 30 seconds (Kafka default for request.timeout.ms)

use anyhow::Result;
use dashmap::DashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::oneshot;
use tracing::{debug, info, warn};

/// Tracks acks=-1 requests waiting for ISR quorum
pub struct IsrAckTracker {
    /// Pending acks: (topic, partition, offset) → WaitEntry
    /// DashMap provides lock-free reads and concurrent writes
    pending: DashMap<PendingKey, WaitEntry>,
}

/// Key for pending map
type PendingKey = (String, i32, i64);  // (topic, partition, offset)

/// Entry for a single pending acks=-1 request
struct WaitEntry {
    /// Channel to notify when quorum reached
    /// CRITICAL: Must be Option to allow taking ownership when notifying
    tx: Option<oneshot::Sender<Result<()>>>,

    /// Required quorum size (e.g., 2 for 3 replicas)
    /// Calculated as ceil(replicas / 2)
    quorum_size: usize,

    /// Nodes that have ACKed so far
    /// Note: Vec is fine here since ISR is typically small (< 10 nodes)
    acked_nodes: Vec<u64>,

    /// Timestamp when registered (for timeout tracking)
    timestamp: Instant,
}

impl IsrAckTracker {
    /// Create a new ISR ACK tracker
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            pending: DashMap::new(),
        })
    }

    /// Register a new acks=-1 request waiting for quorum
    ///
    /// Called by ProduceHandler after WAL write completes.
    /// The producer will block on the rx channel until quorum is reached.
    ///
    /// # Arguments
    /// * `topic` - Topic name
    /// * `partition` - Partition number
    /// * `offset` - Base offset of the batch
    /// * `quorum_size` - Number of ACKs needed (e.g., ceil(replicas / 2))
    /// * `tx` - Channel to notify when quorum reached
    pub fn register_wait(
        &self,
        topic: String,
        partition: i32,
        offset: i64,
        quorum_size: usize,
        tx: oneshot::Sender<Result<()>>,
    ) {
        let key = (topic.clone(), partition, offset);

        info!(
            "📝 IsrAckTracker: Registered acks=-1 wait: {}-{} offset {} (quorum: {})",
            topic, partition, offset, quorum_size
        );

        self.pending.insert(key, WaitEntry {
            tx: Some(tx),
            quorum_size,
            acked_nodes: Vec::new(),
            timestamp: Instant::now(),
        });

        info!(
            "📊 IsrAckTracker: Total pending requests: {}",
            self.pending.len()
        );
    }

    /// Record an ACK from a follower
    ///
    /// Called by WalReplicationManager when receiving ACK frame from follower.
    /// This is the HOT PATH - must be fast and lock-free.
    ///
    /// # Arguments
    /// * `topic` - Topic name
    /// * `partition` - Partition number
    /// * `offset` - Base offset that was ACKed
    /// * `node_id` - Node ID of the follower that sent the ACK
    pub fn record_ack(&self, topic: &str, partition: i32, offset: i64, node_id: u64) {
        let key = (topic.to_string(), partition, offset);

        info!(
            "📨 IsrAckTracker: record_ack() called for {}-{} offset {} from node {} (pending count: {})",
            topic, partition, offset, node_id, self.pending.len()
        );

        // Check if we have a pending wait for this offset
        if let Some(mut entry) = self.pending.get_mut(&key) {
            // Add node to acked list (if not already present)
            if !entry.acked_nodes.contains(&node_id) {
                entry.acked_nodes.push(node_id);

                info!(
                    "✅ IsrAckTracker: Received ACK from node {} for {}-{} offset {} ({}/{} ACKs)",
                    node_id, topic, partition, offset,
                    entry.acked_nodes.len(),
                    entry.quorum_size
                );
            } else {
                // Duplicate ACK (shouldn't happen in normal operation)
                warn!(
                    "⚠️  IsrAckTracker: Duplicate ACK from node {} for {}-{} offset {}",
                    node_id, topic, partition, offset
                );
                return;
            }

            // Check if quorum reached
            if entry.acked_nodes.len() >= entry.quorum_size {
                // Quorum reached! Notify waiting producer
                let ack_count = entry.acked_nodes.len();
                let quorum = entry.quorum_size;

                info!(
                    "🎉 IsrAckTracker: QUORUM REACHED for {}-{} offset {}: {}/{} ACKs - notifying producer",
                    topic, partition, offset, ack_count, quorum
                );

                // Take ownership of tx (must drop RefMut before removing from map)
                let tx = entry.tx.take();
                drop(entry);  // Release RefMut

                // Remove from pending map
                if let Some((_, wait_entry)) = self.pending.remove(&key) {
                    // Notify producer
                    if let Some(tx) = tx.or(wait_entry.tx) {
                        let _ = tx.send(Ok(()));
                        info!(
                            "✅ IsrAckTracker: Producer notified for {}-{} offset {}: {}/{} ACKs",
                            topic, partition, offset, ack_count, quorum
                        );
                    } else {
                        warn!(
                            "⚠️  IsrAckTracker: No channel to notify for {}-{} offset {}",
                            topic, partition, offset
                        );
                    }
                } else {
                    warn!(
                        "⚠️  IsrAckTracker: Failed to remove entry for {}-{} offset {}",
                        topic, partition, offset
                    );
                }
            }
        } else {
            // ACK for offset that's not being tracked
            // This can happen if:
            // 1. Producer timed out and we cleaned up
            // 2. ACK arrived after quorum already reached
            // 3. Follower sent unsolicited ACK
            warn!(
                "⚠️  IsrAckTracker: Received ACK for NON-TRACKED offset: {}-{} offset {} from node {}",
                topic, partition, offset, node_id
            );
        }
    }

    /// Clean up timed-out entries
    ///
    /// Called periodically from background task (every 5 seconds).
    /// Removes entries older than `timeout` and notifies with timeout error.
    ///
    /// # Arguments
    /// * `timeout` - How long to wait before timing out (default: 30s)
    pub fn cleanup_expired(&self, timeout: Duration) {
        let now = Instant::now();
        let mut expired_keys = Vec::new();

        // First pass: identify expired entries
        for entry in self.pending.iter() {
            if now.duration_since(entry.timestamp) > timeout {
                expired_keys.push(entry.key().clone());
            }
        }

        // Second pass: remove and notify
        for key in expired_keys {
            if let Some((_, mut wait_entry)) = self.pending.remove(&key) {
                warn!(
                    "⏱️  ISR quorum timeout for {}-{} offset {}: {}/{} ACKs received",
                    key.0, key.1, key.2,
                    wait_entry.acked_nodes.len(),
                    wait_entry.quorum_size
                );

                // Take ownership of tx and notify
                if let Some(tx) = wait_entry.tx.take() {
                    let _ = tx.send(Err(anyhow::anyhow!(
                        "ISR quorum timeout for {}-{} offset {} ({}/{} ACKs)",
                        key.0, key.1, key.2,
                        wait_entry.acked_nodes.len(),
                        wait_entry.quorum_size
                    )));
                }
            }
        }
    }

    /// Get current number of pending acks=-1 requests (for metrics)
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Get detailed stats (for debugging/monitoring)
    pub fn stats(&self) -> IsrAckStats {
        let mut total_acks = 0;
        let mut waiting_for_quorum = 0;
        let mut oldest_age_ms = 0u64;

        for entry in self.pending.iter() {
            total_acks += entry.acked_nodes.len();
            if entry.acked_nodes.len() < entry.quorum_size {
                waiting_for_quorum += 1;
            }
            let age_ms = entry.timestamp.elapsed().as_millis() as u64;
            if age_ms > oldest_age_ms {
                oldest_age_ms = age_ms;
            }
        }

        IsrAckStats {
            pending_requests: self.pending.len(),
            total_acks_received: total_acks,
            waiting_for_quorum,
            oldest_request_age_ms: oldest_age_ms,
        }
    }
}

/// Statistics for ISR ACK tracker (for monitoring)
#[derive(Debug, Clone)]
pub struct IsrAckStats {
    /// Number of pending acks=-1 requests
    pub pending_requests: usize,
    /// Total ACKs received across all pending requests
    pub total_acks_received: usize,
    /// Number of requests still waiting for quorum
    pub waiting_for_quorum: usize,
    /// Age of oldest pending request in milliseconds
    pub oldest_request_age_ms: u64,
}

impl Default for IsrAckTracker {
    fn default() -> Self {
        Self {
            pending: DashMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::sync::oneshot;

    #[tokio::test]
    async fn test_quorum_reached() {
        let tracker = IsrAckTracker::new();
        let (tx, rx) = oneshot::channel();

        // Register wait for quorum of 2
        tracker.register_wait("test".to_string(), 0, 100, 2, tx);

        // First ACK (not enough)
        tracker.record_ack("test", 0, 100, 1);
        assert_eq!(tracker.pending_count(), 1);

        // Second ACK (quorum reached!)
        tracker.record_ack("test", 0, 100, 2);

        // Should be removed from pending
        assert_eq!(tracker.pending_count(), 0);

        // Should notify via channel
        assert!(rx.await.is_ok());
    }

    #[tokio::test]
    async fn test_timeout() {
        let tracker = IsrAckTracker::new();
        let (tx, rx) = oneshot::channel();

        // Register wait for quorum of 2
        tracker.register_wait("test".to_string(), 0, 100, 2, tx);

        // Only one ACK
        tracker.record_ack("test", 0, 100, 1);

        // Clean up with very short timeout
        tracker.cleanup_expired(Duration::from_millis(1));
        tokio::time::sleep(Duration::from_millis(10)).await;
        tracker.cleanup_expired(Duration::from_millis(1));

        // Should be removed
        assert_eq!(tracker.pending_count(), 0);

        // Should receive timeout error
        let result = rx.await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_err());
    }

    #[tokio::test]
    async fn test_duplicate_ack() {
        let tracker = IsrAckTracker::new();
        let (tx, _rx) = oneshot::channel();

        tracker.register_wait("test".to_string(), 0, 100, 2, tx);

        // Same node ACKs twice
        tracker.record_ack("test", 0, 100, 1);
        tracker.record_ack("test", 0, 100, 1);  // Duplicate

        // Should still be waiting (only 1 unique ACK)
        assert_eq!(tracker.pending_count(), 1);
    }

    #[test]
    fn test_stats() {
        let tracker = IsrAckTracker::new();
        let (tx1, _rx1) = oneshot::channel();
        let (tx2, _rx2) = oneshot::channel();

        tracker.register_wait("test".to_string(), 0, 100, 2, tx1);
        tracker.register_wait("test".to_string(), 1, 200, 2, tx2);

        tracker.record_ack("test", 0, 100, 1);
        tracker.record_ack("test", 1, 200, 1);
        tracker.record_ack("test", 1, 200, 2);  // Second one reaches quorum

        let stats = tracker.stats();
        assert_eq!(stats.pending_requests, 1);  // One removed after quorum
        assert_eq!(stats.waiting_for_quorum, 1);
    }
}
