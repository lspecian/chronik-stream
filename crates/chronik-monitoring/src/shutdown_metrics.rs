//! Shutdown metrics for graceful shutdown monitoring

use prometheus::{Counter, Gauge, Histogram, HistogramOpts};

/// Shutdown metrics for monitoring graceful shutdown process
pub struct ShutdownMetrics {
    /// Total number of shutdowns initiated
    pub shutdown_initiated_total: Counter,

    /// Total number of successful leadership transfers
    pub leadership_transfers_successful: Counter,

    /// Total number of failed leadership transfers
    pub leadership_transfers_failed: Counter,

    /// Current shutdown state (0=Running, 1=DrainRequests, 2=TransferLeadership, 3=SyncWAL, 4=Shutdown)
    pub shutdown_state: Gauge,

    /// Number of in-flight requests during shutdown
    pub in_flight_requests: Gauge,

    /// Time taken for complete shutdown (seconds)
    pub shutdown_duration_seconds: Histogram,

    /// Time taken for leadership transfer per partition (seconds)
    pub leadership_transfer_duration_seconds: Histogram,

    /// Time taken to drain requests (seconds)
    pub drain_duration_seconds: Histogram,

    /// Time taken to sync WAL (seconds)
    pub wal_sync_duration_seconds: Histogram,
}

impl ShutdownMetrics {
    /// Create new shutdown metrics
    pub fn new() -> Self {
        let shutdown_duration_opts = HistogramOpts::new(
            "chronik_shutdown_duration_seconds",
            "Time taken for complete shutdown in seconds",
        )
        .buckets(vec![1.0, 5.0, 10.0, 15.0, 20.0, 30.0, 45.0, 60.0]);

        let transfer_duration_opts = HistogramOpts::new(
            "chronik_leadership_transfer_duration_seconds",
            "Time taken for leadership transfer per partition in seconds",
        )
        .buckets(vec![0.1, 0.5, 1.0, 2.0, 3.0, 5.0, 10.0]);

        let drain_duration_opts = HistogramOpts::new(
            "chronik_drain_duration_seconds",
            "Time taken to drain in-flight requests in seconds",
        )
        .buckets(vec![0.1, 0.5, 1.0, 2.0, 5.0, 10.0]);

        let wal_sync_duration_opts = HistogramOpts::new(
            "chronik_wal_sync_duration_seconds",
            "Time taken to sync WAL during shutdown in seconds",
        )
        .buckets(vec![0.01, 0.05, 0.1, 0.5, 1.0, 2.0, 5.0]);

        Self {
            shutdown_initiated_total: Counter::new(
                "chronik_shutdown_initiated_total",
                "Total number of shutdowns initiated",
            )
            .unwrap(),
            leadership_transfers_successful: Counter::new(
                "chronik_leadership_transfers_successful_total",
                "Total number of successful leadership transfers",
            )
            .unwrap(),
            leadership_transfers_failed: Counter::new(
                "chronik_leadership_transfers_failed_total",
                "Total number of failed leadership transfers",
            )
            .unwrap(),
            shutdown_state: Gauge::new(
                "chronik_shutdown_state",
                "Current shutdown state (0=Running, 1=DrainRequests, 2=TransferLeadership, 3=SyncWAL, 4=Shutdown)",
            )
            .unwrap(),
            in_flight_requests: Gauge::new(
                "chronik_shutdown_in_flight_requests",
                "Number of in-flight requests during shutdown",
            )
            .unwrap(),
            shutdown_duration_seconds: Histogram::with_opts(shutdown_duration_opts).unwrap(),
            leadership_transfer_duration_seconds: Histogram::with_opts(transfer_duration_opts).unwrap(),
            drain_duration_seconds: Histogram::with_opts(drain_duration_opts).unwrap(),
            wal_sync_duration_seconds: Histogram::with_opts(wal_sync_duration_opts).unwrap(),
        }
    }
}

impl Default for ShutdownMetrics {
    fn default() -> Self {
        Self::new()
    }
}
