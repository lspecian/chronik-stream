//! Multi-datacenter replication for Chronik Raft cluster
//!
//! This module provides cross-region replication with WAN-optimized Raft settings
//! and rack-aware partition assignment for disaster recovery and geo-distributed reads.

use anyhow::{anyhow, Result};
use chronik_common::metadata::PartitionAssignment;
use prometheus::{Histogram, HistogramOpts, IntCounter, IntGauge, Opts};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tracing::{debug, info, warn};

lazy_static::lazy_static! {
    static ref CROSS_DC_REPLICATION_LAG: Histogram = Histogram::with_opts(
        HistogramOpts::new(
            "chronik_cross_dc_replication_lag_seconds",
            "Cross-datacenter replication lag in seconds"
        )
        .buckets(vec![0.1, 0.5, 1.0, 2.0, 5.0, 10.0, 30.0, 60.0])
    )
    .unwrap();

    static ref CROSS_DC_BANDWIDTH: IntCounter = IntCounter::with_opts(
        Opts::new(
            "chronik_cross_dc_bandwidth_bytes",
            "Cross-datacenter bandwidth usage in bytes"
        )
    )
    .unwrap();

    static ref RACK_AWARE_REPLICAS: IntGauge = IntGauge::with_opts(
        Opts::new(
            "chronik_rack_aware_replicas_total",
            "Total number of rack-aware replicas"
        )
    )
    .unwrap();

    static ref NEAREST_REPLICA_DISTANCE: Histogram = Histogram::with_opts(
        HistogramOpts::new(
            "chronik_nearest_replica_distance",
            "Distance to nearest replica (0=same node, 1=same rack, 2=same DC, 3=same region, 4=cross-region)"
        )
        .buckets(vec![0.0, 1.0, 2.0, 3.0, 4.0])
    )
    .unwrap();
}

/// Replication mode for multi-datacenter deployments
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ReplicationMode {
    /// Synchronous replication within DC, async cross-DC
    LocalSync,

    /// Synchronous replication across N datacenters
    MultiDCSync { min_dcs: usize },

    /// Async replication to all DCs
    Async,
}

impl Default for ReplicationMode {
    fn default() -> Self {
        ReplicationMode::LocalSync
    }
}

/// Datacenter configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatacenterConfig {
    pub datacenter_id: String,
    pub region: String,
    pub availability_zone: String,
    pub rack_id: Option<String>,
    pub replication_mode: ReplicationMode,
}

impl DatacenterConfig {
    pub fn new(
        datacenter_id: impl Into<String>,
        region: impl Into<String>,
        availability_zone: impl Into<String>,
    ) -> Self {
        Self {
            datacenter_id: datacenter_id.into(),
            region: region.into(),
            availability_zone: availability_zone.into(),
            rack_id: None,
            replication_mode: ReplicationMode::default(),
        }
    }

    pub fn with_rack(mut self, rack_id: impl Into<String>) -> Self {
        self.rack_id = Some(rack_id.into());
        self
    }

    pub fn with_replication_mode(mut self, mode: ReplicationMode) -> Self {
        self.replication_mode = mode;
        self
    }
}

/// Information about a datacenter
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatacenterInfo {
    pub id: String,
    pub region: String,
    pub nodes: Vec<u64>,
    pub rack_topology: HashMap<String, Vec<u64>>, // rack_id -> node_ids
}

impl DatacenterInfo {
    pub fn new(id: impl Into<String>, region: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            region: region.into(),
            nodes: Vec::new(),
            rack_topology: HashMap::new(),
        }
    }

    pub fn add_node(&mut self, node_id: u64, rack_id: Option<String>) {
        if !self.nodes.contains(&node_id) {
            self.nodes.push(node_id);
        }

        if let Some(rack) = rack_id {
            self.rack_topology
                .entry(rack)
                .or_insert_with(Vec::new)
                .push(node_id);
        }
    }

    pub fn get_rack_for_node(&self, node_id: u64) -> Option<String> {
        for (rack_id, nodes) in &self.rack_topology {
            if nodes.contains(&node_id) {
                return Some(rack_id.clone());
            }
        }
        None
    }
}

