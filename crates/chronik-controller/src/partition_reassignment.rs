//! Partition reassignment and rolling upgrade management.
//!
//! Provides functionality for safely moving partitions between brokers
//! and performing rolling upgrades of cluster nodes without downtime.

use crate::{
    ControllerState, TopicConfig, BrokerInfo,
    metadata_sync::{MetadataSyncManager, MetadataUpdate},
    backend_client::BackendServiceManager,
};
use chronik_common::{Result, Error};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, SystemTime, Instant};
use tokio::sync::{RwLock, mpsc};
use tracing::{info, warn, error, debug, span, Level};
use uuid::Uuid;

/// Partition reassignment manager
pub struct PartitionReassignmentManager {
    /// Cluster state reference
    cluster_state: Arc<RwLock<ControllerState>>,
    
    /// Metadata sync manager
    sync_manager: Arc<MetadataSyncManager>,
    
    /// Backend service manager
    backend_manager: Option<Arc<BackendServiceManager>>,
    
    /// Active reassignment operations
    active_reassignments: Arc<RwLock<HashMap<String, PartitionReassignment>>>,
    
    /// Rolling upgrade operations
    rolling_upgrades: Arc<RwLock<HashMap<String, RollingUpgrade>>>,
    
    /// Operation channel for async processing
    operation_sender: mpsc::UnboundedSender<ReassignmentOperation>,
    
    /// Operation receiver
    operation_receiver: Arc<RwLock<Option<mpsc::UnboundedReceiver<ReassignmentOperation>>>>,
    
