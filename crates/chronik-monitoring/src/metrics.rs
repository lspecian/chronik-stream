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

    // WAL metrics - Phase 1 hardening
    static ref WAL_WRITES_TOTAL: CounterVec = register_counter_vec!(
        "chronik_wal_writes_total",
        "Total number of WAL write operations",
        &["topic", "partition", "result"]
    ).unwrap();
    
    static ref WAL_WRITE_DURATION: HistogramVec = register_histogram_vec!(
        "chronik_wal_write_duration_seconds",
        "Time spent writing to WAL in seconds",
        &["topic", "partition"],
        vec![0.0001, 0.0005, 0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0]
    ).unwrap();
    
    static ref WAL_FSYNC_DURATION: HistogramVec = register_histogram_vec!(
        "chronik_wal_fsync_duration_seconds",
        "Time spent in WAL fsync operations in seconds",
        &["topic", "partition"],
        vec![0.0001, 0.0005, 0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0]
    ).unwrap();
    
    static ref WAL_RECOVERY_DURATION: HistogramVec = register_histogram_vec!(
        "chronik_wal_recovery_duration_seconds",
        "Time spent in WAL recovery operations in seconds",
        &["topic", "partition", "phase"],
        vec![0.001, 0.005, 0.01, 0.05, 0.1, 0.5, 1.0, 5.0, 10.0, 30.0, 60.0]
    ).unwrap();
    
    static ref WAL_SEGMENTS_TOTAL: GaugeVec = register_gauge_vec!(
        "chronik_wal_segments_total",
        "Total number of WAL segments",
        &["topic", "partition", "status"]
    ).unwrap();
    
    static ref WAL_BYTES_WRITTEN: CounterVec = register_counter_vec!(
        "chronik_wal_bytes_written_total",
        "Total bytes written to WAL",
        &["topic", "partition"]
    ).unwrap();
    
    static ref WAL_SEGMENT_ROTATIONS: CounterVec = register_counter_vec!(
        "chronik_wal_segment_rotations_total",
        "Total WAL segment rotations",
        &["topic", "partition", "trigger"]
    ).unwrap();
    
    static ref WAL_ERRORS_TOTAL: CounterVec = register_counter_vec!(
        "chronik_wal_errors_total",
        "Total WAL operation errors",
        &["topic", "partition", "operation", "error_type"]
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
        
        // Register WAL metrics
        registry.register(Box::new(WAL_WRITES_TOTAL.clone())).unwrap();
        registry.register(Box::new(WAL_WRITE_DURATION.clone())).unwrap();
        registry.register(Box::new(WAL_FSYNC_DURATION.clone())).unwrap();
        registry.register(Box::new(WAL_RECOVERY_DURATION.clone())).unwrap();
        registry.register(Box::new(WAL_SEGMENTS_TOTAL.clone())).unwrap();
        registry.register(Box::new(WAL_BYTES_WRITTEN.clone())).unwrap();
        registry.register(Box::new(WAL_SEGMENT_ROTATIONS.clone())).unwrap();
        registry.register(Box::new(WAL_ERRORS_TOTAL.clone())).unwrap();
        
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
    
    /// Get WAL metrics
    pub fn wal(&self) -> WalMetrics {
        WalMetrics::new()
    }

    /// Get Raft metrics
    pub fn raft(&self) -> crate::raft_metrics::RaftMetrics {
        crate::raft_metrics::RaftMetrics::new()
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

/// WAL metrics - Phase 1 hardening implementation
pub struct WalMetrics;

impl WalMetrics {
    fn new() -> Self {
        Self
    }
    
    /// Record a WAL write operation
    pub fn record_write(&self, topic: &str, partition: i32, success: bool, duration: f64, bytes: u64) {
        let result = if success { "success" } else { "failure" };
        let partition_str = partition.to_string();
        
        WAL_WRITES_TOTAL
            .with_label_values(&[topic, &partition_str, result])
            .inc();
        
        if success {
            WAL_WRITE_DURATION
                .with_label_values(&[topic, &partition_str])
                .observe(duration);
            
            WAL_BYTES_WRITTEN
                .with_label_values(&[topic, &partition_str])
                .inc_by(bytes as f64);
        }
    }
    
    /// Record WAL fsync operation duration
    pub fn record_fsync(&self, topic: &str, partition: i32, duration: f64) {
        let partition_str = partition.to_string();
        WAL_FSYNC_DURATION
            .with_label_values(&[topic, &partition_str])
            .observe(duration);
    }
    
    /// Record WAL recovery operation
    pub fn record_recovery(&self, topic: &str, partition: i32, phase: &str, duration: f64) {
        let partition_str = partition.to_string();
        WAL_RECOVERY_DURATION
            .with_label_values(&[topic, &partition_str, phase])
            .observe(duration);
    }
    
    /// Update WAL segment count
    pub fn set_segment_count(&self, topic: &str, partition: i32, status: &str, count: usize) {
        let partition_str = partition.to_string();
        WAL_SEGMENTS_TOTAL
            .with_label_values(&[topic, &partition_str, status])
            .set(count as f64);
    }
    
    /// Record WAL segment rotation
    pub fn record_segment_rotation(&self, topic: &str, partition: i32, trigger: &str) {
        let partition_str = partition.to_string();
        WAL_SEGMENT_ROTATIONS
            .with_label_values(&[topic, &partition_str, trigger])
            .inc();
    }
    
    /// Record WAL error
    pub fn record_error(&self, topic: &str, partition: i32, operation: &str, error_type: &str) {
        let partition_str = partition.to_string();
        WAL_ERRORS_TOTAL
            .with_label_values(&[topic, &partition_str, operation, error_type])
            .inc();
    }
    
    /// Record WAL batch write operation
    pub fn record_batch_write(&self, topic: &str, partition: i32, batch_size: usize, duration: f64, total_bytes: u64) {
        let partition_str = partition.to_string();
        
        // Record batch as single successful write operation
        WAL_WRITES_TOTAL
            .with_label_values(&[topic, &partition_str, "success"])
            .inc_by(batch_size as f64);
        
        WAL_WRITE_DURATION
            .with_label_values(&[topic, &partition_str])
            .observe(duration);
        
        WAL_BYTES_WRITTEN
            .with_label_values(&[topic, &partition_str])
            .inc_by(total_bytes as f64);
    }
    
    /// Helper method to time WAL operations
    pub fn time_operation<F, R>(&self, topic: &str, partition: i32, operation: F) -> R 
    where
        F: FnOnce() -> R,
    {
        let start = std::time::Instant::now();
        let result = operation();
        let duration = start.elapsed().as_secs_f64();
        
        // This is a generic timing helper - specific record methods should be called by the operation
        // to provide proper success/failure tracking
        
        result
    }
}