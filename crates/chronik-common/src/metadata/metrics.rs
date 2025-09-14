//! Metrics for WAL-based metadata operations

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

/// Metrics collector for WAL metadata operations
#[derive(Debug)]
pub struct WalMetadataMetrics {
    // Operation counters
    pub topic_creates: AtomicU64,
    pub topic_gets: AtomicU64,
    pub topic_lists: AtomicU64,
    pub topic_deletes: AtomicU64,
    pub consumer_group_creates: AtomicU64,
    pub consumer_group_gets: AtomicU64,
    pub offset_commits: AtomicU64,
    pub offset_gets: AtomicU64,
    pub segment_persists: AtomicU64,
    pub segment_gets: AtomicU64,
    pub segment_lists: AtomicU64,

    // Error counters
    pub total_errors: AtomicU64,
    pub serialization_errors: AtomicU64,
    pub storage_errors: AtomicU64,
    pub not_found_errors: AtomicU64,

    // WAL-specific metrics
    pub wal_appends: AtomicU64,
    pub wal_reads: AtomicU64,
    pub wal_recovery_events: AtomicU64,
    pub wal_recovery_time_ms: AtomicU64,

    // Cache metrics
    pub cache_hits: AtomicU64,
    pub cache_misses: AtomicU64,
    pub cache_size: AtomicUsize,
    pub cache_evictions: AtomicU64,

    // Performance metrics
    pub avg_operation_time_ns: AtomicU64,
    pub max_operation_time_ns: AtomicU64,
    pub total_operations: AtomicU64,

    // State metrics
    pub total_topics: AtomicUsize,
    pub total_consumer_groups: AtomicUsize,
    pub total_segments: AtomicUsize,
    pub memory_usage_bytes: AtomicUsize,
}

impl Default for WalMetadataMetrics {
    fn default() -> Self {
        Self::new()
    }
}

impl WalMetadataMetrics {
    /// Create a new metrics collector
    pub fn new() -> Self {
        Self {
            topic_creates: AtomicU64::new(0),
            topic_gets: AtomicU64::new(0),
            topic_lists: AtomicU64::new(0),
            topic_deletes: AtomicU64::new(0),
            consumer_group_creates: AtomicU64::new(0),
            consumer_group_gets: AtomicU64::new(0),
            offset_commits: AtomicU64::new(0),
            offset_gets: AtomicU64::new(0),
            segment_persists: AtomicU64::new(0),
            segment_gets: AtomicU64::new(0),
            segment_lists: AtomicU64::new(0),

            total_errors: AtomicU64::new(0),
            serialization_errors: AtomicU64::new(0),
            storage_errors: AtomicU64::new(0),
            not_found_errors: AtomicU64::new(0),

            wal_appends: AtomicU64::new(0),
            wal_reads: AtomicU64::new(0),
            wal_recovery_events: AtomicU64::new(0),
            wal_recovery_time_ms: AtomicU64::new(0),

            cache_hits: AtomicU64::new(0),
            cache_misses: AtomicU64::new(0),
            cache_size: AtomicUsize::new(0),
            cache_evictions: AtomicU64::new(0),

            avg_operation_time_ns: AtomicU64::new(0),
            max_operation_time_ns: AtomicU64::new(0),
            total_operations: AtomicU64::new(0),

            total_topics: AtomicUsize::new(0),
            total_consumer_groups: AtomicUsize::new(0),
            total_segments: AtomicUsize::new(0),
            memory_usage_bytes: AtomicUsize::new(0),
        }
    }

    /// Record a topic operation
    pub fn record_topic_operation(&self, operation: TopicOperation, duration: Duration) {
        match operation {
            TopicOperation::Create => self.topic_creates.fetch_add(1, Ordering::Relaxed),
            TopicOperation::Get => self.topic_gets.fetch_add(1, Ordering::Relaxed),
            TopicOperation::List => self.topic_lists.fetch_add(1, Ordering::Relaxed),
            TopicOperation::Delete => self.topic_deletes.fetch_add(1, Ordering::Relaxed),
        };

        self.record_operation_time(duration);
    }

