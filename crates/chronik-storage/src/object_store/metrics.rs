//! Metrics collection for object storage operations.

use dashmap::DashMap;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

/// Metrics collector for object storage operations
#[derive(Debug, Clone)]
pub struct ObjectStoreMetrics {
    inner: Arc<MetricsInner>,
}

#[derive(Debug)]
struct MetricsInner {
    operation_counts: DashMap<String, AtomicU64>,
    operation_durations: DashMap<String, AtomicU64>,
    error_counts: DashMap<String, AtomicU64>,
    bytes_transferred: AtomicU64,
    active_operations: AtomicU64,
}

impl Default for ObjectStoreMetrics {
    fn default() -> Self {
        Self::new()
    }
}

impl ObjectStoreMetrics {
    /// Create a new metrics collector
    pub fn new() -> Self {
        Self {
            inner: Arc::new(MetricsInner {
                operation_counts: DashMap::new(),
                operation_durations: DashMap::new(),
                error_counts: DashMap::new(),
                bytes_transferred: AtomicU64::new(0),
                active_operations: AtomicU64::new(0),
            }),
        }
    }

    /// Record the start of an operation
    pub fn operation_started(&self, operation: &str) -> OperationMetrics {
        self.inner.active_operations.fetch_add(1, Ordering::Relaxed);
        OperationMetrics {
            operation: operation.to_string(),
            start_time: Instant::now(),
            metrics: self.clone(),
        }
    }

    /// Get operation count
    pub fn operation_count(&self, operation: &str) -> u64 {
        self.inner
            .operation_counts
            .get(operation)
            .map(|c| c.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// Get average operation duration in milliseconds
    pub fn average_duration_ms(&self, operation: &str) -> Option<f64> {
        let count = self.operation_count(operation);
        if count == 0 {
            return None;
        }

        let total_duration = self.inner
            .operation_durations
            .get(operation)
            .map(|d| d.load(Ordering::Relaxed))
            .unwrap_or(0);

        Some(total_duration as f64 / count as f64)
    }

    /// Get error count
    pub fn error_count(&self, operation: &str) -> u64 {
        self.inner
            .error_counts
            .get(operation)
            .map(|c| c.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// Get total bytes transferred
    pub fn bytes_transferred(&self) -> u64 {
        self.inner.bytes_transferred.load(Ordering::Relaxed)
    }

    /// Get number of active operations
    pub fn active_operations(&self) -> u64 {
        self.inner.active_operations.load(Ordering::Relaxed)
    }

    /// Record bytes transferred
    pub fn record_bytes_transferred(&self, bytes: u64) {
        self.inner.bytes_transferred.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Get all metrics as a snapshot
    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            operation_counts: self.inner
                .operation_counts
                .iter()
                .map(|entry| (entry.key().clone(), entry.value().load(Ordering::Relaxed)))
                .collect(),
            error_counts: self.inner
                .error_counts
                .iter()
                .map(|entry| (entry.key().clone(), entry.value().load(Ordering::Relaxed)))
                .collect(),
            bytes_transferred: self.bytes_transferred(),
            active_operations: self.active_operations(),
        }
    }
}

/// Metrics for a specific operation in progress
pub struct OperationMetrics {
    operation: String,
    start_time: Instant,
    metrics: ObjectStoreMetrics,
}

impl OperationMetrics {
    /// Record successful completion of the operation
    pub fn success(self) {
        self.complete(false);
    }

    /// Record successful completion with bytes transferred
    pub fn success_with_bytes(self, bytes: u64) {
        self.metrics.record_bytes_transferred(bytes);
        self.complete(false);
    }

    /// Record error completion of the operation
    pub fn error(self) {
        self.complete(true);
    }

    fn complete(self, is_error: bool) {
        let duration = self.start_time.elapsed();
        
        // Update operation count
        self.metrics.inner.operation_counts
            .entry(self.operation.clone())
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(1, Ordering::Relaxed);

        // Update duration
        self.metrics.inner.operation_durations
            .entry(self.operation.clone())
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(duration.as_millis() as u64, Ordering::Relaxed);

        // Update error count if needed
        if is_error {
            self.metrics.inner.error_counts
                .entry(self.operation.clone())
                .or_insert_with(|| AtomicU64::new(0))
                .fetch_add(1, Ordering::Relaxed);
        }

        // Decrement active operations
        self.metrics.inner.active_operations.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Snapshot of metrics at a point in time
#[derive(Debug, Clone)]
pub struct MetricsSnapshot {
    pub operation_counts: HashMap<String, u64>,
    pub error_counts: HashMap<String, u64>,
    pub bytes_transferred: u64,
    pub active_operations: u64,
}

impl MetricsSnapshot {
    /// Calculate success rate for an operation
    pub fn success_rate(&self, operation: &str) -> Option<f64> {
        let total = self.operation_counts.get(operation).copied().unwrap_or(0);
        if total == 0 {
            return None;
        }

        let errors = self.error_counts.get(operation).copied().unwrap_or(0);
        let successes = total.saturating_sub(errors);
        
        Some(successes as f64 / total as f64)
    }

    /// Get operations sorted by frequency
    pub fn operations_by_frequency(&self) -> Vec<(String, u64)> {
        let mut ops: Vec<_> = self.operation_counts.iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        ops.sort_by(|a, b| b.1.cmp(&a.1));
        ops
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn test_metrics_basic_operations() {
        let metrics = ObjectStoreMetrics::new();
        
        // Test operation tracking
        let op1 = metrics.operation_started("get");
        thread::sleep(Duration::from_millis(10));
        op1.success();
        
        assert_eq!(metrics.operation_count("get"), 1);
        assert!(metrics.average_duration_ms("get").unwrap() >= 10.0);
        
        // Test error tracking
        let op2 = metrics.operation_started("put");
        op2.error();
        
        assert_eq!(metrics.operation_count("put"), 1);
        assert_eq!(metrics.error_count("put"), 1);
    }

    #[test]
    fn test_metrics_bytes_transferred() {
        let metrics = ObjectStoreMetrics::new();
        
        let op = metrics.operation_started("put");
        op.success_with_bytes(1024);
        
        assert_eq!(metrics.bytes_transferred(), 1024);
        
        metrics.record_bytes_transferred(512);
        assert_eq!(metrics.bytes_transferred(), 1536);
    }

    #[test]
    fn test_metrics_snapshot() {
        let metrics = ObjectStoreMetrics::new();
        
        let op1 = metrics.operation_started("get");
        op1.success();
        
        let op2 = metrics.operation_started("put");
        op2.error();
        
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.operation_counts.get("get"), Some(&1));
        assert_eq!(snapshot.operation_counts.get("put"), Some(&1));
        assert_eq!(snapshot.error_counts.get("put"), Some(&1));
        assert_eq!(snapshot.success_rate("get"), Some(1.0));
        assert_eq!(snapshot.success_rate("put"), Some(0.0));
    }

    #[test]
    fn test_active_operations_tracking() {
        let metrics = ObjectStoreMetrics::new();
        
        assert_eq!(metrics.active_operations(), 0);
        
        let op1 = metrics.operation_started("get");
        assert_eq!(metrics.active_operations(), 1);
        
        let op2 = metrics.operation_started("put");
        assert_eq!(metrics.active_operations(), 2);
        
        op1.success();
        assert_eq!(metrics.active_operations(), 1);
        
        op2.error();
        assert_eq!(metrics.active_operations(), 0);
    }
}