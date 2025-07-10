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
    // Get topics from metadata store
    let metadata_topics = state.metadata_store.list_topics()
        .await
        .map_err(|e| crate::error::AdminError::Internal(e.to_string()))?;
    
    let topics: Vec<Topic> = metadata_topics.into_iter().map(|mt| {
        Topic {
            name: mt.name,
            partitions: mt.config.partition_count as i32,
            replication_factor: mt.config.replication_factor as i32,
            config: TopicConfig {
                retention_ms: mt.config.retention_ms.unwrap_or(7 * 24 * 60 * 60 * 1000),
                segment_bytes: mt.config.segment_bytes,
                min_insync_replicas: mt.config.replication_factor as i32,
                compression_type: "snappy".to_string(),
            },
        }
    }).collect();
    
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
    use chronik_common::metadata::traits::TopicConfig as MetadataTopicConfig;
    
    // Check if topic already exists
    if let Ok(Some(_)) = state.metadata_store.get_topic(&req.name).await {
        return Err(crate::error::AdminError::Conflict(
            format!("Topic {} already exists", req.name)
        ));
    }
    
    // Create topic configuration
    let config = if let Some(cfg) = req.config {
        MetadataTopicConfig {
            partition_count: req.partitions as u32,
            replication_factor: req.replication_factor as u32,
            retention_ms: Some(cfg.retention_ms),
            segment_bytes: cfg.segment_bytes,
            config: std::collections::HashMap::new(),
        }
    } else {
        MetadataTopicConfig {
            partition_count: req.partitions as u32,
            replication_factor: req.replication_factor as u32,
            ..Default::default()
        }
    };
    
    // Create topic in metadata store
    state.metadata_store.create_topic(&req.name, config)
        .await
        .map_err(|e| crate::error::AdminError::Internal(e.to_string()))?;
    
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
    // Get from metadata store
    let topic_metadata = state.metadata_store.get_topic(&name)
        .await
        .map_err(|e| crate::error::AdminError::Internal(e.to_string()))?;
    
    match topic_metadata {
        Some(mt) => {
            Ok(Json(Topic {
                name: mt.name,
                partitions: mt.config.partition_count as i32,
                replication_factor: mt.config.replication_factor as i32,
                config: TopicConfig {
                    retention_ms: mt.config.retention_ms.unwrap_or(7 * 24 * 60 * 60 * 1000),
                    segment_bytes: mt.config.segment_bytes,
                    min_insync_replicas: mt.config.replication_factor as i32,
                    compression_type: "snappy".to_string(),
                },
            }))
        },
        None => Err(crate::error::AdminError::NotFound(format!("Topic {} not found", name)))
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