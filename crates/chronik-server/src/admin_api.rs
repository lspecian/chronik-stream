//! HTTP Admin API for Cluster Management (v2.2.7+) and Schema Registry
//!
//! Provides REST endpoints for managing cluster membership:
//! - POST /admin/add-node - Add new node to cluster
//! - POST /admin/remove-node - Remove node from cluster
//! - GET /admin/status - Get cluster status
//!
//! And Confluent-compatible Schema Registry API:
//! - POST /subjects/{subject}/versions - Register a schema
//! - GET /schemas/ids/{id} - Get schema by ID
//! - GET /subjects/{subject}/versions/latest - Get latest schema
//! - GET /subjects - List all subjects
//! - DELETE /subjects/{subject} - Delete a subject
//!
//! This API is only available when running in cluster mode.
//!
//! ## Authentication
//!
//! ### Admin API
//!
//! The admin API requires API key authentication via the `X-API-Key` header.
//! Configure the API key via the `CHRONIK_ADMIN_API_KEY` environment variable.
//! If not set, authentication is disabled (INSECURE - for development only).
//!
//! ### Schema Registry (Confluent-compatible)
//!
//! Schema Registry supports optional HTTP Basic Auth (like Confluent):
//! - `CHRONIK_SCHEMA_REGISTRY_AUTH_ENABLED=true` - Enable authentication
//! - `CHRONIK_SCHEMA_REGISTRY_USERS=admin:secret,readonly:pass` - Define users
//!
//! When enabled, clients must provide `Authorization: Basic <base64>` header.
//! Example: `curl -u admin:secret http://localhost:10001/subjects`
//!
//! By default, authentication is disabled (Confluent-compatible default).

use anyhow::Result;
use axum::{
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
    Json, Router, Server,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{debug, error, info, warn};

use crate::isr_tracker::IsrTracker;
use crate::raft_cluster::RaftCluster;
use crate::schema_registry::{
    CompatibilityConfig, CompatibilityLevel, GetSchemaResponse, GetSubjectVersionResponse,
    RegisterSchemaRequest, RegisterSchemaResponse, SchemaRegistry, SchemaRegistryConfig,
    SchemaRegistryError, SchemaType, SchemaVersionRef,
};
use chronik_common::metadata::traits::MetadataStore;

/// Admin API state shared across handlers
#[derive(Clone)]
pub struct AdminApiState {
    pub raft_cluster: Arc<RaftCluster>,
    pub metadata_store: Arc<dyn MetadataStore>,
    pub api_key: Option<String>,
    /// v2.2.22: ISR tracker for accurate in-sync replica queries
    pub isr_tracker: Option<Arc<IsrTracker>>,
    /// Phase 5: Schema Registry for Confluent-compatible schema management
    pub schema_registry: Arc<SchemaRegistry>,
}

/// Request to add a new node to the cluster
#[derive(Debug, Deserialize)]
pub struct AddNodeRequest {
    /// Node ID (must be unique)
    pub node_id: u64,
    /// Kafka broker address (host:port)
    pub kafka_addr: String,
    /// WAL replication address (host:port)
    pub wal_addr: String,
    /// Raft gRPC address (host:port)
    pub raft_addr: String,
}

/// Response from add-node request
#[derive(Debug, Serialize, Deserialize)]
pub struct AddNodeResponse {
    pub success: bool,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_id: Option<u64>,
}

/// Request to remove a node from the cluster (Priority 4)
#[derive(Debug, Deserialize)]
pub struct RemoveNodeRequest {
    /// Node ID to remove
    pub node_id: u64,
    /// Force removal (skip partition reassignment for dead nodes)
    #[serde(default)]
    pub force: bool,
}

/// Response from remove-node request (Priority 4)
#[derive(Debug, Serialize, Deserialize)]
pub struct RemoveNodeResponse {
    pub success: bool,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_id: Option<u64>,
}

/// Health check response
#[derive(Debug, Serialize, Deserialize)]
pub struct HealthResponse {
    pub status: String,
    pub node_id: u64,
    pub is_leader: bool,
    pub cluster_nodes: Vec<u64>,
}

/// Cluster status response (Priority 3)
#[derive(Debug, Serialize, Deserialize)]
pub struct ClusterStatusResponse {
    /// Current node ID
    pub node_id: u64,
    /// Current leader node ID (if known)
    pub leader_id: Option<u64>,
    /// Whether this node is the leader
    pub is_leader: bool,
    /// List of all nodes in the cluster
    pub nodes: Vec<NodeInfo>,
    /// Partition assignments
    pub partitions: Vec<PartitionInfo>,
}

/// Information about a cluster node
#[derive(Debug, Serialize, Deserialize)]
pub struct NodeInfo {
    pub node_id: u64,
    pub address: String,
    pub is_leader: bool,
}

/// Information about a partition
#[derive(Debug, Serialize, Deserialize)]
pub struct PartitionInfo {
    pub topic: String,
    pub partition: i32,
    pub leader: Option<u64>,
    pub replicas: Vec<u64>,
    pub isr: Vec<u64>,
}

/// Response from rebalance request
#[derive(Debug, Serialize, Deserialize)]
pub struct RebalanceResponse {
    pub success: bool,
    pub message: String,
    pub topics_rebalanced: usize,
}

/// Generic error response
#[derive(Debug, Serialize)]
struct ErrorResponse {
    success: bool,
    message: String,
}

/// Admin API error wrapper
pub struct AdminApiError(anyhow::Error);

impl IntoResponse for AdminApiError {
    fn into_response(self) -> Response {
        let message = self.0.to_string();
        error!("Admin API error: {}", message);

        // Return a generic error JSON
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                success: false,
                message,
            }),
        )
            .into_response()
    }
}

