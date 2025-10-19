//! Monitoring and observability for Chronik Stream.

pub mod metrics;
pub mod tracing;
pub mod server;
pub mod raft_metrics;

pub use metrics::{
    MetricsRegistry,
    ControllerMetrics,
    IngestMetrics,
    QueryMetrics,
    JanitorMetrics,
};
pub use tracing::{init_tracing, TracingConfig};
pub use server::{MetricsServer, ServerMetrics, ConnectionMetrics};
pub use raft_metrics::RaftMetrics;

use anyhow::Result;

/// Initialize monitoring (metrics + tracing)
pub async fn init_monitoring(
    service_name: &str,
    metrics_port: u16,
    tracing_config: Option<TracingConfig>,
) -> Result<MetricsRegistry> {
    // Initialize tracing if config provided
    if let Some(config) = tracing_config {
        init_tracing(service_name, config)?;
    }
    
    // Create metrics registry
    let registry = MetricsRegistry::new();
    
    // Start metrics server
    let server = MetricsServer::new(registry.clone(), metrics_port);
    tokio::spawn(async move {
        if let Err(e) = server.run().await {
            ::tracing::error!("Metrics server error: {}", e);
        }
    });
    
    Ok(registry)
}