    /// Record a consumer group operation
    pub fn record_consumer_group_operation(&self, operation: ConsumerGroupOperation, duration: Duration) {
        match operation {
            ConsumerGroupOperation::Create => self.consumer_group_creates.fetch_add(1, Ordering::Relaxed),
            ConsumerGroupOperation::Get => self.consumer_group_gets.fetch_add(1, Ordering::Relaxed),
        };

        self.record_operation_time(duration);
    }

    /// Record an offset operation
    pub fn record_offset_operation(&self, operation: OffsetOperation, duration: Duration) {
        match operation {
            OffsetOperation::Commit => self.offset_commits.fetch_add(1, Ordering::Relaxed),
            OffsetOperation::Get => self.offset_gets.fetch_add(1, Ordering::Relaxed),
        };

        self.record_operation_time(duration);
    }

    /// Record a segment operation
    pub fn record_segment_operation(&self, operation: SegmentOperation, duration: Duration) {
        match operation {
            SegmentOperation::Persist => self.segment_persists.fetch_add(1, Ordering::Relaxed),
            SegmentOperation::Get => self.segment_gets.fetch_add(1, Ordering::Relaxed),
            SegmentOperation::List => self.segment_lists.fetch_add(1, Ordering::Relaxed),
        };

        self.record_operation_time(duration);
    }

    /// Record an error
    pub fn record_error(&self, error_type: ErrorType) {
        self.total_errors.fetch_add(1, Ordering::Relaxed);

        match error_type {
            ErrorType::Serialization => self.serialization_errors.fetch_add(1, Ordering::Relaxed),
            ErrorType::Storage => self.storage_errors.fetch_add(1, Ordering::Relaxed),
            ErrorType::NotFound => self.not_found_errors.fetch_add(1, Ordering::Relaxed),
        };
    }

    /// Record a WAL operation
    pub fn record_wal_operation(&self, operation: WalOperation, count: u64) {
        match operation {
            WalOperation::Append => self.wal_appends.fetch_add(count, Ordering::Relaxed),
            WalOperation::Read => self.wal_reads.fetch_add(count, Ordering::Relaxed),
        };
    }

    /// Record WAL recovery metrics
    pub fn record_wal_recovery(&self, events_recovered: u64, recovery_time: Duration) {
        self.wal_recovery_events.fetch_add(events_recovered, Ordering::Relaxed);
        self.wal_recovery_time_ms.store(recovery_time.as_millis() as u64, Ordering::Relaxed);
    }

    /// Record cache operation
    pub fn record_cache_operation(&self, operation: CacheOperation) {
        match operation {
            CacheOperation::Hit => self.cache_hits.fetch_add(1, Ordering::Relaxed),
            CacheOperation::Miss => self.cache_misses.fetch_add(1, Ordering::Relaxed),
            CacheOperation::Eviction => self.cache_evictions.fetch_add(1, Ordering::Relaxed),
        };
    }

    /// Update cache size
    pub fn update_cache_size(&self, size: usize) {
        self.cache_size.store(size, Ordering::Relaxed);
    }

    /// Update state counts
    pub fn update_state_counts(&self, topics: usize, groups: usize, segments: usize, memory_bytes: usize) {
        self.total_topics.store(topics, Ordering::Relaxed);
        self.total_consumer_groups.store(groups, Ordering::Relaxed);
        self.total_segments.store(segments, Ordering::Relaxed);
        self.memory_usage_bytes.store(memory_bytes, Ordering::Relaxed);
    }

