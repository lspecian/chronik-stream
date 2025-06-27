//! Admin REST API service for Controller.
//!
//! Provides HTTP endpoints for cluster management, topic operations,
//! and metadata queries.

use crate::{
    metastore_adapter::ControllerMetadataStore,
    ControllerState, TopicConfig, BrokerInfo, ConsumerGroup,
    metadata_sync::{MetadataSyncManager, SyncStatus},
    backend_client::BackendServiceManager,
    partition_reassignment::{
        PartitionReassignmentManager, PartitionAssignment, UpgradeType,
        ReassignmentConfig, UpgradeConfig,
    },
};
use axum::{
    Router, Json,
    extract::{State, Path, Query},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post, put, delete},
};
use chronik_common::Error;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::Arc,
    time::SystemTime,
};
use tokio::sync::RwLock;
use tower::ServiceBuilder;
use tower_http::{
    trace::TraceLayer,
    cors::CorsLayer,
    timeout::TimeoutLayer,
};
use tracing::{info, error, warn, span, Level};
use uuid::Uuid;

/// Admin service for controller management operations
pub struct AdminService {
    /// Metastore client for persistent storage
    pub metastore: ControllerMetadataStore,
    /// In-memory cluster state
    pub cluster_state: Arc<RwLock<ControllerState>>,
    /// Metadata synchronization manager
    pub sync_manager: Arc<MetadataSyncManager>,
    /// Backend service manager for node communication
    pub backend_manager: Option<Arc<BackendServiceManager>>,
    /// Partition reassignment manager
    pub reassignment_manager: Option<Arc<PartitionReassignmentManager>>,
}

impl AdminService {
    /// Create new admin service instance
    pub fn new(
        metastore: ControllerMetadataStore,
        cluster_state: Arc<RwLock<ControllerState>>,
        sync_manager: Arc<MetadataSyncManager>,
    ) -> Self {
        Self {
            metastore,
            cluster_state,
            sync_manager,
            backend_manager: None,
            reassignment_manager: None,
        }
    }
    
    /// Set the backend service manager
    pub fn with_backend_manager(mut self, backend_manager: Arc<BackendServiceManager>) -> Self {
        self.backend_manager = Some(backend_manager);
        self
    }
    
    /// Set the partition reassignment manager
    pub fn with_reassignment_manager(mut self, reassignment_manager: Arc<PartitionReassignmentManager>) -> Self {
        self.reassignment_manager = Some(reassignment_manager);
        self
    }

    /// Create router with all admin endpoints
    pub fn router(self: Arc<Self>) -> Router {
        Router::new()
            // Cluster management endpoints
            .route("/api/v1/cluster/status", get(Self::get_cluster_status))
            .route("/api/v1/cluster/health", get(Self::get_cluster_health))
            .route("/api/v1/cluster/brokers", get(Self::list_brokers))
            .route("/api/v1/cluster/brokers/:id", get(Self::get_broker))
            
            // Topic management
            .route("/api/v1/topics", get(Self::list_topics).post(Self::create_topic))
            .route("/api/v1/topics/:name", get(Self::get_topic).delete(Self::delete_topic))
            .route("/api/v1/topics/:name/config", get(Self::get_topic_config).put(Self::update_topic_config))
            .route("/api/v1/topics/:name/partitions", get(Self::get_topic_partitions))
            
            // Consumer group management
            .route("/api/v1/consumer-groups", get(Self::list_consumer_groups))
            .route("/api/v1/consumer-groups/:id", get(Self::get_consumer_group).delete(Self::delete_consumer_group))
            .route("/api/v1/consumer-groups/:id/reset", post(Self::reset_consumer_group))
            
            // Metadata sync endpoints
            .route("/api/v1/sync/status", get(Self::get_sync_status))
            .route("/api/v1/sync/trigger", post(Self::trigger_sync))
            .route("/api/v1/metadata/version", get(Self::get_metadata_version))
            
            // Admin operations
            .route("/api/v1/admin/rebalance", post(Self::trigger_rebalance))
            .route("/api/v1/admin/leader-election", post(Self::trigger_leader_election))
            
            // Backend service endpoints
            .route("/api/v1/backend/stats", get(Self::get_backend_stats))
            .route("/api/v1/backend/refresh", post(Self::refresh_backend_connections))
            
            // Partition reassignment endpoints
            .route("/api/v1/reassignments", get(Self::list_reassignments).post(Self::start_reassignment))
            .route("/api/v1/reassignments/:id", get(Self::get_reassignment).delete(Self::cancel_reassignment))
            
            // Rolling upgrade endpoints
            .route("/api/v1/upgrades", get(Self::list_upgrades).post(Self::start_upgrade))
            .route("/api/v1/upgrades/:id", get(Self::get_upgrade).delete(Self::cancel_upgrade))
            
            // TODO: Add middleware layers when Service trait bounds are resolved
            // .layer(TraceLayer::new_for_http())
            // .layer(CorsLayer::permissive())
            // .layer(TimeoutLayer::new(std::time::Duration::from_secs(30)))
            .with_state(self)
    }
    
