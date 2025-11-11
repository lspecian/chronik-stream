//! Unified lock-free atomic metrics for all Chronik components
//!
//! This module provides a single, comprehensive metrics system using atomic counters
//! that are deadlock-free and high-performance. It replaces the previous Prometheus-based
//! system which suffered from gather() deadlocks.
//!
//! All metrics are exposed in Prometheus text format for compatibility with Prometheus scrapers.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::OnceLock;

/// Global unified metrics instance
pub fn global_metrics() -> &'static UnifiedMetrics {
    static METRICS: OnceLock<UnifiedMetrics> = OnceLock::new();
    METRICS.get_or_init(UnifiedMetrics::new)
}

/// Unified metrics collector for all Chronik components
#[derive(Debug)]
pub struct UnifiedMetrics {
    // ========== Controller Metrics ==========
    pub controller_elections: AtomicU64,
    pub controller_election_failures: AtomicU64,
    pub controller_is_leader: AtomicU64, // 1 if leader, 0 if follower

    // ========== Metadata Operations ==========
    pub metadata_topic_creates: AtomicU64,
    pub metadata_topic_gets: AtomicU64,
    pub metadata_topic_lists: AtomicU64,
    pub metadata_topic_deletes: AtomicU64,
    pub metadata_consumer_group_creates: AtomicU64,
    pub metadata_consumer_group_gets: AtomicU64,
    pub metadata_offset_commits: AtomicU64,
    pub metadata_offset_gets: AtomicU64,

    // ========== Phase 3: Metadata Performance & Lease Metrics ==========
    pub metadata_write_latency_sum_ms: AtomicU64,      // Sum for histogram
    pub metadata_write_count: AtomicU64,                // Count for average
    pub metadata_read_latency_sum_ms: AtomicU64,       // Sum for histogram
    pub metadata_read_count: AtomicU64,                 // Count for average
    pub metadata_topic_creation_throughput: AtomicU64,  // Topics/sec (updated periodically)
    pub metadata_lease_expirations: AtomicU64,          // Lease expiration events
    pub metadata_replication_lag_ms: AtomicU64,         // Current lag (gauge)

    // ========== WalMetadataMetrics Migration ==========
    // Segment operations (from WalMetadataMetrics)
    pub metadata_segment_persists: AtomicU64,
    pub metadata_segment_gets: AtomicU64,
    pub metadata_segment_lists: AtomicU64,
    // Cache metrics (from WalMetadataMetrics)
    pub metadata_cache_hits: AtomicU64,
    pub metadata_cache_misses: AtomicU64,
    pub metadata_cache_size: AtomicUsize,
    pub metadata_cache_evictions: AtomicU64,
    // Error metrics (from WalMetadataMetrics - note: total_errors already exists globally)
    pub metadata_not_found_errors: AtomicU64,
    // State metrics (from WalMetadataMetrics - note: total_topics/consumer_groups already exist)
    pub metadata_total_segments: AtomicUsize,
    pub metadata_memory_usage_bytes: AtomicUsize,

    // ========== Ingest/Produce Metrics ==========
    pub messages_received_total: AtomicU64,
    pub messages_stored_total: AtomicU64,
    pub produce_requests_total: AtomicU64,
    pub produce_errors_total: AtomicU64,
    pub bytes_produced_total: AtomicU64,

    // ========== Fetch/Consume Metrics ==========
    pub fetch_requests_total: AtomicU64,
    pub fetch_errors_total: AtomicU64,
    pub bytes_fetched_total: AtomicU64,

    // ========== WAL Metrics ==========
    pub wal_writes_total: AtomicU64,
    pub wal_write_errors: AtomicU64,
    pub wal_bytes_written: AtomicU64,
    pub wal_fsync_total: AtomicU64,
    pub wal_segment_rotations: AtomicU64,
    pub wal_recovery_events: AtomicU64,
    pub wal_recovery_time_ms: AtomicU64,
    pub wal_batch_size_sum: AtomicU64,      // Sum of all batch sizes for histogram
    pub wal_batch_count: AtomicU64,         // Number of batches for average calculation

    // ========== ProduceHandler Flush Metrics (v2.1.0) ==========
    pub produce_flush_total: AtomicU64,           // Total number of flushes
    pub produce_batches_flushed_sum: AtomicU64,   // Sum of batches per flush
    pub produce_profile_active: AtomicU64,        // Current profile: 0=Low, 1=Balanced, 2=High, 3=Extreme

    // ========== Raft Metrics ==========
    pub raft_leader_count: AtomicU64,
    pub raft_follower_count: AtomicU64,
    pub raft_election_count: AtomicU64,
    pub raft_election_failures: AtomicU64,
    pub raft_append_entries_sent: AtomicU64,
    pub raft_append_entries_received: AtomicU64,
    pub raft_heartbeat_timeouts: AtomicU64,
    pub raft_commit_index: AtomicU64,
    pub raft_last_applied: AtomicU64,

    // ========== Search/Query Metrics ==========
    pub search_queries_total: AtomicU64,
    pub search_query_errors: AtomicU64,
    pub aggregation_windows_active: AtomicU64,

