//! Metrics HTTP server.

use crate::MetricsRegistry;
use anyhow::Result;
use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::get,
    Router,
    Server,
};
use std::net::SocketAddr;
use tower_http::trace::TraceLayer;

/// Server metrics placeholder - actual metrics are in UnifiedMetrics
///
/// These structs are kept for backward compatibility but actual metric
/// recording now happens through the unified atomic metrics system.
pub struct ServerMetrics {}

impl ServerMetrics {
    pub fn new() -> Self {
        Self {}
    }
}

impl Default for ServerMetrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Connection metrics placeholder - actual metrics are in UnifiedMetrics
pub struct ConnectionMetrics {}

impl ConnectionMetrics {
    pub fn new() -> Self {
        Self {}
    }
}

impl Default for ConnectionMetrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Metrics server
pub struct MetricsServer {
    registry: MetricsRegistry,
    port: u16,
}

impl MetricsServer {
    /// Create new metrics server
    pub fn new(registry: MetricsRegistry, port: u16) -> Self {
        Self { registry, port }
    }
    
    /// Run the metrics server
    pub async fn run(self) -> Result<()> {
        let app = Router::new()
            .route("/metrics", get(metrics_handler))
            .route("/health", get(health_handler))
            .route("/ready", get(ready_handler))
            .layer(TraceLayer::new_for_http())
            .with_state(self.registry);
        
        let addr = SocketAddr::from(([0, 0, 0, 0], self.port));
        tracing::info!("Metrics server listening on {}", addr);

        // In axum 0.6, Server is re-exported from axum
        Server::bind(&addr)
            .serve(app.into_make_service())
            .await?;

        Ok(())
    }
}

/// Metrics endpoint handler
///
/// Uses lock-free atomic counters from UnifiedMetrics to avoid deadlocks.
/// This is fast, reliable, and deadlock-proof.
async fn metrics_handler(
    State(_registry): State<MetricsRegistry>,
) -> impl IntoResponse {
    use crate::unified_metrics::global_metrics;

    tracing::debug!("Metrics endpoint called");

    // Get Prometheus-formatted metrics from atomic counters (lock-free, instant)
    let prometheus_output = global_metrics().format_prometheus();

    tracing::debug!("Successfully generated {} bytes of metrics", prometheus_output.len());

    // Return as Prometheus text format
    (
        StatusCode::OK,
        [("content-type", "text/plain; version=0.0.4")],
        prometheus_output
    )
}

/// Health check handler
async fn health_handler() -> impl IntoResponse {
    (StatusCode::OK, "OK")
}

/// Readiness check handler
async fn ready_handler() -> impl IntoResponse {
    // TODO: Add actual readiness checks
    (StatusCode::OK, "Ready")
}