    /// Start the admin service on the specified address
    pub async fn start(self, addr: std::net::SocketAddr) -> Result<(), chronik_common::Error> {
        let app = Arc::new(self).router();
        
        info!("Starting admin API server on {}", addr);
        
        let listener = tokio::net::TcpListener::bind(addr).await
            .map_err(|e| chronik_common::Error::Io(e))?;
            
        axum::serve(listener, app).await
            .map_err(|e| chronik_common::Error::Internal(format!("Server error: {}", e)))
    }

    /// Get overall cluster status
    async fn get_cluster_status(State(svc): State<Arc<AdminService>>) -> Result<Json<ClusterStatus>, AdminError> {
        let _span = span!(Level::INFO, "get_cluster_status");
        let _guard = _span.enter();

        let state = svc.cluster_state.read().await;
        let sync_status = svc.sync_manager.get_status().await;
        
        // Get backend stats if available
        let backend_stats = if let Some(backend_manager) = &svc.backend_manager {
            backend_manager.get_cluster_stats().await.ok()
        } else {
            None
        };
        
        let status = ClusterStatus {
            healthy: state.is_healthy(),
            broker_count: state.brokers.len(),
            topic_count: state.topics.len(),
            total_partitions: state.total_partitions(),
            consumer_groups: state.consumer_groups.len(),
            metadata_version: state.metadata_version,
            last_sync: sync_status.last_sync_time,
            controller_id: state.controller_id,
            leader_epoch: state.leader_epoch,
            backend_stats,
        };

        Ok(Json(status))
    }

    /// Get cluster health information
    async fn get_cluster_health(State(svc): State<Arc<AdminService>>) -> Result<Json<ClusterHealth>, AdminError> {
        let _span = span!(Level::INFO, "get_cluster_health");
        let _guard = _span.enter();

        let state = svc.cluster_state.read().await;
        let sync_status = svc.sync_manager.get_status().await;

        let health = ClusterHealth {
            status: if state.is_healthy() { "healthy".to_string() } else { "unhealthy".to_string() },
            brokers_online: state.get_online_brokers().len(),
            brokers_total: state.brokers.len(),
            metadata_sync_healthy: sync_status.is_healthy(),
            last_metadata_sync: sync_status.last_sync_time,
            raft_status: state.raft_status.clone(),
            issues: state.get_health_issues(),
        };

        Ok(Json(health))
    }

    /// List all brokers
    async fn list_brokers(
        State(svc): State<Arc<AdminService>>,
        Query(params): Query<ListBrokersQuery>,
    ) -> Result<Json<BrokerList>, AdminError> {
        let _span = span!(Level::INFO, "list_brokers");
        let _guard = _span.enter();

        let state = svc.cluster_state.read().await;
        let mut brokers: Vec<_> = state.brokers.values().cloned().collect();

        // Filter by status if requested
        if let Some(status) = params.status {
            brokers.retain(|b| {
                b.status.as_deref().unwrap_or("unknown").to_lowercase() == status.to_lowercase()
            });
        }

        // Sort by ID
        brokers.sort_by_key(|b| b.id);

        Ok(Json(BrokerList { brokers }))
    }

