//! Prometheus metrics for Chronik Stream.

use prometheus::{
    register_counter_vec, register_gauge_vec, register_histogram_vec, 
    CounterVec, GaugeVec, HistogramVec, exponential_buckets,
};
use std::sync::OnceLock;

/// Global metrics registry.
pub struct Metrics {
    // Kafka-compatible metrics
    pub bytes_in_total: CounterVec,
    pub bytes_out_total: CounterVec,
    pub messages_in_total: CounterVec,
    pub request_latency: HistogramVec,
    pub under_replicated_partitions: GaugeVec,
    
    // Search metrics
    pub search_latency: HistogramVec,
    pub search_queries_total: CounterVec,
    pub segment_scan_bytes: CounterVec,
    
    // Storage metrics
    pub segment_writes_total: CounterVec,
    pub segment_reads_total: CounterVec,
    pub object_store_latency: HistogramVec,
}

static METRICS: OnceLock<Metrics> = OnceLock::new();

impl Metrics {
    /// Initialize the global metrics registry.
    pub fn init() -> Result<(), prometheus::Error> {
        let metrics = Metrics {
            bytes_in_total: register_counter_vec!(
                "kafka_server_brokertopicmetrics_bytes_in_total",
                "Total bytes received",
                &["topic", "partition"]
            )?,
            
            bytes_out_total: register_counter_vec!(
                "kafka_server_brokertopicmetrics_bytes_out_total",
                "Total bytes sent",
                &["topic", "partition"]
            )?,
            
            messages_in_total: register_counter_vec!(
                "kafka_server_brokertopicmetrics_messages_in_total",
                "Total messages received",
                &["topic", "partition"]
            )?,
            
            request_latency: register_histogram_vec!(
                "kafka_network_requestmetrics_total_time_ms",
                "Request processing time in milliseconds",
                &["request_type"],
                exponential_buckets(0.1, 2.0, 10)?
            )?,
            
            under_replicated_partitions: register_gauge_vec!(
                "kafka_server_replica_manager_under_replicated_partitions",
                "Number of under-replicated partitions",
                &["topic"]
            )?,
            
            search_latency: register_histogram_vec!(
                "chronik_search_query_latency_seconds",
                "Search query execution time in seconds",
                &["index"],
                exponential_buckets(0.001, 2.0, 10)?
            )?,
            
            search_queries_total: register_counter_vec!(
                "chronik_search_queries_total",
                "Total number of search queries",
                &["index", "status"]
            )?,
            
            segment_scan_bytes: register_counter_vec!(
                "chronik_search_segment_scan_bytes_total",
                "Total bytes scanned from segments",
                &["index"]
            )?,
            
            segment_writes_total: register_counter_vec!(
                "chronik_storage_segment_writes_total",
                "Total segment writes",
                &["topic", "partition", "status"]
            )?,
            
            segment_reads_total: register_counter_vec!(
                "chronik_storage_segment_reads_total",
                "Total segment reads",
                &["topic", "partition", "status"]
            )?,
            
            object_store_latency: register_histogram_vec!(
                "chronik_storage_object_store_latency_seconds",
                "Object store operation latency",
                &["operation", "backend"],
                exponential_buckets(0.001, 2.0, 10)?
            )?,
        };
        
        METRICS.set(metrics).map_err(|_| {
            prometheus::Error::Msg("Metrics already initialized".to_string())
        })?;
        
        Ok(())
    }
    
    /// Get the global metrics instance.
    pub fn get() -> &'static Metrics {
        METRICS.get().expect("Metrics not initialized")
    }
}