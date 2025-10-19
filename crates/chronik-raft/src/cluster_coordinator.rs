//! Cluster Coordinator - Bootstrap and manage Raft cluster
//!
//! This module handles cluster initialization, quorum detection, and peer health
//! monitoring. It creates the special __meta partition for cluster metadata and
//! coordinates cluster-wide operations.

use crate::{PartitionReplica, RaftConfig, RaftError, RaftGroupManager, Result};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

/// Configuration for cluster bootstrap
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterConfig {
    /// This node's ID
    pub node_id: u64,

    /// List of all peer nodes in the cluster
    /// Format: node_id -> address (e.g., "node1:5001")
    pub peers: Vec<PeerConfig>,

    /// Quorum wait timeout (how long to wait for peers during bootstrap)
    pub quorum_wait_timeout: Duration,

    /// Heartbeat interval (how often to check peer health)
    pub heartbeat_interval: Duration,

    /// Peer timeout (mark peer as dead after this duration)
    pub peer_timeout: Duration,
}

/// Configuration for a peer node
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerConfig {
    /// Peer node ID
    pub node_id: u64,

    /// Peer address (e.g., "192.168.1.10:5001")
    pub address: String,
}

impl Default for ClusterConfig {
    fn default() -> Self {
        Self {
            node_id: 1,
            peers: Vec::new(),
            quorum_wait_timeout: Duration::from_secs(30),
            heartbeat_interval: Duration::from_secs(5),
            peer_timeout: Duration::from_secs(15),
        }
    }
}

/// Health information for a peer node
#[derive(Debug, Clone)]
pub struct PeerHealth {
    /// Last successful heartbeat
    pub last_heartbeat: Instant,

    /// Is the peer currently alive
    pub is_alive: bool,

    /// Peer address
    pub address: String,
}

impl PeerHealth {
    fn new(address: String) -> Self {
        Self {
            last_heartbeat: Instant::now(),
            is_alive: false,
            address,
        }
    }

    fn mark_alive(&mut self) {
        self.last_heartbeat = Instant::now();
        self.is_alive = true;
    }

    fn mark_dead(&mut self) {
        self.is_alive = false;
    }

    fn is_timeout(&self, timeout: Duration) -> bool {
        self.last_heartbeat.elapsed() > timeout
    }
}

/// Coordinates cluster bootstrap and peer management
pub struct ClusterCoordinator {
    /// This node's ID
    node_id: u64,

    /// Cluster configuration
    cluster_config: ClusterConfig,

    /// Raft group manager (shared)
    raft_group_manager: Arc<RaftGroupManager>,

    /// Special metadata partition replica (__meta/0)
    metadata_replica: Arc<PartitionReplica>,

    /// Peer health tracking
    peer_health: Arc<DashMap<u64, PeerHealth>>,

    /// Bootstrap completion flag
    bootstrap_complete: AtomicBool,

    /// Heartbeat task handle
    heartbeat_task: parking_lot::Mutex<Option<JoinHandle<()>>>,

    /// Shutdown signal
    shutdown: Arc<AtomicBool>,
}