    /// Get specific broker information
    async fn get_broker(
        State(svc): State<Arc<AdminService>>,
        Path(broker_id): Path<u32>,
    ) -> Result<Json<BrokerInfo>, AdminError> {
        let _span = span!(Level::INFO, "get_broker", broker_id = %broker_id);
        let _guard = _span.enter();

        let state = svc.cluster_state.read().await;
        let broker = state.brokers.get(&broker_id)
            .ok_or(AdminError::BrokerNotFound(broker_id))?;

        Ok(Json(broker.clone()))
    }

    /// List all topics
    async fn list_topics(
        State(svc): State<Arc<AdminService>>,
        Query(params): Query<ListTopicsQuery>,
    ) -> Result<Json<TopicList>, AdminError> {
        let _span = span!(Level::INFO, "list_topics");
        let _guard = _span.enter();

        let state = svc.cluster_state.read().await;
        let mut topics: Vec<_> = state.topics.values().cloned().collect();

        // Filter by pattern if provided
        if let Some(pattern) = params.pattern {
            topics.retain(|t| t.name.contains(&pattern));
        }

        // Sort by name
        topics.sort_by(|a, b| a.name.cmp(&b.name));

        Ok(Json(TopicList { topics }))
    }

    /// Get specific topic information
    async fn get_topic(
        State(svc): State<Arc<AdminService>>,
        Path(topic_name): Path<String>,
    ) -> Result<Json<TopicConfig>, AdminError> {
        let _span = span!(Level::INFO, "get_topic", topic = %topic_name);
        let _guard = _span.enter();

        let state = svc.cluster_state.read().await;
        let topic = state.topics.get(&topic_name)
            .ok_or(AdminError::TopicNotFound(topic_name))?;

        Ok(Json(topic.clone()))
    }

    /// Create new topic
    async fn create_topic(
        State(svc): State<Arc<AdminService>>,
        Json(req): Json<CreateTopicRequest>,
    ) -> Result<Json<TopicConfig>, AdminError> {
        let _span = span!(Level::INFO, "create_topic", topic = %req.name);
        let _guard = _span.enter();

        // Validate request
        req.validate()?;

        info!("Creating topic: {}", req.name);

        // Check if topic already exists
        {
            let state = svc.cluster_state.read().await;
            if state.topics.contains_key(&req.name) {
                return Err(AdminError::TopicAlreadyExists(req.name));
            }
        }

        // Create topic configuration
        let topic_config = TopicConfig {
            name: req.name.clone(),
            partition_count: req.partitions,
            replication_factor: req.replication_factor,
            config: req.config.unwrap_or_default(),
            created_at: SystemTime::now(),
            version: 1,
        };

        // Store in metastore
        svc.metastore.create_topic(&topic_config).await
            .map_err(AdminError::MetastoreError)?;

        // Update in-memory state
        {
            let mut state = svc.cluster_state.write().await;
            state.topics.insert(req.name.clone(), topic_config.clone());
            state.increment_metadata_version();
        }

        // Trigger metadata sync to propagate to cluster
        svc.sync_manager.trigger_topic_sync().await
            .map_err(AdminError::SyncError)?;
        
        // Create search index if backend manager is available
        if let Some(backend_manager) = &svc.backend_manager {
            if let Err(e) = backend_manager.create_search_index(&topic_config).await {
                warn!("Failed to create search index for topic {}: {}", req.name, e);
                // Continue anyway - index can be created later
            }
        }

        info!("Successfully created topic: {}", req.name);
        Ok(Json(topic_config))
    }