    // ========== Storage/Janitor Metrics ==========
    pub segments_cleaned_total: AtomicU64,
    pub compaction_runs_total: AtomicU64,
    pub compaction_failures: AtomicU64,
    pub storage_usage_bytes: AtomicU64,

    // ========== Error Metrics ==========
    pub total_errors: AtomicU64,
    pub serialization_errors: AtomicU64,
    pub storage_errors: AtomicU64,
    pub network_errors: AtomicU64,

    // ========== Performance Metrics ==========
    pub total_operations: AtomicU64,
    pub avg_operation_time_ns: AtomicU64,
    pub max_operation_time_ns: AtomicU64,

    // ========== State Metrics ==========
    pub total_topics: AtomicUsize,
    pub total_partitions: AtomicUsize,
    pub total_consumer_groups: AtomicUsize,
    pub active_connections: AtomicUsize,
}

impl Default for UnifiedMetrics {
    fn default() -> Self {
        Self::new()
    }
}

impl UnifiedMetrics {
    /// Create a new unified metrics collector
    pub fn new() -> Self {
        Self {
            // Controller
            controller_elections: AtomicU64::new(0),
            controller_election_failures: AtomicU64::new(0),
            controller_is_leader: AtomicU64::new(0),

            // Metadata
            metadata_topic_creates: AtomicU64::new(0),
            metadata_topic_gets: AtomicU64::new(0),
            metadata_topic_lists: AtomicU64::new(0),
            metadata_topic_deletes: AtomicU64::new(0),
            metadata_consumer_group_creates: AtomicU64::new(0),
            metadata_consumer_group_gets: AtomicU64::new(0),
            metadata_offset_commits: AtomicU64::new(0),
            metadata_offset_gets: AtomicU64::new(0),

            // Phase 3: Metadata Performance & Lease Metrics
            metadata_write_latency_sum_ms: AtomicU64::new(0),
            metadata_write_count: AtomicU64::new(0),
            metadata_read_latency_sum_ms: AtomicU64::new(0),
            metadata_read_count: AtomicU64::new(0),
            metadata_topic_creation_throughput: AtomicU64::new(0),
            metadata_lease_expirations: AtomicU64::new(0),
            metadata_replication_lag_ms: AtomicU64::new(0),

            // WalMetadataMetrics Migration
            metadata_segment_persists: AtomicU64::new(0),
            metadata_segment_gets: AtomicU64::new(0),
            metadata_segment_lists: AtomicU64::new(0),
            metadata_cache_hits: AtomicU64::new(0),
            metadata_cache_misses: AtomicU64::new(0),
            metadata_cache_size: AtomicUsize::new(0),
            metadata_cache_evictions: AtomicU64::new(0),
            metadata_not_found_errors: AtomicU64::new(0),
            metadata_total_segments: AtomicUsize::new(0),
            metadata_memory_usage_bytes: AtomicUsize::new(0),

            // Ingest/Produce
            messages_received_total: AtomicU64::new(0),
            messages_stored_total: AtomicU64::new(0),
            produce_requests_total: AtomicU64::new(0),
            produce_errors_total: AtomicU64::new(0),
            bytes_produced_total: AtomicU64::new(0),

            // Fetch/Consume
            fetch_requests_total: AtomicU64::new(0),
            fetch_errors_total: AtomicU64::new(0),
            bytes_fetched_total: AtomicU64::new(0),

            // WAL
            wal_writes_total: AtomicU64::new(0),
            wal_write_errors: AtomicU64::new(0),
            wal_bytes_written: AtomicU64::new(0),
            wal_fsync_total: AtomicU64::new(0),
            wal_segment_rotations: AtomicU64::new(0),
            wal_recovery_events: AtomicU64::new(0),
            wal_recovery_time_ms: AtomicU64::new(0),
            wal_batch_size_sum: AtomicU64::new(0),
            wal_batch_count: AtomicU64::new(0),

            // ProduceHandler Flush
            produce_flush_total: AtomicU64::new(0),
            produce_batches_flushed_sum: AtomicU64::new(0),
            produce_profile_active: AtomicU64::new(2), // Default: HighThroughput (v2.1.0)

            // Raft
            raft_leader_count: AtomicU64::new(0),
            raft_follower_count: AtomicU64::new(0),
            raft_election_count: AtomicU64::new(0),
            raft_election_failures: AtomicU64::new(0),
            raft_append_entries_sent: AtomicU64::new(0),
            raft_append_entries_received: AtomicU64::new(0),
            raft_heartbeat_timeouts: AtomicU64::new(0),
            raft_commit_index: AtomicU64::new(0),
            raft_last_applied: AtomicU64::new(0),

            // Search/Query
            search_queries_total: AtomicU64::new(0),
            search_query_errors: AtomicU64::new(0),
            aggregation_windows_active: AtomicU64::new(0),

            // Storage/Janitor
            segments_cleaned_total: AtomicU64::new(0),
            compaction_runs_total: AtomicU64::new(0),
            compaction_failures: AtomicU64::new(0),
            storage_usage_bytes: AtomicU64::new(0),

            // Errors
            total_errors: AtomicU64::new(0),
            serialization_errors: AtomicU64::new(0),
            storage_errors: AtomicU64::new(0),
            network_errors: AtomicU64::new(0),

            // Performance
            total_operations: AtomicU64::new(0),
            avg_operation_time_ns: AtomicU64::new(0),
            max_operation_time_ns: AtomicU64::new(0),

            // State
            total_topics: AtomicUsize::new(0),
            total_partitions: AtomicUsize::new(0),
            total_consumer_groups: AtomicUsize::new(0),
            active_connections: AtomicUsize::new(0),
        }
    }

