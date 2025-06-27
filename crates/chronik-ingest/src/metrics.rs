//! Metrics collection for the ingest node.
//! 
//! Provides Prometheus-compatible metrics for monitoring:
//! - Request rates and latencies
//! - Error rates and types
//! - Resource utilization
//! - Consumer group statistics
//! - Storage performance

use prometheus::{
    CounterVec, Gauge, GaugeVec, HistogramVec,
    HistogramOpts, Opts, Registry,
};
use std::sync::Arc;
use std::time::Instant;

lazy_static::lazy_static! {
    /// Global metrics registry
    static ref METRICS_REGISTRY: Registry = Registry::new();
    
    /// Request metrics
    static ref REQUEST_COUNTER: CounterVec = CounterVec::new(
        Opts::new("chronik_ingest_requests_total", "Total number of requests"),
        &["api_key", "status"]
    ).unwrap();
    
    static ref REQUEST_DURATION: HistogramVec = HistogramVec::new(
        HistogramOpts::new("chronik_ingest_request_duration_seconds", "Request duration in seconds")
            .buckets(vec![0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0]),
        &["api_key"]
    ).unwrap();
    
    /// Produce metrics
    static ref PRODUCE_RECORDS: CounterVec = CounterVec::new(
        Opts::new("chronik_ingest_produce_records_total", "Total records produced"),
        &["topic", "partition"]
    ).unwrap();
    
    static ref PRODUCE_BYTES: CounterVec = CounterVec::new(
        Opts::new("chronik_ingest_produce_bytes_total", "Total bytes produced"),
        &["topic", "partition"]
    ).unwrap();
    
    static ref PRODUCE_ERRORS: CounterVec = CounterVec::new(
        Opts::new("chronik_ingest_produce_errors_total", "Total produce errors"),
        &["topic", "partition", "error_type"]
    ).unwrap();
    
    /// Fetch metrics
    static ref FETCH_RECORDS: CounterVec = CounterVec::new(
        Opts::new("chronik_ingest_fetch_records_total", "Total records fetched"),
        &["topic", "partition"]
    ).unwrap();
    
    static ref FETCH_BYTES: CounterVec = CounterVec::new(
        Opts::new("chronik_ingest_fetch_bytes_total", "Total bytes fetched"),
        &["topic", "partition"]
    ).unwrap();
    
    /// Consumer group metrics
    static ref CONSUMER_GROUPS: Gauge = Gauge::new(
        "chronik_ingest_consumer_groups_active", "Number of active consumer groups"
    ).unwrap();
    
    static ref CONSUMER_GROUP_MEMBERS: GaugeVec = GaugeVec::new(
        Opts::new("chronik_ingest_consumer_group_members", "Number of members in group"),
        &["group_id"]
    ).unwrap();
    
    static ref CONSUMER_GROUP_REBALANCES: CounterVec = CounterVec::new(
        Opts::new("chronik_ingest_consumer_group_rebalances_total", "Total rebalances"),
        &["group_id"]
    ).unwrap();
    
    /// Storage metrics
    static ref SEGMENT_WRITES: CounterVec = CounterVec::new(
        Opts::new("chronik_ingest_segment_writes_total", "Total segment writes"),
        &["topic", "partition"]
    ).unwrap();
    
    static ref SEGMENT_SIZE: HistogramVec = HistogramVec::new(
        HistogramOpts::new("chronik_ingest_segment_size_bytes", "Segment size in bytes")
            .buckets(vec![
                1024.0, // 1KB
                10240.0, // 10KB
                102400.0, // 100KB
                1048576.0, // 1MB
                10485760.0, // 10MB
                104857600.0, // 100MB
                1073741824.0, // 1GB
            ]),
        &["topic", "partition"]
    ).unwrap();
    
    static ref SEGMENT_WRITE_DURATION: HistogramVec = HistogramVec::new(
        HistogramOpts::new("chronik_ingest_segment_write_duration_seconds", "Segment write duration")
            .buckets(vec![0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0]),
        &["topic", "partition"]
    ).unwrap();
    
    /// Resource metrics
    static ref MEMORY_USAGE: Gauge = Gauge::new(
        "chronik_ingest_memory_usage_bytes", "Current memory usage in bytes"
    ).unwrap();
    
    static ref CONNECTION_POOL_ACTIVE: Gauge = Gauge::new(
        "chronik_ingest_connections_active", "Active connections"
    ).unwrap();
    
    static ref CONNECTION_POOL_IDLE: Gauge = Gauge::new(
        "chronik_ingest_connections_idle", "Idle connections"
    ).unwrap();
}

