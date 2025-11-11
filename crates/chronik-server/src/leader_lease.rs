//! Leader lease management for Phase 3: Fast Follower Reads
//!
//! This module implements leader leases that allow followers to safely read
//! from their local replicated state without forwarding to the leader.
//!
//! **How it works**:
//! 1. Leader sends periodic heartbeats to followers (every 1-2 seconds)
//! 2. Followers update their lease: "I know leader is alive until T + 5 seconds"
//! 3. If lease valid: follower reads from local state (1-2ms, no RPC)
//! 4. If lease expired: follower forwards to leader (2-5ms, safe fallback)
//!
//! **Safety guarantee**: Bounded staleness (max 5 seconds)

use std::time::{Duration, Instant};
use std::sync::Arc;
use tokio::sync::RwLock;

/// Leader lease tracker for followers
///
/// Tracks when leader last sent heartbeat and when lease expires.
/// Used to determine if follower can safely read from local state.
#[derive(Debug, Clone)]
pub struct LeaderLease {
    /// ID of the current leader
    pub leader_id: u64,

    /// Raft term of the leader
    pub term: u64,

    /// When we last received a heartbeat from the leader
    pub last_heartbeat: Instant,

    /// When the lease expires (last_heartbeat + lease_duration)
    pub lease_expires_at: Instant,
}

impl LeaderLease {
    /// Create a new lease from a heartbeat
    pub fn new(leader_id: u64, term: u64, lease_duration: Duration) -> Self {
        let now = Instant::now();
        Self {
            leader_id,
            term,
            last_heartbeat: now,
            lease_expires_at: now + lease_duration,
        }
    }

    /// Check if lease is still valid
    ///
    /// Returns true if current time < lease_expires_at
    pub fn is_valid(&self) -> bool {
        Instant::now() < self.lease_expires_at
    }

    /// Time remaining until lease expires (0 if already expired)
    pub fn time_remaining(&self) -> Duration {
        let now = Instant::now();
        if now < self.lease_expires_at {
            self.lease_expires_at.duration_since(now)
        } else {
            Duration::from_secs(0)
        }
    }

    /// Update lease from new heartbeat
    ///
    /// Extends the lease expiry time to now + lease_duration
    pub fn update(&mut self, leader_id: u64, term: u64, lease_duration: Duration) {
        self.leader_id = leader_id;
        self.term = term;
        self.last_heartbeat = Instant::now();
        self.lease_expires_at = Instant::now() + lease_duration;
    }
}

/// Leader lease manager for a node
///
/// Manages the current leader lease state for a follower node.
/// Provides thread-safe access to lease information.
pub struct LeaseManager {
    /// Current lease (None if no leader or lease expired)
    current_lease: Arc<RwLock<Option<LeaderLease>>>,

    /// How long each lease lasts (default: 5 seconds)
    lease_duration: Duration,
}

impl LeaseManager {
    /// Create a new LeaseManager
    ///
    /// # Arguments
    /// * `lease_duration` - How long each lease is valid (recommended: 5s)
    pub fn new(lease_duration: Duration) -> Self {
        Self {
            current_lease: Arc::new(RwLock::new(None)),
            lease_duration,
        }
    }

    /// Update lease from a heartbeat message
    ///
    /// Called when follower receives heartbeat from leader.
    /// Creates new lease or extends existing one.
    pub async fn update_lease(&self, leader_id: u64, term: u64) {
        let mut lease = self.current_lease.write().await;
        match lease.as_mut() {
            Some(existing) => {
                existing.update(leader_id, term, self.lease_duration);
                tracing::debug!(
                    "Extended leader lease: leader={}, term={}, expires_in={:.1}s",
                    leader_id,
                    term,
                    existing.time_remaining().as_secs_f64()
                );
            }
            None => {
                let new_lease = LeaderLease::new(leader_id, term, self.lease_duration);
                tracing::info!(
                    "✅ Established leader lease: leader={}, term={}, valid_for={}s",
                    leader_id,
                    term,
                    self.lease_duration.as_secs()
                );
                *lease = Some(new_lease);
            }
        }
    }

    /// Check if we have a valid lease
    ///
    /// Returns true if lease exists and hasn't expired
    pub async fn has_valid_lease(&self) -> bool {
        let lease = self.current_lease.read().await;
        lease.as_ref().map(|l| l.is_valid()).unwrap_or(false)
    }

    /// Get current leader ID (if lease valid)
    ///
    /// Returns Some(leader_id) if lease is valid, None otherwise
    pub async fn get_leader_id(&self) -> Option<u64> {
        let lease = self.current_lease.read().await;
        lease.as_ref().filter(|l| l.is_valid()).map(|l| l.leader_id)
    }