    /// Format all metrics in Prometheus text format
    ///
    /// This is the main export function that generates Prometheus-compatible output
    /// from all atomic counters. This is lock-free and deadlock-proof.
    pub fn format_prometheus(&self) -> String {
        let mut output = String::with_capacity(16384); // Pre-allocate reasonable size

        // ========== Controller Metrics ==========
        output.push_str("# HELP chronik_controller_elections_total Total number of controller elections\n");
        output.push_str("# TYPE chronik_controller_elections_total counter\n");
        output.push_str(&format!("chronik_controller_elections_total{{result=\"success\"}} {}\n",
            self.controller_elections.load(Ordering::Relaxed)));
        output.push_str(&format!("chronik_controller_elections_total{{result=\"failure\"}} {}\n",
            self.controller_election_failures.load(Ordering::Relaxed)));

        output.push_str("# HELP chronik_controller_state Current controller state (1=leader, 0=follower)\n");
        output.push_str("# TYPE chronik_controller_state gauge\n");
        output.push_str(&format!("chronik_controller_state {}\n",
            self.controller_is_leader.load(Ordering::Relaxed)));

        // ========== Metadata Operations ==========
        output.push_str("# HELP chronik_metadata_operations_total Total metadata operations\n");
        output.push_str("# TYPE chronik_metadata_operations_total counter\n");
        output.push_str(&format!("chronik_metadata_operations_total{{operation=\"topic_create\"}} {}\n",
            self.metadata_topic_creates.load(Ordering::Relaxed)));
        output.push_str(&format!("chronik_metadata_operations_total{{operation=\"topic_get\"}} {}\n",
            self.metadata_topic_gets.load(Ordering::Relaxed)));
        output.push_str(&format!("chronik_metadata_operations_total{{operation=\"topic_list\"}} {}\n",
            self.metadata_topic_lists.load(Ordering::Relaxed)));
        output.push_str(&format!("chronik_metadata_operations_total{{operation=\"topic_delete\"}} {}\n",
            self.metadata_topic_deletes.load(Ordering::Relaxed)));
        output.push_str(&format!("chronik_metadata_operations_total{{operation=\"consumer_group_create\"}} {}\n",
            self.metadata_consumer_group_creates.load(Ordering::Relaxed)));
        output.push_str(&format!("chronik_metadata_operations_total{{operation=\"consumer_group_get\"}} {}\n",
            self.metadata_consumer_group_gets.load(Ordering::Relaxed)));
        output.push_str(&format!("chronik_metadata_operations_total{{operation=\"offset_commit\"}} {}\n",
            self.metadata_offset_commits.load(Ordering::Relaxed)));
        output.push_str(&format!("chronik_metadata_operations_total{{operation=\"offset_get\"}} {}\n",
            self.metadata_offset_gets.load(Ordering::Relaxed)));

        // ========== Phase 3: Metadata Performance & Lease Metrics ==========
        let write_count = self.metadata_write_count.load(Ordering::Relaxed);
        let write_sum = self.metadata_write_latency_sum_ms.load(Ordering::Relaxed);
        let write_avg = if write_count > 0 { write_sum / write_count } else { 0 };

        output.push_str("# HELP chronik_metadata_write_latency_ms Metadata write latency in milliseconds\n");
        output.push_str("# TYPE chronik_metadata_write_latency_ms gauge\n");
        output.push_str(&format!("chronik_metadata_write_latency_ms{{type=\"average\"}} {}\n", write_avg));

        let read_count = self.metadata_read_count.load(Ordering::Relaxed);
        let read_sum = self.metadata_read_latency_sum_ms.load(Ordering::Relaxed);
        let read_avg = if read_count > 0 { read_sum / read_count } else { 0 };

        output.push_str("# HELP chronik_metadata_read_latency_ms Metadata read latency in milliseconds\n");
        output.push_str("# TYPE chronik_metadata_read_latency_ms gauge\n");
        output.push_str(&format!("chronik_metadata_read_latency_ms{{type=\"average\"}} {}\n", read_avg));

        output.push_str("# HELP chronik_metadata_topic_creation_throughput Topics created per second\n");
        output.push_str("# TYPE chronik_metadata_topic_creation_throughput gauge\n");
        output.push_str(&format!("chronik_metadata_topic_creation_throughput {}\n",
            self.metadata_topic_creation_throughput.load(Ordering::Relaxed)));

        output.push_str("# HELP chronik_metadata_lease_expirations_total Lease expiration events\n");
        output.push_str("# TYPE chronik_metadata_lease_expirations_total counter\n");
        output.push_str(&format!("chronik_metadata_lease_expirations_total {}\n",
            self.metadata_lease_expirations.load(Ordering::Relaxed)));

        output.push_str("# HELP chronik_metadata_replication_lag_ms Metadata replication lag in milliseconds\n");
        output.push_str("# TYPE chronik_metadata_replication_lag_ms gauge\n");
        output.push_str(&format!("chronik_metadata_replication_lag_ms {}\n",
            self.metadata_replication_lag_ms.load(Ordering::Relaxed)));

        // ========== WalMetadataMetrics Migration: Segment Operations ==========
        output.push_str("# HELP chronik_metadata_segment_operations_total Metadata segment operations\n");
        output.push_str("# TYPE chronik_metadata_segment_operations_total counter\n");
        output.push_str(&format!("chronik_metadata_segment_operations_total{{operation=\"persist\"}} {}\n",
            self.metadata_segment_persists.load(Ordering::Relaxed)));
        output.push_str(&format!("chronik_metadata_segment_operations_total{{operation=\"get\"}} {}\n",
            self.metadata_segment_gets.load(Ordering::Relaxed)));
        output.push_str(&format!("chronik_metadata_segment_operations_total{{operation=\"list\"}} {}\n",
            self.metadata_segment_lists.load(Ordering::Relaxed)));

        // ========== WalMetadataMetrics Migration: Cache Operations ==========
        output.push_str("# HELP chronik_metadata_cache_operations_total Metadata cache operations\n");
        output.push_str("# TYPE chronik_metadata_cache_operations_total counter\n");
        output.push_str(&format!("chronik_metadata_cache_operations_total{{operation=\"hit\"}} {}\n",
            self.metadata_cache_hits.load(Ordering::Relaxed)));
        output.push_str(&format!("chronik_metadata_cache_operations_total{{operation=\"miss\"}} {}\n",
            self.metadata_cache_misses.load(Ordering::Relaxed)));
        output.push_str(&format!("chronik_metadata_cache_operations_total{{operation=\"eviction\"}} {}\n",
            self.metadata_cache_evictions.load(Ordering::Relaxed)));

        let cache_hits = self.metadata_cache_hits.load(Ordering::Relaxed);
        let cache_misses = self.metadata_cache_misses.load(Ordering::Relaxed);
        let cache_total = cache_hits + cache_misses;
        let cache_hit_rate = if cache_total > 0 {
            cache_hits as f64 / cache_total as f64
        } else {
            0.0
        };

        output.push_str("# HELP chronik_metadata_cache_hit_rate Metadata cache hit rate (0.0 to 1.0)\n");
        output.push_str("# TYPE chronik_metadata_cache_hit_rate gauge\n");
        output.push_str(&format!("chronik_metadata_cache_hit_rate {:.4}\n", cache_hit_rate));

        output.push_str("# HELP chronik_metadata_cache_size Metadata cache size\n");
        output.push_str("# TYPE chronik_metadata_cache_size gauge\n");
        output.push_str(&format!("chronik_metadata_cache_size {}\n",
            self.metadata_cache_size.load(Ordering::Relaxed)));

        // ========== WalMetadataMetrics Migration: Error Metrics ==========
        output.push_str("# HELP chronik_metadata_not_found_errors_total Metadata not-found errors\n");
        output.push_str("# TYPE chronik_metadata_not_found_errors_total counter\n");
        output.push_str(&format!("chronik_metadata_not_found_errors_total {}\n",
            self.metadata_not_found_errors.load(Ordering::Relaxed)));

        // ========== WalMetadataMetrics Migration: State Metrics ==========
        output.push_str("# HELP chronik_metadata_total_segments Total metadata segments\n");
        output.push_str("# TYPE chronik_metadata_total_segments gauge\n");
        output.push_str(&format!("chronik_metadata_total_segments {}\n",
            self.metadata_total_segments.load(Ordering::Relaxed)));

        output.push_str("# HELP chronik_metadata_memory_usage_bytes Metadata memory usage in bytes\n");
        output.push_str("# TYPE chronik_metadata_memory_usage_bytes gauge\n");
        output.push_str(&format!("chronik_metadata_memory_usage_bytes {}\n",
            self.metadata_memory_usage_bytes.load(Ordering::Relaxed)));

        // ========== Ingest/Produce Metrics ==========
        output.push_str("# HELP chronik_messages_received_total Total messages received\n");
        output.push_str("# TYPE chronik_messages_received_total counter\n");
        output.push_str(&format!("chronik_messages_received_total {}\n",
            self.messages_received_total.load(Ordering::Relaxed)));

        output.push_str("# HELP chronik_messages_stored_total Total messages stored\n");
        output.push_str("# TYPE chronik_messages_stored_total counter\n");
        output.push_str(&format!("chronik_messages_stored_total {}\n",
            self.messages_stored_total.load(Ordering::Relaxed)));

        output.push_str("# HELP chronik_produce_requests_total Total produce requests\n");
        output.push_str("# TYPE chronik_produce_requests_total counter\n");
        output.push_str(&format!("chronik_produce_requests_total{{result=\"success\"}} {}\n",
            self.produce_requests_total.load(Ordering::Relaxed)));
        output.push_str(&format!("chronik_produce_requests_total{{result=\"error\"}} {}\n",
            self.produce_errors_total.load(Ordering::Relaxed)));

        output.push_str("# HELP chronik_bytes_produced_total Total bytes produced\n");
        output.push_str("# TYPE chronik_bytes_produced_total counter\n");
        output.push_str(&format!("chronik_bytes_produced_total {}\n",
            self.bytes_produced_total.load(Ordering::Relaxed)));

        // ========== Fetch/Consume Metrics ==========
        output.push_str("# HELP chronik_fetch_requests_total Total fetch requests\n");
        output.push_str("# TYPE chronik_fetch_requests_total counter\n");
        output.push_str(&format!("chronik_fetch_requests_total{{result=\"success\"}} {}\n",
            self.fetch_requests_total.load(Ordering::Relaxed)));
        output.push_str(&format!("chronik_fetch_requests_total{{result=\"error\"}} {}\n",
            self.fetch_errors_total.load(Ordering::Relaxed)));

        output.push_str("# HELP chronik_bytes_fetched_total Total bytes fetched\n");
        output.push_str("# TYPE chronik_bytes_fetched_total counter\n");
        output.push_str(&format!("chronik_bytes_fetched_total {}\n",
            self.bytes_fetched_total.load(Ordering::Relaxed)));

        // ========== WAL Metrics ==========
        output.push_str("# HELP chronik_wal_writes_total Total WAL write operations\n");
        output.push_str("# TYPE chronik_wal_writes_total counter\n");
        output.push_str(&format!("chronik_wal_writes_total{{result=\"success\"}} {}\n",
            self.wal_writes_total.load(Ordering::Relaxed)));
        output.push_str(&format!("chronik_wal_writes_total{{result=\"error\"}} {}\n",
            self.wal_write_errors.load(Ordering::Relaxed)));

        output.push_str("# HELP chronik_wal_bytes_written_total Total bytes written to WAL\n");
        output.push_str("# TYPE chronik_wal_bytes_written_total counter\n");
        output.push_str(&format!("chronik_wal_bytes_written_total {}\n",
            self.wal_bytes_written.load(Ordering::Relaxed)));

        output.push_str("# HELP chronik_wal_fsync_total Total WAL fsync operations\n");
        output.push_str("# TYPE chronik_wal_fsync_total counter\n");
        output.push_str(&format!("chronik_wal_fsync_total {}\n",
            self.wal_fsync_total.load(Ordering::Relaxed)));

        output.push_str("# HELP chronik_wal_segment_rotations_total Total WAL segment rotations\n");
        output.push_str("# TYPE chronik_wal_segment_rotations_total counter\n");
        output.push_str(&format!("chronik_wal_segment_rotations_total {}\n",
            self.wal_segment_rotations.load(Ordering::Relaxed)));

        output.push_str("# HELP chronik_wal_recovery_events_total Total events recovered during WAL recovery\n");
        output.push_str("# TYPE chronik_wal_recovery_events_total counter\n");
        output.push_str(&format!("chronik_wal_recovery_events_total {}\n",
            self.wal_recovery_events.load(Ordering::Relaxed)));

        output.push_str("# HELP chronik_wal_recovery_time_ms WAL recovery time in milliseconds\n");
        output.push_str("# TYPE chronik_wal_recovery_time_ms gauge\n");
        output.push_str(&format!("chronik_wal_recovery_time_ms {}\n",
            self.wal_recovery_time_ms.load(Ordering::Relaxed)));

        // ========== WAL Batch Size Histogram ==========
        let wal_batch_count = self.wal_batch_count.load(Ordering::Relaxed);
        let avg_batch_size = if wal_batch_count > 0 {
            self.wal_batch_size_sum.load(Ordering::Relaxed) / wal_batch_count
        } else {
            0
        };
        output.push_str("# HELP chronik_wal_batch_size_avg Average WAL batch size (messages per fsync)\n");
        output.push_str("# TYPE chronik_wal_batch_size_avg gauge\n");
        output.push_str(&format!("chronik_wal_batch_size_avg {}\n", avg_batch_size));

        output.push_str("# HELP chronik_wal_batch_count_total Total number of WAL batches processed\n");
        output.push_str("# TYPE chronik_wal_batch_count_total counter\n");
        output.push_str(&format!("chronik_wal_batch_count_total {}\n", wal_batch_count));

        // ========== ProduceHandler Flush Metrics ==========
        output.push_str("# HELP chronik_produce_flush_total Total number of produce flushes\n");
        output.push_str("# TYPE chronik_produce_flush_total counter\n");
        output.push_str(&format!("chronik_produce_flush_total {}\n",
            self.produce_flush_total.load(Ordering::Relaxed)));

        let produce_flush_count = self.produce_flush_total.load(Ordering::Relaxed);
        let avg_batches_per_flush = if produce_flush_count > 0 {
            self.produce_batches_flushed_sum.load(Ordering::Relaxed) / produce_flush_count
        } else {
            0
        };
        output.push_str("# HELP chronik_produce_batches_per_flush_avg Average batches per flush\n");
        output.push_str("# TYPE chronik_produce_batches_per_flush_avg gauge\n");
        output.push_str(&format!("chronik_produce_batches_per_flush_avg {}\n", avg_batches_per_flush));

        let profile = self.produce_profile_active.load(Ordering::Relaxed);
        output.push_str("# HELP chronik_produce_profile_active Current active ProduceFlushProfile (0=Low, 1=Balanced, 2=High, 3=Extreme)\n");
        output.push_str("# TYPE chronik_produce_profile_active gauge\n");
        output.push_str(&format!("chronik_produce_profile_active {}\n", profile));

        // ========== Raft Metrics ==========
        output.push_str("# HELP chronik_raft_leader_count Number of partitions where this node is leader\n");
        output.push_str("# TYPE chronik_raft_leader_count gauge\n");
        output.push_str(&format!("chronik_raft_leader_count {}\n",
            self.raft_leader_count.load(Ordering::Relaxed)));

        output.push_str("# HELP chronik_raft_follower_count Number of partitions where this node is follower\n");
        output.push_str("# TYPE chronik_raft_follower_count gauge\n");
        output.push_str(&format!("chronik_raft_follower_count {}\n",
            self.raft_follower_count.load(Ordering::Relaxed)));

        output.push_str("# HELP chronik_raft_election_count_total Total Raft elections\n");
        output.push_str("# TYPE chronik_raft_election_count_total counter\n");
        output.push_str(&format!("chronik_raft_election_count_total{{result=\"success\"}} {}\n",
            self.raft_election_count.load(Ordering::Relaxed)));
        output.push_str(&format!("chronik_raft_election_count_total{{result=\"failure\"}} {}\n",
            self.raft_election_failures.load(Ordering::Relaxed)));

        output.push_str("# HELP chronik_raft_append_entries_total Total Raft AppendEntries messages\n");
        output.push_str("# TYPE chronik_raft_append_entries_total counter\n");
        output.push_str(&format!("chronik_raft_append_entries_total{{direction=\"sent\"}} {}\n",
            self.raft_append_entries_sent.load(Ordering::Relaxed)));
        output.push_str(&format!("chronik_raft_append_entries_total{{direction=\"received\"}} {}\n",
            self.raft_append_entries_received.load(Ordering::Relaxed)));

        output.push_str("# HELP chronik_raft_heartbeat_timeouts_total Total Raft heartbeat timeouts\n");
        output.push_str("# TYPE chronik_raft_heartbeat_timeouts_total counter\n");
        output.push_str(&format!("chronik_raft_heartbeat_timeouts_total {}\n",
            self.raft_heartbeat_timeouts.load(Ordering::Relaxed)));

        output.push_str("# HELP chronik_raft_commit_index Current Raft commit index\n");
        output.push_str("# TYPE chronik_raft_commit_index gauge\n");
        output.push_str(&format!("chronik_raft_commit_index {}\n",
            self.raft_commit_index.load(Ordering::Relaxed)));

        output.push_str("# HELP chronik_raft_last_applied Current Raft last applied index\n");
        output.push_str("# TYPE chronik_raft_last_applied gauge\n");
        output.push_str(&format!("chronik_raft_last_applied {}\n",
            self.raft_last_applied.load(Ordering::Relaxed)));

        // ========== Search/Query Metrics ==========
        output.push_str("# HELP chronik_search_queries_total Total search queries\n");
        output.push_str("# TYPE chronik_search_queries_total counter\n");
        output.push_str(&format!("chronik_search_queries_total{{result=\"success\"}} {}\n",
            self.search_queries_total.load(Ordering::Relaxed)));
        output.push_str(&format!("chronik_search_queries_total{{result=\"error\"}} {}\n",
            self.search_query_errors.load(Ordering::Relaxed)));

        output.push_str("# HELP chronik_aggregation_windows_active Active aggregation windows\n");
        output.push_str("# TYPE chronik_aggregation_windows_active gauge\n");
        output.push_str(&format!("chronik_aggregation_windows_active {}\n",
            self.aggregation_windows_active.load(Ordering::Relaxed)));

        // ========== Storage/Janitor Metrics ==========
        output.push_str("# HELP chronik_segments_cleaned_total Total segments cleaned\n");
        output.push_str("# TYPE chronik_segments_cleaned_total counter\n");
        output.push_str(&format!("chronik_segments_cleaned_total {}\n",
            self.segments_cleaned_total.load(Ordering::Relaxed)));

        output.push_str("# HELP chronik_compaction_runs_total Total compaction runs\n");
        output.push_str("# TYPE chronik_compaction_runs_total counter\n");
        output.push_str(&format!("chronik_compaction_runs_total{{result=\"success\"}} {}\n",
            self.compaction_runs_total.load(Ordering::Relaxed)));
        output.push_str(&format!("chronik_compaction_runs_total{{result=\"failure\"}} {}\n",
            self.compaction_failures.load(Ordering::Relaxed)));

        output.push_str("# HELP chronik_storage_usage_bytes Storage usage in bytes\n");
        output.push_str("# TYPE chronik_storage_usage_bytes gauge\n");
        output.push_str(&format!("chronik_storage_usage_bytes {}\n",
            self.storage_usage_bytes.load(Ordering::Relaxed)));

        // ========== Error Metrics ==========
        output.push_str("# HELP chronik_errors_total Total errors by type\n");
        output.push_str("# TYPE chronik_errors_total counter\n");
        output.push_str(&format!("chronik_errors_total{{type=\"total\"}} {}\n",
            self.total_errors.load(Ordering::Relaxed)));
        output.push_str(&format!("chronik_errors_total{{type=\"serialization\"}} {}\n",
            self.serialization_errors.load(Ordering::Relaxed)));
        output.push_str(&format!("chronik_errors_total{{type=\"storage\"}} {}\n",
            self.storage_errors.load(Ordering::Relaxed)));
        output.push_str(&format!("chronik_errors_total{{type=\"network\"}} {}\n",
            self.network_errors.load(Ordering::Relaxed)));

        // ========== Performance Metrics ==========
        output.push_str("# HELP chronik_operations_total Total operations performed\n");
        output.push_str("# TYPE chronik_operations_total counter\n");
        output.push_str(&format!("chronik_operations_total {}\n",
            self.total_operations.load(Ordering::Relaxed)));

        output.push_str("# HELP chronik_operation_duration_ns Operation duration in nanoseconds\n");
        output.push_str("# TYPE chronik_operation_duration_ns gauge\n");
        output.push_str(&format!("chronik_operation_duration_ns{{type=\"average\"}} {}\n",
            self.avg_operation_time_ns.load(Ordering::Relaxed)));
        output.push_str(&format!("chronik_operation_duration_ns{{type=\"max\"}} {}\n",
            self.max_operation_time_ns.load(Ordering::Relaxed)));

        // ========== State Metrics ==========
        output.push_str("# HELP chronik_state_count Current count of entities\n");
        output.push_str("# TYPE chronik_state_count gauge\n");
        output.push_str(&format!("chronik_state_count{{type=\"topics\"}} {}\n",
            self.total_topics.load(Ordering::Relaxed)));
        output.push_str(&format!("chronik_state_count{{type=\"partitions\"}} {}\n",
            self.total_partitions.load(Ordering::Relaxed)));
        output.push_str(&format!("chronik_state_count{{type=\"consumer_groups\"}} {}\n",
            self.total_consumer_groups.load(Ordering::Relaxed)));
        output.push_str(&format!("chronik_state_count{{type=\"active_connections\"}} {}\n",
            self.active_connections.load(Ordering::Relaxed)));

        output
    }
}

