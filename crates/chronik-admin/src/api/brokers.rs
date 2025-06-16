//! Broker management endpoints.

use crate::{error::AdminResult, state::AppState};
use axum::{
    extract::{Path, State},
    Json,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Broker information
#[derive(Debug, Serialize, ToSchema)]
pub struct Broker {
    /// Broker ID
    pub id: i32,
    
    /// Hostname
    pub host: String,
    
    /// Port
    pub port: i32,
    
    /// Rack ID
    pub rack: Option<String>,
    
    /// Status
    pub status: String,
    
    /// Version
    pub version: String,
}

/// Broker configuration
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct BrokerConfig {
    /// Number of network threads
    pub num_network_threads: i32,
    
    /// Number of IO threads
    pub num_io_threads: i32,
    
    /// Socket send buffer size
    pub socket_send_buffer_bytes: i32,
    
    /// Socket receive buffer size
    pub socket_receive_buffer_bytes: i32,
    
    /// Socket request max bytes
    pub socket_request_max_bytes: i32,
}

/// List brokers
#[utoipa::path(
    get,
    path = "/api/v1/brokers",
    responses(
        (status = 200, description = "List of brokers", body = Vec<Broker>),
    ),
    tag = "brokers"
)]
pub async fn list_brokers(
    State(state): State<AppState>,
) -> AdminResult<Json<Vec<Broker>>> {
    // TODO: Get from metastore
    let brokers = vec![
        Broker {
            id: 1,
            host: "broker-1.chronik.local".to_string(),
            port: 9092,
            rack: Some("rack-1".to_string()),
            status: "online".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        },
        Broker {
            id: 2,
            host: "broker-2.chronik.local".to_string(),
            port: 9092,
            rack: Some("rack-2".to_string()),
            status: "online".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        },
        Broker {
            id: 3,
            host: "broker-3.chronik.local".to_string(),
            port: 9092,
            rack: Some("rack-3".to_string()),
            status: "online".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        },
    ];
    
    Ok(Json(brokers))
}

/// Get broker
#[utoipa::path(
    get,
    path = "/api/v1/brokers/{id}",
    params(
        ("id" = i32, Path, description = "Broker ID")
    ),
    responses(
        (status = 200, description = "Broker information", body = Broker),
        (status = 404, description = "Broker not found"),
    ),
    tag = "brokers"
)]
pub async fn get_broker(
    State(state): State<AppState>,
    Path(id): Path<i32>,
) -> AdminResult<Json<Broker>> {
    // TODO: Get from metastore
    if id >= 1 && id <= 3 {
        Ok(Json(Broker {
            id,
            host: format!("broker-{}.chronik.local", id),
            port: 9092,
            rack: Some(format!("rack-{}", id)),
            status: "online".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        }))
    } else {
        Err(crate::error::AdminError::NotFound(format!("Broker {} not found", id)))
    }
}

/// Get broker configuration
pub async fn get_broker_config(
    State(state): State<AppState>,
    Path(id): Path<i32>,
) -> AdminResult<Json<BrokerConfig>> {
    // TODO: Get from controller
    Ok(Json(BrokerConfig {
        num_network_threads: 8,
        num_io_threads: 8,
        socket_send_buffer_bytes: 102400,
        socket_receive_buffer_bytes: 102400,
        socket_request_max_bytes: 104857600,
    }))
}

/// Update broker configuration
pub async fn update_broker_config(
    State(state): State<AppState>,
    Path(id): Path<i32>,
    Json(config): Json<BrokerConfig>,
) -> AdminResult<()> {
    // TODO: Update via controller
    Ok(())
}