/// Initialize metrics
pub fn init_metrics() -> Result<(), prometheus::Error> {
    // Register request metrics
    METRICS_REGISTRY.register(Box::new(REQUEST_COUNTER.clone()))?;
    METRICS_REGISTRY.register(Box::new(REQUEST_DURATION.clone()))?;
    
    // Register produce metrics
    METRICS_REGISTRY.register(Box::new(PRODUCE_RECORDS.clone()))?;
    METRICS_REGISTRY.register(Box::new(PRODUCE_BYTES.clone()))?;
    METRICS_REGISTRY.register(Box::new(PRODUCE_ERRORS.clone()))?;
    
    // Register fetch metrics
    METRICS_REGISTRY.register(Box::new(FETCH_RECORDS.clone()))?;
    METRICS_REGISTRY.register(Box::new(FETCH_BYTES.clone()))?;
    
    // Register consumer group metrics
    METRICS_REGISTRY.register(Box::new(CONSUMER_GROUPS.clone()))?;
    METRICS_REGISTRY.register(Box::new(CONSUMER_GROUP_MEMBERS.clone()))?;
    METRICS_REGISTRY.register(Box::new(CONSUMER_GROUP_REBALANCES.clone()))?;
    
    // Register storage metrics
    METRICS_REGISTRY.register(Box::new(SEGMENT_WRITES.clone()))?;
    METRICS_REGISTRY.register(Box::new(SEGMENT_SIZE.clone()))?;
    METRICS_REGISTRY.register(Box::new(SEGMENT_WRITE_DURATION.clone()))?;
    
    // Register resource metrics
    METRICS_REGISTRY.register(Box::new(MEMORY_USAGE.clone()))?;
    METRICS_REGISTRY.register(Box::new(CONNECTION_POOL_ACTIVE.clone()))?;
    METRICS_REGISTRY.register(Box::new(CONNECTION_POOL_IDLE.clone()))?;
    
    Ok(())
}

/// Get the metrics registry
pub fn registry() -> &'static Registry {
    &METRICS_REGISTRY
}

/// Request metrics tracker
pub struct RequestMetrics {
    api_key: String,
    start_time: Instant,
}

impl RequestMetrics {
    /// Create new request metrics
    pub fn new(api_key: i16) -> Self {
        let api_key_name = api_key_to_string(api_key);
        Self {
            api_key: api_key_name,
            start_time: Instant::now(),
        }
    }
    
    /// Record request completion
    pub fn record_completion(self, status: &str) {
        REQUEST_COUNTER
            .with_label_values(&[&self.api_key, status])
            .inc();
        
        REQUEST_DURATION
            .with_label_values(&[&self.api_key])
            .observe(self.start_time.elapsed().as_secs_f64());
    }
}

/// Produce metrics helpers
pub fn record_produce(topic: &str, partition: i32, records: u64, bytes: u64) {
    PRODUCE_RECORDS
        .with_label_values(&[topic, &partition.to_string()])
        .inc_by(records as f64);
    
    PRODUCE_BYTES
        .with_label_values(&[topic, &partition.to_string()])
        .inc_by(bytes as f64);
}

pub fn record_produce_error(topic: &str, partition: i32, error_type: &str) {
    PRODUCE_ERRORS
        .with_label_values(&[topic, &partition.to_string(), error_type])
        .inc();
}

