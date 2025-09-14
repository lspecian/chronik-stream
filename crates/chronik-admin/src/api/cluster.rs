//! Cluster management endpoints.

use crate::{error::AdminResult, state::AppState};
use axum::{
    extract::State,
    Json,
};
use serde::Serialize;
use utoipa::ToSchema;

/// Cluster information
#[derive(Debug, Serialize, ToSchema)]
pub struct ClusterInfo {
    /// Cluster ID
    pub cluster_id: String,
    
    /// Cluster name
    pub name: String,
    
    /// Version
    pub version: String,
    
    /// Number of brokers
    pub broker_count: u32,
    
    /// Number of topics
    pub topic_count: u32,
    
    /// Number of partitions
    pub partition_count: u32,
    
    /// Controller leader
    pub controller_leader: String,
}

/// Health status
#[derive(Debug, Serialize, ToSchema)]
pub struct HealthStatus {
    /// Overall status
    pub status: String,
    
    /// Component health
    pub components: Vec<ComponentHealth>,
}

/// Component health
#[derive(Debug, Serialize, ToSchema)]
pub struct ComponentHealth {
    /// Component name
    pub name: String,
    
    /// Health status
    pub status: String,
    
    /// Optional message
    pub message: Option<String>,
}

/// Cluster metrics
#[derive(Debug, Serialize, ToSchema)]
pub struct ClusterMetrics {
    /// Messages per second
    pub messages_per_sec: f64,
    
    /// Bytes in per second
    pub bytes_in_per_sec: f64,
    
    /// Bytes out per second
    pub bytes_out_per_sec: f64,
    
    /// Total messages
    pub total_messages: u64,
    
    /// Total bytes
    pub total_bytes: u64,
}

/// Get cluster information
#[utoipa::path(
    get,
    path = "/api/v1/cluster/info",
    responses(
        (status = 200, description = "Cluster information", body = ClusterInfo),
        (status = 503, description = "Controller unavailable"),
    ),
    tag = "cluster"
)]
pub async fn get_info(
    State(state): State<AppState>,
) -> AdminResult<Json<ClusterInfo>> {
    // TODO: Get from controller
    Ok(Json(ClusterInfo {
        cluster_id: "chronik-cluster-1".to_string(),
        name: "Chronik Stream Cluster".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        broker_count: 3,
        topic_count: 10,
        partition_count: 30,
        controller_leader: "controller-1".to_string(),
    }))
}

/// Health check endpoint
#[utoipa::path(
    get,
    path = "/api/v1/cluster/health",
    responses(
        (status = 200, description = "Health status", body = HealthStatus),
    ),
    tag = "cluster"
)]
pub async fn health_check(
    State(_state): State<AppState>,
) -> AdminResult<Json<HealthStatus>> {
    let mut components = vec![];
    
    // Check metadata store - 
    components.push(ComponentHealth {
        name: "metadata".to_string(),
        status: "healthy".to_string(),
        message: None,
    });
    
    // Check controller
    // TODO: Implement controller health check
    components.push(ComponentHealth {
        name: "controller".to_string(),
        status: "healthy".to_string(),
        message: None,
    });
    
    // Overall status
    let overall_status = if components.iter().all(|c| c.status == "healthy") {
        "healthy"
    } else {
        "degraded"
    };
    
    Ok(Json(HealthStatus {
        status: overall_status.to_string(),
        components,
    }))
}

/// Get cluster metrics
#[utoipa::path(
    get,
    path = "/api/v1/cluster/metrics",
    responses(
        (status = 200, description = "Cluster metrics", body = ClusterMetrics),
        (status = 503, description = "Metrics unavailable"),
    ),
    tag = "cluster"
)]
pub async fn get_metrics(
    State(state): State<AppState>,
) -> AdminResult<Json<ClusterMetrics>> {
    // TODO: Get from metrics system
    Ok(Json(ClusterMetrics {
        messages_per_sec: 1000.0,
        bytes_in_per_sec: 1024.0 * 1024.0,
        bytes_out_per_sec: 2048.0 * 1024.0,
        total_messages: 1_000_000,
        total_bytes: 1024 * 1024 * 1024,
    }))
}