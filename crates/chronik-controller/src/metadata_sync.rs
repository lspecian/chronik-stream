//! Metadata synchronization manager for Controller.
//!
//! Handles change detection and propagation of metadata updates
//! to all cluster components (ingest nodes, search nodes).

use crate::{
    metastore_adapter::ControllerMetadataStore,
    ControllerState, BrokerInfo,
    backend_client::BackendServiceManager,
};
use chronik_common::Error;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, SystemTime, Instant},
};
use tokio::sync::{RwLock, mpsc};
use tracing::{info, warn, error, debug, span, Level};
use uuid::Uuid;

// Custom serde module for Instant
mod instant_serde {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    pub fn serialize<S>(instant: &Instant, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // Convert Instant to a duration since UNIX_EPOCH for serialization
        // Note: This is an approximation since Instant doesn't have a fixed reference point
        let duration_since_epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        duration_since_epoch.as_nanos().serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Instant, D::Error>
    where
        D: Deserializer<'de>,
    {
        let nanos = u128::deserialize(deserializer)?;
        // For deserialization, we'll just use the current Instant
        // This is not perfect but sufficient for our use case
        Ok(Instant::now())
    }
}

/// Metadata synchronization manager
pub struct MetadataSyncManager {
    /// Metastore client for reading metadata
    metastore: ControllerMetadataStore,
    
    /// In-memory cluster state
    cluster_state: Arc<RwLock<ControllerState>>,
    
    /// Cached broker information
    brokers: Arc<RwLock<HashMap<u32, BrokerMetadata>>>,
    
    /// Cached topic information
    topics: Arc<RwLock<HashMap<String, TopicMetadata>>>,
    
    /// Cached consumer group information
    consumer_groups: Arc<RwLock<HashMap<String, ConsumerGroupMetadata>>>,
    
    /// Sync interval for periodic updates
    sync_interval: Duration,
    
    /// Current sync status
    sync_status: Arc<RwLock<SyncStatusInternal>>,
    
    /// Update channel for immediate propagation
    update_sender: mpsc::UnboundedSender<MetadataUpdate>,
    
    /// Update receiver
    update_receiver: Arc<RwLock<Option<mpsc::UnboundedReceiver<MetadataUpdate>>>>,
    
