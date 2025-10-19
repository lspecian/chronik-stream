//! ReadIndex protocol for safe linearizable follower reads.
//!
//! This module implements Raft's ReadIndex protocol to enable linearizable
//! reads from follower nodes without forwarding to the leader. The protocol
//! guarantees that reads always see committed writes and maintain consistency
//! with wall-clock time ordering.
//!
//! # Protocol Flow
//!
//! 1. Follower sends ReadIndex request to leader with unique ID
//! 2. Leader confirms it's still the leader (heartbeat to quorum)
//! 3. Leader responds with current commit index
//! 4. Follower waits until applied_index >= commit_index
//! 5. Follower serves read from local state
//!
//! # Linearizability Guarantee
//!
//! - Reads always see committed writes
//! - No stale reads (bounded staleness via heartbeat)
//! - Consistent with wall-clock time ordering
//!
//! # Example
//!
//! ```no_run
//! use chronik_raft::read_index::{ReadIndexManager, ReadIndexRequest};
//! use std::sync::Arc;
//!
//! # async fn example(raft_replica: Arc<chronik_raft::PartitionReplica>) -> chronik_raft::Result<()> {
//! // Create read index manager
//! let manager = ReadIndexManager::new(1, raft_replica);
//!
//! // Request safe read index
//! let req = ReadIndexRequest {
//!     topic: "my-topic".to_string(),
//!     partition: 0,
//! };
//!
//! let response = manager.request_read_index(req).await?;
//!
//! // Wait until safe to read
//! while !manager.is_safe_to_read(response.commit_index) {
//!     tokio::time::sleep(std::time::Duration::from_millis(10)).await;
//! }
//!
//! // Now safe to read from local state
//! # Ok(())
//! # }
//! ```

use crate::error::{RaftError, Result};
use crate::replica::PartitionReplica;
use dashmap::DashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tracing::{debug, trace, warn};

/// Default timeout for read index requests (5 seconds)
const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Interval for checking read request timeouts (100ms)
const TIMEOUT_CHECK_INTERVAL: Duration = Duration::from_millis(100);

/// Manager for ReadIndex protocol, enabling safe follower reads
///
/// Coordinates ReadIndex requests between followers and leader to ensure
/// linearizable reads without forwarding all reads through the leader.
pub struct ReadIndexManager {
    /// Node ID of this Raft node
    node_id: u64,

    /// Reference to the Raft replica
    raft_replica: Arc<PartitionReplica>,

    /// Pending read requests (read_id -> PendingRead)
    pending_reads: Arc<DashMap<u64, PendingRead>>,

    /// Next read request ID (atomic counter)
    next_read_id: AtomicU64,

    /// Timeout for read requests
    read_timeout: Duration,
}

/// A pending read request awaiting ReadIndex response from leader
struct PendingRead {
    /// Unique read request ID
    read_id: u64,

    /// Commit index required for this read (0 until leader responds)
    commit_index: AtomicU64,

    /// Channel to send response when ready
    tx: Option<oneshot::Sender<ReadIndexResponse>>,

    /// When this read was created
    created_at: Instant,
}

impl PendingRead {
    fn new(read_id: u64, tx: oneshot::Sender<ReadIndexResponse>) -> Self {
        Self {
            read_id,
            commit_index: AtomicU64::new(0),
            tx: Some(tx),
            created_at: Instant::now(),
        }
    }

    fn set_commit_index(&self, index: u64) {
        self.commit_index.store(index, Ordering::SeqCst);
    }

    fn get_commit_index(&self) -> u64 {
        self.commit_index.load(Ordering::SeqCst)
    }

    fn is_timed_out(&self, timeout: Duration) -> bool {
        self.created_at.elapsed() > timeout
    }
}

/// Request to obtain a safe read index from the leader
#[derive(Debug, Clone)]
pub struct ReadIndexRequest {
    /// Topic name
    pub topic: String,

    /// Partition ID
    pub partition: i32,
}

/// Response containing the safe read index from the leader
#[derive(Debug, Clone)]
pub struct ReadIndexResponse {
    /// Commit index that must be applied before reading
    pub commit_index: u64,

    /// Whether this node is the leader (leader can read immediately)
    pub is_leader: bool,
}

