/// Async response pipeline for WAL batch commits
///
/// This module implements asynchronous response delivery for Kafka produce requests
/// with acks=1 or acks=-1. It eliminates the 168x performance bottleneck caused by
/// synchronous blocking on WAL fsync operations.
///
/// ## Architecture
///
/// ```text
/// ┌─────────────────────────────────────────────────────────────────┐
/// │                    ResponsePipeline Flow                        │
/// ├─────────────────────────────────────────────────────────────────┤
/// │                                                                 │
/// │  1. PRODUCE REQUEST (acks=1)                                    │
/// │     ├─ WAL.append(acks=0) - no blocking!                        │
/// │     ├─ ResponsePipeline.register(topic, partition, offset, tx)  │
/// │     └─ Return IMMEDIATELY to request handler                    │
/// │                                                                 │
/// │  2. GROUP COMMIT (background worker, every 50ms)                │
/// │     ├─ fsync batch (topic1-p0: offsets 100-199)                 │
/// │     ├─ Trigger callback: notify_batch_committed()               │
/// │     └─ ResponsePipeline receives notification                   │
/// │                                                                 │
/// │  3. RESPONSE DELIVERY                                           │
/// │     ├─ O(1) bucket lookup by (topic, partition)                 │
/// │     ├─ O(log n) range scan in BTreeMap for offset range         │
/// │     ├─ Send success response via oneshot channel                │
/// │     └─ Kafka client receives ProduceResponse                    │
/// │                                                                 │
/// │  4. TIMEOUT CLEANUP (background task, every 2s)                 │
/// │     ├─ Scan buckets for responses older than 30s                │
/// │     └─ Send timeout errors                                      │
/// │                                                                 │
/// └─────────────────────────────────────────────────────────────────┘
/// ```
///
/// ## v2.4.1 Performance Fix
///
/// Storage changed from flat `DashMap<ResponseKey, PendingResponse>` to
/// partition-bucketed `DashMap<(topic, partition), BTreeMap<offset, response>>`.
/// This changes `notify_batch_committed()` from O(n) over ALL pending entries
/// to O(1) bucket lookup + O(log n + k) range scan within one partition.
/// Under bulk load (280+ req/sec), this prevents cascading timeouts.

use dashmap::DashMap;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::interval;
use tracing::{debug, warn, error, info};

use chronik_protocol::produce_types::ProduceResponsePartition;

/// A pending response waiting for WAL fsync confirmation
#[derive(Debug)]
pub struct PendingResponse {
    /// Channel to send response back to caller
    pub response_tx: oneshot::Sender<ProduceResponsePartition>,

    /// When this response was registered
    pub registered_at: Instant,

    /// Last offset in this batch (for range checks)
    pub last_offset: i64,

    /// Topic name (for logging)
    pub topic: String,

    /// Partition ID (for logging)
    pub partition: i32,
}

/// Configuration for ResponsePipeline
#[derive(Debug, Clone)]
pub struct ResponsePipelineConfig {
    /// Timeout for pending responses (default: 30 seconds)
    pub response_timeout: Duration,

    /// Cleanup interval (default: 2 seconds)
    pub cleanup_interval: Duration,

    /// Enable detailed metrics (default: true)
    pub enable_metrics: bool,
}

impl Default for ResponsePipelineConfig {
    fn default() -> Self {
        Self {
            response_timeout: Duration::from_secs(30),
            cleanup_interval: Duration::from_secs(2),
            enable_metrics: true,
        }
    }
}

/// Metrics for ResponsePipeline
#[derive(Debug, Default)]
pub struct ResponsePipelineMetrics {
    /// Total responses registered
    pub registered: AtomicU64,

    /// Total responses successfully delivered
    pub delivered: AtomicU64,

    /// Total responses timed out
    pub timeouts: AtomicU64,

    /// Total responses dropped (caller gone)
    pub dropped: AtomicU64,

    /// Current pending count
    pub pending_count: AtomicU64,
}

impl ResponsePipelineMetrics {
    pub fn registered(&self) -> u64 {
        self.registered.load(Ordering::Relaxed)
    }

    pub fn delivered(&self) -> u64 {
        self.delivered.load(Ordering::Relaxed)
    }

    pub fn timeouts(&self) -> u64 {
        self.timeouts.load(Ordering::Relaxed)
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    pub fn pending_count(&self) -> u64 {
        self.pending_count.load(Ordering::Relaxed)
    }
}

/// Partition bucket key: (topic, partition)
type BucketKey = (String, i32);

/// Async response pipeline for WAL batch commits.
///
/// Uses partition-bucketed storage for O(log n) lookups instead of O(n) scans.
pub struct ResponsePipeline {
    /// Pending responses bucketed by (topic, partition), sorted by base_offset
    pending: Arc<DashMap<BucketKey, BTreeMap<i64, PendingResponse>>>,