    /// Delete topic
    async fn delete_topic(
        State(svc): State<Arc<AdminService>>,
        Path(topic_name): Path<String>,
    ) -> Result<StatusCode, AdminError> {
        let _span = span!(Level::INFO, "delete_topic", topic = %topic_name);
        let _guard = _span.enter();

        info!("Deleting topic: {}", topic_name);

        // Check if topic exists
        {
            let state = svc.cluster_state.read().await;
            if !state.topics.contains_key(&topic_name) {
                return Err(AdminError::TopicNotFound(topic_name));
            }
        }

        // Delete from metastore
        svc.metastore.delete_topic(&topic_name).await
            .map_err(AdminError::MetastoreError)?;

        // Remove from in-memory state
        {
            let mut state = svc.cluster_state.write().await;
            state.topics.remove(&topic_name);
            state.increment_metadata_version();
        }

        // Trigger metadata sync
        svc.sync_manager.trigger_topic_sync().await
            .map_err(AdminError::SyncError)?;
        
        // Delete search index if backend manager is available
        if let Some(backend_manager) = &svc.backend_manager {
            if let Err(e) = backend_manager.delete_search_index(&topic_name).await {
                warn!("Failed to delete search index for topic {}: {}", topic_name, e);
                // Continue anyway
            }
        }

        info!("Successfully deleted topic: {}", topic_name);
        Ok(StatusCode::NO_CONTENT)
    }

    /// Get topic configuration
    async fn get_topic_config(
        State(svc): State<Arc<AdminService>>,
        Path(topic_name): Path<String>,
    ) -> Result<Json<HashMap<String, String>>, AdminError> {
        let _span = span!(Level::INFO, "get_topic_config", topic = %topic_name);
        let _guard = _span.enter();

        let state = svc.cluster_state.read().await;
        let topic = state.topics.get(&topic_name)
            .ok_or(AdminError::TopicNotFound(topic_name))?;

        Ok(Json(topic.config.clone()))
    }

    /// Update topic configuration
    async fn update_topic_config(
        State(svc): State<Arc<AdminService>>,
        Path(topic_name): Path<String>,
        Json(config): Json<HashMap<String, String>>,
    ) -> Result<StatusCode, AdminError> {
        let _span = span!(Level::INFO, "update_topic_config", topic = %topic_name);
        let _guard = _span.enter();

        info!("Updating topic config: {}", topic_name);

        // Validate configuration keys
        for key in config.keys() {
            if !is_valid_topic_config_key(key) {
                return Err(AdminError::InvalidTopicConfig(key.clone()));
            }
        }

        // Update in metastore
        svc.metastore.update_topic_config(&topic_name, &config).await
            .map_err(AdminError::MetastoreError)?;

        // Update in-memory state
        {
            let mut state = svc.cluster_state.write().await;
            if let Some(topic) = state.topics.get_mut(&topic_name) {
                topic.config.extend(config);
                topic.version += 1;
                state.increment_metadata_version();
            } else {
                return Err(AdminError::TopicNotFound(topic_name));
            }
        }

        // Trigger metadata sync
        svc.sync_manager.trigger_topic_sync().await
            .map_err(AdminError::SyncError)?;

        info!("Successfully updated topic config: {}", topic_name);
        Ok(StatusCode::NO_CONTENT)
    }

    /// List consumer groups
    async fn list_consumer_groups(
        State(svc): State<Arc<AdminService>>,
    ) -> Result<Json<ConsumerGroupList>, AdminError> {
        let _span = span!(Level::INFO, "list_consumer_groups");
        let _guard = _span.enter();

        let state = svc.cluster_state.read().await;
        let groups: Vec<_> = state.consumer_groups.values().cloned().collect();

        Ok(Json(ConsumerGroupList { groups }))
    }

    /// Get consumer group information
    async fn get_consumer_group(
        State(svc): State<Arc<AdminService>>,
        Path(group_id): Path<String>,
    ) -> Result<Json<ConsumerGroup>, AdminError> {
        let _span = span!(Level::INFO, "get_consumer_group", group_id = %group_id);
        let _guard = _span.enter();

        let state = svc.cluster_state.read().await;
        let group = state.consumer_groups.get(&group_id)
            .ok_or(AdminError::ConsumerGroupNotFound(group_id))?;

        Ok(Json(group.clone()))
    }

    /// Get sync status
    async fn get_sync_status(State(svc): State<Arc<AdminService>>) -> Result<Json<SyncStatus>, AdminError> {
        let _span = span!(Level::INFO, "get_sync_status");
        let _guard = _span.enter();

        let status = svc.sync_manager.get_status().await;
        Ok(Json(status))
    }