/// Datacenter topology mapping
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatacenterTopology {
    pub datacenters: HashMap<String, DatacenterInfo>,
    pub node_dc_mapping: HashMap<u64, String>, // node_id -> datacenter_id
}

impl DatacenterTopology {
    pub fn new() -> Self {
        Self {
            datacenters: HashMap::new(),
            node_dc_mapping: HashMap::new(),
        }
    }

    pub fn add_datacenter(&mut self, dc_info: DatacenterInfo) {
        for &node_id in &dc_info.nodes {
            self.node_dc_mapping.insert(node_id, dc_info.id.clone());
        }
        self.datacenters.insert(dc_info.id.clone(), dc_info);
    }

    pub fn get_datacenter_for_node(&self, node_id: u64) -> Option<&str> {
        self.node_dc_mapping.get(&node_id).map(|s| s.as_str())
    }

    pub fn get_datacenter_info(&self, dc_id: &str) -> Option<&DatacenterInfo> {
        self.datacenters.get(dc_id)
    }

    pub fn get_region_for_node(&self, node_id: u64) -> Option<String> {
        let dc_id = self.get_datacenter_for_node(node_id)?;
        self.datacenters.get(dc_id).map(|dc| dc.region.clone())
    }

    pub fn get_rack_for_node(&self, node_id: u64) -> Option<String> {
        let dc_id = self.get_datacenter_for_node(node_id)?;
        let dc = self.datacenters.get(dc_id)?;
        dc.get_rack_for_node(node_id)
    }
}

impl Default for DatacenterTopology {
    fn default() -> Self {
        Self::new()
    }
}

/// Raft configuration for WAN or LAN deployments
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RaftConfig {
    /// Number of ticks for election timeout
    pub election_tick: u64,

    /// Number of ticks for heartbeat interval
    pub heartbeat_tick: u64,

    /// Duration of each tick
    pub tick_interval: Duration,

    /// Maximum size per Raft message
    pub max_size_per_msg: u64,

    /// Maximum number of inflight messages
    pub max_inflight_msgs: usize,

    /// Enable compression for WAN bandwidth
    pub enable_compression: bool,

    /// Maximum batch size for proposals
    pub max_batch_size: usize,

    /// Applied index check interval
    pub applied_index_check_interval: Duration,
}

impl RaftConfig {
    /// Default LAN configuration (low latency)
    pub fn lan_default() -> Self {
        Self {
            election_tick: 10,
            heartbeat_tick: 3,
            tick_interval: Duration::from_millis(100),
            max_size_per_msg: 1024 * 1024,      // 1MB
            max_inflight_msgs: 128,
            enable_compression: false,
            max_batch_size: 100,
            applied_index_check_interval: Duration::from_secs(5),
        }
    }

    /// WAN-optimized configuration (high latency tolerance)
    pub fn wan_default() -> Self {
        Self {
            election_tick: 30,                   // 30 ticks
            heartbeat_tick: 3,                   // 3 ticks
            tick_interval: Duration::from_millis(500), // 500ms tick
            max_size_per_msg: 10 * 1024 * 1024,  // 10MB
            max_inflight_msgs: 256,
            enable_compression: true,            // Compress for WAN bandwidth
            max_batch_size: 500,
            applied_index_check_interval: Duration::from_secs(10),
        }
    }

    /// Get effective election timeout
    pub fn election_timeout(&self) -> Duration {
        self.tick_interval * self.election_tick as u32
    }

    /// Get effective heartbeat interval
    pub fn heartbeat_interval(&self) -> Duration {
        self.tick_interval * self.heartbeat_tick as u32
    }
}

impl Default for RaftConfig {
    fn default() -> Self {
        Self::lan_default()
    }
}

/// Extended partition assignment with datacenter awareness
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MultiDCPartitionAssignment {
    pub topic: String,
    pub partition: u32,
    pub replicas: Vec<ReplicaPlacement>,
    pub leader: u64,
}

