//! Example of integrating monitoring into a service.
//!
//! Serves Prometheus metrics on :8080/metrics while recording simulated
//! produce requests through [`MetricsRecorder`], the same entry point the
//! broker uses.

use anyhow::Result;
use chronik_monitoring::{init_monitoring, MetricsRecorder, TracingConfig};
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

    let _metrics = init_monitoring("example-service", 8080, Some(tracing_config)).await?;

    // Simulate message processing
    for i in 0..100 {
        process_message(i).await;
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }

    // Keep running so the metrics endpoint can be scraped
    tokio::signal::ctrl_c().await?;

    Ok(())
}

#[instrument]
async fn process_message(msg_id: i32) {
    info!("Processing message {}", msg_id);

    // Simulate processing time
    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

    // One successful produce request carrying one 128-byte message
    MetricsRecorder::record_produce(true, 128, 1);

    info!("Message {} processed", msg_id);
}