    /// Configuration
    config: ResponsePipelineConfig,

    /// Metrics
    metrics: Arc<ResponsePipelineMetrics>,

    /// Background cleanup task handle
    cleanup_handle: Option<JoinHandle<()>>,
}

impl ResponsePipeline {
    /// Create a new ResponsePipeline with default configuration
    pub fn new() -> Self {
        Self::with_config(ResponsePipelineConfig::default())
    }

    /// Create a new ResponsePipeline with custom configuration
    pub fn with_config(config: ResponsePipelineConfig) -> Self {
        let pending = Arc::new(DashMap::<BucketKey, BTreeMap<i64, PendingResponse>>::new());
        let metrics = Arc::new(ResponsePipelineMetrics::default());

        // Spawn background cleanup task
        let cleanup_handle = {
            let pending = pending.clone();
            let metrics = metrics.clone();
            let timeout = config.response_timeout;
            let cleanup_interval_duration = config.cleanup_interval;

            Some(tokio::spawn(async move {
                let mut interval = interval(cleanup_interval_duration);

                loop {
                    interval.tick().await;

                    let now = Instant::now();
                    let mut timed_out_count = 0u64;
                    let mut dropped_count = 0u64;

                    // Iterate over each partition bucket
                    let bucket_keys: Vec<BucketKey> = pending.iter()
                        .map(|entry| entry.key().clone())
                        .collect();

                    for bucket_key in bucket_keys {
                        let mut stale_offsets = Vec::new();

                        // Find stale entries in this bucket
                        if let Some(bucket) = pending.get(&bucket_key) {
                            for (offset, response) in bucket.iter() {
                                if now.duration_since(response.registered_at) > timeout {
                                    stale_offsets.push(*offset);
                                }
                            }
                        }

                        if stale_offsets.is_empty() {
                            continue;
                        }

                        // Remove and notify stale entries
                        if let Some(mut bucket) = pending.get_mut(&bucket_key) {
                            for offset in stale_offsets {
                                if let Some(response) = bucket.remove(&offset) {
                                    warn!(
                                        "Response timeout: topic={} partition={} base_offset={} last_offset={} age={:?}",
                                        response.topic, response.partition, offset,
                                        response.last_offset, now.duration_since(response.registered_at)
                                    );

                                    let error_response = ProduceResponsePartition {
                                        index: response.partition,
                                        error_code: 7, // REQUEST_TIMED_OUT
                                        base_offset: -1,
                                        log_append_time: -1,
                                        log_start_offset: -1,
                                        record_errors: Some(vec![]),
                                        error_message: Some("WAL fsync timeout".to_string()),
                                    };

                                    if response.response_tx.send(error_response).is_err() {
                                        dropped_count += 1;
                                    } else {
                                        timed_out_count += 1;
                                    }

                                    metrics.pending_count.fetch_sub(1, Ordering::Relaxed);
                                }
                            }

                            // Remove empty bucket
                            if bucket.is_empty() {
                                drop(bucket);
                                pending.remove(&bucket_key);
                            }
                        }
                    }

                    if timed_out_count > 0 || dropped_count > 0 {
                        metrics.timeouts.fetch_add(timed_out_count, Ordering::Relaxed);
                        metrics.dropped.fetch_add(dropped_count, Ordering::Relaxed);
                    }
                }
            }))
        };

        Self {
            pending,
            config,
            metrics,
            cleanup_handle,
        }
    }