    /// Record operation timing
    fn record_operation_time(&self, duration: Duration) {
        let duration_ns = duration.as_nanos() as u64;

        // Update total operations
        let _total_ops = self.total_operations.fetch_add(1, Ordering::Relaxed) + 1;

        // Update max operation time
        let mut current_max = self.max_operation_time_ns.load(Ordering::Relaxed);
        while duration_ns > current_max {
            match self.max_operation_time_ns.compare_exchange_weak(
                current_max,
                duration_ns,
                Ordering::Relaxed,
                Ordering::Relaxed
            ) {
                Ok(_) => break,
                Err(new_max) => current_max = new_max,
            }
        }

        // Update running average (simple moving average approximation)
        let current_avg = self.avg_operation_time_ns.load(Ordering::Relaxed);
        let new_avg = ((current_avg * (_total_ops - 1)) + duration_ns) / _total_ops;
        self.avg_operation_time_ns.store(new_avg, Ordering::Relaxed);
    }

    /// Get current cache hit rate (0.0 - 1.0)
    pub fn cache_hit_rate(&self) -> f64 {
        let hits = self.cache_hits.load(Ordering::Relaxed) as f64;
        let misses = self.cache_misses.load(Ordering::Relaxed) as f64;
        let total = hits + misses;

        if total > 0.0 {
            hits / total
        } else {
            0.0
        }
    }

    /// Get operations per second (approximation)
    pub fn operations_per_second(&self) -> f64 {
        let total_ops = self.total_operations.load(Ordering::Relaxed);
        let avg_time_ns = self.avg_operation_time_ns.load(Ordering::Relaxed);

        if avg_time_ns > 0 {
            1_000_000_000.0 / (avg_time_ns as f64)
        } else {
            0.0
        }
    }

    /// Get error rate (errors per operation)
    pub fn error_rate(&self) -> f64 {
        let total_errors = self.total_errors.load(Ordering::Relaxed) as f64;
        let total_ops = self.total_operations.load(Ordering::Relaxed) as f64;

        if total_ops > 0.0 {
            total_errors / total_ops
        } else {
            0.0
        }
    }

    /// Export metrics as a structured report
    pub fn export_report(&self) -> MetricsReport {
        MetricsReport {
            // Operation counts
            topic_operations: TopicOperationMetrics {
                creates: self.topic_creates.load(Ordering::Relaxed),
                gets: self.topic_gets.load(Ordering::Relaxed),
                lists: self.topic_lists.load(Ordering::Relaxed),
                deletes: self.topic_deletes.load(Ordering::Relaxed),
            },
            consumer_group_operations: ConsumerGroupOperationMetrics {
                creates: self.consumer_group_creates.load(Ordering::Relaxed),
                gets: self.consumer_group_gets.load(Ordering::Relaxed),
            },
            offset_operations: OffsetOperationMetrics {
                commits: self.offset_commits.load(Ordering::Relaxed),
                gets: self.offset_gets.load(Ordering::Relaxed),
            },
            segment_operations: SegmentOperationMetrics {
                persists: self.segment_persists.load(Ordering::Relaxed),
                gets: self.segment_gets.load(Ordering::Relaxed),
                lists: self.segment_lists.load(Ordering::Relaxed),
            },

            // Error metrics
            errors: ErrorMetrics {
                total: self.total_errors.load(Ordering::Relaxed),
                serialization: self.serialization_errors.load(Ordering::Relaxed),
                storage: self.storage_errors.load(Ordering::Relaxed),
                not_found: self.not_found_errors.load(Ordering::Relaxed),
            },

            // WAL metrics
            wal: WalMetrics {
                appends: self.wal_appends.load(Ordering::Relaxed),
                reads: self.wal_reads.load(Ordering::Relaxed),
                recovery_events: self.wal_recovery_events.load(Ordering::Relaxed),
                recovery_time_ms: self.wal_recovery_time_ms.load(Ordering::Relaxed),
            },

            // Cache metrics
            cache: CacheMetrics {
                hits: self.cache_hits.load(Ordering::Relaxed),
                misses: self.cache_misses.load(Ordering::Relaxed),
                hit_rate: self.cache_hit_rate(),
                size: self.cache_size.load(Ordering::Relaxed),
                evictions: self.cache_evictions.load(Ordering::Relaxed),
            },

            // Performance metrics
            performance: PerformanceMetrics {
                total_operations: self.total_operations.load(Ordering::Relaxed),
                avg_operation_time_ns: self.avg_operation_time_ns.load(Ordering::Relaxed),
                max_operation_time_ns: self.max_operation_time_ns.load(Ordering::Relaxed),
                operations_per_second: self.operations_per_second(),
                error_rate: self.error_rate(),
            },

            // State metrics
            state: StateMetrics {
                total_topics: self.total_topics.load(Ordering::Relaxed),
                total_consumer_groups: self.total_consumer_groups.load(Ordering::Relaxed),
                total_segments: self.total_segments.load(Ordering::Relaxed),
                memory_usage_bytes: self.memory_usage_bytes.load(Ordering::Relaxed),
            },
        }
    }
}