impl<E> From<E> for AdminApiError
where
    E: Into<anyhow::Error>,
{
    fn from(err: E) -> Self {
        Self(err.into())
    }
}

/// Helper function to call propose_remove_node
///
/// NOTE: This helper exists because calling `propose_remove_node` directly
/// in the handler causes a Rust compiler type inference issue with axum 0.6's
/// Handler trait. The method internally calls `reassign_partitions_from_node`
/// which has multiple nested await points in a loop, creating a complex Future
/// type that confuses the compiler. Boxing the future resolves this.
fn do_remove_node(
    raft_cluster: Arc<RaftCluster>,
    node_id: u64,
    force: bool,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send>> {
    Box::pin(async move {
        raft_cluster.propose_remove_node(node_id, force).await
    })
}

/// POST /admin/remove-node - Remove a node from the cluster
///
/// **Implementation Note**: This handler uses a helper function `do_remove_node` that
/// boxes the Future to resolve a Rust compiler type inference issue with axum 0.6's
/// Handler trait. Direct calls to `propose_remove_node` fail because the method
/// internally calls `reassign_partitions_from_node` which has multiple nested await
/// points in a loop, creating a complex Future type that confuses the compiler.
async fn handle_node_removal(
    State(state): State<AdminApiState>,
    Json(req): Json<RemoveNodeRequest>,
) -> Result<Json<RemoveNodeResponse>, AdminApiError> {
    info!(
        "Admin API: Received remove-node request for node {} (force={})",
        req.node_id, req.force
    );

    // Call helper function instead of direct method call
    match do_remove_node(state.raft_cluster.clone(), req.node_id, req.force).await {
        Ok(()) => {
            info!("✓ Successfully proposed removing node {}", req.node_id);
            Ok(Json(RemoveNodeResponse {
                success: true,
                message: format!(
                    "Node {} removal proposed. Waiting for Raft consensus...",
                    req.node_id
                ),
                node_id: Some(req.node_id),
            }))
        }
        Err(e) => {
            error!("Failed to propose removing node {}: {}", req.node_id, e);
            Ok(Json(RemoveNodeResponse {
                success: false,
                message: format!("Failed to remove node: {}", e),
                node_id: None,
            }))
        }
    }
}

/// POST /admin/add-node - Add a new node to the cluster
async fn handle_add_node(
    State(state): State<AdminApiState>,
    Json(req): Json<AddNodeRequest>,
) -> Result<Json<AddNodeResponse>, AdminApiError> {
    info!(
        "Admin API: Received add-node request for node {} (kafka={}, wal={}, raft={})",
        req.node_id, req.kafka_addr, req.wal_addr, req.raft_addr
    );

    // Validate addresses (basic format check)
    if !req.kafka_addr.contains(':') || !req.wal_addr.contains(':') || !req.raft_addr.contains(':')
    {
        return Ok(Json(AddNodeResponse {
            success: false,
            message: "Invalid address format. Expected 'host:port'".to_string(),
            node_id: None,
        }));
    }

    // Call RaftCluster::propose_add_node()
    match state
        .raft_cluster
        .propose_add_node(
            req.node_id,
            req.kafka_addr.clone(),
            req.wal_addr.clone(),
            req.raft_addr.clone(),
        )
        .await
    {
        Ok(_) => {
            info!("✓ Successfully proposed adding node {}", req.node_id);
            Ok(Json(AddNodeResponse {
                success: true,
                message: format!(
                    "Node {} addition proposed. Waiting for Raft consensus...",
                    req.node_id
                ),
                node_id: Some(req.node_id),
            }))
        }
        Err(e) => {
            error!("Failed to propose adding node {}: {}", req.node_id, e);
            Ok(Json(AddNodeResponse {
                success: false,
                message: format!("Failed to add node: {}", e),
                node_id: None,
            }))
        }
    }
}

/// GET /admin/health - Health check endpoint
async fn handle_health(
    State(state): State<AdminApiState>,
) -> Result<Json<HealthResponse>, AdminApiError> {
    let cluster_nodes = state.raft_cluster.get_all_nodes().await;
    let node_id = state.raft_cluster.node_id();

    // Check if this node is the leader
    let is_leader = state.raft_cluster.is_leader().await;

    Ok(Json(HealthResponse {
        status: "ok".to_string(),
        node_id,
        is_leader,
        cluster_nodes,
    }))
}

/// GET /admin/status - Get detailed cluster status (Priority 3)
async fn handle_status(
    State(state): State<AdminApiState>,
) -> Result<Json<ClusterStatusResponse>, AdminApiError> {
    info!("Admin API: Received cluster status request");

    let node_id = state.raft_cluster.node_id();
    let is_leader = state.raft_cluster.is_leader().await;
    let (leader_ready, leader_id, _) = state.raft_cluster.is_leader_ready().await;

    // Get all nodes with their information
    let node_info = state.raft_cluster.get_node_info();
    let nodes: Vec<NodeInfo> = node_info.iter().map(|(id, addr)| {
        NodeInfo {
            node_id: *id,
            address: addr.clone(),
            is_leader: *id == leader_id,
        }
    }).collect();

    // Get all partition assignments from metadata store (not Raft - v2.2.13 fix)
    // v2.2.22: Pass isr_tracker for accurate ISR queries
    let partitions = match collect_partition_info_from_metadata(
        state.metadata_store.clone(),
        state.isr_tracker.clone(),
    ).await {
        Ok(parts) => parts,
        Err(e) => {
            warn!("Failed to collect partition info from metadata store: {:?}", e);
            Vec::new()
        }
    };

    Ok(Json(ClusterStatusResponse {
        node_id,
        leader_id: if leader_ready { Some(leader_id) } else { None },
        is_leader,
        nodes,
        partitions,
    }))
}

/// POST /admin/rebalance - Trigger manual partition rebalancing
async fn handle_rebalance(
    State(state): State<AdminApiState>,
) -> Result<Json<RebalanceResponse>, AdminApiError> {
    info!("Admin API: Received rebalance request");

    // Get current cluster nodes
    let current_nodes = state.raft_cluster.get_all_nodes().await;
    let node_count = current_nodes.len();

    if node_count == 0 {
        return Ok(Json(RebalanceResponse {
            success: false,
            message: "No nodes available in cluster".to_string(),
            topics_rebalanced: 0,
        }));
    }

    // Get all topics
    let topics = match state.metadata_store.list_topics().await {
        Ok(t) => t,
        Err(e) => {
            return Ok(Json(RebalanceResponse {
                success: false,
                message: format!("Failed to list topics: {:?}", e),
                topics_rebalanced: 0,
            }));
        }
    };

    if topics.is_empty() {
        return Ok(Json(RebalanceResponse {
            success: true,
            message: "No topics to rebalance".to_string(),
            topics_rebalanced: 0,
        }));
    }

    info!("Rebalancing {} topics across {} nodes", topics.len(), node_count);

    // Get replication factor from config (default to min of node_count and 3)
    let replication_factor = std::cmp::min(node_count, 3);
    let mut topics_rebalanced = 0;
    let mut errors = Vec::new();

    for topic_meta in &topics {
        let topic = &topic_meta.name;
        let partition_count = topic_meta.config.partition_count as i32;

        if partition_count == 0 {
            continue;
        }

        // Calculate new assignments using round-robin
        for partition in 0..partition_count {
            let mut replicas = Vec::new();
            for offset in 0..replication_factor {
                let node_index = (partition as usize + offset) % node_count;
                replicas.push(current_nodes[node_index]);
            }

            let leader_id = replicas.first().copied().unwrap_or(0);

            let assignment = chronik_common::metadata::PartitionAssignment {
                topic: topic.clone(),
                partition: partition as u32,
                broker_id: leader_id as i32,
                is_leader: true,
                replicas: replicas.clone(),
                leader_id,
            };

            if let Err(e) = state.metadata_store.assign_partition(assignment).await {
                errors.push(format!("{}-{}: {:?}", topic, partition, e));
            }
        }

        topics_rebalanced += 1;
        info!("✓ Rebalanced topic '{}' ({} partitions)", topic, partition_count);
    }

    if errors.is_empty() {
        Ok(Json(RebalanceResponse {
            success: true,
            message: format!(
                "Successfully rebalanced {} topics across {} nodes",
                topics_rebalanced, node_count
            ),
            topics_rebalanced,
        }))
    } else {
        Ok(Json(RebalanceResponse {
            success: false,
            message: format!(
                "Rebalanced {} topics with {} errors: {}",
                topics_rebalanced,
                errors.len(),
                errors.join(", ")
            ),
            topics_rebalanced,
        }))
    }
}

/// Helper function to collect all partition information from metadata store
///
/// Queries metadata store for all topics and their partition assignments,
/// converting them to PartitionInfo format for the admin API.
///
/// v2.2.22: Uses IsrTracker (if available) for accurate ISR queries
async fn collect_partition_info_from_metadata(
    metadata_store: Arc<dyn MetadataStore>,
    isr_tracker: Option<Arc<IsrTracker>>,
) -> Result<Vec<PartitionInfo>> {
    let mut all_partitions = Vec::new();

    // List all topics
    let topics = metadata_store.list_topics().await
        .map_err(|e| anyhow::anyhow!("Failed to list topics: {:?}", e))?;

    // For each topic, get partition assignments
    for topic_meta in topics {
        match metadata_store.get_partition_assignments(&topic_meta.name).await {
            Ok(assignments) => {
                for assignment in assignments {
                    // v2.2.22: Use IsrTracker for accurate ISR if available
                    let isr = if let Some(ref tracker) = isr_tracker {
                        // Get leader high watermark for ISR calculation
                        let leader_offset = metadata_store
                            .get_partition_offset(&assignment.topic, assignment.partition)
                            .await
                            .ok()
                            .flatten()
                            .map(|(hw, _)| hw)  // Extract high watermark from (hw, log_start_offset) tuple
                            .unwrap_or(0);

                        // Get ISR from tracker - includes replicas that have caught up
                        let tracked_isr = tracker.get_isr(
                            &assignment.topic,
                            assignment.partition as i32,
                            leader_offset,
                            &assignment.replicas,
                        );

                        // If tracker has data, use it; otherwise fall back to all replicas
                        // (on cluster startup before any ACKs, ISR = replicas is correct)
                        if tracked_isr.is_empty() {
                            assignment.replicas.clone()
                        } else {
                            // Always include leader in ISR (leader is always in-sync with itself)
                            let mut isr_with_leader = tracked_isr;
                            if !isr_with_leader.contains(&assignment.leader_id) {
                                isr_with_leader.insert(0, assignment.leader_id);
                            }
                            isr_with_leader
                        }
                    } else {
                        // No tracker available - fall back to ISR = replicas
                        assignment.replicas.clone()
                    };

                    all_partitions.push(PartitionInfo {
                        topic: assignment.topic.clone(),
                        partition: assignment.partition as i32,
                        leader: Some(assignment.leader_id),
                        replicas: assignment.replicas.clone(),
                        isr,
                    });
                }
            }
            Err(e) => {
                warn!("Failed to get partition assignments for topic {}: {:?}", topic_meta.name, e);
            }
        }
    }

    Ok(all_partitions)
}

// =============================================================================
// Schema Registry HTTP Handlers (Confluent-compatible REST API)
// =============================================================================

/// Schema Registry error response
#[derive(Debug, Serialize)]
struct SchemaErrorResponse {
    error_code: u32,
    message: String,
}

impl From<SchemaRegistryError> for (StatusCode, Json<SchemaErrorResponse>) {
    fn from(err: SchemaRegistryError) -> Self {
        let (code, status) = match &err {
            SchemaRegistryError::SchemaNotFound(_) => (40403, StatusCode::NOT_FOUND),
            SchemaRegistryError::SubjectNotFound(_) => (40401, StatusCode::NOT_FOUND),
            SchemaRegistryError::VersionNotFound(_, _) => (40402, StatusCode::NOT_FOUND),
            SchemaRegistryError::InvalidSchema(_) => (42201, StatusCode::UNPROCESSABLE_ENTITY),
            SchemaRegistryError::Incompatible(_) => (409, StatusCode::CONFLICT),
            SchemaRegistryError::SubjectAlreadyExists(_) => (409, StatusCode::CONFLICT),
        };

        (
            status,
            Json(SchemaErrorResponse {
                error_code: code,
                message: err.to_string(),
            }),
        )
    }
}

/// POST /subjects/{subject}/versions - Register a new schema under a subject
async fn handle_register_schema(
    State(state): State<AdminApiState>,
    Path(subject): Path<String>,
    Json(req): Json<RegisterSchemaRequest>,
) -> Result<Json<RegisterSchemaResponse>, (StatusCode, Json<SchemaErrorResponse>)> {
    debug!("Schema Registry: Registering schema for subject '{}'", subject);

    let schema_type = req.schema_type;
    let references = req.references;

    match state
        .schema_registry
        .register_schema(&subject, &req.schema, schema_type, references)
        .await
    {
        Ok(id) => {
            info!("Schema Registry: Registered schema ID {} for subject '{}'", id, subject);
            Ok(Json(RegisterSchemaResponse { id }))
        }
        Err(e) => {
            warn!("Schema Registry: Failed to register schema for '{}': {}", subject, e);
            Err(e.into())
        }
    }
}

/// GET /schemas/ids/{id} - Get schema by global ID
async fn handle_get_schema_by_id(
    State(state): State<AdminApiState>,
    Path(id): Path<u32>,
) -> Result<Json<GetSchemaResponse>, (StatusCode, Json<SchemaErrorResponse>)> {
    debug!("Schema Registry: Getting schema by ID {}", id);

    match state.schema_registry.get_schema(id).await {
        Some(schema) => Ok(Json(GetSchemaResponse {
            schema: schema.schema,
            schema_type: schema.schema_type,
            references: schema.references,
        })),
        None => Err(SchemaRegistryError::SchemaNotFound(id).into()),
    }
}

/// GET /subjects/{subject}/versions/{version} - Get schema by subject and version
async fn handle_get_subject_version(
    State(state): State<AdminApiState>,
    Path((subject, version_str)): Path<(String, String)>,
) -> Result<Json<GetSubjectVersionResponse>, (StatusCode, Json<SchemaErrorResponse>)> {
    debug!("Schema Registry: Getting subject '{}' version '{}'", subject, version_str);

    let version_ref = version_str
        .parse::<SchemaVersionRef>()
        .map_err(|_| SchemaRegistryError::InvalidSchema("Invalid version".to_string()))?;

    match state.schema_registry.get_schema_by_subject(&subject, version_ref).await {
        Some(sv) => Ok(Json(GetSubjectVersionResponse {
            subject: subject.clone(),
            version: sv.version,
            id: sv.schema_id,
            schema: sv.schema,
            schema_type: sv.schema_type,
        })),
        None => {
            match version_ref {
                SchemaVersionRef::Latest => Err(SchemaRegistryError::SubjectNotFound(subject).into()),
                SchemaVersionRef::Version(v) => Err(SchemaRegistryError::VersionNotFound(subject, v).into()),
            }
        }
    }
}

/// GET /subjects - List all subjects
async fn handle_list_subjects(
    State(state): State<AdminApiState>,
) -> Json<Vec<String>> {
    debug!("Schema Registry: Listing all subjects");
    let subjects = state.schema_registry.list_subjects().await;
    Json(subjects)
}

/// GET /subjects/{subject}/versions - List all versions for a subject
async fn handle_list_versions(
    State(state): State<AdminApiState>,
    Path(subject): Path<String>,
) -> Result<Json<Vec<u32>>, (StatusCode, Json<SchemaErrorResponse>)> {
    debug!("Schema Registry: Listing versions for subject '{}'", subject);

    match state.schema_registry.list_versions(&subject).await {
        Some(versions) => Ok(Json(versions)),
        None => Err(SchemaRegistryError::SubjectNotFound(subject).into()),
    }
}

/// DELETE /subjects/{subject} - Delete a subject and all its versions
async fn handle_delete_subject(
    State(state): State<AdminApiState>,
    Path(subject): Path<String>,
) -> Result<Json<Vec<u32>>, (StatusCode, Json<SchemaErrorResponse>)> {
    debug!("Schema Registry: Deleting subject '{}'", subject);

    match state.schema_registry.delete_subject(&subject, false).await {
        Ok(versions) => {
            info!("Schema Registry: Deleted subject '{}' ({} versions)", subject, versions.len());
            Ok(Json(versions))
        }
        Err(e) => Err(e.into()),
    }
}

/// DELETE /subjects/{subject}/versions/{version} - Delete a specific version
async fn handle_delete_version(
    State(state): State<AdminApiState>,
    Path((subject, version)): Path<(String, u32)>,
) -> Result<Json<u32>, (StatusCode, Json<SchemaErrorResponse>)> {
    debug!("Schema Registry: Deleting subject '{}' version {}", subject, version);

    match state.schema_registry.delete_version(&subject, version, false).await {
        Ok(v) => {
            info!("Schema Registry: Deleted version {} of subject '{}'", v, subject);
            Ok(Json(v))
        }
        Err(e) => Err(e.into()),
    }
}

/// GET /config - Get global compatibility level
async fn handle_get_global_config(
    State(state): State<AdminApiState>,
) -> Json<CompatibilityConfig> {
    debug!("Schema Registry: Getting global compatibility config");
    let level = state.schema_registry.get_compatibility(None).await;
    Json(CompatibilityConfig {
        compatibility_level: level,
    })
}

/// PUT /config - Set global compatibility level
async fn handle_set_global_config(
    State(state): State<AdminApiState>,
    Json(config): Json<CompatibilityConfig>,
) -> Result<Json<CompatibilityConfig>, (StatusCode, Json<SchemaErrorResponse>)> {
    debug!("Schema Registry: Setting global compatibility to {:?}", config.compatibility_level);

    match state.schema_registry.set_compatibility(None, config.compatibility_level).await {
        Ok(()) => {
            info!("Schema Registry: Set global compatibility to {:?}", config.compatibility_level);
            Ok(Json(config))
        }
        Err(e) => Err(e.into()),
    }
}

/// GET /config/{subject} - Get subject-specific compatibility level
async fn handle_get_subject_config(
    State(state): State<AdminApiState>,
    Path(subject): Path<String>,
) -> Json<CompatibilityConfig> {
    debug!("Schema Registry: Getting compatibility config for subject '{}'", subject);
    let level = state.schema_registry.get_compatibility(Some(&subject)).await;
    Json(CompatibilityConfig {
        compatibility_level: level,
    })
}

/// PUT /config/{subject} - Set subject-specific compatibility level
async fn handle_set_subject_config(
    State(state): State<AdminApiState>,
    Path(subject): Path<String>,
    Json(config): Json<CompatibilityConfig>,
) -> Result<Json<CompatibilityConfig>, (StatusCode, Json<SchemaErrorResponse>)> {
    debug!("Schema Registry: Setting compatibility for subject '{}' to {:?}", subject, config.compatibility_level);

    match state.schema_registry.set_compatibility(Some(&subject), config.compatibility_level).await {
        Ok(()) => {
            info!("Schema Registry: Set compatibility for '{}' to {:?}", subject, config.compatibility_level);
            Ok(Json(config))
        }
        Err(e) => Err(e.into()),
    }
}

/// Authentication middleware for Admin API (X-API-Key header)
async fn auth_middleware(
    State(state): State<AdminApiState>,
    headers: HeaderMap,
    request: Request<Body>,
    next: Next<Body>,
) -> Result<Response, StatusCode> {
    // Skip auth if no API key is configured
    let Some(expected_key) = &state.api_key else {
        return Ok(next.run(request).await);
    };

    // Check X-API-Key header
    let provided_key = headers
        .get("X-API-Key")
        .and_then(|v| v.to_str().ok());

    match provided_key {
        Some(key) if key == expected_key => Ok(next.run(request).await),
        Some(_) => {
            warn!("Admin API: Invalid API key provided");
            Err(StatusCode::UNAUTHORIZED)
        }
        None => {
            warn!("Admin API: No API key provided");
            Err(StatusCode::UNAUTHORIZED)
        }
    }
}

/// HTTP Basic Auth middleware for Schema Registry (Confluent-compatible)
///
/// Validates credentials from `Authorization: Basic <base64>` header.
/// If auth is disabled via config, requests pass through without validation.
async fn schema_registry_auth_middleware(
    State(state): State<AdminApiState>,
    headers: HeaderMap,
    request: Request<Body>,
    next: Next<Body>,
) -> Result<Response, StatusCode> {
    // Skip auth if not required
    if !state.schema_registry.is_auth_required() {
        return Ok(next.run(request).await);
    }

    // Get Authorization header
    let auth_header = headers
        .get("Authorization")
        .and_then(|v| v.to_str().ok());

    let Some(auth_value) = auth_header else {
        warn!("Schema Registry: No Authorization header provided");
        return Err(StatusCode::UNAUTHORIZED);
    };

    // Parse Basic auth: "Basic <base64(username:password)>"
    if !auth_value.starts_with("Basic ") {
        warn!("Schema Registry: Invalid Authorization scheme (expected Basic)");
        return Err(StatusCode::UNAUTHORIZED);
    }

    let encoded = &auth_value[6..]; // Skip "Basic "
    let decoded = match base64_decode(encoded) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(s) => s,
            Err(_) => {
                warn!("Schema Registry: Invalid UTF-8 in credentials");
                return Err(StatusCode::UNAUTHORIZED);
            }
        },
        Err(_) => {
            warn!("Schema Registry: Invalid base64 encoding");
            return Err(StatusCode::UNAUTHORIZED);
        }
    };

    // Split into username:password
    let parts: Vec<&str> = decoded.splitn(2, ':').collect();
    if parts.len() != 2 {
        warn!("Schema Registry: Invalid credentials format (expected username:password)");
        return Err(StatusCode::UNAUTHORIZED);
    }

    let (username, password) = (parts[0], parts[1]);

    // Validate credentials
    if state.schema_registry.validate_credentials(username, password) {
        debug!("Schema Registry: User '{}' authenticated successfully", username);
        Ok(next.run(request).await)
    } else {
        warn!("Schema Registry: Invalid credentials for user '{}'", username);
        Err(StatusCode::UNAUTHORIZED)
    }
}

