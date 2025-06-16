//! Prometheus metrics for Chronik Stream.

use lazy_static::lazy_static;
use prometheus::{
    register_counter_vec, register_gauge_vec, register_histogram_vec,
    CounterVec, GaugeVec, HistogramVec, Registry,
};
use std::sync::Arc;

lazy_static! {
    // Controller metrics
    static ref CONTROLLER_ELECTION_COUNTER: CounterVec = register_counter_vec!(
        "chronik_controller_elections_total",
        "Total number of controller elections",
        &["result"]
    ).unwrap();
    
    static ref CONTROLLER_STATE_GAUGE: GaugeVec = register_gauge_vec!(
        "chronik_controller_state",
        "Current controller state (1=leader, 0=follower)",
        &["node_id"]
    ).unwrap();
    
    static ref METADATA_OPERATIONS: CounterVec = register_counter_vec!(
        "chronik_metadata_operations_total",
        "Total metadata operations",
        &["operation", "result"]
    ).unwrap();
    
    // Ingest metrics
    static ref MESSAGES_RECEIVED: CounterVec = register_counter_vec!(
        "chronik_messages_received_total",
        "Total messages received",
        &["topic", "partition"]
    ).unwrap();
    
    static ref MESSAGES_STORED: CounterVec = register_counter_vec!(
        "chronik_messages_stored_total",
        "Total messages stored",
        &["topic", "partition"]
    ).unwrap();
    
    static ref SEGMENT_SIZE: HistogramVec = register_histogram_vec!(
        "chronik_segment_size_bytes",
        "Size of segments in bytes",
        &["topic", "partition"]
    ).unwrap();
    
    static ref PRODUCE_LATENCY: HistogramVec = register_histogram_vec!(
        "chronik_produce_latency_seconds",
        "Produce request latency",
        &["topic"]
    ).unwrap();
    
    static ref FETCH_LATENCY: HistogramVec = register_histogram_vec!(
        "chronik_fetch_latency_seconds",
        "Fetch request latency",
        &["topic"]
    ).unwrap();
    
    // Query metrics
    static ref SEARCH_QUERIES: CounterVec = register_counter_vec!(
        "chronik_search_queries_total",
        "Total search queries",
        &["index", "result"]
    ).unwrap();
    
    static ref SEARCH_LATENCY: HistogramVec = register_histogram_vec!(
        "chronik_search_latency_seconds",
        "Search query latency",
        &["index"]
    ).unwrap();
    
    static ref AGGREGATION_WINDOWS: GaugeVec = register_gauge_vec!(
        "chronik_aggregation_windows_active",
        "Number of active aggregation windows",
        &["aggregation"]
    ).unwrap();
    
    // Janitor metrics
    static ref SEGMENTS_CLEANED: CounterVec = register_counter_vec!(
        "chronik_segments_cleaned_total",
        "Total segments cleaned",
        &["topic", "reason"]
    ).unwrap();
    
    static ref COMPACTION_RUNS: CounterVec = register_counter_vec!(
        "chronik_compaction_runs_total",
        "Total compaction runs",
        &["topic", "result"]
    ).unwrap();
    
    static ref STORAGE_USAGE: GaugeVec = register_gauge_vec!(
        "chronik_storage_usage_bytes",
        "Storage usage in bytes",
        &["backend", "bucket"]
    ).unwrap();
}

/// Metrics registry
#[derive(Clone)]
pub struct MetricsRegistry {
    registry: Arc<Registry>,
}

impl MetricsRegistry {
    /// Create new metrics registry
    pub fn new() -> Self {
        let registry = Registry::new();
        
        // Register all metrics
        registry.register(Box::new(CONTROLLER_ELECTION_COUNTER.clone())).unwrap();
        registry.register(Box::new(CONTROLLER_STATE_GAUGE.clone())).unwrap();
        registry.register(Box::new(METADATA_OPERATIONS.clone())).unwrap();
        registry.register(Box::new(MESSAGES_RECEIVED.clone())).unwrap();
        registry.register(Box::new(MESSAGES_STORED.clone())).unwrap();
        registry.register(Box::new(SEGMENT_SIZE.clone())).unwrap();
        registry.register(Box::new(PRODUCE_LATENCY.clone())).unwrap();
        registry.register(Box::new(FETCH_LATENCY.clone())).unwrap();
        registry.register(Box::new(SEARCH_QUERIES.clone())).unwrap();
        registry.register(Box::new(SEARCH_LATENCY.clone())).unwrap();
        registry.register(Box::new(AGGREGATION_WINDOWS.clone())).unwrap();
        registry.register(Box::new(SEGMENTS_CLEANED.clone())).unwrap();
        registry.register(Box::new(COMPACTION_RUNS.clone())).unwrap();
        registry.register(Box::new(STORAGE_USAGE.clone())).unwrap();
        
        Self {
            registry: Arc::new(registry),
        }
    }
    
