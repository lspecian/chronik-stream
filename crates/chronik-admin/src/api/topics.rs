//! Topic management endpoints.

use crate::{error::AdminResult, state::AppState};
use axum::{
    extract::{Path, State},
    Json,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Topic information
#[derive(Debug, Serialize, ToSchema)]
pub struct Topic {
    /// Topic name
    pub name: String,
    
    /// Number of partitions
    pub partitions: i32,
    
    /// Replication factor
    pub replication_factor: i32,
    
    /// Configuration
    pub config: TopicConfig,
}

/// Topic configuration
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct TopicConfig {
    /// Retention time in milliseconds
    pub retention_ms: i64,
    
    /// Segment size in bytes
    pub segment_bytes: i64,
    
    /// Minimum in-sync replicas
    pub min_insync_replicas: i32,
    
    /// Compression type
    pub compression_type: String,
}

/// Create topic request
#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateTopicRequest {
    /// Topic name
    pub name: String,
    
    /// Number of partitions
    pub partitions: i32,
    
    /// Replication factor
    pub replication_factor: i32,
    
    /// Optional configuration
    pub config: Option<TopicConfig>,
}

/// Partition information
#[derive(Debug, Serialize, ToSchema)]
pub struct Partition {
    /// Partition ID
    pub id: i32,
    
    /// Leader broker ID
    pub leader: i32,
    
    /// Replica broker IDs
    pub replicas: Vec<i32>,
    
    /// In-sync replica broker IDs
    pub isr: Vec<i32>,
}

/// List topics
#[utoipa::path(
    get,
    path = "/api/v1/topics",
    responses(
        (status = 200, description = "List of topics", body = Vec<Topic>),
    ),
    tag = "topics"
)]
pub async fn list_topics(
    State(state): State<AppState>,
) -> AdminResult<Json<Vec<Topic>>> {
    // TODO: Get from metastore
    let topics = vec![
        Topic {
            name: "events".to_string(),
            partitions: 3,
            replication_factor: 2,
            config: TopicConfig {
                retention_ms: 7 * 24 * 60 * 60 * 1000, // 7 days
                segment_bytes: 1024 * 1024 * 1024, // 1GB
                min_insync_replicas: 2,
                compression_type: "snappy".to_string(),
            },
        },
    ];
    
    Ok(Json(topics))
}

/// Create topic
#[utoipa::path(
    post,
    path = "/api/v1/topics",
    request_body = CreateTopicRequest,
    responses(
        (status = 201, description = "Topic created"),
        (status = 409, description = "Topic already exists"),
    ),
    tag = "topics"
)]
pub async fn create_topic(
    State(state): State<AppState>,
    Json(req): Json<CreateTopicRequest>,
) -> AdminResult<()> {
    // TODO: Create via controller
    Ok(())
}

/// Get topic
#[utoipa::path(
    get,
    path = "/api/v1/topics/{name}",
    params(
        ("name" = String, Path, description = "Topic name")
    ),
    responses(
        (status = 200, description = "Topic information", body = Topic),
        (status = 404, description = "Topic not found"),
    ),
    tag = "topics"
)]
pub async fn get_topic(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> AdminResult<Json<Topic>> {
    // TODO: Get from metastore
    if name == "events" {
        Ok(Json(Topic {
            name: name.clone(),
            partitions: 3,
            replication_factor: 2,
            config: TopicConfig {
                retention_ms: 7 * 24 * 60 * 60 * 1000,
                segment_bytes: 1024 * 1024 * 1024,
                min_insync_replicas: 2,
                compression_type: "snappy".to_string(),
            },
        }))
    } else {
        Err(crate::error::AdminError::NotFound(format!("Topic {} not found", name)))
    }
}

/// Delete topic
#[utoipa::path(
    delete,
    path = "/api/v1/topics/{name}",
    params(
        ("name" = String, Path, description = "Topic name")
    ),
    responses(
        (status = 204, description = "Topic deleted"),
        (status = 404, description = "Topic not found"),
    ),
    tag = "topics"
)]
pub async fn delete_topic(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> AdminResult<()> {
    // TODO: Delete via controller
    Ok(())
}

/// Get topic configuration
pub async fn get_topic_config(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> AdminResult<Json<TopicConfig>> {
    // TODO: Get from metastore
    Ok(Json(TopicConfig {
        retention_ms: 7 * 24 * 60 * 60 * 1000,
        segment_bytes: 1024 * 1024 * 1024,
        min_insync_replicas: 2,
        compression_type: "snappy".to_string(),
    }))
}

/// Update topic configuration
pub async fn update_topic_config(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(config): Json<TopicConfig>,
) -> AdminResult<()> {
    // TODO: Update via controller
    Ok(())
}

/// List partitions
pub async fn list_partitions(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> AdminResult<Json<Vec<Partition>>> {
    // TODO: Get from metastore
    let partitions = vec![
        Partition {
            id: 0,
            leader: 1,
            replicas: vec![1, 2],
            isr: vec![1, 2],
        },
        Partition {
            id: 1,
            leader: 2,
            replicas: vec![2, 3],
            isr: vec![2, 3],
        },
        Partition {
            id: 2,
            leader: 3,
            replicas: vec![3, 1],
            isr: vec![3, 1],
        },
    ];
    
    Ok(Json(partitions))
}

/// Update partitions
pub async fn update_partitions(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(partitions): Json<i32>,
) -> AdminResult<()> {
    // TODO: Update via controller
    Ok(())
}