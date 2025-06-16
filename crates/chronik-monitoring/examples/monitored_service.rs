//! Example of integrating monitoring into a service.

use anyhow::Result;
use chronik_monitoring::{init_monitoring, TracingConfig};
use tracing::{info, instrument};

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize monitoring
    let tracing_config = TracingConfig {
        otlp_endpoint: "http://localhost:4317".to_string(),
        service_version: "0.1.0".to_string(),
        sampling_ratio: 1.0,
        log_level: "info".to_string(),
    };
    
    let metrics = init_monitoring(
        "example-service",
        8080,
        Some(tracing_config)
    ).await?;
    
    // Example: Record some metrics
    let ingest_metrics = metrics.ingest();
    
    // Simulate message processing
    for i in 0..100 {
        process_message(&ingest_metrics, i).await;
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }
    
    // Keep running
    tokio::signal::ctrl_c().await?;
    
    Ok(())
}

#[instrument]
async fn process_message(metrics: &chronik_monitoring::IngestMetrics, msg_id: i32) {
    info!("Processing message {}", msg_id);
    
    // Record metrics
    metrics.record_message_received("test-topic", 0);
    
    // Simulate processing time
    let start = std::time::Instant::now();
    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    
    metrics.record_message_stored("test-topic", 0);
    metrics.record_produce_latency("test-topic", start.elapsed().as_secs_f64());
    
    info!("Message {} processed", msg_id);
}