    /// Get the Prometheus registry
    pub fn registry(&self) -> &Registry {
        &self.registry
    }
    
    /// Get controller metrics
    pub fn controller(&self) -> ControllerMetrics {
        ControllerMetrics::new()
    }
    
    /// Get ingest metrics
    pub fn ingest(&self) -> IngestMetrics {
        IngestMetrics::new()
    }
    
    /// Get query metrics
    pub fn query(&self) -> QueryMetrics {
        QueryMetrics::new()
    }
    
    /// Get janitor metrics
    pub fn janitor(&self) -> JanitorMetrics {
        JanitorMetrics::new()
    }
}

/// Controller metrics
pub struct ControllerMetrics;

impl ControllerMetrics {
    fn new() -> Self {
        Self
    }
    
    /// Record election attempt
    pub fn record_election(&self, success: bool) {
        let result = if success { "success" } else { "failure" };
        CONTROLLER_ELECTION_COUNTER.with_label_values(&[result]).inc();
    }
    
    /// Set controller state
    pub fn set_state(&self, node_id: &str, is_leader: bool) {
        let value = if is_leader { 1.0 } else { 0.0 };
        CONTROLLER_STATE_GAUGE.with_label_values(&[node_id]).set(value);
    }
    
    /// Record metadata operation
    pub fn record_metadata_operation(&self, operation: &str, success: bool) {
        let result = if success { "success" } else { "failure" };
        METADATA_OPERATIONS.with_label_values(&[operation, result]).inc();
    }
}

/// Ingest metrics
pub struct IngestMetrics;

impl IngestMetrics {
    fn new() -> Self {
        Self
    }
    
    /// Record message received
    pub fn record_message_received(&self, topic: &str, partition: i32) {
        MESSAGES_RECEIVED
            .with_label_values(&[topic, &partition.to_string()])
            .inc();
    }
    
    /// Record message stored
    pub fn record_message_stored(&self, topic: &str, partition: i32) {
        MESSAGES_STORED
            .with_label_values(&[topic, &partition.to_string()])
            .inc();
    }
    
    /// Record segment size
    pub fn record_segment_size(&self, topic: &str, partition: i32, size: u64) {
        SEGMENT_SIZE
            .with_label_values(&[topic, &partition.to_string()])
            .observe(size as f64);
    }
    
    /// Record produce latency
    pub fn record_produce_latency(&self, topic: &str, latency: f64) {
        PRODUCE_LATENCY
            .with_label_values(&[topic])
            .observe(latency);
    }
    
    /// Record fetch latency
    pub fn record_fetch_latency(&self, topic: &str, latency: f64) {
        FETCH_LATENCY
            .with_label_values(&[topic])
            .observe(latency);
    }
}

/// Query metrics
pub struct QueryMetrics;

impl QueryMetrics {
    fn new() -> Self {
        Self
    }
    
    /// Record search query
    pub fn record_search_query(&self, index: &str, success: bool) {
        let result = if success { "success" } else { "failure" };
        SEARCH_QUERIES.with_label_values(&[index, result]).inc();
    }
    
    /// Record search latency
    pub fn record_search_latency(&self, index: &str, latency: f64) {
        SEARCH_LATENCY
            .with_label_values(&[index])
            .observe(latency);
    }
    
    /// Set active aggregation windows
    pub fn set_aggregation_windows(&self, aggregation: &str, count: usize) {
        AGGREGATION_WINDOWS
            .with_label_values(&[aggregation])
            .set(count as f64);
    }
}

/// Janitor metrics
pub struct JanitorMetrics;

impl JanitorMetrics {
    fn new() -> Self {
        Self
    }
    
    /// Record segment cleaned
    pub fn record_segment_cleaned(&self, topic: &str, reason: &str) {
        SEGMENTS_CLEANED.with_label_values(&[topic, reason]).inc();
    }
    
    /// Record compaction run
    pub fn record_compaction_run(&self, topic: &str, success: bool) {
        let result = if success { "success" } else { "failure" };
        COMPACTION_RUNS.with_label_values(&[topic, result]).inc();
    }
    
    /// Set storage usage
    pub fn set_storage_usage(&self, backend: &str, bucket: &str, bytes: u64) {
        STORAGE_USAGE
            .with_label_values(&[backend, bucket])
            .set(bytes as f64);
    }
}