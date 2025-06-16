//! Chronik Stream Admin API server.

use anyhow::Result;
use axum::middleware;
use chronik_admin::{api, config::AdminConfig, state::AppState};
use chronik_monitoring::{init_monitoring, TracingConfig};
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize monitoring
    let tracing_config = TracingConfig {
        otlp_endpoint: std::env::var("OTLP_ENDPOINT")
            .unwrap_or_else(|_| "http://localhost:4317".to_string()),
        ..Default::default()
    };
    
    init_monitoring(
        "chronik-admin",
        8081, // Metrics port
        Some(tracing_config),
    ).await?;
    
    info!("Starting Chronik Stream Admin API");
    
    // Load configuration
    let config = load_config()?;
    
    // Create application state
    let state = AppState::new(config.clone()).await?;
    
    // Create router
    let app = api::create_router(state.clone())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            chronik_admin::handlers::auth_middleware,
        ));
    
    // Start server
    let addr = SocketAddr::from(([0, 0, 0, 0], config.server.port));
    let listener = TcpListener::bind(addr).await?;
    info!("Admin API listening on {}", addr);
    
    axum::serve(listener, app).await?;
    
    Ok(())
}

/// Load configuration
fn load_config() -> Result<AdminConfig> {
    let mut config = AdminConfig::default();
    
    // Override with environment variables
    if let Ok(database_url) = std::env::var("DATABASE_URL") {
        config.database.url = database_url;
    }
    
    if let Ok(controller_endpoints) = std::env::var("CONTROLLER_ENDPOINTS") {
        config.controller.endpoints = controller_endpoints
            .split(',')
            .map(|s| s.trim().to_string())
            .collect();
    }
    
    Ok(config)
}