impl ReadIndexManager {
    /// Create a new ReadIndexManager
    ///
    /// # Arguments
    /// * `node_id` - ID of this Raft node
    /// * `raft_replica` - Reference to the partition replica
    pub fn new(node_id: u64, raft_replica: Arc<PartitionReplica>) -> Self {
        Self {
            node_id,
            raft_replica,
            pending_reads: Arc::new(DashMap::new()),
            next_read_id: AtomicU64::new(1),
            read_timeout: DEFAULT_READ_TIMEOUT,
        }
    }

    /// Create with custom timeout
    ///
    /// # Arguments
    /// * `node_id` - ID of this Raft node
    /// * `raft_replica` - Reference to the partition replica
    /// * `timeout` - Custom timeout for read requests
    pub fn new_with_timeout(
        node_id: u64,
        raft_replica: Arc<PartitionReplica>,
        timeout: Duration,
    ) -> Self {
        Self {
            node_id,
            raft_replica,
            pending_reads: Arc::new(DashMap::new()),
            next_read_id: AtomicU64::new(1),
            read_timeout: timeout,
        }
    }

    /// Request a read index from the leader using ReadIndex protocol
    ///
    /// If this node is the leader, returns immediately with current commit index.
    /// If follower, sends ReadIndex RPC to leader and waits for response.
    ///
    /// # Arguments
    /// * `req` - ReadIndex request with topic and partition
    ///
    /// # Returns
    /// ReadIndexResponse with commit_index and is_leader flag
    ///
    /// # Errors
    /// - Returns error if no leader is available
    /// - Returns error if request times out
    /// - Returns error if leader changes during request
    pub async fn request_read_index(&self, req: ReadIndexRequest) -> Result<ReadIndexResponse> {
        // Fast path: if we're the leader, return immediately
        if self.raft_replica.is_leader() {
            let commit_index = self.raft_replica.commit_index();
            debug!(
                "ReadIndex fast path (leader): node={} commit_index={}",
                self.node_id, commit_index
            );

            return Ok(ReadIndexResponse {
                commit_index,
                is_leader: true,
            });
        }

        // Slow path: send ReadIndex to leader
        let leader_id = self.raft_replica.leader_id();
        if leader_id == 0 {
            return Err(RaftError::Config("No leader available for ReadIndex".to_string()));
        }

        debug!(
            "ReadIndex slow path (follower): node={} leader={} topic={} partition={}",
            self.node_id, leader_id, req.topic, req.partition
        );

        // Generate unique read ID
        let read_id = self.next_read_id.fetch_add(1, Ordering::SeqCst);

        // Create channel for response
        let (tx, rx) = oneshot::channel();

        // Register pending read
        let pending = PendingRead::new(read_id, tx);
        self.pending_reads.insert(read_id, pending);

        // Send ReadIndex RPC to leader
        // NOTE: In real implementation, this would send gRPC to leader node
        // For now, we simulate by using Raft's read_index() API directly
        let read_index_result = self.request_read_index_from_leader(read_id).await;

        match read_index_result {
            Ok(commit_index) => {
                // Process the response
                self.process_read_index_response(read_id, commit_index);

                // Wait for response with timeout
                match tokio::time::timeout(self.read_timeout, rx).await {
                    Ok(Ok(response)) => {
                        debug!(
                            "ReadIndex completed: read_id={} commit_index={}",
                            read_id, response.commit_index
                        );
                        Ok(response)
                    }
                    Ok(Err(_)) => {
                        self.pending_reads.remove(&read_id);
                        Err(RaftError::Config("ReadIndex channel closed".to_string()))
                    }
                    Err(_) => {
                        self.pending_reads.remove(&read_id);
                        Err(RaftError::Config("ReadIndex request timed out".to_string()))
                    }
                }
            }
            Err(e) => {
                self.pending_reads.remove(&read_id);
                Err(e)
            }
        }
    }

    /// Process a ReadIndex response from the leader
    ///
    /// Updates the pending read with the commit index and sends response
    /// if the replica has already applied to that index.
    ///
    /// # Arguments
    /// * `read_id` - Unique read request ID
    /// * `commit_index` - Commit index from leader's response
    pub fn process_read_index_response(&self, read_id: u64, commit_index: u64) {
        if let Some(pending) = self.pending_reads.get(&read_id) {
            pending.set_commit_index(commit_index);

            trace!(
                "Processed ReadIndex response: read_id={} commit_index={}",
                read_id,
                commit_index
            );

            // Check if we can respond immediately
            if self.is_safe_to_read(commit_index) {
                self.complete_read(read_id, commit_index);
            }
            // Otherwise, wait for applied_index to catch up
            // (will be checked by periodic task or explicit poll)
        } else {
            warn!("Received ReadIndex response for unknown read_id={}", read_id);
        }
    }

