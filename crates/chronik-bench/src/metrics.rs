/// Prometheus metrics exporter (optional feature)

#[cfg(feature = "prometheus")]
use anyhow::Result;
#[cfg(feature = "prometheus")]
use prometheus::{Encoder, Gauge, IntCounter, Registry, TextEncoder};
#[cfg(feature = "prometheus")]
use std::sync::Arc;
#[cfg(feature = "prometheus")]
use tracing::info;
#[cfg(feature = "prometheus")]
use warp::Filter;

#[cfg(feature = "prometheus")]
lazy_static::lazy_static! {
    static ref REGISTRY: Registry = Registry::new();

    // Counters
    static ref MESSAGES_SENT: IntCounter =
        IntCounter::new("chronik_bench_messages_sent_total", "Total messages sent")
            .expect("Failed to create messages_sent counter");

    static ref MESSAGES_FAILED: IntCounter =
        IntCounter::new("chronik_bench_messages_failed_total", "Total messages failed")
            .expect("Failed to create messages_failed counter");

    static ref BYTES_SENT: IntCounter =
        IntCounter::new("chronik_bench_bytes_sent_total", "Total bytes sent")
            .expect("Failed to create bytes_sent counter");

    // Gauges for latency percentiles
    static ref LATENCY_P50: Gauge =
        Gauge::new("chronik_bench_latency_p50_microseconds", "p50 latency in microseconds")
            .expect("Failed to create latency_p50 gauge");

    static ref LATENCY_P90: Gauge =
        Gauge::new("chronik_bench_latency_p90_microseconds", "p90 latency in microseconds")
            .expect("Failed to create latency_p90 gauge");

    static ref LATENCY_P99: Gauge =
        Gauge::new("chronik_bench_latency_p99_microseconds", "p99 latency in microseconds")
            .expect("Failed to create latency_p99 gauge");

    static ref LATENCY_P999: Gauge =
        Gauge::new("chronik_bench_latency_p999_microseconds", "p99.9 latency in microseconds")
            .expect("Failed to create latency_p999 gauge");

    static ref THROUGHPUT_MSG_PER_SEC: Gauge =
        Gauge::new("chronik_bench_throughput_msg_per_sec", "Throughput in messages per second")
            .expect("Failed to create throughput gauge");

    static ref THROUGHPUT_MB_PER_SEC: Gauge =
        Gauge::new("chronik_bench_throughput_mb_per_sec", "Throughput in MB per second")
            .expect("Failed to create throughput_mb gauge");
}

#[cfg(feature = "prometheus")]
pub async fn start_prometheus_exporter(port: u16) -> Result<tokio::task::JoinHandle<()>> {
    // Register metrics
    REGISTRY.register(Box::new(MESSAGES_SENT.clone()))?;
    REGISTRY.register(Box::new(MESSAGES_FAILED.clone()))?;
    REGISTRY.register(Box::new(BYTES_SENT.clone()))?;
    REGISTRY.register(Box::new(LATENCY_P50.clone()))?;
    REGISTRY.register(Box::new(LATENCY_P90.clone()))?;
    REGISTRY.register(Box::new(LATENCY_P99.clone()))?;
    REGISTRY.register(Box::new(LATENCY_P999.clone()))?;
    REGISTRY.register(Box::new(THROUGHPUT_MSG_PER_SEC.clone()))?;
    REGISTRY.register(Box::new(THROUGHPUT_MB_PER_SEC.clone()))?;

    info!("Starting Prometheus metrics exporter on port {}", port);

    let registry = Arc::new(REGISTRY.clone());

    let metrics_route = warp::path!("metrics").map(move || {
        let encoder = TextEncoder::new();
        let metric_families = registry.gather();
        let mut buffer = Vec::new();
        encoder.encode(&metric_families, &mut buffer).unwrap();
        String::from_utf8(buffer).unwrap()
    });

    let handle = tokio::spawn(async move {
        warp::serve(metrics_route)
            .run(([0, 0, 0, 0], port))
            .await;
    });

    Ok(handle)
}

#[cfg(feature = "prometheus")]
pub fn record_message_sent(bytes: u64) {
    MESSAGES_SENT.inc();
    BYTES_SENT.inc_by(bytes);
}

#[cfg(feature = "prometheus")]
pub fn record_message_failed() {
    MESSAGES_FAILED.inc();
}

#[cfg(feature = "prometheus")]
pub fn update_latency_stats(p50: u64, p90: u64, p99: u64, p999: u64) {
    LATENCY_P50.set(p50 as f64);
    LATENCY_P90.set(p90 as f64);
    LATENCY_P99.set(p99 as f64);
    LATENCY_P999.set(p999 as f64);
}

#[cfg(feature = "prometheus")]
pub fn update_throughput_stats(msg_per_sec: f64, mb_per_sec: f64) {
    THROUGHPUT_MSG_PER_SEC.set(msg_per_sec);
    THROUGHPUT_MB_PER_SEC.set(mb_per_sec);
}

// Dummy implementations for when prometheus feature is disabled
#[cfg(not(feature = "prometheus"))]
pub async fn start_prometheus_exporter(_port: u16) -> anyhow::Result<()> {
    Ok(())
}