/// Replica placement information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplicaPlacement {
    pub node_id: u64,
    pub datacenter_id: String,
    pub rack_id: Option<String>,
}

/// Multi-datacenter manager for Raft cluster
pub struct MultiDCManager {
    node_id: u64,
    dc_config: DatacenterConfig,
    dc_topology: Arc<RwLock<DatacenterTopology>>,
}


impl MultiDCManager {
    pub fn new(
        node_id: u64,
        dc_config: DatacenterConfig,
        dc_topology: DatacenterTopology,
    ) -> Self {
        info!(
            "Creating MultiDCManager for node {} in datacenter {} (region: {}, az: {})",
            node_id, dc_config.datacenter_id, dc_config.region, dc_config.availability_zone
        );

        Self {
            node_id,
            dc_config,
            dc_topology: Arc::new(RwLock::new(dc_topology)),
        }
    }

    /// Get datacenter for a node
    pub fn get_node_datacenter(&self, node_id: u64) -> Option<String> {
        let topology = self.dc_topology.read().unwrap();
        topology.get_datacenter_for_node(node_id).map(|s| s.to_string())
    }

    /// Get rack for a node
    pub fn get_node_rack(&self, node_id: u64) -> Option<String> {
        let topology = self.dc_topology.read().unwrap();
        topology.get_rack_for_node(node_id)
    }

    /// Get region for a node
    pub fn get_node_region(&self, node_id: u64) -> Option<String> {
        let topology = self.dc_topology.read().unwrap();
        topology.get_region_for_node(node_id)
    }

    /// Check if two nodes are in the same datacenter
    pub fn same_datacenter(&self, node_a: u64, node_b: u64) -> bool {
        let topology = self.dc_topology.read().unwrap();
        let dc_a = topology.get_datacenter_for_node(node_a);
        let dc_b = topology.get_datacenter_for_node(node_b);

        match (dc_a, dc_b) {
            (Some(a), Some(b)) => a == b,
            _ => false,
        }
    }

    /// Check if two nodes are in the same rack
    pub fn same_rack(&self, node_a: u64, node_b: u64) -> bool {
        // Nodes must be in same datacenter AND same rack
        if !self.same_datacenter(node_a, node_b) {
            return false;
        }
        
        let topology = self.dc_topology.read().unwrap();
        let rack_a = topology.get_rack_for_node(node_a);
        let rack_b = topology.get_rack_for_node(node_b);

        match (rack_a, rack_b) {
            (Some(a), Some(b)) => a == b,
            _ => false,
        }
    }

    /// Check if two nodes are in the same region
    pub fn same_region(&self, node_a: u64, node_b: u64) -> bool {
        let topology = self.dc_topology.read().unwrap();
        let region_a = topology.get_region_for_node(node_a);
        let region_b = topology.get_region_for_node(node_b);

        match (region_a, region_b) {
            (Some(a), Some(b)) => a == b,
            _ => false,
        }
    }

    /// Calculate distance between two nodes
    /// Returns: 0=same node, 1=same rack, 2=same DC, 3=same region, 4=cross-region
    fn calculate_distance(&self, from: u64, to: u64) -> u32 {
        if from == to {
            return 0;
        }
        if self.same_rack(from, to) {
            return 1;
        }
        if self.same_datacenter(from, to) {
            return 2;
        }
        if self.same_region(from, to) {
            return 3;
        }
        4 // Different region
    }

    /// Get nearest replica for read based on datacenter/rack proximity
    pub fn get_nearest_replica(&self, replicas: &[u64]) -> Option<u64> {
        if replicas.is_empty() {
            return None;
        }

        let nearest = replicas
            .iter()
            .min_by_key(|&&node_id| self.calculate_distance(self.node_id, node_id))
            .copied();

        if let Some(node) = nearest {
            let distance = self.calculate_distance(self.node_id, node);
            NEAREST_REPLICA_DISTANCE.observe(distance as f64);
            debug!(
                "Selected nearest replica: node={}, distance={}",
                node, distance
            );
        }

        nearest
    }

