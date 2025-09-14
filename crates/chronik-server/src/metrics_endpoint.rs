//! HTTP metrics endpoint for WAL metadata operations

use axum::{response::Json, Router, routing::get, extract::Query};
use serde::Deserialize;
use std::collections::HashMap;

use chronik_common::metadata::{global_metrics, MetricsReport};

#[derive(Debug, Deserialize)]
pub struct MetricsQuery {
    /// Format: json (default), prometheus
    #[serde(default = "default_format")]
    format: String,
}

fn default_format() -> String {
    "json".to_string()
}

/// Get metadata store metrics
pub async fn get_metrics(Query(query): Query<MetricsQuery>) -> Result<String, String> {
    let metrics = global_metrics();
    let report = metrics.export_report();

    match query.format.as_str() {
        "json" => {
            match serde_json::to_string_pretty(&report) {
                Ok(json) => Ok(json),
                Err(e) => Err(format!("Failed to serialize metrics: {}", e)),
            }
        }
        "prometheus" => {
            Ok(format_prometheus_metrics(&report))
        }
        _ => Err("Unsupported format. Use 'json' or 'prometheus'".to_string()),
    }
}

/// Format metrics in Prometheus format
fn format_prometheus_metrics(report: &MetricsReport) -> String {
    let mut output = String::new();

    // Topic operation metrics
    output.push_str("# HELP chronik_metadata_topic_operations_total Total number of topic operations\n");
    output.push_str("# TYPE chronik_metadata_topic_operations_total counter\n");
    output.push_str(&format!("chronik_metadata_topic_operations_total{{operation=\"create\"}} {}\n", report.topic_operations.creates));
    output.push_str(&format!("chronik_metadata_topic_operations_total{{operation=\"get\"}} {}\n", report.topic_operations.gets));
    output.push_str(&format!("chronik_metadata_topic_operations_total{{operation=\"list\"}} {}\n", report.topic_operations.lists));
    output.push_str(&format!("chronik_metadata_topic_operations_total{{operation=\"delete\"}} {}\n", report.topic_operations.deletes));

    // Consumer group operation metrics
    output.push_str("# HELP chronik_metadata_consumer_group_operations_total Total number of consumer group operations\n");
    output.push_str("# TYPE chronik_metadata_consumer_group_operations_total counter\n");
    output.push_str(&format!("chronik_metadata_consumer_group_operations_total{{operation=\"create\"}} {}\n", report.consumer_group_operations.creates));
    output.push_str(&format!("chronik_metadata_consumer_group_operations_total{{operation=\"get\"}} {}\n", report.consumer_group_operations.gets));

    // Offset operation metrics
    output.push_str("# HELP chronik_metadata_offset_operations_total Total number of offset operations\n");
    output.push_str("# TYPE chronik_metadata_offset_operations_total counter\n");
    output.push_str(&format!("chronik_metadata_offset_operations_total{{operation=\"commit\"}} {}\n", report.offset_operations.commits));
    output.push_str(&format!("chronik_metadata_offset_operations_total{{operation=\"get\"}} {}\n", report.offset_operations.gets));

    // Segment operation metrics
    output.push_str("# HELP chronik_metadata_segment_operations_total Total number of segment operations\n");
    output.push_str("# TYPE chronik_metadata_segment_operations_total counter\n");
    output.push_str(&format!("chronik_metadata_segment_operations_total{{operation=\"persist\"}} {}\n", report.segment_operations.persists));
    output.push_str(&format!("chronik_metadata_segment_operations_total{{operation=\"get\"}} {}\n", report.segment_operations.gets));
    output.push_str(&format!("chronik_metadata_segment_operations_total{{operation=\"list\"}} {}\n", report.segment_operations.lists));

    // Error metrics
    output.push_str("# HELP chronik_metadata_errors_total Total number of metadata errors\n");
    output.push_str("# TYPE chronik_metadata_errors_total counter\n");
    output.push_str(&format!("chronik_metadata_errors_total{{type=\"total\"}} {}\n", report.errors.total));
    output.push_str(&format!("chronik_metadata_errors_total{{type=\"serialization\"}} {}\n", report.errors.serialization));
    output.push_str(&format!("chronik_metadata_errors_total{{type=\"storage\"}} {}\n", report.errors.storage));
    output.push_str(&format!("chronik_metadata_errors_total{{type=\"not_found\"}} {}\n", report.errors.not_found));

    // WAL metrics
    output.push_str("# HELP chronik_metadata_wal_operations_total Total number of WAL operations\n");
    output.push_str("# TYPE chronik_metadata_wal_operations_total counter\n");
    output.push_str(&format!("chronik_metadata_wal_operations_total{{operation=\"append\"}} {}\n", report.wal.appends));
    output.push_str(&format!("chronik_metadata_wal_operations_total{{operation=\"read\"}} {}\n", report.wal.reads));

    output.push_str("# HELP chronik_metadata_wal_recovery_events_total Total number of events recovered during WAL recovery\n");
    output.push_str("# TYPE chronik_metadata_wal_recovery_events_total counter\n");
    output.push_str(&format!("chronik_metadata_wal_recovery_events_total {}\n", report.wal.recovery_events));

    output.push_str("# HELP chronik_metadata_wal_recovery_time_ms Time taken for WAL recovery in milliseconds\n");
    output.push_str("# TYPE chronik_metadata_wal_recovery_time_ms gauge\n");
    output.push_str(&format!("chronik_metadata_wal_recovery_time_ms {}\n", report.wal.recovery_time_ms));

    // Cache metrics
    output.push_str("# HELP chronik_metadata_cache_operations_total Total number of cache operations\n");
    output.push_str("# TYPE chronik_metadata_cache_operations_total counter\n");
    output.push_str(&format!("chronik_metadata_cache_operations_total{{operation=\"hit\"}} {}\n", report.cache.hits));
    output.push_str(&format!("chronik_metadata_cache_operations_total{{operation=\"miss\"}} {}\n", report.cache.misses));
    output.push_str(&format!("chronik_metadata_cache_operations_total{{operation=\"eviction\"}} {}\n", report.cache.evictions));

    output.push_str("# HELP chronik_metadata_cache_hit_rate Cache hit rate (0.0 to 1.0)\n");
    output.push_str("# TYPE chronik_metadata_cache_hit_rate gauge\n");
    output.push_str(&format!("chronik_metadata_cache_hit_rate {}\n", report.cache.hit_rate));

    output.push_str("# HELP chronik_metadata_cache_size Current cache size\n");
    output.push_str("# TYPE chronik_metadata_cache_size gauge\n");
    output.push_str(&format!("chronik_metadata_cache_size {}\n", report.cache.size));

    // Performance metrics
    output.push_str("# HELP chronik_metadata_operations_total Total number of metadata operations\n");
    output.push_str("# TYPE chronik_metadata_operations_total counter\n");
    output.push_str(&format!("chronik_metadata_operations_total {}\n", report.performance.total_operations));

    output.push_str("# HELP chronik_metadata_operation_duration_ns Average operation duration in nanoseconds\n");
    output.push_str("# TYPE chronik_metadata_operation_duration_ns gauge\n");
    output.push_str(&format!("chronik_metadata_operation_duration_ns{{type=\"average\"}} {}\n", report.performance.avg_operation_time_ns));
    output.push_str(&format!("chronik_metadata_operation_duration_ns{{type=\"max\"}} {}\n", report.performance.max_operation_time_ns));

    output.push_str("# HELP chronik_metadata_operations_per_second Operations per second\n");
    output.push_str("# TYPE chronik_metadata_operations_per_second gauge\n");
    output.push_str(&format!("chronik_metadata_operations_per_second {}\n", report.performance.operations_per_second));

    output.push_str("# HELP chronik_metadata_error_rate Error rate (errors per operation)\n");
    output.push_str("# TYPE chronik_metadata_error_rate gauge\n");
    output.push_str(&format!("chronik_metadata_error_rate {}\n", report.performance.error_rate));

    // State metrics
    output.push_str("# HELP chronik_metadata_state_count Current count of metadata entities\n");
    output.push_str("# TYPE chronik_metadata_state_count gauge\n");
    output.push_str(&format!("chronik_metadata_state_count{{type=\"topics\"}} {}\n", report.state.total_topics));
    output.push_str(&format!("chronik_metadata_state_count{{type=\"consumer_groups\"}} {}\n", report.state.total_consumer_groups));
    output.push_str(&format!("chronik_metadata_state_count{{type=\"segments\"}} {}\n", report.state.total_segments));

    output.push_str("# HELP chronik_metadata_memory_usage_bytes Memory usage in bytes\n");
    output.push_str("# TYPE chronik_metadata_memory_usage_bytes gauge\n");
    output.push_str(&format!("chronik_metadata_memory_usage_bytes {}\n", report.state.memory_usage_bytes));

    output
}