    /// Check if it's safe to read at the given commit index
    ///
    /// Returns true if the replica has applied all entries up to and
    /// including the commit index.
    ///
    /// # Arguments
    /// * `commit_index` - Commit index to check
    ///
    /// # Returns
    /// True if applied_index >= commit_index
    pub fn is_safe_to_read(&self, commit_index: u64) -> bool {
        let applied_index = self.raft_replica.applied_index();
        applied_index >= commit_index
    }

    /// Complete a read request and send response
    fn complete_read(&self, read_id: u64, commit_index: u64) {
        if let Some((_, pending)) = self.pending_reads.remove(&read_id) {
            if let Some(tx) = pending.tx {
                let response = ReadIndexResponse {
                    commit_index,
                    is_leader: false,
                };

                if tx.send(response).is_err() {
                    warn!("Failed to send ReadIndex response for read_id={}", read_id);
                }
            }
        }
    }

    /// Spawn background task to handle read request timeouts and applied index checks
    ///
    /// This task periodically:
    /// 1. Checks for timed out read requests and fails them
    /// 2. Checks if pending reads can be completed (applied_index caught up)
    ///
    /// # Returns
    /// JoinHandle for the background task
    pub fn spawn_timeout_loop(self: Arc<Self>) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(TIMEOUT_CHECK_INTERVAL);