    /// Configuration
    config: ReassignmentConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReassignmentConfig {
    /// Maximum concurrent reassignments
    pub max_concurrent_reassignments: usize,
    
    /// Throttle rate for data movement (bytes per second)
    pub throttle_rate: u64,
    
    /// Timeout for reassignment operations
    #[serde(with = "serde_duration")]
    pub reassignment_timeout: Duration,
    
    /// Timeout for rolling upgrade operations
    #[serde(with = "serde_duration")]
    pub upgrade_timeout: Duration,
    
    /// Minimum ISR size during reassignment
    pub min_isr_size: usize,
    
    /// Health check interval
    #[serde(with = "serde_duration")]
    pub health_check_interval: Duration,
    
    /// Maximum retries for failed operations
    pub max_retries: usize,
}

impl Default for ReassignmentConfig {
    fn default() -> Self {
        Self {
            max_concurrent_reassignments: 3,
            throttle_rate: 100_000_000, // 100 MB/s
            reassignment_timeout: Duration::from_secs(3600), // 1 hour
            upgrade_timeout: Duration::from_secs(1800), // 30 minutes
            min_isr_size: 1,
            health_check_interval: Duration::from_secs(30),
            max_retries: 3,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartitionReassignment {
    /// Unique operation ID
    pub id: String,
    
    /// Topic name
    pub topic: String,
    
    /// Partition assignments to change
    pub partitions: Vec<PartitionAssignment>,
    
    /// Current status
    pub status: ReassignmentStatus,
    
    /// Progress information
    pub progress: ReassignmentProgress,
    
    /// Creation time
    pub created_at: SystemTime,
    
    /// Last updated time
    pub updated_at: SystemTime,
    
    /// Error information if failed
    pub error: Option<String>,
    
    /// Reassignment configuration
    pub config: ReassignmentConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartitionAssignment {
    /// Partition ID
    pub partition_id: u32,
    
    /// Current replica set
    pub current_replicas: Vec<u32>,
    
    /// Target replica set
    pub target_replicas: Vec<u32>,
    
    /// Current leader
    pub current_leader: Option<u32>,
    
    /// Target leader (if specified)
    pub target_leader: Option<u32>,
    
    /// ISR at start of reassignment
    pub initial_isr: Vec<u32>,
    
    /// Current ISR
    pub current_isr: Vec<u32>,
    
    /// Bytes transferred so far
    pub bytes_transferred: u64,
    
    /// Total bytes to transfer
    pub total_bytes: u64,
    
    /// Status of this partition reassignment
    pub status: PartitionReassignmentStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ReassignmentStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum PartitionReassignmentStatus {
    Pending,
    AddingReplicas,
    CatchingUp,
    RemovingReplicas,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReassignmentProgress {
    /// Number of partitions completed
    pub partitions_completed: usize,
    
    /// Total number of partitions
    pub total_partitions: usize,
    
    /// Total bytes transferred
    pub bytes_transferred: u64,
    
    /// Total bytes to transfer
    pub total_bytes: u64,
    
    /// Estimated completion time
    pub estimated_completion: Option<SystemTime>,
    
    /// Current throughput (bytes per second)
    pub current_throughput: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RollingUpgrade {
    /// Unique operation ID
    pub id: String,
    
    /// Upgrade type
    pub upgrade_type: UpgradeType,
    
    /// Target version/image
    pub target_version: String,
    
    /// Nodes to upgrade
    pub nodes: Vec<NodeUpgrade>,
    
    /// Current status
    pub status: ReassignmentStatus,
    
    /// Creation time
    pub created_at: SystemTime,
    
    /// Last updated time
    pub updated_at: SystemTime,
    
    /// Error information
    pub error: Option<String>,
    
    /// Upgrade configuration
    pub config: UpgradeConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum UpgradeType {
    Controller,
    Ingest,
    Search,
    All,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeUpgrade {
    /// Node ID
    pub node_id: u32,
    
    /// Node type
    pub node_type: UpgradeType,
    
    /// Current version
    pub current_version: String,
    
    /// Target version
    pub target_version: String,
    
    /// Upgrade status
    pub status: NodeUpgradeStatus,
    
    /// Start time
    pub started_at: Option<SystemTime>,
    
    /// Completion time
    pub completed_at: Option<SystemTime>,
    
    /// Error information
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum NodeUpgradeStatus {
    Pending,
    DrainStarted,
    Draining,
    Upgrading,
    Starting,
    Validating,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpgradeConfig {
    /// Maximum number of nodes to upgrade concurrently
    pub max_concurrent_upgrades: usize,
    
    /// Drain timeout per node
    #[serde(with = "serde_duration")]
    pub drain_timeout: Duration,
    
    /// Startup timeout per node
    #[serde(with = "serde_duration")]
    pub startup_timeout: Duration,
    
    /// Validation timeout per node
    #[serde(with = "serde_duration")]
    pub validation_timeout: Duration,
    
    /// Wait time between node upgrades
    #[serde(with = "serde_duration")]
    pub inter_node_delay: Duration,
}

mod serde_duration {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::time::Duration;

    pub fn serialize<S>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        duration.as_secs().serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Duration, D::Error>
    where
        D: Deserializer<'de>,
    {
        let secs = u64::deserialize(deserializer)?;
        Ok(Duration::from_secs(secs))
    }
}

impl Default for UpgradeConfig {
    fn default() -> Self {
        Self {
            max_concurrent_upgrades: 1,
            drain_timeout: Duration::from_secs(300),
            startup_timeout: Duration::from_secs(180),
            validation_timeout: Duration::from_secs(120),
            inter_node_delay: Duration::from_secs(30),
        }
    }
}

#[derive(Debug, Clone)]
enum ReassignmentOperation {
    StartReassignment(String),
    UpdateProgress(String, ReassignmentProgress),
    CompleteReassignment(String),
    FailReassignment(String, String),
    StartUpgrade(String),
    UpdateUpgrade(String, String),
    CompleteUpgrade(String),
    FailUpgrade(String, String),
}

impl PartitionReassignmentManager {
    /// Create new partition reassignment manager
    pub fn new(
        cluster_state: Arc<RwLock<ControllerState>>,
        sync_manager: Arc<MetadataSyncManager>,
        config: ReassignmentConfig,
    ) -> Self {
        let (operation_sender, operation_receiver) = mpsc::unbounded_channel();
        
        Self {
            cluster_state,
            sync_manager,
            backend_manager: None,
            active_reassignments: Arc::new(RwLock::new(HashMap::new())),
            rolling_upgrades: Arc::new(RwLock::new(HashMap::new())),
            operation_sender,
            operation_receiver: Arc::new(RwLock::new(Some(operation_receiver))),
            config,
        }
    }
    
    /// Set the backend service manager
    pub fn with_backend_manager(mut self, backend_manager: Arc<BackendServiceManager>) -> Self {
        self.backend_manager = Some(backend_manager);
        self
    }
    
    /// Start the reassignment manager
    pub async fn start(self: Arc<Self>) {
        info!("Starting partition reassignment manager");
        
        // Start operation processor
        self.clone().start_operation_processor().await;
        
        // Start health checker
        self.clone().start_health_checker().await;
    }
    
    /// Start operation processor
    async fn start_operation_processor(self: Arc<Self>) {
        let mut receiver = {
            let mut receiver_lock = self.operation_receiver.write().await;
            receiver_lock.take().expect("Operation receiver should be available")
        };
        
        tokio::spawn(async move {
            while let Some(operation) = receiver.recv().await {
                if let Err(e) = self.process_operation(operation).await {
                    error!("Failed to process reassignment operation: {}", e);
                }
            }
        });
    }
    
    /// Start health checker
    async fn start_health_checker(self: Arc<Self>) {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(self.config.health_check_interval);
            
            loop {
                interval.tick().await;
                
                if let Err(e) = self.check_operation_health().await {
                    error!("Health check failed: {}", e);
                }
            }
        });
    }
    
    /// Start a partition reassignment
    pub async fn start_partition_reassignment(
        &self,
        topic: String,
        partition_assignments: Vec<PartitionAssignment>,
    ) -> Result<String> {
        let _span = span!(Level::INFO, "start_partition_reassignment", topic = %topic);
        let _guard = _span.enter();
        
        info!("Starting partition reassignment for topic {}", topic);
        
        // Validate the reassignment request
        self.validate_reassignment_request(&topic, &partition_assignments).await?;
        
        // Create reassignment operation
        let reassignment_id = Uuid::new_v4().to_string();
        let reassignment = PartitionReassignment {
            id: reassignment_id.clone(),
            topic: topic.clone(),
            partitions: partition_assignments.clone(),
            status: ReassignmentStatus::Pending,
            progress: ReassignmentProgress {
                partitions_completed: 0,
                total_partitions: partition_assignments.len(),
                bytes_transferred: 0,
                total_bytes: partition_assignments.iter().map(|p| p.total_bytes).sum(),
                estimated_completion: None,
                current_throughput: 0,
            },
            created_at: SystemTime::now(),
            updated_at: SystemTime::now(),
            error: None,
            config: self.config.clone(),
        };
        
        // Store the reassignment
        {
            let mut reassignments = self.active_reassignments.write().await;
            reassignments.insert(reassignment_id.clone(), reassignment);
        }
        
        // Trigger the reassignment
        self.operation_sender.send(ReassignmentOperation::StartReassignment(reassignment_id.clone()))
            .map_err(|e| Error::Internal(format!("Failed to queue reassignment: {}", e)))?;
        
        info!("Partition reassignment {} queued for topic {}", reassignment_id, topic);
        Ok(reassignment_id)
    }
    
    /// Start a rolling upgrade
    pub async fn start_rolling_upgrade(
        &self,
        upgrade_type: UpgradeType,
        target_version: String,
        config: Option<UpgradeConfig>,
    ) -> Result<String> {
        let _span = span!(Level::INFO, "start_rolling_upgrade", upgrade_type = ?upgrade_type);
        let _guard = _span.enter();
        
        info!("Starting rolling upgrade to version {} for {:?}", target_version, upgrade_type);
        
        // Get nodes to upgrade
        let nodes = self.get_nodes_for_upgrade(&upgrade_type).await?;
        
        // Create upgrade operation
        let upgrade_id = Uuid::new_v4().to_string();
        let upgrade = RollingUpgrade {
            id: upgrade_id.clone(),
            upgrade_type: upgrade_type.clone(),
            target_version: target_version.clone(),
            nodes,
            status: ReassignmentStatus::Pending,
            created_at: SystemTime::now(),
            updated_at: SystemTime::now(),
            error: None,
            config: config.unwrap_or_default(),
        };
        
        // Store the upgrade
        {
            let mut upgrades = self.rolling_upgrades.write().await;
            upgrades.insert(upgrade_id.clone(), upgrade);
        }
        
        // Trigger the upgrade
        self.operation_sender.send(ReassignmentOperation::StartUpgrade(upgrade_id.clone()))
            .map_err(|e| Error::Internal(format!("Failed to queue upgrade: {}", e)))?;
        
        info!("Rolling upgrade {} queued for {:?}", upgrade_id, upgrade_type);
        Ok(upgrade_id)
    }
    
    /// Get status of a partition reassignment
    pub async fn get_reassignment_status(&self, reassignment_id: &str) -> Option<PartitionReassignment> {
        let reassignments = self.active_reassignments.read().await;
        reassignments.get(reassignment_id).cloned()
    }
    
    /// Get status of a rolling upgrade
    pub async fn get_upgrade_status(&self, upgrade_id: &str) -> Option<RollingUpgrade> {
        let upgrades = self.rolling_upgrades.read().await;
        upgrades.get(upgrade_id).cloned()
    }
    
    /// List all active reassignments
    pub async fn list_active_reassignments(&self) -> Vec<PartitionReassignment> {
        let reassignments = self.active_reassignments.read().await;
        reassignments.values().cloned().collect()
    }
    
    /// List all active upgrades
    pub async fn list_active_upgrades(&self) -> Vec<RollingUpgrade> {
        let upgrades = self.rolling_upgrades.read().await;
        upgrades.values().cloned().collect()
    }
    
    /// Cancel a partition reassignment
    pub async fn cancel_reassignment(&self, reassignment_id: &str) -> Result<()> {
        let _span = span!(Level::INFO, "cancel_reassignment", id = %reassignment_id);
        let _guard = _span.enter();
        
        let mut reassignments = self.active_reassignments.write().await;
        if let Some(reassignment) = reassignments.get_mut(reassignment_id) {
            if reassignment.status == ReassignmentStatus::InProgress {
                reassignment.status = ReassignmentStatus::Cancelled;
                reassignment.updated_at = SystemTime::now();
                info!("Cancelled reassignment {}", reassignment_id);
                Ok(())
            } else {
                Err(Error::InvalidOperation(format!("Cannot cancel reassignment in status {:?}", reassignment.status)))
            }
        } else {
            Err(Error::NotFound(format!("Reassignment {} not found", reassignment_id)))
        }
    }
    
    /// Cancel a rolling upgrade
    pub async fn cancel_upgrade(&self, upgrade_id: &str) -> Result<()> {
        let _span = span!(Level::INFO, "cancel_upgrade", id = %upgrade_id);
        let _guard = _span.enter();
        
        let mut upgrades = self.rolling_upgrades.write().await;
        if let Some(upgrade) = upgrades.get_mut(upgrade_id) {
            if upgrade.status == ReassignmentStatus::InProgress {
                upgrade.status = ReassignmentStatus::Cancelled;
                upgrade.updated_at = SystemTime::now();
                info!("Cancelled upgrade {}", upgrade_id);
                Ok(())
            } else {
                Err(Error::InvalidOperation(format!("Cannot cancel upgrade in status {:?}", upgrade.status)))
            }
        } else {
            Err(Error::NotFound(format!("Upgrade {} not found", upgrade_id)))
        }
    }
    
    /// Validate a reassignment request
    async fn validate_reassignment_request(
        &self,
        topic: &str,
        assignments: &[PartitionAssignment],
    ) -> Result<()> {
        let cluster_state = self.cluster_state.read().await;
        
        // Check if topic exists
        if !cluster_state.topics.contains_key(topic) {
            return Err(Error::NotFound(format!("Topic {} does not exist", topic)));
        }
        
        // Check if all brokers exist and are online
        let online_brokers: HashSet<_> = cluster_state.brokers.values()
            .filter(|b| b.status.as_deref() == Some("online"))
            .map(|b| b.id)
            .collect();
        
        for assignment in assignments {
            for &broker_id in &assignment.target_replicas {
                if !online_brokers.contains(&broker_id) {
                    return Err(Error::InvalidOperation(format!("Broker {} is not online", broker_id)));
                }
            }
            
            if assignment.target_replicas.len() < self.config.min_isr_size {
                return Err(Error::InvalidOperation(format!(
                    "Target replica count {} is less than minimum ISR size {}",
                    assignment.target_replicas.len(),
                    self.config.min_isr_size
                )));
            }
        }
        
        // Check concurrent reassignment limit
        let active_count = self.active_reassignments.read().await
            .values()
            .filter(|r| r.status == ReassignmentStatus::InProgress)
            .count();
            
        if active_count >= self.config.max_concurrent_reassignments {
            return Err(Error::ResourceExhausted(format!(
                "Maximum concurrent reassignments ({}) reached",
                self.config.max_concurrent_reassignments
            )));
        }
        
        Ok(())
    }
    
    /// Get nodes for upgrade based on type
    async fn get_nodes_for_upgrade(&self, upgrade_type: &UpgradeType) -> Result<Vec<NodeUpgrade>> {
        let cluster_state = self.cluster_state.read().await;
        let mut nodes = Vec::new();
        
        match upgrade_type {
            UpgradeType::Controller => {
                // For now, assume single controller
                nodes.push(NodeUpgrade {
                    node_id: 1, // Controller ID
                    node_type: UpgradeType::Controller,
                    current_version: "1.0.0".to_string(), // Would be read from cluster state
                    target_version: "1.1.0".to_string(),  // From upgrade request
                    status: NodeUpgradeStatus::Pending,
                    started_at: None,
                    completed_at: None,
                    error: None,
                });
            }
            UpgradeType::Ingest => {
                for (broker_id, broker) in &cluster_state.brokers {
                    nodes.push(NodeUpgrade {
                        node_id: *broker_id,
                        node_type: UpgradeType::Ingest,
                        current_version: broker.version.clone().unwrap_or_else(|| "unknown".to_string()),
                        target_version: "1.1.0".to_string(),
                        status: NodeUpgradeStatus::Pending,
                        started_at: None,
                        completed_at: None,
                        error: None,
                    });
                }
            }
            UpgradeType::Search => {
                // Search nodes would be tracked separately
                // For now, assume single search node
                nodes.push(NodeUpgrade {
                    node_id: 100, // Search node ID
                    node_type: UpgradeType::Search,
                    current_version: "1.0.0".to_string(),
                    target_version: "1.1.0".to_string(),
                    status: NodeUpgradeStatus::Pending,
                    started_at: None,
                    completed_at: None,
                    error: None,
                });
            }
            UpgradeType::All => {
                // Recursive call to get all nodes using Box::pin to avoid infinite recursion
                let controller_future = Box::pin(self.get_nodes_for_upgrade(&UpgradeType::Controller));
                let ingest_future = Box::pin(self.get_nodes_for_upgrade(&UpgradeType::Ingest));
                let search_future = Box::pin(self.get_nodes_for_upgrade(&UpgradeType::Search));
                
                let mut controller_nodes = controller_future.await?;
                let mut ingest_nodes = ingest_future.await?;
                let mut search_nodes = search_future.await?;
                
                nodes.append(&mut controller_nodes);
                nodes.append(&mut ingest_nodes);
                nodes.append(&mut search_nodes);
            }
        }
        
        Ok(nodes)
    }
    
    /// Process a reassignment operation
    async fn process_operation(&self, operation: ReassignmentOperation) -> Result<()> {
        match operation {
            ReassignmentOperation::StartReassignment(id) => {
                self.execute_reassignment(&id).await
            }
            ReassignmentOperation::StartUpgrade(id) => {
                self.execute_upgrade(&id).await
            }
            _ => {
                debug!("Processing operation: {:?}", operation);
                Ok(())
            }
        }
    }
    
    /// Execute a partition reassignment
    async fn execute_reassignment(&self, reassignment_id: &str) -> Result<()> {
        let _span = span!(Level::INFO, "execute_reassignment", id = %reassignment_id);
        let _guard = _span.enter();
        
        info!("Executing partition reassignment {}", reassignment_id);
        
        // Update status to in progress
        {
            let mut reassignments = self.active_reassignments.write().await;
            if let Some(reassignment) = reassignments.get_mut(reassignment_id) {
                reassignment.status = ReassignmentStatus::InProgress;
                reassignment.updated_at = SystemTime::now();
            }
        }
        
        // In a real implementation, this would:
        // 1. Add new replicas to partition
        // 2. Wait for them to catch up
        // 3. Update leader if needed
        // 4. Remove old replicas
        // 5. Update metadata
        
        // For this implementation, we'll simulate the process
        tokio::time::sleep(Duration::from_secs(5)).await;
        
        // Mark as completed
        {
            let mut reassignments = self.active_reassignments.write().await;
            if let Some(reassignment) = reassignments.get_mut(reassignment_id) {
                reassignment.status = ReassignmentStatus::Completed;
                reassignment.updated_at = SystemTime::now();
                reassignment.progress.partitions_completed = reassignment.progress.total_partitions;
            }
        }
        
        info!("Completed partition reassignment {}", reassignment_id);
        Ok(())
    }
    
    /// Execute a rolling upgrade
    async fn execute_upgrade(&self, upgrade_id: &str) -> Result<()> {
        let _span = span!(Level::INFO, "execute_upgrade", id = %upgrade_id);
        let _guard = _span.enter();
        
        info!("Executing rolling upgrade {}", upgrade_id);
        
        // Update status to in progress
        {
            let mut upgrades = self.rolling_upgrades.write().await;
            if let Some(upgrade) = upgrades.get_mut(upgrade_id) {
                upgrade.status = ReassignmentStatus::InProgress;
                upgrade.updated_at = SystemTime::now();
            }
        }
        
        // Get the upgrade details
        let nodes = {
            let upgrades = self.rolling_upgrades.read().await;
            upgrades.get(upgrade_id).map(|u| u.nodes.clone()).unwrap_or_default()
        };
        
        // Process each node sequentially
        for (i, mut node) in nodes.into_iter().enumerate() {
            info!("Upgrading node {} ({:?})", node.node_id, node.node_type);
            
            // Update node status
            node.status = NodeUpgradeStatus::DrainStarted;
            node.started_at = Some(SystemTime::now());
            
            // Simulate upgrade process
            tokio::time::sleep(Duration::from_secs(2)).await;
            node.status = NodeUpgradeStatus::Draining;
            
            tokio::time::sleep(Duration::from_secs(3)).await;
            node.status = NodeUpgradeStatus::Upgrading;
            
            tokio::time::sleep(Duration::from_secs(5)).await;
            node.status = NodeUpgradeStatus::Starting;
            
            tokio::time::sleep(Duration::from_secs(2)).await;
            node.status = NodeUpgradeStatus::Validating;
            
            tokio::time::sleep(Duration::from_secs(1)).await;
            node.status = NodeUpgradeStatus::Completed;
            node.completed_at = Some(SystemTime::now());
            
            // Update the node in the upgrade
            {
                let mut upgrades = self.rolling_upgrades.write().await;
                if let Some(upgrade) = upgrades.get_mut(upgrade_id) {
                    upgrade.nodes[i] = node;
                    upgrade.updated_at = SystemTime::now();
                }
            }
            
            info!("Completed upgrade for node {}", node.node_id);
        }
        
        // Mark upgrade as completed
        {
            let mut upgrades = self.rolling_upgrades.write().await;
            if let Some(upgrade) = upgrades.get_mut(upgrade_id) {
                upgrade.status = ReassignmentStatus::Completed;
                upgrade.updated_at = SystemTime::now();
            }
        }
        
        info!("Completed rolling upgrade {}", upgrade_id);
        Ok(())
    }
    
    /// Check health of ongoing operations
    async fn check_operation_health(&self) -> Result<()> {
        let now = SystemTime::now();
        
        // Check reassignments for timeouts
        {
            let mut reassignments = self.active_reassignments.write().await;
            let timeout_ids: Vec<_> = reassignments.iter()
                .filter_map(|(id, r)| {
                    if r.status == ReassignmentStatus::InProgress {
                        if let Ok(elapsed) = now.duration_since(r.created_at) {
                            if elapsed > self.config.reassignment_timeout {
                                return Some(id.clone());
                            }
                        }
                    }
                    None
                })
                .collect();
                
            for id in timeout_ids {
                if let Some(reassignment) = reassignments.get_mut(&id) {
                    warn!("Reassignment {} timed out", id);
                    reassignment.status = ReassignmentStatus::Failed;
                    reassignment.error = Some("Operation timed out".to_string());
                    reassignment.updated_at = now;
                }
            }
        }
        
        // Check upgrades for timeouts
        {
            let mut upgrades = self.rolling_upgrades.write().await;
            let timeout_ids: Vec<_> = upgrades.iter()
                .filter_map(|(id, u)| {
                    if u.status == ReassignmentStatus::InProgress {
                        if let Ok(elapsed) = now.duration_since(u.created_at) {
                            if elapsed > self.config.upgrade_timeout {
                                return Some(id.clone());
                            }
                        }
                    }
                    None
                })
                .collect();
                
            for id in timeout_ids {
                if let Some(upgrade) = upgrades.get_mut(&id) {
                    warn!("Upgrade {} timed out", id);
                    upgrade.status = ReassignmentStatus::Failed;
                    upgrade.error = Some("Operation timed out".to_string());
                    upgrade.updated_at = now;
                }
            }
        }
        
        Ok(())
    }
    
    /// Clean up completed operations
    pub async fn cleanup_completed_operations(&self, retention_period: Duration) -> Result<()> {
        let cutoff_time = SystemTime::now() - retention_period;
        
        // Clean up old reassignments
        {
            let mut reassignments = self.active_reassignments.write().await;
            reassignments.retain(|_, r| {
                matches!(r.status, ReassignmentStatus::InProgress | ReassignmentStatus::Pending) ||
                r.updated_at > cutoff_time
            });
        }
        
        // Clean up old upgrades
        {
            let mut upgrades = self.rolling_upgrades.write().await;
            upgrades.retain(|_, u| {
                matches!(u.status, ReassignmentStatus::InProgress | ReassignmentStatus::Pending) ||
                u.updated_at > cutoff_time
            });
        }
        
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    
    #[tokio::test]
    async fn test_reassignment_manager_creation() {
        let cluster_state = Arc::new(RwLock::new(ControllerState::default()));
        let temp_dir = TempDir::new().unwrap();
        let metadata_store = Arc::new(crate::ControllerMetadataStore::new(temp_dir.path()).unwrap());
        let sync_manager = Arc::new(MetadataSyncManager::new(
            metadata_store,
            cluster_state.clone(),
            Duration::from_secs(60),
        ));
        
        let config = ReassignmentConfig::default();
        let manager = PartitionReassignmentManager::new(
            cluster_state,
            sync_manager,
            config,
        );
        
        assert_eq!(manager.config.max_concurrent_reassignments, 3);
        assert_eq!(manager.list_active_reassignments().await.len(), 0);
        assert_eq!(manager.list_active_upgrades().await.len(), 0);
    }
    
    #[tokio::test]
    async fn test_partition_assignment_validation() {
        let mut cluster_state = ControllerState::default();
        
        // Add a topic
        cluster_state.topics.insert("test-topic".to_string(), TopicConfig {
            name: "test-topic".to_string(),
            partition_count: 3,
            replication_factor: 2,
            config: HashMap::new(),
            created_at: SystemTime::now(),
            version: 1,
        });
        
        // Add brokers
        cluster_state.brokers.insert(1, BrokerInfo {
            id: 1,
            address: "127.0.0.1:9092".parse().unwrap(),
            rack: None,
            status: Some("online".to_string()),
            version: Some("1.0.0".to_string()),
            metadata_version: Some(1),
        });
        
        let cluster_state = Arc::new(RwLock::new(cluster_state));
        let temp_dir = TempDir::new().unwrap();
        let metadata_store = Arc::new(crate::ControllerMetadataStore::new(temp_dir.path()).unwrap());
        let sync_manager = Arc::new(MetadataSyncManager::new(
            metadata_store,
            cluster_state.clone(),
            Duration::from_secs(60),
        ));
        
        let config = ReassignmentConfig::default();
        let manager = PartitionReassignmentManager::new(
            cluster_state,
            sync_manager,
            config,
        );
        
        // Test valid assignment
        let assignments = vec![PartitionAssignment {
            partition_id: 0,
            current_replicas: vec![1],
            target_replicas: vec![1],
            current_leader: Some(1),
            target_leader: Some(1),
            initial_isr: vec![1],
            current_isr: vec![1],
            bytes_transferred: 0,
            total_bytes: 1000,
            status: PartitionReassignmentStatus::Pending,
        }];
        
        let result = manager.validate_reassignment_request("test-topic", &assignments).await;
        assert!(result.is_ok());
        
        // Test invalid topic
        let result = manager.validate_reassignment_request("nonexistent-topic", &assignments).await;
        assert!(result.is_err());
    }
}