impl ClusterCoordinator {
    /// Create a new cluster coordinator
    ///
    /// # Arguments
    /// * `cluster_config` - Cluster configuration
    /// * `raft_config` - Raft configuration for metadata partition
    /// * `raft_group_manager` - Shared Raft group manager
    ///
    /// # Returns
    /// A new ClusterCoordinator instance
    pub fn new(
        cluster_config: ClusterConfig,
        _raft_config: RaftConfig,
        raft_group_manager: Arc<RaftGroupManager>,
    ) -> Result<Self> {
        info!(
            "Creating ClusterCoordinator for node {} with {} peers",
            cluster_config.node_id,
            cluster_config.peers.len()
        );

        // Extract peer IDs for metadata partition
        let peer_ids: Vec<u64> = cluster_config
            .peers
            .iter()
            .filter(|p| p.node_id != cluster_config.node_id)
            .map(|p| p.node_id)
            .collect();

        // Create metadata partition replica (__meta/0)
        info!(
            "Creating metadata partition with peers: {:?}",
            peer_ids
        );

        let metadata_replica = raft_group_manager
            .get_or_create_replica("__meta", 0, peer_ids.clone())?;

        // Initialize peer health tracking
        let peer_health = Arc::new(DashMap::new());
        for peer in &cluster_config.peers {
            if peer.node_id != cluster_config.node_id {
                peer_health.insert(
                    peer.node_id,
                    PeerHealth::new(peer.address.clone()),
                );
            }
        }

        Ok(Self {
            node_id: cluster_config.node_id,
            cluster_config,
            raft_group_manager,
            metadata_replica,
            peer_health,
            bootstrap_complete: AtomicBool::new(false),
            heartbeat_task: parking_lot::Mutex::new(None),
            shutdown: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Bootstrap the cluster
    ///
    /// This performs the following steps:
    /// 1. Create metadata Raft group (__meta partition)
    /// 2. Wait for quorum (at least (N/2)+1 nodes alive)
    /// 3. If first node: Initialize cluster
    /// 4. If joining node: Request state from leader
    /// 5. Start heartbeat loop
    /// 6. Mark bootstrap complete
    ///
    /// # Returns
    /// Ok(()) if bootstrap successful, Err if failed
    pub async fn bootstrap(&self) -> Result<()> {
        info!(
            "Starting cluster bootstrap for node {} (cluster size: {})",
            self.node_id,
            self.get_cluster_size()
        );

        // Step 1: Metadata Raft group already created in constructor
        info!("Metadata Raft group created: __meta/0");

        // Step 2: Wait for quorum
        info!(
            "Waiting for quorum ({} nodes) with timeout {:?}",
            self.get_quorum_size(),
            self.cluster_config.quorum_wait_timeout
        );

        self.wait_for_quorum(self.cluster_config.quorum_wait_timeout)
            .await?;

        info!(
            "Quorum achieved ({}/{} nodes alive)",
            self.get_live_peers().len() + 1, // +1 for self
            self.get_cluster_size()
        );

        // Step 3: Initialize or join cluster
        if self.is_first_node() {
            info!("First node in cluster - initializing as leader");
            self.initialize_cluster().await?;
        } else {
            info!("Joining existing cluster");
            self.join_cluster().await?;
        }

        // Step 4: Start heartbeat loop
        info!(
            "Starting heartbeat loop (interval: {:?})",
            self.cluster_config.heartbeat_interval
        );
        self.spawn_heartbeat_loop();

        // Step 5: Mark bootstrap complete
        self.bootstrap_complete.store(true, Ordering::SeqCst);

        info!("Cluster bootstrap complete for node {}", self.node_id);

        Ok(())
    }

    /// Wait for quorum of peers to be available
    ///
    /// # Arguments
    /// * `timeout` - Maximum time to wait for quorum
    ///
    /// # Returns
    /// Ok(()) if quorum achieved, Err if timeout
    async fn wait_for_quorum(&self, timeout: Duration) -> Result<()> {
        let start = Instant::now();
        let quorum_size = self.get_quorum_size();

        loop {
            // Check health of all peers
            self.check_all_peers_health().await;

            // Count live peers (including self)
            let live_count = self.get_live_peers().len() + 1;

            debug!(
                "Quorum check: {}/{} nodes alive (need {})",
                live_count,
                self.get_cluster_size(),
                quorum_size
            );

            if live_count >= quorum_size {
                return Ok(());
            }

            // Check timeout
            if start.elapsed() > timeout {
                return Err(RaftError::Config(format!(
                    "Quorum timeout: only {}/{} nodes alive after {:?}",
                    live_count,
                    self.get_cluster_size(),
                    timeout
                )));
            }

            // Wait before next check
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    /// Check health of all peers
    async fn check_all_peers_health(&self) {
        for mut entry in self.peer_health.iter_mut() {
            let peer_id = *entry.key();
            let health = entry.value_mut();

            // Simple health check: try to connect to peer
            // In production, this would be a gRPC health check
            let is_alive = self.check_peer_health(&health.address).await;

            if is_alive {
                if !health.is_alive {
                    info!(peer_id, "Peer became alive");
                }
                health.mark_alive();
            } else if health.is_timeout(self.cluster_config.peer_timeout) {
                if health.is_alive {
                    warn!(peer_id, "Peer marked as dead (timeout)");
                }
                health.mark_dead();
            }
        }
    }

    /// Check if a single peer is healthy
    ///
    /// # Arguments
    /// * `address` - Peer address to check
    ///
    /// # Returns
    /// true if peer is reachable, false otherwise
    async fn check_peer_health(&self, address: &str) -> bool {
        // Simple TCP connect test
        // In production, this would be a proper gRPC health check
        use tokio::net::TcpStream;

        match tokio::time::timeout(
            Duration::from_secs(2),
            TcpStream::connect(address),
        )
        .await
        {
            Ok(Ok(_)) => true,
            _ => false,
        }
    }

    /// Initialize cluster (called by first node)
    async fn initialize_cluster(&self) -> Result<()> {
        info!("Initializing cluster as first node");

        // Campaign to become leader of metadata partition
        // Note: This is internal API for testing/bootstrap
        // In production, Raft will elect a leader automatically
        debug!("Waiting for metadata partition to elect leader");

        // Tick the metadata replica to trigger election
        for _ in 0..10 {
            self.metadata_replica.tick()?;
            tokio::time::sleep(Duration::from_millis(100)).await;

            // Check if we became leader
            if self.metadata_replica.is_leader() {
                info!("Became leader of metadata partition");
                break;
            }
        }

        if !self.metadata_replica.is_leader() {
            warn!("Did not become leader after 1s (may elect later)");
        }

        Ok(())
    }

    /// Join existing cluster (called by non-first nodes)
    async fn join_cluster(&self) -> Result<()> {
        info!("Joining cluster, waiting for metadata partition leader");

        // Wait for metadata partition to have a leader
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(30) {
            let leader_id = self.metadata_replica.leader_id();
            if leader_id != 0 {
                info!(leader_id, "Metadata partition has leader");
                return Ok(());
            }

            self.metadata_replica.tick()?;
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        Err(RaftError::Config(
            "Failed to join cluster: no leader elected within 30s".to_string(),
        ))
    }

    /// Spawn background heartbeat loop
    fn spawn_heartbeat_loop(&self) {
        let peer_health = self.peer_health.clone();
        let cluster_config = self.cluster_config.clone();
        let shutdown = self.shutdown.clone();

        let handle = tokio::spawn(async move {
            info!("Heartbeat loop started");

            while !shutdown.load(Ordering::Relaxed) {
                // Check health of all peers
                for mut entry in peer_health.iter_mut() {
                    let peer_id = *entry.key();
                    let health = entry.value_mut();
                    let address = health.address.clone();

                    // Check peer health (simple TCP connect)
                    let is_alive = Self::check_peer_health_static(&address).await;

                    if is_alive {
                        health.mark_alive();
                    } else if health.is_timeout(cluster_config.peer_timeout) {
                        if health.is_alive {
                            warn!(peer_id, "Peer failed health check");
                        }
                        health.mark_dead();
                    }
                }

                // Sleep until next heartbeat
                tokio::time::sleep(cluster_config.heartbeat_interval).await;
            }

            info!("Heartbeat loop stopped");
        });

        *self.heartbeat_task.lock() = Some(handle);
    }

    /// Static version of check_peer_health for use in spawned tasks
    async fn check_peer_health_static(address: &str) -> bool {
        use tokio::net::TcpStream;

        match tokio::time::timeout(
            Duration::from_secs(2),
            TcpStream::connect(address),
        )
        .await
        {
            Ok(Ok(_)) => true,
            _ => false,
        }
    }

    /// Check if bootstrap is complete
    pub fn is_bootstrap_complete(&self) -> bool {
        self.bootstrap_complete.load(Ordering::SeqCst)
    }

    /// Get list of currently alive peer IDs
    pub fn get_live_peers(&self) -> Vec<u64> {
        self.peer_health
            .iter()
            .filter(|entry| entry.value().is_alive)
            .map(|entry| *entry.key())
            .collect()
    }

    /// Get quorum size (majority: (N/2)+1)
    pub fn get_quorum_size(&self) -> usize {
        (self.get_cluster_size() / 2) + 1
    }

    /// Get total cluster size (including this node)
    pub fn get_cluster_size(&self) -> usize {
        self.cluster_config.peers.len() + 1
    }

    /// Check if this is the first node in the cluster
    fn is_first_node(&self) -> bool {
        // First node is the one with the lowest node ID
        self.cluster_config
            .peers
            .iter()
            .all(|p| p.node_id > self.node_id)
    }

    /// Get reference to metadata partition replica
    pub fn metadata_replica(&self) -> &Arc<PartitionReplica> {
        &self.metadata_replica
    }

    /// Shutdown the coordinator
    pub async fn shutdown(&self) {
        info!("Shutting down ClusterCoordinator");

        // Signal shutdown
        self.shutdown.store(true, Ordering::Relaxed);

        // Wait for heartbeat task
        if let Some(handle) = self.heartbeat_task.lock().take() {
            if let Err(e) = handle.await {
                error!("Error waiting for heartbeat task: {}", e);
            }
        }

        debug!("ClusterCoordinator shutdown complete");
    }
}

impl Drop for ClusterCoordinator {
    fn drop(&mut self) {
        // Signal shutdown
        self.shutdown.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::MemoryLogStorage;

    fn create_test_config(node_id: u64, peers: Vec<(u64, &str)>) -> ClusterConfig {
        ClusterConfig {
            node_id,
            peers: peers
                .into_iter()
                .map(|(id, addr)| PeerConfig {
                    node_id: id,
                    address: addr.to_string(),
                })
                .collect(),
            quorum_wait_timeout: Duration::from_secs(5),
            heartbeat_interval: Duration::from_millis(100),
            peer_timeout: Duration::from_secs(1),
        }
    }

    fn create_test_raft_config(node_id: u64) -> RaftConfig {
        RaftConfig {
            node_id,
            listen_addr: format!("127.0.0.1:{}", 5000 + node_id),
            election_timeout_ms: 300,
            heartbeat_interval_ms: 30,
            max_entries_per_batch: 100,
            snapshot_threshold: 10_000,
        }
    }

    #[test]
    fn test_quorum_calculation() {
        // 3-node cluster
        let config = create_test_config(1, vec![(2, "127.0.0.1:5002"), (3, "127.0.0.1:5003")]);
        let raft_config = create_test_raft_config(1);
        let manager = Arc::new(RaftGroupManager::new(
            1,
            raft_config.clone(),
            || Arc::new(MemoryLogStorage::new()),
        ));

        let coordinator = ClusterCoordinator::new(config, raft_config, manager).unwrap();

        assert_eq!(coordinator.get_cluster_size(), 3);
        assert_eq!(coordinator.get_quorum_size(), 2); // (3/2)+1 = 2

        // 5-node cluster
        let config = create_test_config(
            1,
            vec![
                (2, "127.0.0.1:5002"),
                (3, "127.0.0.1:5003"),
                (4, "127.0.0.1:5004"),
                (5, "127.0.0.1:5005"),
            ],
        );
        let raft_config = create_test_raft_config(1);
        let manager = Arc::new(RaftGroupManager::new(
            1,
            raft_config.clone(),
            || Arc::new(MemoryLogStorage::new()),
        ));

        let coordinator = ClusterCoordinator::new(config, raft_config, manager).unwrap();

        assert_eq!(coordinator.get_cluster_size(), 5);
        assert_eq!(coordinator.get_quorum_size(), 3); // (5/2)+1 = 3
    }

    #[test]
    fn test_first_node_detection() {
        // Node 1 with peers 2, 3
        let config = create_test_config(1, vec![(2, "127.0.0.1:5002"), (3, "127.0.0.1:5003")]);
        let raft_config = create_test_raft_config(1);
        let manager = Arc::new(RaftGroupManager::new(
            1,
            raft_config.clone(),
            || Arc::new(MemoryLogStorage::new()),
        ));

        let coordinator = ClusterCoordinator::new(config, raft_config, manager).unwrap();
        assert!(coordinator.is_first_node());

        // Node 2 with peers 1, 3
        let config = create_test_config(2, vec![(1, "127.0.0.1:5001"), (3, "127.0.0.1:5003")]);
        let raft_config = create_test_raft_config(2);
        let manager = Arc::new(RaftGroupManager::new(
            2,
            raft_config.clone(),
            || Arc::new(MemoryLogStorage::new()),
        ));

        let coordinator = ClusterCoordinator::new(config, raft_config, manager).unwrap();
        assert!(!coordinator.is_first_node());
    }

    #[test]
    fn test_bootstrap_complete_flag() {
        let config = create_test_config(1, vec![(2, "127.0.0.1:5002"), (3, "127.0.0.1:5003")]);
        let raft_config = create_test_raft_config(1);
        let manager = Arc::new(RaftGroupManager::new(
            1,
            raft_config.clone(),
            || Arc::new(MemoryLogStorage::new()),
        ));

        let coordinator = ClusterCoordinator::new(config, raft_config, manager).unwrap();

        assert!(!coordinator.is_bootstrap_complete());

        coordinator.bootstrap_complete.store(true, Ordering::SeqCst);
        assert!(coordinator.is_bootstrap_complete());
    }

    #[test]
    fn test_live_peers_initially_empty() {
        let config = create_test_config(1, vec![(2, "127.0.0.1:5002"), (3, "127.0.0.1:5003")]);
        let raft_config = create_test_raft_config(1);
        let manager = Arc::new(RaftGroupManager::new(
            1,
            raft_config.clone(),
            || Arc::new(MemoryLogStorage::new()),
        ));

        let coordinator = ClusterCoordinator::new(config, raft_config, manager).unwrap();

        // Initially no peers are alive (haven't done health checks yet)
        assert_eq!(coordinator.get_live_peers().len(), 0);
    }

    #[test]
    fn test_peer_health_initialization() {
        let config = create_test_config(1, vec![(2, "127.0.0.1:5002"), (3, "127.0.0.1:5003")]);
        let raft_config = create_test_raft_config(1);
        let manager = Arc::new(RaftGroupManager::new(
            1,
            raft_config.clone(),
            || Arc::new(MemoryLogStorage::new()),
        ));

        let coordinator = ClusterCoordinator::new(config, raft_config, manager).unwrap();

        // Should have 2 peer health entries
        assert_eq!(coordinator.peer_health.len(), 2);

        // Check peer health entries exist
        assert!(coordinator.peer_health.contains_key(&2));
        assert!(coordinator.peer_health.contains_key(&3));

        // Check addresses
        let peer2 = coordinator.peer_health.get(&2).unwrap();
        assert_eq!(peer2.address, "127.0.0.1:5002");
    }

    #[tokio::test]
    async fn test_shutdown() {
        let config = create_test_config(1, vec![(2, "127.0.0.1:5002"), (3, "127.0.0.1:5003")]);
        let raft_config = create_test_raft_config(1);
        let manager = Arc::new(RaftGroupManager::new(
            1,
            raft_config.clone(),
            || Arc::new(MemoryLogStorage::new()),
        ));

        let coordinator = ClusterCoordinator::new(config, raft_config, manager).unwrap();

        // Spawn heartbeat loop
        coordinator.spawn_heartbeat_loop();

        // Wait a bit
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Shutdown
        coordinator.shutdown().await;

        // Verify shutdown flag is set
        assert!(coordinator.shutdown.load(Ordering::Relaxed));
    }

    #[test]
    fn test_metadata_partition_created() {
        let config = create_test_config(1, vec![(2, "127.0.0.1:5002"), (3, "127.0.0.1:5003")]);
        let raft_config = create_test_raft_config(1);
        let manager = Arc::new(RaftGroupManager::new(
            1,
            raft_config.clone(),
            || Arc::new(MemoryLogStorage::new()),
        ));

        let coordinator = ClusterCoordinator::new(config, raft_config, manager.clone()).unwrap();

        // Verify metadata partition exists in manager
        assert!(manager.has_replica("__meta", 0));

        // Verify coordinator has reference
        assert_eq!(coordinator.metadata_replica().topic(), "__meta");
        assert_eq!(coordinator.metadata_replica().partition(), 0);
    }
}