/// Operation types for metrics tracking
#[derive(Debug, Clone, Copy)]
pub enum TopicOperation {
    Create,
    Get,
    List,
    Delete,
}

#[derive(Debug, Clone, Copy)]
pub enum ConsumerGroupOperation {
    Create,
    Get,
}

#[derive(Debug, Clone, Copy)]
pub enum OffsetOperation {
    Commit,
    Get,
}

#[derive(Debug, Clone, Copy)]
pub enum SegmentOperation {
    Persist,
    Get,
    List,
}

#[derive(Debug, Clone, Copy)]
pub enum ErrorType {
    Serialization,
    Storage,
    NotFound,
}

#[derive(Debug, Clone, Copy)]
pub enum WalOperation {
    Append,
    Read,
}

#[derive(Debug, Clone, Copy)]
pub enum CacheOperation {
    Hit,
    Miss,
    Eviction,
}

/// Structured metrics report
#[derive(Debug, Clone, serde::Serialize)]
pub struct MetricsReport {
    pub topic_operations: TopicOperationMetrics,
    pub consumer_group_operations: ConsumerGroupOperationMetrics,
    pub offset_operations: OffsetOperationMetrics,
    pub segment_operations: SegmentOperationMetrics,
    pub errors: ErrorMetrics,
    pub wal: WalMetrics,
    pub cache: CacheMetrics,
    pub performance: PerformanceMetrics,
    pub state: StateMetrics,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TopicOperationMetrics {
    pub creates: u64,
    pub gets: u64,
    pub lists: u64,
    pub deletes: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ConsumerGroupOperationMetrics {
    pub creates: u64,
    pub gets: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct OffsetOperationMetrics {
    pub commits: u64,
    pub gets: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SegmentOperationMetrics {
    pub persists: u64,
    pub gets: u64,
    pub lists: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ErrorMetrics {
    pub total: u64,
    pub serialization: u64,
    pub storage: u64,
    pub not_found: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct WalMetrics {
    pub appends: u64,
    pub reads: u64,
    pub recovery_events: u64,
    pub recovery_time_ms: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CacheMetrics {
    pub hits: u64,
    pub misses: u64,
    pub hit_rate: f64,
    pub size: usize,
    pub evictions: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PerformanceMetrics {
    pub total_operations: u64,
    pub avg_operation_time_ns: u64,
    pub max_operation_time_ns: u64,
    pub operations_per_second: f64,
    pub error_rate: f64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct StateMetrics {
    pub total_topics: usize,
    pub total_consumer_groups: usize,
    pub total_segments: usize,
    pub memory_usage_bytes: usize,
}

/// Global metrics instance
pub fn global_metrics() -> &'static WalMetadataMetrics {
    use std::sync::OnceLock;
    static METRICS: OnceLock<WalMetadataMetrics> = OnceLock::new();
    METRICS.get_or_init(|| WalMetadataMetrics::new())
}

/// Convenience macro for timing operations
#[macro_export]
macro_rules! time_operation {
    ($metrics:expr, $operation:expr, $code:block) => {{
        let start = std::time::Instant::now();
        let result = $code;
        let duration = start.elapsed();
        $metrics.record_operation_time(duration);
        result
    }};
}