/// Get health status
pub async fn get_health() -> Json<HashMap<String, String>> {
    let mut health = HashMap::new();
    health.insert("status".to_string(), "healthy".to_string());
    health.insert("component".to_string(), "wal-metadata-store".to_string());

    // Add basic metrics
    let metrics = global_metrics();
    let report = metrics.export_report();
    health.insert("total_operations".to_string(), report.performance.total_operations.to_string());
    health.insert("error_rate".to_string(), format!("{:.4}", report.performance.error_rate));
    health.insert("operations_per_second".to_string(), format!("{:.2}", report.performance.operations_per_second));
    health.insert("cache_hit_rate".to_string(), format!("{:.4}", report.cache.hit_rate));

    Json(health)
}

/// Create metrics router
pub fn metrics_router() -> Router {
    Router::new()
        .route("/metrics", get(|query| async move {
            match get_metrics(query).await {
                Ok(response) => response,
                Err(e) => format!("Error: {}", e),
            }
        }))
        .route("/health", get(get_health))
        .route("/health/metadata", get(get_health))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_metrics_json_format() {
        let query = MetricsQuery { format: "json".to_string() };
        let result = get_metrics(Query(query)).await;
        assert!(result.is_ok());

        let json_str = result.unwrap();
        let _: MetricsReport = serde_json::from_str(&json_str).unwrap();
    }

    #[tokio::test]
    async fn test_metrics_prometheus_format() {
        let query = MetricsQuery { format: "prometheus".to_string() };
        let result = get_metrics(Query(query)).await;
        assert!(result.is_ok());

        let prometheus_str = result.unwrap();
        assert!(prometheus_str.contains("chronik_metadata_"));
        assert!(prometheus_str.contains("# HELP"));
        assert!(prometheus_str.contains("# TYPE"));
    }

    #[test]
    fn test_prometheus_format() {
        let report = MetricsReport {
            topic_operations: chronik_common::metadata::metrics::TopicOperationMetrics {
                creates: 10,
                gets: 20,
                lists: 5,
                deletes: 2,
            },
            consumer_group_operations: chronik_common::metadata::metrics::ConsumerGroupOperationMetrics {
                creates: 3,
                gets: 15,
            },
            offset_operations: chronik_common::metadata::metrics::OffsetOperationMetrics {
                commits: 100,
                gets: 50,
            },
            segment_operations: chronik_common::metadata::metrics::SegmentOperationMetrics {
                persists: 25,
                gets: 40,
                lists: 8,
            },
            errors: chronik_common::metadata::metrics::ErrorMetrics {
                total: 2,
                serialization: 1,
                storage: 0,
                not_found: 1,
            },
            wal: chronik_common::metadata::metrics::WalMetrics {
                appends: 135,
                reads: 60,
                recovery_events: 100,
                recovery_time_ms: 250,
            },
            cache: chronik_common::metadata::metrics::CacheMetrics {
                hits: 90,
                misses: 10,
                hit_rate: 0.9,
                size: 1000,
                evictions: 5,
            },
            performance: chronik_common::metadata::metrics::PerformanceMetrics {
                total_operations: 135,
                avg_operation_time_ns: 50000,
                max_operation_time_ns: 200000,
                operations_per_second: 20000.0,
                error_rate: 0.0148,
            },
            state: chronik_common::metadata::metrics::StateMetrics {
                total_topics: 10,
                total_consumer_groups: 3,
                total_segments: 25,
                memory_usage_bytes: 1048576,
            },
        };

        let prometheus_output = format_prometheus_metrics(&report);

        // Verify key metrics are present
        assert!(prometheus_output.contains("chronik_metadata_topic_operations_total{operation=\"create\"} 10"));
        assert!(prometheus_output.contains("chronik_metadata_cache_hit_rate 0.9"));
        assert!(prometheus_output.contains("chronik_metadata_operations_per_second 20000"));
        assert!(prometheus_output.contains("chronik_metadata_memory_usage_bytes 1048576"));
    }
}