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
use prometheus::{Encoder, TextEncoder, Counter, Gauge, Histogram, HistogramOpts};
use std::net::SocketAddr;
use tower_http::trace::TraceLayer;

/// Server metrics for monitoring TCP server performance
pub struct ServerMetrics {
    // Connection metrics
    pub connections_total: Counter,
    pub connections_active: Gauge,
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
    pub pending_requests: Gauge,
    pub backpressure_events: Counter,
    
    // Protocol metrics
    pub frame_errors: Counter,
    pub tls_handshake_failures: Counter,
}

impl ServerMetrics {
    /// Create new server metrics
    pub fn new() -> Self {
        let histogram_opts = HistogramOpts::new("request_duration", "Request duration in seconds")
            .buckets(vec![0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0]);
        
        Self {
            connections_total: Counter::new("connections_total", "Total connections").unwrap(),
            connections_active: Gauge::new("connections_active", "Active connections").unwrap(),
            connections_rejected: Counter::new("connections_rejected", "Rejected connections").unwrap(),
            requests_total: Counter::new("requests_total", "Total requests").unwrap(),
            request_errors: Counter::new("request_errors", "Request errors").unwrap(),
            request_timeouts: Counter::new("request_timeouts", "Request timeouts").unwrap(),
            request_duration: Histogram::with_opts(histogram_opts).unwrap(),
            bytes_received: Counter::new("bytes_received", "Bytes received").unwrap(),
            bytes_sent: Counter::new("bytes_sent", "Bytes sent").unwrap(),
            pending_requests: Gauge::new("pending_requests", "Pending requests").unwrap(),
            backpressure_events: Counter::new("backpressure_events", "Backpressure events").unwrap(),
            frame_errors: Counter::new("frame_errors", "Frame errors").unwrap(),
            tls_handshake_failures: Counter::new("tls_handshake_failures", "TLS handshake failures").unwrap(),
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
            requests: Counter::new("connection_requests", "Requests per connection").unwrap(),
            errors: Counter::new("connection_errors", "Errors per connection").unwrap(),
            bytes_sent: Counter::new("connection_bytes_sent", "Bytes sent per connection").unwrap(),
            bytes_received: Counter::new("connection_bytes_received", "Bytes received per connection").unwrap(),
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