    /// Trigger manual metadata sync
    async fn trigger_sync(State(svc): State<Arc<AdminService>>) -> Result<StatusCode, AdminError> {
        let _span = span!(Level::INFO, "trigger_sync");
        let _guard = _span.enter();

        info!("Triggering manual metadata sync");

        svc.sync_manager.trigger_full_sync().await
            .map_err(AdminError::SyncError)?;

        Ok(StatusCode::ACCEPTED)
    }
    
    /// Get backend service statistics
    async fn get_backend_stats(State(svc): State<Arc<AdminService>>) -> Result<Json<BackendStats>, AdminError> {
        let _span = span!(Level::INFO, "get_backend_stats");
        let _guard = _span.enter();

        if let Some(backend_manager) = &svc.backend_manager {
            let stats = backend_manager.get_cluster_stats().await
                .map_err(|e| AdminError::Internal(format!("Failed to get backend stats: {}", e)))?;
            
            Ok(Json(BackendStats {
                active_ingest_nodes: stats.active_ingest_nodes,
                total_ingest_nodes: stats.total_ingest_nodes,
                active_search_nodes: stats.active_search_nodes,
                total_search_nodes: stats.total_search_nodes,
                search_docs_count: stats.search_docs_count,
                search_index_count: stats.search_index_count,
            }))
        } else {
            Err(AdminError::Unavailable("Backend manager not configured".to_string()))
        }
    }
    
    /// Refresh backend service connections
    async fn refresh_backend_connections(State(svc): State<Arc<AdminService>>) -> Result<StatusCode, AdminError> {
        let _span = span!(Level::INFO, "refresh_backend_connections");
        let _guard = _span.enter();

        info!("Refreshing backend service connections");

        if let Some(backend_manager) = &svc.backend_manager {
            backend_manager.initialize().await
                .map_err(|e| AdminError::Internal(format!("Failed to refresh connections: {}", e)))?;
            
            Ok(StatusCode::ACCEPTED)
        } else {
            Err(AdminError::Unavailable("Backend manager not configured".to_string()))
        }
    }
    
    /// Placeholder for missing endpoints
    async fn get_topic_partitions() -> Result<StatusCode, AdminError> {
        Err(AdminError::NotImplemented("Topic partitions endpoint not implemented".to_string()))
    }
    
    async fn delete_consumer_group() -> Result<StatusCode, AdminError> {
        Err(AdminError::NotImplemented("Delete consumer group not implemented".to_string()))
    }
    
    async fn reset_consumer_group() -> Result<StatusCode, AdminError> {
        Err(AdminError::NotImplemented("Reset consumer group not implemented".to_string()))
    }
    
    async fn get_metadata_version() -> Result<StatusCode, AdminError> {
        Err(AdminError::NotImplemented("Get metadata version not implemented".to_string()))
    }
    
    async fn trigger_rebalance() -> Result<StatusCode, AdminError> {
        Err(AdminError::NotImplemented("Trigger rebalance not implemented".to_string()))
    }
    
    async fn trigger_leader_election() -> Result<StatusCode, AdminError> {
        Err(AdminError::NotImplemented("Trigger leader election not implemented".to_string()))
    }
    
    /// List all partition reassignments
    async fn list_reassignments(State(svc): State<Arc<AdminService>>) -> Result<Json<ReassignmentList>, AdminError> {
        let _span = span!(Level::INFO, "list_reassignments");
        let _guard = _span.enter();

        if let Some(reassignment_manager) = &svc.reassignment_manager {
            let reassignments = reassignment_manager.list_active_reassignments().await;
            Ok(Json(ReassignmentList { reassignments }))
        } else {
            Err(AdminError::Unavailable("Reassignment manager not configured".to_string()))
        }
    }
    