    /// Register a pending response for a produce request
    pub fn register(
        &self,
        topic: String,
        partition: i32,
        base_offset: i64,
        last_offset: i64,
        response_tx: oneshot::Sender<ProduceResponsePartition>,
    ) {
        let response = PendingResponse {
            response_tx,
            registered_at: Instant::now(),
            last_offset,
            topic: topic.clone(),
            partition,
        };

        debug!(
            "Registering pending response: topic={} partition={} base_offset={} last_offset={}",
            topic, partition, base_offset, last_offset
        );

        let bucket_key = (topic, partition);
        self.pending
            .entry(bucket_key)
            .or_insert_with(BTreeMap::new)
            .insert(base_offset, response);

        if self.config.enable_metrics {
            self.metrics.registered.fetch_add(1, Ordering::Relaxed);
            self.metrics.pending_count.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Notify that a WAL batch has been committed successfully.
    ///
    /// Uses O(1) bucket lookup + O(log n) BTreeMap range scan instead of
    /// O(n) scan over all pending entries.
    pub fn notify_batch_committed(
        &self,
        topic: &str,
        partition: i32,
        min_offset: i64,
        max_offset: i64,
    ) {
        let bucket_key = (topic.to_string(), partition);

        let mut bucket = match self.pending.get_mut(&bucket_key) {
            Some(b) => b,
            None => {
                debug!(
                    "BATCH_COMMIT_CALLBACK: no pending responses for topic={} partition={}",
                    topic, partition
                );
                return;
            }
        };

        let mut delivered = 0u64;
        let mut dropped = 0u64;

        // Collect offsets in the committed range.
        // BTreeMap range gives us O(log n + k) instead of O(n).
        let offsets_in_range: Vec<i64> = bucket
            .range(min_offset..=max_offset)
            .filter(|(_, resp)| resp.last_offset <= max_offset)
            .map(|(offset, _)| *offset)
            .collect();

        for offset in offsets_in_range {
            if let Some(response) = bucket.remove(&offset) {
                let latency = response.registered_at.elapsed();

                debug!(
                    "Delivering async response: topic={} partition={} base_offset={} latency={:?}",
                    response.topic, response.partition, offset, latency
                );

                let success_response = ProduceResponsePartition {
                    index: response.partition,
                    error_code: 0, // NO_ERROR
                    base_offset: offset,
                    log_append_time: -1,
                    log_start_offset: -1,
                    record_errors: Some(vec![]),
                    error_message: None,
                };

                if response.response_tx.send(success_response).is_err() {
                    warn!(
                        "Caller dropped for response: topic={} partition={} base_offset={}",
                        topic, partition, offset
                    );
                    dropped += 1;
                } else {
                    delivered += 1;
                }

                if self.config.enable_metrics {
                    self.metrics.pending_count.fetch_sub(1, Ordering::Relaxed);
                }
            }
        }

        // Clean up empty bucket
        if bucket.is_empty() {
            drop(bucket);
            self.pending.remove(&bucket_key);
        }

        if self.config.enable_metrics && (delivered > 0 || dropped > 0) {
            self.metrics.delivered.fetch_add(delivered, Ordering::Relaxed);
            self.metrics.dropped.fetch_add(dropped, Ordering::Relaxed);
        }

        if delivered > 0 || dropped > 0 {
            debug!(
                "Batch commit responses: topic={} partition={} delivered={} dropped={}",
                topic, partition, delivered, dropped
            );
        }
    }

    /// Notify that a WAL batch commit failed.
    ///
    /// Uses O(1) bucket lookup + O(log n) range scan.
    pub fn notify_batch_failed(
        &self,
        topic: &str,
        partition: i32,
        min_offset: i64,
        max_offset: i64,
        error_message: &str,
    ) {
        error!(
            "WAL batch commit FAILED: topic={} partition={} offsets={}..={} error={}",
            topic, partition, min_offset, max_offset, error_message
        );

        let bucket_key = (topic.to_string(), partition);

        let mut bucket = match self.pending.get_mut(&bucket_key) {
            Some(b) => b,
            None => return,
        };

        let offsets_in_range: Vec<i64> = bucket
            .range(min_offset..=max_offset)
            .filter(|(_, resp)| resp.last_offset <= max_offset)
            .map(|(offset, _)| *offset)
            .collect();

        for offset in offsets_in_range {
            if let Some(response) = bucket.remove(&offset) {
                let error_response = ProduceResponsePartition {
                    index: response.partition,
                    error_code: 1, // UNKNOWN_SERVER_ERROR
                    base_offset: -1,
                    log_append_time: -1,
                    log_start_offset: -1,
                    record_errors: Some(vec![]),
                    error_message: Some(error_message.to_string()),
                };

                let _ = response.response_tx.send(error_response);

                if self.config.enable_metrics {
                    self.metrics.pending_count.fetch_sub(1, Ordering::Relaxed);
                }
            }
        }

        if bucket.is_empty() {
            drop(bucket);
            self.pending.remove(&bucket_key);
        }
    }

    /// Get current metrics
    pub fn metrics(&self) -> &ResponsePipelineMetrics {
        &self.metrics
    }

    /// Get current pending count
    pub fn pending_count(&self) -> usize {
        self.pending.iter().map(|entry| entry.value().len()).sum()
    }
}

impl Drop for ResponsePipeline {
    fn drop(&mut self) {
        if let Some(handle) = self.cleanup_handle.take() {
            handle.abort();
        }

        for entry in self.pending.iter() {
            let (topic, partition) = entry.key();
            let count = entry.value().len();
            if count > 0 {
                warn!(
                    "ResponsePipeline dropping with {} pending responses: topic={} partition={}",
                    count, topic, partition
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_response_pipeline_basic() {
        let pipeline = ResponsePipeline::new();

        let (tx, rx) = oneshot::channel();

        // Register response
        pipeline.register("test-topic".to_string(), 0, 100, 199, tx);

        assert_eq!(pipeline.pending_count(), 1);

        // Notify batch committed
        pipeline.notify_batch_committed("test-topic", 0, 100, 199);

        // Should receive response
        let response = rx.await.unwrap();
        assert_eq!(response.error_code, 0);
        assert_eq!(response.base_offset, 100);

        assert_eq!(pipeline.pending_count(), 0);
    }

    #[tokio::test]
    async fn test_response_pipeline_timeout() {
        let config = ResponsePipelineConfig {
            response_timeout: Duration::from_millis(100),
            cleanup_interval: Duration::from_millis(50),
            enable_metrics: true,
        };

        let pipeline = ResponsePipeline::with_config(config);

        let (tx, rx) = oneshot::channel();

        // Register response
        pipeline.register("test-topic".to_string(), 0, 100, 199, tx);

        // Wait for timeout
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Should receive timeout error
        let response = rx.await.unwrap();
        assert_eq!(response.error_code, 7); // REQUEST_TIMED_OUT

        assert_eq!(pipeline.pending_count(), 0);
        assert!(pipeline.metrics().timeouts() > 0);
    }

    #[tokio::test]
    async fn test_response_pipeline_batch_failed() {
        let pipeline = ResponsePipeline::new();

        let (tx, rx) = oneshot::channel();

        // Register response
        pipeline.register("test-topic".to_string(), 0, 100, 199, tx);

        // Notify batch failed
        pipeline.notify_batch_failed("test-topic", 0, 100, 199, "Disk error");

        // Should receive error response
        let response = rx.await.unwrap();
        assert_eq!(response.error_code, 1); // UNKNOWN_SERVER_ERROR
        assert_eq!(response.error_message, Some("Disk error".to_string()));

        assert_eq!(pipeline.pending_count(), 0);
    }

    #[tokio::test]
    async fn test_response_pipeline_multi_partition() {
        let pipeline = ResponsePipeline::new();

        // Register responses across 24 partitions
        let mut receivers = Vec::new();
        for partition in 0..24 {
            for batch in 0..10 {
                let (tx, rx) = oneshot::channel();
                let base_offset = (batch * 100) as i64;
                let last_offset = base_offset + 99;
                pipeline.register("topic".to_string(), partition, base_offset, last_offset, tx);
                receivers.push((partition, base_offset, rx));
            }
        }

        assert_eq!(pipeline.pending_count(), 240);

        // Commit all batches for each partition
        for partition in 0..24 {
            pipeline.notify_batch_committed("topic", partition, 0, 999);
        }

        // All should be delivered
        for (partition, base_offset, rx) in receivers {
            let response = rx.await.unwrap();
            assert_eq!(response.error_code, 0, "Failed for partition={} offset={}", partition, base_offset);
            assert_eq!(response.base_offset, base_offset);
        }

        assert_eq!(pipeline.pending_count(), 0);
    }

    #[tokio::test]
    async fn test_response_pipeline_partial_commit() {
        let pipeline = ResponsePipeline::new();

        let (tx1, rx1) = oneshot::channel();
        let (tx2, rx2) = oneshot::channel();
        let (tx3, _rx3) = oneshot::channel();

        // Register 3 batches: offsets 0-99, 100-199, 200-299
        pipeline.register("topic".to_string(), 0, 0, 99, tx1);
        pipeline.register("topic".to_string(), 0, 100, 199, tx2);
        pipeline.register("topic".to_string(), 0, 200, 299, tx3);

        assert_eq!(pipeline.pending_count(), 3);

        // Only commit first two batches
        pipeline.notify_batch_committed("topic", 0, 0, 199);

        let r1 = rx1.await.unwrap();
        assert_eq!(r1.error_code, 0);
        assert_eq!(r1.base_offset, 0);

        let r2 = rx2.await.unwrap();
        assert_eq!(r2.error_code, 0);
        assert_eq!(r2.base_offset, 100);

        // Third batch still pending
        assert_eq!(pipeline.pending_count(), 1);
    }

    #[tokio::test]
    async fn test_no_pending_for_topic() {
        let pipeline = ResponsePipeline::new();

        // Should not panic when notifying for non-existent topic
        pipeline.notify_batch_committed("nonexistent", 0, 0, 100);
        pipeline.notify_batch_failed("nonexistent", 0, 0, 100, "error");

        assert_eq!(pipeline.pending_count(), 0);
    }
}