/// Simple base64 decoder (no external dependency)
fn base64_decode(input: &str) -> Result<Vec<u8>, &'static str> {
    const DECODE_TABLE: [i8; 256] = {
        let mut table = [-1i8; 256];
        let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut i = 0;
        while i < 64 {
            table[alphabet[i] as usize] = i as i8;
            i += 1;
        }
        table
    };

    let input = input.trim_end_matches('=');
    let mut output = Vec::with_capacity(input.len() * 3 / 4);
    let mut buffer = 0u32;
    let mut bits = 0u32;

    for &byte in input.as_bytes() {
        let value = DECODE_TABLE[byte as usize];
        if value < 0 {
            return Err("Invalid base64 character");
        }
        buffer = (buffer << 6) | (value as u32);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }

    Ok(output)
}

/// Create the admin API router
pub fn create_admin_router(state: AdminApiState) -> Router {
    // Admin routes require authentication
    let protected_routes = Router::new()
        .route("/admin/add-node", post(handle_add_node))
        .route("/admin/remove-node", post(handle_node_removal))
        .route("/admin/status", get(handle_status))
        .route("/admin/rebalance", post(handle_rebalance))
        .layer(middleware::from_fn_with_state(state.clone(), auth_middleware));

    // Health check is public
    let public_routes = Router::new()
        .route("/admin/health", get(handle_health));

    // Schema Registry routes (Confluent-compatible)
    // HTTP Basic Auth is optional, enabled via CHRONIK_SCHEMA_REGISTRY_AUTH_ENABLED
    let schema_registry_routes = Router::new()
        // Subject management
        .route("/subjects", get(handle_list_subjects))
        .route("/subjects/:subject/versions", get(handle_list_versions))
        .route("/subjects/:subject/versions", post(handle_register_schema))
        .route("/subjects/:subject/versions/:version", get(handle_get_subject_version))
        .route("/subjects/:subject/versions/:version", delete(handle_delete_version))
        .route("/subjects/:subject", delete(handle_delete_subject))
        // Schema by ID
        .route("/schemas/ids/:id", get(handle_get_schema_by_id))
        // Compatibility configuration
        .route("/config", get(handle_get_global_config))
        .route("/config", put(handle_set_global_config))
        .route("/config/:subject", get(handle_get_subject_config))
        .route("/config/:subject", put(handle_set_subject_config))
        // Apply HTTP Basic Auth middleware (no-op if auth not enabled)
        .layer(middleware::from_fn_with_state(state.clone(), schema_registry_auth_middleware));

    Router::new()
        .merge(protected_routes)
        .merge(public_routes)
        .merge(schema_registry_routes)
        .with_state(state)
}