    /// Start a partition reassignment
    async fn start_reassignment(
        State(svc): State<Arc<AdminService>>,
        Json(req): Json<StartReassignmentRequest>,
    ) -> Result<Json<ReassignmentResponse>, AdminError> {
        let _span = span!(Level::INFO, "start_reassignment", topic = %req.topic);
        let _guard = _span.enter();

        info!("Starting partition reassignment for topic {}", req.topic);

        if let Some(reassignment_manager) = &svc.reassignment_manager {
            let reassignment_id = reassignment_manager
                .start_partition_reassignment(req.topic, req.partitions)
                .await
                .map_err(|e| AdminError::Internal(format!("Failed to start reassignment: {}", e)))?;
            
            Ok(Json(ReassignmentResponse {
                id: reassignment_id,
                status: "started".to_string(),
                message: "Partition reassignment has been queued".to_string(),
            }))
        } else {
            Err(AdminError::Unavailable("Reassignment manager not configured".to_string()))
        }
    }
    
    /// Get reassignment status
    async fn get_reassignment(
        State(svc): State<Arc<AdminService>>,
        Path(reassignment_id): Path<String>,
    ) -> Result<Json<crate::partition_reassignment::PartitionReassignment>, AdminError> {
        let _span = span!(Level::INFO, "get_reassignment", id = %reassignment_id);
        let _guard = _span.enter();

        if let Some(reassignment_manager) = &svc.reassignment_manager {
            if let Some(reassignment) = reassignment_manager.get_reassignment_status(&reassignment_id).await {
                Ok(Json(reassignment))
            } else {
                Err(AdminError::NotFound(format!("Reassignment {} not found", reassignment_id)))
            }
        } else {
            Err(AdminError::Unavailable("Reassignment manager not configured".to_string()))
        }
    }
    
    /// Cancel a partition reassignment
    async fn cancel_reassignment(
        State(svc): State<Arc<AdminService>>,
        Path(reassignment_id): Path<String>,
    ) -> Result<StatusCode, AdminError> {
        let _span = span!(Level::INFO, "cancel_reassignment", id = %reassignment_id);
        let _guard = _span.enter();

        info!("Cancelling reassignment {}", reassignment_id);

        if let Some(reassignment_manager) = &svc.reassignment_manager {
            reassignment_manager
                .cancel_reassignment(&reassignment_id)
                .await
                .map_err(|e| AdminError::Internal(format!("Failed to cancel reassignment: {}", e)))?;
            
            Ok(StatusCode::NO_CONTENT)
        } else {
            Err(AdminError::Unavailable("Reassignment manager not configured".to_string()))
        }
    }
    
    /// List all rolling upgrades
    async fn list_upgrades(State(svc): State<Arc<AdminService>>) -> Result<Json<UpgradeList>, AdminError> {
        let _span = span!(Level::INFO, "list_upgrades");
        let _guard = _span.enter();

        if let Some(reassignment_manager) = &svc.reassignment_manager {
            let upgrades = reassignment_manager.list_active_upgrades().await;
            Ok(Json(UpgradeList { upgrades }))
        } else {
            Err(AdminError::Unavailable("Reassignment manager not configured".to_string()))
        }
    }
    
    /// Start a rolling upgrade
    async fn start_upgrade(
        State(svc): State<Arc<AdminService>>,
        Json(req): Json<StartUpgradeRequest>,
    ) -> Result<Json<UpgradeResponse>, AdminError> {
        let _span = span!(Level::INFO, "start_upgrade", upgrade_type = ?req.upgrade_type);
        let _guard = _span.enter();

        info!("Starting rolling upgrade for {:?} to version {}", req.upgrade_type, req.target_version);

        if let Some(reassignment_manager) = &svc.reassignment_manager {
            let upgrade_id = reassignment_manager
                .start_rolling_upgrade(req.upgrade_type, req.target_version, req.config)
                .await
                .map_err(|e| AdminError::Internal(format!("Failed to start upgrade: {}", e)))?;
            
            Ok(Json(UpgradeResponse {
                id: upgrade_id,
                status: "started".to_string(),
                message: "Rolling upgrade has been queued".to_string(),
            }))
        } else {
            Err(AdminError::Unavailable("Reassignment manager not configured".to_string()))
        }
    }
    