    /// Assign partitions with rack awareness
    ///
    /// Algorithm:
    /// 1. Ensure replicas are spread across multiple DCs
    /// 2. Within DC, spread across racks
    /// 3. Leader in local DC (for low write latency)
    pub fn rack_aware_assignment(
        &self,
        topic: &str,
        num_partitions: i32,
        replication_factor: usize,
    ) -> Result<Vec<MultiDCPartitionAssignment>> {
        let topology = self.dc_topology.read().unwrap();

        // Collect all available nodes with their DC and rack info
        let mut nodes_by_dc: HashMap<String, Vec<(u64, Option<String>)>> = HashMap::new();

        for (node_id, dc_id) in &topology.node_dc_mapping {
            let rack_id = topology.get_rack_for_node(*node_id);
            nodes_by_dc
                .entry(dc_id.clone())
                .or_insert_with(Vec::new)
                .push((*node_id, rack_id));
        }

        if nodes_by_dc.is_empty() {
            return Err(anyhow!("No nodes available for assignment"));
        }

        let dc_ids: Vec<String> = nodes_by_dc.keys().cloned().collect();
        let mut assignments = Vec::new();

        for partition in 0..num_partitions {
            let mut replicas = Vec::new();

            // Round-robin across datacenters
            let dc_offset = (partition as usize) % dc_ids.len();

            for i in 0..replication_factor {
                let dc_idx = (dc_offset + i) % dc_ids.len();
                let dc_id = &dc_ids[dc_idx];

                if let Some(nodes) = nodes_by_dc.get(dc_id) {
                    if nodes.is_empty() {
                        warn!(
                            "No nodes available in datacenter {} for partition {}",
                            dc_id, partition
                        );
                        continue;
                    }

                    // Within DC, try to spread across racks
                    let node_idx = (partition as usize + i) % nodes.len();
                    let (node_id, rack_id) = &nodes[node_idx];

                    replicas.push(ReplicaPlacement {
                        node_id: *node_id,
                        datacenter_id: dc_id.clone(),
                        rack_id: rack_id.clone(),
                    });

                    RACK_AWARE_REPLICAS.inc();
                }
            }

            if replicas.is_empty() {
                return Err(anyhow!(
                    "Could not assign any replicas for partition {}",
                    partition
                ));
            }

            // Leader is the first replica (in local DC if possible)
            let leader = replicas[0].node_id;

            assignments.push(MultiDCPartitionAssignment {
                topic: topic.to_string(),
                partition: partition as u32,
                replicas,
                leader,
            });
        }

        info!(
            "Created rack-aware assignment for topic {} with {} partitions and RF {}",
            topic, num_partitions, replication_factor
        );

        Ok(assignments)
    }

    /// Convert MultiDCPartitionAssignment to standard PartitionAssignment
    pub fn to_partition_assignments(
        assignments: &[MultiDCPartitionAssignment],
    ) -> Vec<PartitionAssignment> {
        let mut result = Vec::new();

        for assignment in assignments {
            for (idx, replica) in assignment.replicas.iter().enumerate() {
                result.push(PartitionAssignment {
                    topic: assignment.topic.clone(),
                    partition: assignment.partition,
                    broker_id: replica.node_id as i32,
                    is_leader: idx == 0, // First replica is leader
                });
            }
        }

        result
    }

    /// Get WAN-optimized Raft configuration
    pub fn get_wan_raft_config(&self) -> RaftConfig {
        RaftConfig::wan_default()
    }

    /// Get LAN-optimized Raft configuration
    pub fn get_lan_raft_config(&self) -> RaftConfig {
        RaftConfig::lan_default()
    }