/// Start the admin API HTTP/HTTPS server
///
/// This spawns a background task that runs the HTTP server on the specified port.
///
/// # Authentication
///
/// If `CHRONIK_ADMIN_API_KEY` is set, the admin API will require authentication via X-API-Key header.
/// If not set, authentication is disabled (INSECURE - for development only).
///
/// # TLS
///
/// TLS is optional but recommended for production. Set environment variables:
/// - `CHRONIK_ADMIN_TLS_CERT` - Path to TLS certificate file (PEM format)
/// - `CHRONIK_ADMIN_TLS_KEY` - Path to TLS private key file (PEM format)
///
/// Both must be set to enable TLS. If only one is set, TLS is disabled with a warning.
///
/// # ISR Tracker (v2.2.22)
///
/// Optional ISR tracker for accurate in-sync replica queries. If provided,
/// the cluster status endpoint will show actual ISR based on follower acknowledgements.
///
/// # Schema Registry (Phase 5)
///
/// The Schema Registry is always enabled and provides Confluent-compatible REST API
/// for schema management. Schema Registry routes do NOT require authentication.
pub async fn start_admin_api(
    raft_cluster: Arc<RaftCluster>,
    metadata_store: Arc<dyn MetadataStore>,
    port: u16,
    api_key: Option<String>,
    isr_tracker: Option<Arc<IsrTracker>>,
    schema_registry: Arc<SchemaRegistry>,
) -> Result<()> {
    let effective_key = match api_key.or_else(|| std::env::var("CHRONIK_ADMIN_API_KEY").ok()) {
        Some(key) => {
            info!("✓ Admin API authentication enabled");
            Some(key)
        }
        None => {
            warn!("⚠ CHRONIK_ADMIN_API_KEY not set - Admin API will run WITHOUT authentication!");
            warn!("⚠ This is INSECURE for production. Set CHRONIK_ADMIN_API_KEY to enable auth.");
            None
        }
    };

    let state = AdminApiState {
        raft_cluster,
        metadata_store,
        api_key: effective_key,
        isr_tracker,
        schema_registry,
    };

    let app = create_admin_router(state);

    let addr: std::net::SocketAddr = format!("0.0.0.0:{}", port).parse()?;

    // For now, just use HTTP (TLS would require axum-server crate which we don't have)
    // Log a warning if user tries to enable TLS
    if std::env::var("CHRONIK_ADMIN_TLS_CERT").is_ok() || std::env::var("CHRONIK_ADMIN_TLS_KEY").is_ok() {
        warn!("⚠ TLS configuration detected but axum-server crate not available");
        warn!("⚠ Admin API will run over HTTP. To enable TLS, add axum-server dependency.");
    }

    info!("Starting Admin API + Schema Registry HTTP server on {}", addr);
    info!("Schema Registry endpoints available at /subjects, /schemas/ids, /config");

    tokio::spawn(async move {
        if let Err(e) = Server::bind(&addr)
            .serve(app.into_make_service())
            .await
        {
            error!("Admin API server error: {}", e);
        }
    });

    Ok(())
}