    /// Get upgrade status
    async fn get_upgrade(
        State(svc): State<Arc<AdminService>>,
        Path(upgrade_id): Path<String>,
    ) -> Result<Json<crate::partition_reassignment::RollingUpgrade>, AdminError> {
        let _span = span!(Level::INFO, "get_upgrade", id = %upgrade_id);
        let _guard = _span.enter();

        if let Some(reassignment_manager) = &svc.reassignment_manager {
            if let Some(upgrade) = reassignment_manager.get_upgrade_status(&upgrade_id).await {
                Ok(Json(upgrade))
            } else {
                Err(AdminError::NotFound(format!("Upgrade {} not found", upgrade_id)))
            }
        } else {
            Err(AdminError::Unavailable("Reassignment manager not configured".to_string()))
        }
    }
    
    /// Cancel a rolling upgrade
    async fn cancel_upgrade(
        State(svc): State<Arc<AdminService>>,
        Path(upgrade_id): Path<String>,
    ) -> Result<StatusCode, AdminError> {
        let _span = span!(Level::INFO, "cancel_upgrade", id = %upgrade_id);
        let _guard = _span.enter();

        info!("Cancelling upgrade {}", upgrade_id);

        if let Some(reassignment_manager) = &svc.reassignment_manager {
            reassignment_manager
                .cancel_upgrade(&upgrade_id)
                .await
                .map_err(|e| AdminError::Internal(format!("Failed to cancel upgrade: {}", e)))?;
            
            Ok(StatusCode::NO_CONTENT)
        } else {
            Err(AdminError::Unavailable("Reassignment manager not configured".to_string()))
        }
    }
}

// Request/Response types

#[derive(Debug, Serialize)]
pub struct ClusterStatus {
    pub healthy: bool,
    pub broker_count: usize,
    pub topic_count: usize,
    pub total_partitions: usize,
    pub consumer_groups: usize,
    pub metadata_version: u64,
    pub last_sync: Option<SystemTime>,
    pub controller_id: Option<u32>,
    pub leader_epoch: u64,
    pub backend_stats: Option<crate::backend_client::ClusterStats>,
}