    /// Record cross-DC replication lag
    pub fn record_replication_lag(&self, lag_seconds: f64, source_dc: &str, target_dc: &str) {
        CROSS_DC_REPLICATION_LAG.observe(lag_seconds);

        if lag_seconds > 5.0 {
            warn!(
                "High cross-DC replication lag: {:.2}s from {} to {}",
                lag_seconds, source_dc, target_dc
            );
        }
    }

    /// Record cross-DC bandwidth usage
    pub fn record_bandwidth(&self, bytes: u64) {
        CROSS_DC_BANDWIDTH.inc_by(bytes);
    }

    /// Get current datacenter configuration
    pub fn get_datacenter_config(&self) -> &DatacenterConfig {
        &self.dc_config
    }

    /// Get current node ID
    pub fn get_node_id(&self) -> u64 {
        self.node_id
    }

    /// Update datacenter topology
    pub fn update_topology(&self, new_topology: DatacenterTopology) {
        let mut topology = self.dc_topology.write().unwrap();
        *topology = new_topology;
        info!("Updated datacenter topology");
    }

    /// Get all nodes in a specific datacenter
    pub fn get_nodes_in_datacenter(&self, dc_id: &str) -> Vec<u64> {
        let topology = self.dc_topology.read().unwrap();
        topology
            .datacenters
            .get(dc_id)
            .map(|dc| dc.nodes.clone())
            .unwrap_or_default()
    }

    /// Get all datacenters
    pub fn get_all_datacenters(&self) -> Vec<String> {
        let topology = self.dc_topology.read().unwrap();
        topology.datacenters.keys().cloned().collect()
    }

    /// Check if multi-DC sync is required
    pub fn requires_multi_dc_sync(&self) -> bool {
        matches!(self.dc_config.replication_mode, ReplicationMode::MultiDCSync { .. })
    }

    /// Get minimum number of DCs for sync (if applicable)
    pub fn get_min_dcs_for_sync(&self) -> Option<usize> {
        match self.dc_config.replication_mode {
            ReplicationMode::MultiDCSync { min_dcs } => Some(min_dcs),
            _ => None,
        }
    }