    /// Backend service manager for propagating updates
    backend_manager: Option<Arc<BackendServiceManager>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BrokerMetadata {
    pub id: u32,
    pub address: SocketAddr,
    pub rack: Option<String>,
    pub status: BrokerStatus,
    #[serde(with = "instant_serde")]
    pub last_heartbeat: Instant,
    pub version: String,
    pub metadata_version: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum BrokerStatus {
    Online,
    Offline,
    Draining,
    Unknown,
}

impl std::fmt::Display for BrokerStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BrokerStatus::Online => write!(f, "online"),
            BrokerStatus::Offline => write!(f, "offline"),
            BrokerStatus::Draining => write!(f, "draining"),
            BrokerStatus::Unknown => write!(f, "unknown"),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TopicMetadata {
    pub name: String,
    pub partition_count: u32,
    pub replication_factor: u16,
    pub config: HashMap<String, String>,
    pub version: u64,
    pub last_modified: SystemTime,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConsumerGroupMetadata {
    pub group_id: String,
    pub topics: Vec<String>,
    pub members: Vec<ConsumerMember>,
    pub state: GroupState,
    pub last_modified: SystemTime,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConsumerMember {
    pub member_id: String,
    pub client_id: String,
    pub host: String,
    pub session_timeout: Duration,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum GroupState {
    Empty,
    PreparingRebalance,
    CompletingRebalance,
    Stable,
    Dead,
}

#[derive(Debug)]
struct SyncStatusInternal {
    last_sync_time: Option<SystemTime>,
    sync_in_progress: bool,
    last_sync_duration: Option<Duration>,
    sync_errors: Vec<String>,
    metadata_version: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum MetadataUpdate {
    BrokerUpdate {
        broker_id: u32,
        address: SocketAddr,
        status: BrokerStatus,
        version: String,
    },
    BrokerRemoval {
        broker_id: u32,
    },
    TopicUpdate {
        topic_name: String,
        version: u64,
        changes: Vec<TopicChange>,
        partition_count: u32,
        replication_factor: u16,
        config: HashMap<String, String>,
    },
    TopicDeletion {
        topic_name: String,
    },
    ConsumerGroupUpdate {
        group_id: String,
        topics: Vec<String>,
        state: GroupState,
        member_count: usize,
    },
    ConsumerGroupDeletion {
        group_id: String,
    },
    FullSync {
        brokers: Vec<BrokerMetadata>,
        topics: Vec<TopicMetadata>,
        consumer_groups: Vec<ConsumerGroupMetadata>,
        metadata_version: u64,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum TopicChange {
    Created,
    PartitionCountChanged { old: u32, new: u32 },
    ReplicationFactorChanged { old: u16, new: u16 },
    ConfigChanged { key: String, old_value: String, new_value: String },
    ConfigAdded { key: String, value: String },
    ConfigRemoved { key: String },
}

#[derive(Debug, Serialize)]
pub struct SyncStatus {
    pub last_sync_time: Option<SystemTime>,
    pub sync_in_progress: bool,
    pub last_sync_duration: Option<Duration>,
    pub sync_errors: Vec<String>,
    pub metadata_version: u64,
}

impl SyncStatus {
    pub fn is_healthy(&self) -> bool {
        self.sync_errors.is_empty() && 
        self.last_sync_time.is_some() &&
        !self.sync_in_progress
    }
}

impl MetadataSyncManager {
    /// Create new metadata sync manager
    pub fn new(
        metastore: ControllerMetadataStore,
        cluster_state: Arc<RwLock<ControllerState>>,
        sync_interval: Duration,
    ) -> Self {
        let (update_sender, update_receiver) = mpsc::unbounded_channel();
        
        Self {
            metastore,
            cluster_state,
            brokers: Arc::new(RwLock::new(HashMap::new())),
            topics: Arc::new(RwLock::new(HashMap::new())),
            consumer_groups: Arc::new(RwLock::new(HashMap::new())),
            sync_interval,
            sync_status: Arc::new(RwLock::new(SyncStatusInternal {
                last_sync_time: None,
                sync_in_progress: false,
                last_sync_duration: None,
                sync_errors: Vec::new(),
                metadata_version: 0,
            })),
            update_sender,
            update_receiver: Arc::new(RwLock::new(Some(update_receiver))),
            backend_manager: None,
        }
    }

    /// Set the backend service manager
    pub fn with_backend_manager(mut self, backend_manager: Arc<BackendServiceManager>) -> Self {
        self.backend_manager = Some(backend_manager);
        self
    }
    
    /// Start the metadata synchronization loop
    pub async fn start_sync_loop(self: Arc<Self>) {
        info!("Starting metadata sync loop with interval: {:?}", self.sync_interval);
        
        let mut interval = tokio::time::interval(self.sync_interval);
        
        // Start update propagation handler
        self.clone().start_update_propagation().await;
        
        loop {
            interval.tick().await;
            
            let start_time = Instant::now();
            let _span = span!(Level::INFO, "metadata_sync");
            let _guard = _span.enter();
            
            // Mark sync as in progress
            {
                let mut status = self.sync_status.write().await;
                status.sync_in_progress = true;
                status.sync_errors.clear();
            }
            
            match self.sync_metadata().await {
                Ok(()) => {
                    let duration = start_time.elapsed();
                    debug!("Metadata sync completed successfully in {:?}", duration);
                    
                    let mut status = self.sync_status.write().await;
                    status.last_sync_time = Some(SystemTime::now());
                    status.last_sync_duration = Some(duration);
                    status.sync_in_progress = false;
                },
                Err(e) => {
                    let duration = start_time.elapsed();
                    error!("Metadata sync failed after {:?}: {}", duration, e);
                    
                    let mut status = self.sync_status.write().await;
                    status.last_sync_time = Some(SystemTime::now());
                    status.last_sync_duration = Some(duration);
                    status.sync_in_progress = false;
                    status.sync_errors.push(e.to_string());
                }
            }
        }
    }

    /// Start update propagation handler
    async fn start_update_propagation(self: Arc<Self>) {
        let mut receiver = {
            let mut receiver_lock = self.update_receiver.write().await;
            receiver_lock.take().expect("Update receiver should be available")
        };
        
        info!("Starting metadata update propagation handler");
        
        tokio::spawn(async move {
            while let Some(update) = receiver.recv().await {
                let _span = span!(Level::DEBUG, "propagate_update", update_type = ?std::mem::discriminant(&update));
                let _guard = _span.enter();
                
                if let Err(e) = self.propagate_update(&update).await {
                    error!("Failed to propagate update: {}", e);
                }
            }
            
            info!("Metadata update propagation handler stopped");
        });
    }

    /// Synchronize all metadata from metastore
    async fn sync_metadata(&self) -> Result<(), Error> {
        debug!("Starting metadata synchronization");
        
        // Sync in dependency order
        self.sync_brokers().await?;
        self.sync_topics().await?;
        self.sync_consumer_groups().await?;
        
        // Update metadata version
        let cluster_state = self.cluster_state.read().await;
        let mut status = self.sync_status.write().await;
        status.metadata_version = cluster_state.metadata_version;
        
        debug!("Metadata synchronization completed");
        Ok(())
    }

    /// Synchronize broker metadata
    async fn sync_brokers(&self) -> Result<(), Error> {
        let _span = span!(Level::DEBUG, "sync_brokers");
        let _guard = _span.enter();
        
        // Get current brokers from metastore
        let current_brokers = self.metastore.get_all_brokers().await?;
        let mut brokers = self.brokers.write().await;
        
        // Detect removed brokers
        let removed_broker_ids: Vec<_> = brokers.keys()
            .filter(|id| !current_brokers.iter().any(|b| &b.id == *id))
            .cloned()
            .collect();
        
        for broker_id in removed_broker_ids {
            info!("Broker {} removed from cluster", broker_id);
            brokers.remove(&broker_id);
            
            // Send removal update
            let update = MetadataUpdate::BrokerRemoval { broker_id };
            let _ = self.update_sender.send(update);
        }
        
        // Detect new or changed brokers
        for broker_info in current_brokers {
            let broker_metadata = BrokerMetadata {
                id: broker_info.id,
                address: broker_info.address,
                rack: broker_info.rack.clone(),
                status: self.determine_broker_status(&broker_info).await,
                last_heartbeat: Instant::now(),
                version: broker_info.version.clone().unwrap_or_else(|| "unknown".to_string()),
                metadata_version: broker_info.metadata_version.unwrap_or(0),
            };
            
            let needs_update = match brokers.get(&broker_info.id) {
                Some(existing) => self.broker_changed(existing, &broker_metadata),
                None => true,
            };
            
            if needs_update {
                info!("Broker {} added/updated", broker_info.id);
                brokers.insert(broker_info.id, broker_metadata.clone());
                
                // Send update
                let update = MetadataUpdate::BrokerUpdate {
                    broker_id: broker_metadata.id,
                    address: broker_metadata.address,
                    status: broker_metadata.status,
                    version: broker_metadata.version,
                };
                let _ = self.update_sender.send(update);
            }
        }
        
        Ok(())
    }

    /// Synchronize topic metadata
    async fn sync_topics(&self) -> Result<(), Error> {
        let _span = span!(Level::DEBUG, "sync_topics");
        let _guard = _span.enter();
        
        // Get current topics from cluster state (which is updated from metastore)
        let cluster_state = self.cluster_state.read().await;
        let current_topics: Vec<_> = cluster_state.topics.values().cloned().collect();
        drop(cluster_state);
        
        let mut topics = self.topics.write().await;
        
        // Detect removed topics
        let removed_topic_names: Vec<_> = topics.keys()
            .filter(|name| !current_topics.iter().any(|t| &t.name == *name))
            .cloned()
            .collect();
        
        for topic_name in removed_topic_names {
            info!("Topic {} removed", topic_name);
            topics.remove(&topic_name);
            
            // Send deletion update
            let update = MetadataUpdate::TopicDeletion { topic_name };
            let _ = self.update_sender.send(update);
        }
        
        // Detect new or changed topics
        for topic_config in current_topics {
            let topic_metadata = TopicMetadata {
                name: topic_config.name.clone(),
                partition_count: topic_config.partition_count,
                replication_factor: topic_config.replication_factor,
                config: topic_config.config.clone(),
                version: topic_config.version,
                last_modified: topic_config.created_at,
            };
            
            let changes = match topics.get(&topic_config.name) {
                Some(existing) => self.detect_topic_changes(Some(existing), &topic_metadata),
                None => vec![TopicChange::Created],
            };
            
            if !changes.is_empty() {
                info!("Topic {} changes detected: {:?}", topic_config.name, changes);
                topics.insert(topic_config.name.clone(), topic_metadata.clone());
                
                // Send update
                let update = MetadataUpdate::TopicUpdate {
                    topic_name: topic_metadata.name,
                    version: topic_metadata.version,
                    changes,
                    partition_count: topic_metadata.partition_count,
                    replication_factor: topic_metadata.replication_factor,
                    config: topic_metadata.config,
                };
                let _ = self.update_sender.send(update);
            }
        }
        
        Ok(())
    }

    /// Synchronize consumer group metadata
    async fn sync_consumer_groups(&self) -> Result<(), Error> {
        let _span = span!(Level::DEBUG, "sync_consumer_groups");
        let _guard = _span.enter();
        
        // Get current consumer groups from cluster state
        let cluster_state = self.cluster_state.read().await;
        let current_groups: Vec<_> = cluster_state.consumer_groups.values().cloned().collect();
        drop(cluster_state);
        
        let mut consumer_groups = self.consumer_groups.write().await;
        
        // Convert ConsumerGroup to ConsumerGroupMetadata
        for group in current_groups {
            let group_metadata = ConsumerGroupMetadata {
                group_id: group.group_id.clone(),
                topics: group.topics.clone(),
                members: group.members.iter().map(|m| ConsumerMember {
                    member_id: m.member_id.clone(),
                    client_id: m.client_id.clone(),
                    host: m.host.clone(),
                    session_timeout: Duration::from_millis(m.session_timeout_ms as u64),
                }).collect(),
                state: match group.state {
                    crate::GroupState::Empty => GroupState::Empty,
                    crate::GroupState::PreparingRebalance => GroupState::PreparingRebalance,
                    crate::GroupState::CompletingRebalance => GroupState::CompletingRebalance,
                    crate::GroupState::Stable => GroupState::Stable,
                    crate::GroupState::Dead => GroupState::Dead,
                },
                last_modified: SystemTime::now(),
            };
            
            let needs_update = match consumer_groups.get(&group.group_id) {
                Some(existing) => self.consumer_group_changed(existing, &group_metadata),
                None => true,
            };
            
            if needs_update {
                info!("Consumer group {} updated", group.group_id);
                consumer_groups.insert(group.group_id.clone(), group_metadata.clone());
                
                // Send update
                let update = MetadataUpdate::ConsumerGroupUpdate {
                    group_id: group_metadata.group_id,
                    topics: group_metadata.topics,
                    state: group_metadata.state,
                    member_count: group_metadata.members.len(),
                };
                let _ = self.update_sender.send(update);
            }
        }
        
        Ok(())
    }

    /// Detect changes between topic metadata objects
    fn detect_topic_changes(&self, old: Option<&TopicMetadata>, new: &TopicMetadata) -> Vec<TopicChange> {
        let mut changes = Vec::new();
        
        match old {
            None => changes.push(TopicChange::Created),
            Some(old_meta) => {
                // Only check if version is newer
                if new.version <= old_meta.version {
                    return changes;
                }
                
                // Compare partition counts
                if old_meta.partition_count != new.partition_count {
                    changes.push(TopicChange::PartitionCountChanged {
                        old: old_meta.partition_count,
                        new: new.partition_count,
                    });
                }
                
                // Compare replication factor
                if old_meta.replication_factor != new.replication_factor {
                    changes.push(TopicChange::ReplicationFactorChanged {
                        old: old_meta.replication_factor,
                        new: new.replication_factor,
                    });
                }
                
                // Compare configurations
                for (key, new_value) in &new.config {
                    match old_meta.config.get(key) {
                        Some(old_value) if old_value != new_value => {
                            changes.push(TopicChange::ConfigChanged {
                                key: key.clone(),
                                old_value: old_value.clone(),
                                new_value: new_value.clone(),
                            });
                        }
                        None => {
                            changes.push(TopicChange::ConfigAdded {
                                key: key.clone(),
                                value: new_value.clone(),
                            });
                        }
                        _ => {}
                    }
                }
                
                // Check for removed configs
                for key in old_meta.config.keys() {
                    if !new.config.contains_key(key) {
                        changes.push(TopicChange::ConfigRemoved { key: key.clone() });
                    }
                }
            }
        }
        
        changes
    }

    /// Check if broker metadata has changed
    fn broker_changed(&self, old: &BrokerMetadata, new: &BrokerMetadata) -> bool {
        old.address != new.address ||
        old.rack != new.rack ||
        old.status != new.status ||
        old.version != new.version ||
        old.metadata_version != new.metadata_version
    }

    /// Check if consumer group metadata has changed
    fn consumer_group_changed(&self, old: &ConsumerGroupMetadata, new: &ConsumerGroupMetadata) -> bool {
        old.topics != new.topics ||
        old.state != new.state ||
        old.members.len() != new.members.len()
    }

    /// Determine broker status based on current information
    async fn determine_broker_status(&self, broker_info: &BrokerInfo) -> BrokerStatus {
        // Simple status determination - in a real implementation,
        // this would check heartbeats, health checks, etc.
        match broker_info.status.as_deref() {
            Some("online") => BrokerStatus::Online,
            Some("offline") => BrokerStatus::Offline,
            Some("draining") => BrokerStatus::Draining,
            _ => BrokerStatus::Unknown,
        }
    }

    /// Propagate metadata update to cluster nodes
    async fn propagate_update(&self, update: &MetadataUpdate) -> Result<(), Error> {
        debug!("Propagating metadata update: {:?}", std::mem::discriminant(update));
        
        // If backend manager is available, use it for propagation
        if let Some(backend_manager) = &self.backend_manager {
            return backend_manager.broadcast_metadata_update(update).await;
        }
        
        // Otherwise, use the legacy method
        // Get list of nodes to notify
        let (ingest_nodes, search_nodes) = self.get_cluster_nodes().await?;
        
        // Send to ingest nodes
        self.broadcast_to_ingest_nodes(&ingest_nodes, update).await?;
        
        // Send to search nodes
        self.broadcast_to_search_nodes(&search_nodes, update).await?;
        
        Ok(())
    }

    /// Get current cluster nodes
    async fn get_cluster_nodes(&self) -> Result<(Vec<SocketAddr>, Vec<SocketAddr>), Error> {
        let cluster_state = self.cluster_state.read().await;
        
        // Get ingest nodes (brokers)
        let ingest_nodes: Vec<_> = cluster_state.brokers.values()
            .filter(|broker| matches!(broker.status.as_deref(), Some("online")))
            .map(|broker| broker.address)
            .collect();
        
        // Search nodes would be tracked separately in a real implementation
        let search_nodes = Vec::new();
        
        Ok((ingest_nodes, search_nodes))
    }

    /// Broadcast update to ingest nodes
    async fn broadcast_to_ingest_nodes(&self, nodes: &[SocketAddr], update: &MetadataUpdate) -> Result<(), Error> {
        if nodes.is_empty() {
            debug!("No ingest nodes to notify");
            return Ok(());
        }
        
        debug!("Broadcasting update to {} ingest nodes", nodes.len());
        
        // In a real implementation, this would use gRPC calls to notify nodes
        // For now, we'll just log the operation
        for node in nodes {
            debug!("Would notify ingest node at {}", node);
        }
        
        Ok(())
    }

    /// Broadcast update to search nodes
    async fn broadcast_to_search_nodes(&self, nodes: &[SocketAddr], update: &MetadataUpdate) -> Result<(), Error> {
        if nodes.is_empty() {
            debug!("No search nodes to notify");
            return Ok(());
        }
        
        debug!("Broadcasting update to {} search nodes", nodes.len());
        
        // In a real implementation, this would use gRPC calls to notify nodes
        for node in nodes {
            debug!("Would notify search node at {}", node);
        }
        
        Ok(())
    }

    /// Get current sync status
    pub async fn get_status(&self) -> SyncStatus {
        let status = self.sync_status.read().await;
        SyncStatus {
            last_sync_time: status.last_sync_time,
            sync_in_progress: status.sync_in_progress,
            last_sync_duration: status.last_sync_duration,
            sync_errors: status.sync_errors.clone(),
            metadata_version: status.metadata_version,
        }
    }

    /// Trigger immediate full synchronization
    pub async fn trigger_full_sync(&self) -> Result<(), String> {
        info!("Triggering immediate full metadata sync");
        
        match self.sync_metadata().await {
            Ok(()) => {
                info!("Full sync completed successfully");
                Ok(())
            },
            Err(e) => {
                error!("Full sync failed: {}", e);
                Err(e.to_string())
            }
        }
    }

    /// Trigger topic-specific sync
    pub async fn trigger_topic_sync(&self) -> Result<(), String> {
        debug!("Triggering topic metadata sync");
        
        match self.sync_topics().await {
            Ok(()) => {
                debug!("Topic sync completed successfully");
                Ok(())
            },
            Err(e) => {
                error!("Topic sync failed: {}", e);
                Err(e.to_string())
            }
        }
    }
}