#[derive(Debug, Serialize)]
pub struct ClusterHealth {
    pub status: String,
    pub brokers_online: usize,
    pub brokers_total: usize,
    pub metadata_sync_healthy: bool,
    pub last_metadata_sync: Option<SystemTime>,
    pub raft_status: Option<String>,
    pub issues: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct ListBrokersQuery {
    pub status: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct BrokerList {
    pub brokers: Vec<BrokerInfo>,
}

#[derive(Debug, Deserialize)]
pub struct ListTopicsQuery {
    pub pattern: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct TopicList {
    pub topics: Vec<TopicConfig>,
}

#[derive(Debug, Deserialize)]
pub struct CreateTopicRequest {
    pub name: String,
    pub partitions: u32,
    pub replication_factor: u16,
    pub config: Option<HashMap<String, String>>,
}

impl CreateTopicRequest {
    fn validate(&self) -> Result<(), AdminError> {
        if self.name.is_empty() {
            return Err(AdminError::InvalidRequest("Topic name cannot be empty".to_string()));
        }
        
        if self.name.len() > 255 {
            return Err(AdminError::InvalidRequest("Topic name too long".to_string()));
        }
        
        if self.partitions == 0 {
            return Err(AdminError::InvalidRequest("Partition count must be positive".to_string()));
        }
        
        if self.replication_factor == 0 {
            return Err(AdminError::InvalidRequest("Replication factor must be positive".to_string()));
        }

        Ok(())
    }
}

#[derive(Debug, Serialize)]
pub struct ConsumerGroupList {
    pub groups: Vec<ConsumerGroup>,
}


#[derive(Debug, Serialize)]
pub struct BackendStats {
    pub active_ingest_nodes: usize,
    pub total_ingest_nodes: usize,
    pub active_search_nodes: usize,
    pub total_search_nodes: usize,
    pub search_docs_count: usize,
    pub search_index_count: usize,
}

// Partition Reassignment API Types

#[derive(Debug, Deserialize)]
pub struct StartReassignmentRequest {
    pub topic: String,
    pub partitions: Vec<PartitionAssignment>,
}

#[derive(Debug, Serialize)]
pub struct ReassignmentResponse {
    pub id: String,
    pub status: String,
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct ReassignmentList {
    pub reassignments: Vec<crate::partition_reassignment::PartitionReassignment>,
}

// Rolling Upgrade API Types

#[derive(Debug, Deserialize)]
pub struct StartUpgradeRequest {
    pub upgrade_type: UpgradeType,
    pub target_version: String,
    pub config: Option<UpgradeConfig>,
}

#[derive(Debug, Serialize)]
pub struct UpgradeResponse {
    pub id: String,
    pub status: String,
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct UpgradeList {
    pub upgrades: Vec<crate::partition_reassignment::RollingUpgrade>,
}

// Error handling

#[derive(Debug, thiserror::Error)]
pub enum AdminError {
    #[error("Topic not found: {0}")]
    TopicNotFound(String),
    
    #[error("Topic already exists: {0}")]
    TopicAlreadyExists(String),
    
    #[error("Broker not found: {0}")]
    BrokerNotFound(u32),
    
    #[error("Consumer group not found: {0}")]
    ConsumerGroupNotFound(String),
    
    #[error("Invalid request: {0}")]
    InvalidRequest(String),
    
    #[error("Invalid topic configuration key: {0}")]
    InvalidTopicConfig(String),
    
    #[error("Metastore error: {0}")]
    MetastoreError(chronik_common::Error),
    
    #[error("Sync error: {0}")]
    SyncError(String),
    
    #[error("Internal error: {0}")]
    Internal(String),
    
    #[error("Service unavailable: {0}")]
    Unavailable(String),
    
    #[error("Not implemented: {0}")]
    NotImplemented(String),
    
    #[error("Not found: {0}")]
    NotFound(String),
}

impl IntoResponse for AdminError {
    fn into_response(self) -> Response {
        let (status, error_message) = match self {
            AdminError::TopicNotFound(_) | 
            AdminError::BrokerNotFound(_) | 
            AdminError::ConsumerGroupNotFound(_) | 
            AdminError::NotFound(_) => (StatusCode::NOT_FOUND, self.to_string()),
            
            AdminError::TopicAlreadyExists(_) => (StatusCode::CONFLICT, self.to_string()),
            
            AdminError::InvalidRequest(_) | 
            AdminError::InvalidTopicConfig(_) => (StatusCode::BAD_REQUEST, self.to_string()),
            
            AdminError::MetastoreError(_) | 
            AdminError::SyncError(_) | 
            AdminError::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, self.to_string()),
            
            AdminError::Unavailable(_) => (StatusCode::SERVICE_UNAVAILABLE, self.to_string()),
            
            AdminError::NotImplemented(_) => (StatusCode::NOT_IMPLEMENTED, self.to_string()),
        };

        let body = Json(serde_json::json!({
            "error": error_message,
            "timestamp": chrono::Utc::now().to_rfc3339(),
        }));

        (status, body).into_response()
    }
}

// Helper functions

fn is_valid_topic_config_key(key: &str) -> bool {
    matches!(key, 
        "cleanup.policy" | 
        "compression.type" | 
        "delete.retention.ms" | 
        "file.delete.delay.ms" | 
        "flush.messages" | 
        "flush.ms" | 
        "follower.replication.throttled.replicas" | 
        "index.interval.bytes" | 
        "leader.replication.throttled.replicas" | 
        "max.message.bytes" | 
        "message.format.version" | 
        "message.timestamp.difference.max.ms" | 
        "message.timestamp.type" | 
        "min.cleanable.dirty.ratio" | 
        "min.compaction.lag.ms" | 
        "min.insync.replicas" | 
        "preallocate" | 
        "retention.bytes" | 
        "retention.ms" | 
        "segment.bytes" | 
        "segment.index.bytes" | 
        "segment.jitter.ms" | 
        "segment.ms" | 
        "unclean.leader.election.enable"
    )
}