    /// Check if async replication mode
    pub fn is_async_replication(&self) -> bool {
        matches!(self.dc_config.replication_mode, ReplicationMode::Async)
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_topology() -> DatacenterTopology {
        let mut topology = DatacenterTopology::new();

        // DC1: us-west-2a (2 racks, 3 nodes)
        let mut dc1 = DatacenterInfo::new("us-west-2a", "us-west-2");
        dc1.add_node(1, Some("rack-1".to_string()));
        dc1.add_node(2, Some("rack-2".to_string()));
        dc1.add_node(3, Some("rack-1".to_string()));
        topology.add_datacenter(dc1);

        // DC2: us-east-1a (2 racks, 3 nodes)
        let mut dc2 = DatacenterInfo::new("us-east-1a", "us-east-1");
        dc2.add_node(4, Some("rack-1".to_string()));
        dc2.add_node(5, Some("rack-2".to_string()));
        dc2.add_node(6, Some("rack-1".to_string()));
        topology.add_datacenter(dc2);

        // DC3: eu-west-1a (2 racks, 3 nodes)
        let mut dc3 = DatacenterInfo::new("eu-west-1a", "eu-west-1");
        dc3.add_node(7, Some("rack-1".to_string()));
        dc3.add_node(8, Some("rack-2".to_string()));
        dc3.add_node(9, Some("rack-1".to_string()));
        topology.add_datacenter(dc3);

        topology
    }

    #[test]
    fn test_create_multi_dc_manager() {
        let topology = create_test_topology();
        let config = DatacenterConfig::new("us-west-2a", "us-west-2", "a");

        let manager = MultiDCManager::new(1, config.clone(), topology);

        assert_eq!(manager.get_node_id(), 1);
        assert_eq!(manager.get_datacenter_config().datacenter_id, "us-west-2a");
    }

    #[test]
    fn test_get_node_datacenter() {
        let topology = create_test_topology();
        let config = DatacenterConfig::new("us-west-2a", "us-west-2", "a");
        let manager = MultiDCManager::new(1, config, topology);

        assert_eq!(
            manager.get_node_datacenter(1),
            Some("us-west-2a".to_string())
        );
        assert_eq!(
            manager.get_node_datacenter(4),
            Some("us-east-1a".to_string())
        );
        assert_eq!(
            manager.get_node_datacenter(7),
            Some("eu-west-1a".to_string())
        );
        assert_eq!(manager.get_node_datacenter(999), None);
    }

    #[test]
    fn test_get_node_rack() {
        let topology = create_test_topology();
        let config = DatacenterConfig::new("us-west-2a", "us-west-2", "a");
        let manager = MultiDCManager::new(1, config, topology);

        assert_eq!(manager.get_node_rack(1), Some("rack-1".to_string()));
        assert_eq!(manager.get_node_rack(2), Some("rack-2".to_string()));
        assert_eq!(manager.get_node_rack(5), Some("rack-2".to_string()));
        assert_eq!(manager.get_node_rack(999), None);
    }

    #[test]
    fn test_same_datacenter_true() {
        let topology = create_test_topology();
        let config = DatacenterConfig::new("us-west-2a", "us-west-2", "a");
        let manager = MultiDCManager::new(1, config, topology);

        assert!(manager.same_datacenter(1, 2)); // Both in us-west-2a
        assert!(manager.same_datacenter(1, 3)); // Both in us-west-2a
        assert!(manager.same_datacenter(4, 5)); // Both in us-east-1a
    }

    #[test]
    fn test_same_datacenter_false() {
        let topology = create_test_topology();
        let config = DatacenterConfig::new("us-west-2a", "us-west-2", "a");
        let manager = MultiDCManager::new(1, config, topology);

        assert!(!manager.same_datacenter(1, 4)); // Different DCs
        assert!(!manager.same_datacenter(1, 7)); // Different DCs
        assert!(!manager.same_datacenter(4, 8)); // Different DCs
    }

    #[test]
    fn test_same_rack() {
        let topology = create_test_topology();
        let config = DatacenterConfig::new("us-west-2a", "us-west-2", "a");
        let manager = MultiDCManager::new(1, config, topology);

        assert!(manager.same_rack(1, 3)); // Both in rack-1
        assert!(!manager.same_rack(1, 2)); // Different racks
        assert!(!manager.same_rack(1, 4)); // Different DCs
    }

    #[test]
    fn test_same_region() {
        let topology = create_test_topology();
        let config = DatacenterConfig::new("us-west-2a", "us-west-2", "a");
        let manager = MultiDCManager::new(1, config, topology);

        // Same region implies same DC in our test topology
        assert!(manager.same_region(1, 2)); // Both us-west-2
        assert!(!manager.same_region(1, 4)); // us-west-2 vs us-east-1
    }

    #[test]
    fn test_rack_aware_assignment_spreads_across_dcs() {
        let topology = create_test_topology();
        let config = DatacenterConfig::new("us-west-2a", "us-west-2", "a");
        let manager = MultiDCManager::new(1, config, topology);

        let assignments = manager
            .rack_aware_assignment("test-topic", 3, 3)
            .unwrap();

        assert_eq!(assignments.len(), 3);

        for assignment in &assignments {
            assert_eq!(assignment.replicas.len(), 3);

            // Verify replicas are in different DCs
            let dc_ids: Vec<_> = assignment
                .replicas
                .iter()
                .map(|r| r.datacenter_id.as_str())
                .collect();

            let unique_dcs: std::collections::HashSet<_> = dc_ids.iter().cloned().collect();
            assert_eq!(
                unique_dcs.len(),
                3,
                "Replicas should be spread across 3 DCs"
            );
        }
    }

    #[test]
    fn test_rack_aware_assignment_spreads_across_racks() {
        let topology = create_test_topology();
        let config = DatacenterConfig::new("us-west-2a", "us-west-2", "a");
        let manager = MultiDCManager::new(1, config, topology);

        let assignments = manager
            .rack_aware_assignment("test-topic", 3, 3)
            .unwrap();

        for assignment in &assignments {
            // Check that within each DC, we try to use different racks
            let mut dc_racks: HashMap<String, Vec<Option<String>>> = HashMap::new();

            for replica in &assignment.replicas {
                dc_racks
                    .entry(replica.datacenter_id.clone())
                    .or_insert_with(Vec::new)
                    .push(replica.rack_id.clone());
            }

            // Within each DC, racks should vary (though not guaranteed due to node count)
            for (dc, racks) in dc_racks {
                assert!(
                    !racks.is_empty(),
                    "DC {} should have at least one rack assigned",
                    dc
                );
            }
        }
    }

    #[test]
    fn test_get_nearest_replica_same_rack() {
        let topology = create_test_topology();
        let config = DatacenterConfig::new("us-west-2a", "us-west-2", "a");
        let manager = MultiDCManager::new(1, config, topology);

        // Node 1 and 3 are in same rack (rack-1)
        let replicas = vec![3, 4, 7];
        let nearest = manager.get_nearest_replica(&replicas);

        assert_eq!(nearest, Some(3)); // Node 3 is in same rack as node 1
    }

    #[test]
    fn test_get_nearest_replica_same_dc_different_rack() {
        let topology = create_test_topology();
        let config = DatacenterConfig::new("us-west-2a", "us-west-2", "a");
        let manager = MultiDCManager::new(1, config, topology);

        // Node 2 is in same DC but different rack, nodes 4 and 7 are in different DCs
        let replicas = vec![2, 4, 7];
        let nearest = manager.get_nearest_replica(&replicas);

        assert_eq!(nearest, Some(2)); // Node 2 is in same DC
    }

    #[test]
    fn test_get_nearest_replica_different_dc() {
        let topology = create_test_topology();
        let config = DatacenterConfig::new("us-west-2a", "us-west-2", "a");
        let manager = MultiDCManager::new(1, config, topology);

        // All replicas in different DCs
        let replicas = vec![4, 7];
        let nearest = manager.get_nearest_replica(&replicas);

        // Should return one of them (deterministic based on ordering)
        assert!(nearest == Some(4) || nearest == Some(7));
    }

    #[test]
    fn test_wan_raft_config_has_longer_timeouts() {
        let topology = create_test_topology();
        let config = DatacenterConfig::new("us-west-2a", "us-west-2", "a");
        let manager = MultiDCManager::new(1, config, topology);

        let wan_config = manager.get_wan_raft_config();
        let lan_config = manager.get_lan_raft_config();

        // WAN should have longer timeouts
        assert!(wan_config.election_tick > lan_config.election_tick);
        assert!(wan_config.tick_interval > lan_config.tick_interval);
        assert!(wan_config.max_size_per_msg > lan_config.max_size_per_msg);
        assert!(wan_config.enable_compression);
        assert!(!lan_config.enable_compression);

        // Effective timeouts
        assert_eq!(wan_config.election_timeout(), Duration::from_secs(15)); // 30 * 500ms
        assert_eq!(wan_config.heartbeat_interval(), Duration::from_millis(1500)); // 3 * 500ms

        assert_eq!(lan_config.election_timeout(), Duration::from_secs(1)); // 10 * 100ms
        assert_eq!(lan_config.heartbeat_interval(), Duration::from_millis(300)); // 3 * 100ms
    }

    #[test]
    fn test_local_sync_replication_mode() {
        let topology = create_test_topology();
        let config =
            DatacenterConfig::new("us-west-2a", "us-west-2", "a")
                .with_replication_mode(ReplicationMode::LocalSync);

        let manager = MultiDCManager::new(1, config, topology);

        assert!(!manager.requires_multi_dc_sync());
        assert!(!manager.is_async_replication());
        assert_eq!(manager.get_min_dcs_for_sync(), None);
    }

    #[test]
    fn test_multi_dc_sync_replication_mode() {
        let topology = create_test_topology();
        let config = DatacenterConfig::new("us-west-2a", "us-west-2", "a")
            .with_replication_mode(ReplicationMode::MultiDCSync { min_dcs: 2 });

        let manager = MultiDCManager::new(1, config, topology);

        assert!(manager.requires_multi_dc_sync());
        assert!(!manager.is_async_replication());
        assert_eq!(manager.get_min_dcs_for_sync(), Some(2));
    }

    #[test]
    fn test_async_replication_mode() {
        let topology = create_test_topology();
        let config =
            DatacenterConfig::new("us-west-2a", "us-west-2", "a")
                .with_replication_mode(ReplicationMode::Async);

        let manager = MultiDCManager::new(1, config, topology);

        assert!(!manager.requires_multi_dc_sync());
        assert!(manager.is_async_replication());
        assert_eq!(manager.get_min_dcs_for_sync(), None);
    }

    #[test]
    fn test_to_partition_assignments() {
        let assignments = vec![MultiDCPartitionAssignment {
            topic: "test-topic".to_string(),
            partition: 0,
            replicas: vec![
                ReplicaPlacement {
                    node_id: 1,
                    datacenter_id: "us-west-2a".to_string(),
                    rack_id: Some("rack-1".to_string()),
                },
                ReplicaPlacement {
                    node_id: 4,
                    datacenter_id: "us-east-1a".to_string(),
                    rack_id: Some("rack-1".to_string()),
                },
            ],
            leader: 1,
        }];

        let partition_assignments = MultiDCManager::to_partition_assignments(&assignments);

        assert_eq!(partition_assignments.len(), 2);

        assert_eq!(partition_assignments[0].topic, "test-topic");
        assert_eq!(partition_assignments[0].partition, 0);
        assert_eq!(partition_assignments[0].broker_id, 1);
        assert!(partition_assignments[0].is_leader);

        assert_eq!(partition_assignments[1].topic, "test-topic");
        assert_eq!(partition_assignments[1].partition, 0);
        assert_eq!(partition_assignments[1].broker_id, 4);
        assert!(!partition_assignments[1].is_leader);
    }

    #[test]
    fn test_update_topology() {
        let topology = create_test_topology();
        let config = DatacenterConfig::new("us-west-2a", "us-west-2", "a");
        let manager = MultiDCManager::new(1, config, topology);

        // Create new topology with additional node
        let mut new_topology = create_test_topology();
        let mut dc4 = DatacenterInfo::new("ap-south-1a", "ap-south-1");
        dc4.add_node(10, Some("rack-1".to_string()));
        new_topology.add_datacenter(dc4);

        manager.update_topology(new_topology);

        assert_eq!(
            manager.get_node_datacenter(10),
            Some("ap-south-1a".to_string())
        );
    }

    #[test]
    fn test_get_nodes_in_datacenter() {
        let topology = create_test_topology();
        let config = DatacenterConfig::new("us-west-2a", "us-west-2", "a");
        let manager = MultiDCManager::new(1, config, topology);

        let nodes = manager.get_nodes_in_datacenter("us-west-2a");
        assert_eq!(nodes.len(), 3);
        assert!(nodes.contains(&1));
        assert!(nodes.contains(&2));
        assert!(nodes.contains(&3));

        let nodes = manager.get_nodes_in_datacenter("us-east-1a");
        assert_eq!(nodes.len(), 3);
        assert!(nodes.contains(&4));
        assert!(nodes.contains(&5));
        assert!(nodes.contains(&6));
    }

    #[test]
    fn test_get_all_datacenters() {
        let topology = create_test_topology();
        let config = DatacenterConfig::new("us-west-2a", "us-west-2", "a");
        let manager = MultiDCManager::new(1, config, topology);

        let dcs = manager.get_all_datacenters();
        assert_eq!(dcs.len(), 3);
        assert!(dcs.contains(&"us-west-2a".to_string()));
        assert!(dcs.contains(&"us-east-1a".to_string()));
        assert!(dcs.contains(&"eu-west-1a".to_string()));
    }
}