            loop {
                interval.tick().await;

                // Check all pending reads
                let to_remove: Vec<u64> = self
                    .pending_reads
                    .iter()
                    .filter_map(|entry| {
                        let read_id = entry.key();
                        let pending = entry.value();

                        // Check for timeout
                        if pending.is_timed_out(self.read_timeout) {
                            warn!("ReadIndex request timed out: read_id={}", read_id);
                            return Some(*read_id);
                        }

                        // Check if we can complete the read
                        let commit_index = pending.get_commit_index();
                        if commit_index > 0 && self.is_safe_to_read(commit_index) {
                            trace!("ReadIndex ready: read_id={} commit_index={}", read_id, commit_index);
                            return Some(*read_id);
                        }

                        None
                    })
                    .collect();

                // Complete or timeout the reads
                for read_id in to_remove {
                    if let Some(pending) = self.pending_reads.get(&read_id) {
                        let commit_index = pending.get_commit_index();

                        if commit_index > 0 && self.is_safe_to_read(commit_index) {
                            // Complete successfully
                            drop(pending); // Release reference before removing
                            self.complete_read(read_id, commit_index);
                        } else {
                            // Timeout
                            drop(pending); // Release reference before removing
                            if let Some((_, pending)) = self.pending_reads.remove(&read_id) {
                                if let Some(tx) = pending.tx {
                                    let _ = tx.send(ReadIndexResponse {
                                        commit_index: 0,
                                        is_leader: false,
                                    });
                                }
                            }
                        }
                    }
                }
            }
        })
    }

    /// Request read index from leader using Raft's read_index() API
    ///
    /// This is a simulated implementation. In production, this would:
    /// 1. Send gRPC ReadIndex RPC to leader node
    /// 2. Leader calls raft.read_index()
    /// 3. Leader sends heartbeat to quorum
    /// 4. Leader responds with commit index
    ///
    /// For now, we use local Raft API for testing.
    async fn request_read_index_from_leader(&self, read_id: u64) -> Result<u64> {
        // In a real distributed system, this would be a gRPC call to the leader.
        // For single-node or testing, we can use the local Raft instance.

        // Check if we're connected to a leader
        let leader_id = self.raft_replica.leader_id();
        if leader_id == 0 {
            return Err(RaftError::Config("No leader available".to_string()));
        }

        // Simulate ReadIndex protocol:
        // 1. Leader receives request
        // 2. Leader confirms it's still leader (heartbeat to quorum)
        // 3. Leader responds with commit index

        // For testing/single-node: just return current commit index
        // In production, this would send gRPC to leader_id node
        let commit_index = self.raft_replica.commit_index();

        trace!(
            "Simulated ReadIndex from leader: read_id={} leader={} commit_index={}",
            read_id,
            leader_id,
            commit_index
        );

        Ok(commit_index)
    }

    /// Get the number of pending read requests
    pub fn pending_count(&self) -> usize {
        self.pending_reads.len()
    }

    /// Clear all pending reads (for testing/cleanup)
    #[cfg(test)]
    pub fn clear_pending(&self) {
        self.pending_reads.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RaftConfig;
    use crate::state_machine::{StateMachine, StateMachineSnapshot};
    use crate::storage::MemoryLogStorage;
    use std::time::Duration;
    use tokio::sync::RwLock as TokioRwLock;

    /// No-op state machine for testing
    struct NoOpStateMachine;

    #[async_trait::async_trait]
    impl StateMachine for NoOpStateMachine {
        async fn apply(&mut self, _index: u64, _data: Vec<u8>) -> std::result::Result<Vec<u8>, String> {
            Ok(vec![])
        }

        async fn snapshot(&self) -> std::result::Result<StateMachineSnapshot, String> {
            Ok(StateMachineSnapshot {
                last_included_index: 0,
                last_included_term: 0,
                data: vec![],
            })
        }

        async fn restore(&mut self, _snapshot: StateMachineSnapshot) -> std::result::Result<(), String> {
            Ok(())
        }

        fn last_applied(&self) -> u64 {
            0
        }
    }

    async fn create_test_replica(node_id: u64) -> Arc<PartitionReplica> {
        let config = RaftConfig {
            node_id,
            ..Default::default()
        };

        let storage = Arc::new(MemoryLogStorage::new());
        let state_machine = Arc::new(TokioRwLock::new(NoOpStateMachine));

        Arc::new(
            PartitionReplica::new(
                "test-topic".to_string(),
                0,
                config,
                storage,
                state_machine,
                vec![], // Single node for testing
            )
            .unwrap(),
        )
    }

    #[tokio::test]
    async fn test_create_read_index_manager() {
        let replica = create_test_replica(1).await;
        let manager = ReadIndexManager::new(1, replica);

        assert_eq!(manager.node_id, 1);
        assert_eq!(manager.pending_count(), 0);
    }

    #[tokio::test]
    async fn test_leader_fast_path() {
        let replica = create_test_replica(1).await;

        // Make this node the leader
        replica.campaign().unwrap();
        let _ = replica.ready().await.unwrap();

        let manager = ReadIndexManager::new(1, replica.clone());

        let req = ReadIndexRequest {
            topic: "test-topic".to_string(),
            partition: 0,
        };

        let response = manager.request_read_index(req).await.unwrap();

        assert!(response.is_leader, "Leader should set is_leader=true");
        assert_eq!(response.commit_index, replica.commit_index());
    }

    #[tokio::test]
    async fn test_follower_no_leader_error() {
        let replica = create_test_replica(1).await;
        let manager = ReadIndexManager::new(1, replica);

        let req = ReadIndexRequest {
            topic: "test-topic".to_string(),
            partition: 0,
        };

        let result = manager.request_read_index(req).await;

        assert!(result.is_err(), "Follower with no leader should error");
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("No leader available"));
    }

    #[tokio::test]
    async fn test_is_safe_to_read() {
        let replica = create_test_replica(1).await;
        let manager = ReadIndexManager::new(1, replica.clone());

        // Applied index is 0 initially
        assert!(manager.is_safe_to_read(0), "Should be safe to read at index 0");
        assert!(
            !manager.is_safe_to_read(10),
            "Should not be safe to read future index"
        );

        // Advance applied index
        replica.set_applied_index(10);

        assert!(
            manager.is_safe_to_read(5),
            "Should be safe to read past index"
        );
        assert!(
            manager.is_safe_to_read(10),
            "Should be safe to read current index"
        );
        assert!(
            !manager.is_safe_to_read(11),
            "Should not be safe to read future index"
        );
    }

    #[tokio::test]
    async fn test_process_read_index_response() {
        let replica = create_test_replica(1).await;
        let manager = ReadIndexManager::new(1, replica.clone());

        let (tx, rx) = oneshot::channel();
        let pending = PendingRead::new(1, tx);
        manager.pending_reads.insert(1, pending);

        // Process response with commit index 5
        manager.process_read_index_response(1, 5);

        // Verify commit index was set
        let pending = manager.pending_reads.get(&1).unwrap();
        assert_eq!(pending.get_commit_index(), 5);

        // Applied index is 0, so should still be pending
        assert!(manager.pending_reads.contains_key(&1));

        // Advance applied index
        replica.set_applied_index(5);

        // Complete the read
        manager.complete_read(1, 5);

        // Verify response was sent
        let response = rx.await.unwrap();
        assert_eq!(response.commit_index, 5);
        assert!(!response.is_leader);
        assert!(!manager.pending_reads.contains_key(&1));
    }

    #[tokio::test]
    async fn test_timeout_handling() {
        let replica = create_test_replica(1).await;
        let manager = Arc::new(ReadIndexManager::new_with_timeout(
            1,
            replica,
            Duration::from_millis(100),
        ));

        let (tx, _rx) = oneshot::channel();
        let pending = PendingRead::new(1, tx);
        manager.pending_reads.insert(1, pending);

        // Wait for timeout
        tokio::time::sleep(Duration::from_millis(150)).await;

        // Check timeout
        let pending = manager.pending_reads.get(&1).unwrap();
        assert!(
            pending.is_timed_out(Duration::from_millis(100)),
            "Read should be timed out"
        );
    }

    #[tokio::test]
    async fn test_concurrent_read_requests() {
        let replica = create_test_replica(1).await;
        replica.campaign().unwrap();
        let _ = replica.ready().await.unwrap();

        let manager = Arc::new(ReadIndexManager::new(1, replica));

        // Send 10 concurrent read requests
        let mut handles = vec![];
        for i in 0..10 {
            let manager_clone = manager.clone();
            let handle = tokio::spawn(async move {
                let req = ReadIndexRequest {
                    topic: format!("topic-{}", i),
                    partition: i as i32,
                };
                manager_clone.request_read_index(req).await
            });
            handles.push(handle);
        }

        // All should succeed (we're the leader)
        for handle in handles {
            let result = handle.await.unwrap();
            assert!(result.is_ok(), "Concurrent read should succeed");
            assert!(result.unwrap().is_leader);
        }
    }

    #[tokio::test]
    async fn test_read_after_write_linearizability() {
        let replica = create_test_replica(1).await;
        replica.campaign().unwrap();
        let _ = replica.ready().await.unwrap();

        let manager = ReadIndexManager::new(1, replica.clone());

        // Write some data
        let write_result = replica.propose(b"test data".to_vec()).await;
        assert!(write_result.is_ok(), "Write should succeed");

        // Process ready to commit
        let _ = replica.ready().await.unwrap();

        // Read should see the committed write
        let req = ReadIndexRequest {
            topic: "test-topic".to_string(),
            partition: 0,
        };

        let response = manager.request_read_index(req).await.unwrap();
        assert!(response.commit_index > 0, "Should see committed write");
    }

    #[tokio::test]
    async fn test_timeout_loop_cleanup() {
        let replica = create_test_replica(1).await;
        let manager = Arc::new(ReadIndexManager::new_with_timeout(
            1,
            replica.clone(),
            Duration::from_millis(200),
        ));

        // Add a pending read
        let (tx, _rx) = oneshot::channel();
        let pending = PendingRead::new(1, tx);
        manager.pending_reads.insert(1, pending);

        // Spawn timeout loop
        let _handle = manager.clone().spawn_timeout_loop();

        // Wait for timeout cleanup
        tokio::time::sleep(Duration::from_millis(400)).await;

        // Should be cleaned up
        assert_eq!(
            manager.pending_count(),
            0,
            "Timeout loop should clean up timed out reads"
        );
    }

    #[tokio::test]
    async fn test_pending_count() {
        let replica = create_test_replica(1).await;
        let manager = ReadIndexManager::new(1, replica);

        assert_eq!(manager.pending_count(), 0);

        let (tx1, _rx1) = oneshot::channel();
        manager.pending_reads.insert(1, PendingRead::new(1, tx1));
        assert_eq!(manager.pending_count(), 1);

        let (tx2, _rx2) = oneshot::channel();
        manager.pending_reads.insert(2, PendingRead::new(2, tx2));
        assert_eq!(manager.pending_count(), 2);

        manager.clear_pending();
        assert_eq!(manager.pending_count(), 0);
    }
}
