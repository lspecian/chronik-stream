//! Metrics HTTP server.

use crate::MetricsRegistry;
use anyhow::Result;
use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::get,
    Router,
};
use prometheus::{Encoder, TextEncoder};
use prometheus_client::metrics::{counter::Counter, gauge::Gauge, histogram::Histogram};
use std::net::SocketAddr;
use std::sync::atomic::AtomicI64;
use tower_http::trace::TraceLayer;

/// Server metrics for monitoring TCP server performance
pub struct ServerMetrics {
    // Connection metrics
    pub connections_total: Counter,
    pub connections_active: Gauge<i64, AtomicI64>,
    pub connections_rejected: Counter,
    
    // Request metrics
    pub requests_total: Counter,
    pub request_errors: Counter,
    pub request_timeouts: Counter,
    pub request_duration: Histogram,
    
    // Data transfer metrics
    pub bytes_received: Counter,
    pub bytes_sent: Counter,
    
    // Backpressure metrics
    pub pending_requests: Gauge<i64, AtomicI64>,
    pub backpressure_events: Counter,
    
    // Protocol metrics
    pub frame_errors: Counter,
    pub tls_handshake_failures: Counter,
}

impl ServerMetrics {
    /// Create new server metrics
    pub fn new() -> Self {
        Self {
            connections_total: Counter::default(),
            connections_active: Gauge::default(),
            connections_rejected: Counter::default(),
            requests_total: Counter::default(),
            request_errors: Counter::default(),
            request_timeouts: Counter::default(),
            request_duration: Histogram::new(vec![
                0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0
            ].into_iter()),
            bytes_received: Counter::default(),
            bytes_sent: Counter::default(),
            pending_requests: Gauge::default(),
            backpressure_events: Counter::default(),
            frame_errors: Counter::default(),
            tls_handshake_failures: Counter::default(),
        }
    }
}

impl Default for ServerMetrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Connection-specific metrics
pub struct ConnectionMetrics {
    pub requests: Counter,
    pub errors: Counter,
    pub bytes_sent: Counter,
    pub bytes_received: Counter,
}

impl ConnectionMetrics {
    /// Create new connection metrics
    pub fn new() -> Self {
        Self {
            requests: Counter::default(),
            errors: Counter::default(),
            bytes_sent: Counter::default(),
            bytes_received: Counter::default(),
        }
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
        
        axum::Server::bind(&addr)
            .serve(app.into_make_service())
            .await?;
        
        Ok(())
    }
}

/// Metrics endpoint handler
async fn metrics_handler(
    State(registry): State<MetricsRegistry>,
) -> impl IntoResponse {
    let encoder = TextEncoder::new();
    let metric_families = registry.registry().gather();
    
    let mut buffer = Vec::new();
    match encoder.encode(&metric_families, &mut buffer) {
        Ok(_) => (StatusCode::OK, buffer),
        Err(e) => {
            tracing::error!("Failed to encode metrics: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Vec::new())
        }
    }
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