/// Fetch metrics helpers
pub fn record_fetch(topic: &str, partition: i32, records: u64, bytes: u64) {
    FETCH_RECORDS
        .with_label_values(&[topic, &partition.to_string()])
        .inc_by(records as f64);
    
    FETCH_BYTES
        .with_label_values(&[topic, &partition.to_string()])
        .inc_by(bytes as f64);
}

/// Consumer group metrics helpers
pub fn update_consumer_groups(count: i64) {
    CONSUMER_GROUPS.set(count as f64);
}

pub fn update_group_members(group_id: &str, members: i64) {
    CONSUMER_GROUP_MEMBERS
        .with_label_values(&[group_id])
        .set(members as f64);
}

pub fn record_rebalance(group_id: &str) {
    CONSUMER_GROUP_REBALANCES
        .with_label_values(&[group_id])
        .inc();
}

/// Storage metrics helpers
pub fn record_segment_write(topic: &str, partition: i32, size: u64, duration: f64) {
    SEGMENT_WRITES
        .with_label_values(&[topic, &partition.to_string()])
        .inc();
    
    SEGMENT_SIZE
        .with_label_values(&[topic, &partition.to_string()])
        .observe(size as f64);
    
    SEGMENT_WRITE_DURATION
        .with_label_values(&[topic, &partition.to_string()])
        .observe(duration);
}

/// Resource metrics helpers
pub fn update_memory_usage(bytes: u64) {
    MEMORY_USAGE.set(bytes as f64);
}

pub fn update_connection_pool(active: i64, idle: i64) {
    CONNECTION_POOL_ACTIVE.set(active as f64);
    CONNECTION_POOL_IDLE.set(idle as f64);
}

/// Convert API key to string name
fn api_key_to_string(api_key: i16) -> String {
    match api_key {
        0 => "produce".to_string(),
        1 => "fetch".to_string(),
        2 => "list_offsets".to_string(),
        3 => "metadata".to_string(),
        8 => "offset_commit".to_string(),
        9 => "offset_fetch".to_string(),
        10 => "find_coordinator".to_string(),
        11 => "join_group".to_string(),
        12 => "heartbeat".to_string(),
        13 => "leave_group".to_string(),
        14 => "sync_group".to_string(),
        18 => "api_versions".to_string(),
        _ => format!("unknown_{}", api_key),
    }
}

/// Metrics exporter for integration with monitoring systems
#[derive(Clone)]
pub struct MetricsExporter {
    registry: Arc<Registry>,
}

impl MetricsExporter {
    /// Create new metrics exporter
    pub fn new() -> Self {
        Self {
            registry: Arc::new(METRICS_REGISTRY.clone()),
        }
    }
    
    /// Export metrics in Prometheus format
    pub fn export_prometheus(&self) -> Result<String, prometheus::Error> {
        use prometheus::Encoder;
        let encoder = prometheus::TextEncoder::new();
        let metric_families = self.registry.gather();
        
        let mut buffer = Vec::new();
        encoder.encode(&metric_families, &mut buffer)?;
        
        String::from_utf8(buffer)
            .map_err(|e| prometheus::Error::Msg(format!("UTF-8 conversion failed: {}", e)))
    }
    
    /// Start metrics HTTP server
    pub async fn start_http_server(self, port: u16) -> Result<(), Box<dyn std::error::Error>> {
        use axum::{routing::get, Router};
        use std::net::SocketAddr;
        
        let app = Router::new()
            .route("/metrics", get(move || async move {
                match self.export_prometheus() {
                    Ok(metrics) => (
                        axum::http::StatusCode::OK,
                        [(axum::http::header::CONTENT_TYPE, "text/plain; version=0.0.4")],
                        metrics,
                    ),
                    Err(e) => (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        [(axum::http::header::CONTENT_TYPE, "text/plain")],
                        format!("Error generating metrics: {}", e),
                    ),
                }
            }));
        
        let addr = SocketAddr::from(([0, 0, 0, 0], port));
        let listener = tokio::net::TcpListener::bind(addr).await?;
        axum::serve(listener, app).await?;
        
        Ok(())
    }
}