    /// Get lease status for debugging
    pub async fn get_status(&self) -> LeaseStatus {
        let lease = self.current_lease.read().await;
        match lease.as_ref() {
            Some(l) if l.is_valid() => LeaseStatus::Valid {
                leader_id: l.leader_id,
                term: l.term,
                time_remaining: l.time_remaining(),
            },
            Some(l) => LeaseStatus::Expired {
                leader_id: l.leader_id,
                term: l.term,
                expired_ago: Instant::now().duration_since(l.lease_expires_at),
            },
            None => LeaseStatus::NoLease,
        }
    }

    /// Invalidate the current lease
    ///
    /// Called when we detect leader change or term change
    pub async fn invalidate(&self) {
        let mut lease = self.current_lease.write().await;
        if let Some(old_lease) = lease.take() {
            tracing::warn!(
                "Invalidated leader lease: leader={}, term={}",
                old_lease.leader_id,
                old_lease.term
            );
        }
    }
}

/// Lease status for monitoring/debugging
#[derive(Debug, Clone)]
pub enum LeaseStatus {
    /// Valid lease exists
    Valid {
        leader_id: u64,
        term: u64,
        time_remaining: Duration,
    },
    /// Lease exists but expired
    Expired {
        leader_id: u64,
        term: u64,
        expired_ago: Duration,
    },
    /// No lease established
    NoLease,
}

impl std::fmt::Display for LeaseStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LeaseStatus::Valid { leader_id, term, time_remaining } => {
                write!(f, "Valid(leader={}, term={}, remaining={:.1}s)",
                    leader_id, term, time_remaining.as_secs_f64())
            }
            LeaseStatus::Expired { leader_id, term, expired_ago } => {
                write!(f, "Expired(leader={}, term={}, expired_ago={:.1}s)",
                    leader_id, term, expired_ago.as_secs_f64())
            }
            LeaseStatus::NoLease => write!(f, "NoLease"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::sleep;

    #[tokio::test]
    async fn test_lease_creation() {
        let manager = LeaseManager::new(Duration::from_secs(5));

        // No lease initially
        assert!(!manager.has_valid_lease().await);
        assert_eq!(manager.get_leader_id().await, None);

        // Establish lease
        manager.update_lease(1, 10).await;
        assert!(manager.has_valid_lease().await);
        assert_eq!(manager.get_leader_id().await, Some(1));
    }

    #[tokio::test]
    async fn test_lease_expiry() {
        let manager = LeaseManager::new(Duration::from_millis(100));

        // Establish lease
        manager.update_lease(1, 10).await;
        assert!(manager.has_valid_lease().await);

        // Wait for expiry
        sleep(Duration::from_millis(150)).await;
        assert!(!manager.has_valid_lease().await);
        assert_eq!(manager.get_leader_id().await, None);
    }

    #[tokio::test]
    async fn test_lease_renewal() {
        let manager = LeaseManager::new(Duration::from_millis(100));

        // Establish lease
        manager.update_lease(1, 10).await;

        // Wait 50ms (halfway to expiry)
        sleep(Duration::from_millis(50)).await;
        assert!(manager.has_valid_lease().await);

        // Renew lease
        manager.update_lease(1, 10).await;

        // Wait another 75ms (would have expired without renewal)
        sleep(Duration::from_millis(75)).await;
        assert!(manager.has_valid_lease().await);
    }

    #[tokio::test]
    async fn test_lease_invalidation() {
        let manager = LeaseManager::new(Duration::from_secs(5));

        // Establish lease
        manager.update_lease(1, 10).await;
        assert!(manager.has_valid_lease().await);

        // Invalidate
        manager.invalidate().await;
        assert!(!manager.has_valid_lease().await);
    }

    #[tokio::test]
    async fn test_lease_status() {
        let manager = LeaseManager::new(Duration::from_millis(100));

        // No lease
        match manager.get_status().await {
            LeaseStatus::NoLease => {},
            other => panic!("Expected NoLease, got {:?}", other),
        }

        // Valid lease
        manager.update_lease(1, 10).await;
        match manager.get_status().await {
            LeaseStatus::Valid { leader_id, term, .. } => {
                assert_eq!(leader_id, 1);
                assert_eq!(term, 10);
            }
            other => panic!("Expected Valid, got {:?}", other),
        }

        // Expired lease
        sleep(Duration::from_millis(150)).await;
        match manager.get_status().await {
            LeaseStatus::Expired { leader_id, term, .. } => {
                assert_eq!(leader_id, 1);
                assert_eq!(term, 10);
            }
            other => panic!("Expected Expired, got {:?}", other),
        }
    }
}
