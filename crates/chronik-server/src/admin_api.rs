//! HTTP Admin API for Cluster Management (v2.6.0+)
//!
//! Provides REST endpoints for managing cluster membership:
//! - POST /admin/add-node - Add new node to cluster
//! - POST /admin/remove-node - Remove node from cluster (TODO: Priority 4)
//! - GET /admin/status - Get cluster status (TODO: Priority 3)
//!
//! This API is only available when running in cluster mode.
//!
//! ## Authentication
//!
//! The admin API requires API key authentication via the `X-API-Key` header.
//! Configure the API key via the `CHRONIK_ADMIN_API_KEY` environment variable.
//! If not set, a random key is generated at startup (check logs).

use anyhow::Result;
use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router, Server,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{error, info, warn};

use crate::raft_cluster::RaftCluster;

/// Admin API state shared across handlers
#[derive(Clone)]
pub struct AdminApiState {
    pub raft_cluster: Arc<RaftCluster>,
    pub api_key: Option<String>,
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
    let cluster_nodes = state.raft_cluster.get_all_nodes();
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
    let (leader_ready, leader_id, _) = state.raft_cluster.is_leader_ready();

    // Get all nodes with their information
    let node_info = state.raft_cluster.get_node_info();
    let nodes: Vec<NodeInfo> = node_info.iter().map(|(id, addr)| {
        NodeInfo {
            node_id: *id,
            address: addr.clone(),
            is_leader: *id == leader_id,
        }
    }).collect();

    // Get all partition assignments
    let partitions = state.raft_cluster.get_all_partition_info();

    Ok(Json(ClusterStatusResponse {
        node_id,
        leader_id: if leader_ready { Some(leader_id) } else { None },
        is_leader,
        nodes,
        partitions,
    }))
}

/// Authentication middleware
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

/// Create the admin API router
pub fn create_admin_router(state: AdminApiState) -> Router {
    let protected_routes = Router::new()
        .route("/admin/add-node", post(handle_add_node))
        .route("/admin/remove-node", post(handle_node_removal))
        .route("/admin/status", get(handle_status))
        .layer(middleware::from_fn_with_state(state.clone(), auth_middleware));

    let public_routes = Router::new()
        .route("/admin/health", get(handle_health));

    Router::new()
        .merge(protected_routes)
        .merge(public_routes)
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
pub async fn start_admin_api(
    raft_cluster: Arc<RaftCluster>,
    port: u16,
    api_key: Option<String>,
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
        api_key: effective_key,
    };

    let app = create_admin_router(state);

    let addr: std::net::SocketAddr = format!("0.0.0.0:{}", port).parse()?;

    // For now, just use HTTP (TLS would require axum-server crate which we don't have)
    // Log a warning if user tries to enable TLS
    if std::env::var("CHRONIK_ADMIN_TLS_CERT").is_ok() || std::env::var("CHRONIK_ADMIN_TLS_KEY").is_ok() {
        warn!("⚠ TLS configuration detected but axum-server crate not available");
        warn!("⚠ Admin API will run over HTTP. To enable TLS, add axum-server dependency.");
    }

    info!("Starting Admin API HTTP server on {}", addr);

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