/// Helper struct for easy metric recording from other modules
pub struct MetricsRecorder;

impl MetricsRecorder {
    /// Record a controller election
    pub fn record_election(success: bool) {
        let metrics = global_metrics();
        if success {
            metrics.controller_elections.fetch_add(1, Ordering::Relaxed);
        } else {
            metrics.controller_election_failures.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Set controller state
    pub fn set_controller_state(is_leader: bool) {
        global_metrics().controller_is_leader.store(if is_leader { 1 } else { 0 }, Ordering::Relaxed);
    }

    /// Record a produce request
    pub fn record_produce(success: bool, bytes: u64, message_count: u64) {
        let metrics = global_metrics();
        if success {
            metrics.produce_requests_total.fetch_add(1, Ordering::Relaxed);
            metrics.messages_received_total.fetch_add(message_count, Ordering::Relaxed);
            metrics.messages_stored_total.fetch_add(message_count, Ordering::Relaxed);
            metrics.bytes_produced_total.fetch_add(bytes, Ordering::Relaxed);
        } else {
            metrics.produce_errors_total.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Record a fetch request
    pub fn record_fetch(success: bool, bytes: u64) {
        let metrics = global_metrics();
        if success {
            metrics.fetch_requests_total.fetch_add(1, Ordering::Relaxed);
            metrics.bytes_fetched_total.fetch_add(bytes, Ordering::Relaxed);
        } else {
            metrics.fetch_errors_total.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Record a WAL write
    pub fn record_wal_write(success: bool, bytes: u64) {
        let metrics = global_metrics();
        if success {
            metrics.wal_writes_total.fetch_add(1, Ordering::Relaxed);
            metrics.wal_bytes_written.fetch_add(bytes, Ordering::Relaxed);
        } else {
            metrics.wal_write_errors.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Record a WAL fsync
    pub fn record_wal_fsync() {
        global_metrics().wal_fsync_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a WAL segment rotation
    pub fn record_wal_rotation() {
        global_metrics().wal_segment_rotations.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a Raft election
    pub fn record_raft_election(success: bool) {
        let metrics = global_metrics();
        if success {
            metrics.raft_election_count.fetch_add(1, Ordering::Relaxed);
        } else {
            metrics.raft_election_failures.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Update Raft leader/follower counts
    pub fn set_raft_counts(leaders: u64, followers: u64) {
        let metrics = global_metrics();
        metrics.raft_leader_count.store(leaders, Ordering::Relaxed);
        metrics.raft_follower_count.store(followers, Ordering::Relaxed);
    }

    /// Record a search query
    pub fn record_search_query(success: bool) {
        let metrics = global_metrics();
        if success {
            metrics.search_queries_total.fetch_add(1, Ordering::Relaxed);
        } else {
            metrics.search_query_errors.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Update state counts
    pub fn update_state_counts(topics: usize, partitions: usize, groups: usize, connections: usize) {
        let metrics = global_metrics();
        metrics.total_topics.store(topics, Ordering::Relaxed);
        metrics.total_partitions.store(partitions, Ordering::Relaxed);
        metrics.total_consumer_groups.store(groups, Ordering::Relaxed);
        metrics.active_connections.store(connections, Ordering::Relaxed);
    }

    /// Record a ProduceHandler flush
    /// Call this each time ProduceHandler flushes batches to the WAL
    pub fn record_produce_flush(batches_count: u64) {
        let metrics = global_metrics();
        metrics.produce_flush_total.fetch_add(1, Ordering::Relaxed);
        metrics.produce_batches_flushed_sum.fetch_add(batches_count, Ordering::Relaxed);
    }

    /// Set the active ProduceFlushProfile
    /// Call this once during startup or profile change
    /// profile_id: 0=LowLatency, 1=Balanced, 2=HighThroughput, 3=Extreme
    pub fn set_produce_profile(profile_id: u64) {
        global_metrics().produce_profile_active.store(profile_id, Ordering::Relaxed);
    }

    /// Record a WAL batch commit
    /// Call this each time GroupCommitWal commits a batch
    pub fn record_wal_batch(batch_size: u64) {
        let metrics = global_metrics();
        metrics.wal_batch_size_sum.fetch_add(batch_size, Ordering::Relaxed);
        metrics.wal_batch_count.fetch_add(1, Ordering::Relaxed);
    }

    // ========== Phase 3: Metadata Performance & Lease Metrics ==========

    /// Record metadata write latency (Phase 2 WAL writes)
    pub fn record_metadata_write_latency(duration: std::time::Duration) {
        let ms = duration.as_millis() as u64;
        let metrics = global_metrics();
        metrics.metadata_write_latency_sum_ms.fetch_add(ms, Ordering::Relaxed);
        metrics.metadata_write_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Record metadata read latency (Phase 1 forwarding or Phase 3 local read)
    pub fn record_metadata_read_latency(duration: std::time::Duration) {
        let ms = duration.as_millis() as u64;
        let metrics = global_metrics();
        metrics.metadata_read_latency_sum_ms.fetch_add(ms, Ordering::Relaxed);
        metrics.metadata_read_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Record lease expiration event
    pub fn record_lease_expiration() {
        let metrics = global_metrics();
        metrics.metadata_lease_expirations.fetch_add(1, Ordering::Relaxed);
    }

    /// Update topic creation throughput (called periodically, e.g., every second)
    pub fn update_topic_creation_throughput(topics_per_sec: u64) {
        let metrics = global_metrics();
        metrics.metadata_topic_creation_throughput.store(topics_per_sec, Ordering::Relaxed);
    }

    /// Update metadata replication lag (called by followers)
    pub fn update_metadata_replication_lag(lag_ms: u64) {
        let metrics = global_metrics();
        metrics.metadata_replication_lag_ms.store(lag_ms, Ordering::Relaxed);
    }

    // ========== WalMetadataMetrics Migration ==========

    /// Record segment operation
    pub fn record_segment_operation(operation: &str) {
        let metrics = global_metrics();
        match operation {
            "persist" => metrics.metadata_segment_persists.fetch_add(1, Ordering::Relaxed),
            "get" => metrics.metadata_segment_gets.fetch_add(1, Ordering::Relaxed),
            "list" => metrics.metadata_segment_lists.fetch_add(1, Ordering::Relaxed),
            _ => return,
        };
    }

    /// Record cache operation
    pub fn record_cache_operation(operation: &str) {
        let metrics = global_metrics();
        match operation {
            "hit" => metrics.metadata_cache_hits.fetch_add(1, Ordering::Relaxed),
            "miss" => metrics.metadata_cache_misses.fetch_add(1, Ordering::Relaxed),
            "eviction" => metrics.metadata_cache_evictions.fetch_add(1, Ordering::Relaxed),
            _ => return,
        };
    }

    /// Update cache size
    pub fn update_cache_size(size: usize) {
        let metrics = global_metrics();
        metrics.metadata_cache_size.store(size, Ordering::Relaxed);
    }

    /// Record not-found error
    pub fn record_not_found_error() {
        let metrics = global_metrics();
        metrics.metadata_not_found_errors.fetch_add(1, Ordering::Relaxed);
    }

    /// Update metadata state counts
    pub fn update_metadata_state(segments: usize, memory_bytes: usize) {
        let metrics = global_metrics();
        metrics.metadata_total_segments.store(segments, Ordering::Relaxed);
        metrics.metadata_memory_usage_bytes.store(memory_bytes, Ordering::Relaxed);
    }
}
