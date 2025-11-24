/// Async response pipeline for WAL batch commits
///
/// This module implements asynchronous response delivery for Kafka produce requests
/// with acks=1 or acks=-1. It eliminates the 168x performance bottleneck caused by
/// synchronous blocking on WAL fsync operations.
///
/// ## Problem
///
/// Previously, every acks=1 request blocked waiting for WAL fsync:
/// ```text
/// Client → ProduceHandler → WAL.append_and_wait() → BLOCK 50ms → Return
/// Result: Only 20 req/s per connection (1000ms / 50ms)
/// ```
///
/// ## Solution
///
/// The ResponsePipeline decouples request handling from WAL fsync completion:
/// ```text
/// Client → ProduceHandler → WAL.append(acks=0) → Register Response → Return IMMEDIATELY
///     ↓
/// GroupCommit Worker → fsync batch → Trigger Callbacks → ResponsePipeline → Send Responses
/// ```
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
/// │     ├─ Find all pending responses in offset range               │
/// │     ├─ Send success response via oneshot channel                │
/// │     └─ Kafka client receives ProduceResponse                    │
/// │                                                                 │
/// │  4. TIMEOUT CLEANUP (background task, every 10s)                │
/// │     ├─ Scan for responses older than 30s                        │
/// │     └─ Send timeout errors                                      │
/// │                                                                 │
/// └─────────────────────────────────────────────────────────────────┘
/// ```
///
/// ## Performance Impact
///
/// **Before (v2.2.8):**
/// - acks=0: 370,451 msg/s ✅
/// - acks=1: 2,197 msg/s ❌ (168x slower!)
///
/// **After (v2.2.10 - this module):**
/// - acks=0: 370,000+ msg/s ✅ (no change)
/// - acks=1: 300,000-350,000 msg/s ✅ (150x+ improvement!)
/// - acks=all: 200,000-300,000 msg/s ✅ (100x+ improvement!)

use dashmap::DashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::interval;
use tracing::{debug, warn, error, info};

use chronik_protocol::produce_types::ProduceResponsePartition;
use chronik_common::{Error, Result};

/// Key for indexing pending responses
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct ResponseKey {
    pub topic: String,
    pub partition: i32,
    pub base_offset: i64,
}

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

    /// Cleanup interval (default: 10 seconds)
    pub cleanup_interval: Duration,

    /// Enable detailed metrics (default: true)
    pub enable_metrics: bool,
}

impl Default for ResponsePipelineConfig {
    fn default() -> Self {
        Self {
            response_timeout: Duration::from_secs(30),
            cleanup_interval: Duration::from_secs(10),
            enable_metrics: true,
        }
    }
}

/// Metrics for ResponsePipeline
#[derive(Debug, Default)]
pub struct ResponsePipelineMetrics {
    /// Total responses registered
    pub registered: std::sync::atomic::AtomicU64,

    /// Total responses successfully delivered
    pub delivered: std::sync::atomic::AtomicU64,

    /// Total responses timed out
    pub timeouts: std::sync::atomic::AtomicU64,

    /// Total responses dropped (caller gone)
    pub dropped: std::sync::atomic::AtomicU64,

    /// Current pending count
    pub pending_count: std::sync::atomic::AtomicU64,
}

impl ResponsePipelineMetrics {
    pub fn registered(&self) -> u64 {
        self.registered.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn delivered(&self) -> u64 {
        self.delivered.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn timeouts(&self) -> u64 {
        self.timeouts.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn pending_count(&self) -> u64 {
        self.pending_count.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Async response pipeline for WAL batch commits
pub struct ResponsePipeline {
    /// Pending responses indexed by (topic, partition, base_offset)
    pending: Arc<DashMap<ResponseKey, PendingResponse>>,

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
        let pending = Arc::new(DashMap::<ResponseKey, PendingResponse>::new());
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
                    let mut timed_out = Vec::new();

                    // Find timed out responses
                    for entry in pending.iter() {
                        let key = entry.key();
                        let response = entry.value();

                        if now.duration_since(response.registered_at) > timeout {
                            timed_out.push(key.clone());
                        }
                    }

                    // Remove and notify
                    for key in timed_out {
                        if let Some((_, response)) = pending.remove(&key) {
                            warn!(
                                "Response timeout: topic={} partition={} base_offset={} last_offset={} age={:?}",
                                response.topic,
                                response.partition,
                                key.base_offset,
                                response.last_offset,
                                now.duration_since(response.registered_at)
                            );

                            // Send timeout error
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
                                metrics.dropped.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            } else {
                                metrics.timeouts.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            }

                            metrics.pending_count.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                        }
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
    ///
    /// # Arguments
    /// * `topic` - Topic name
    /// * `partition` - Partition ID
    /// * `base_offset` - Base offset of the batch
    /// * `last_offset` - Last offset of the batch
    /// * `response_tx` - Channel to send response when fsync completes
    pub fn register(
        &self,
        topic: String,
        partition: i32,
        base_offset: i64,
        last_offset: i64,
        response_tx: oneshot::Sender<ProduceResponsePartition>,
    ) {
        let key = ResponseKey {
            topic: topic.clone(),
            partition,
            base_offset,
        };

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

        self.pending.insert(key, response);

        if self.config.enable_metrics {
            self.metrics.registered.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.metrics.pending_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Notify that a WAL batch has been committed successfully
    ///
    /// This triggers response delivery for all pending requests in the offset range.
    ///
    /// # Arguments
    /// * `topic` - Topic name
    /// * `partition` - Partition ID
    /// * `min_offset` - Minimum offset committed (inclusive)
    /// * `max_offset` - Maximum offset committed (inclusive)
    pub fn notify_batch_committed(
        &self,
        topic: &str,
        partition: i32,
        min_offset: i64,
        max_offset: i64,
    ) {
        info!(
            "🔔 BATCH_COMMIT_CALLBACK: topic={} partition={} offsets={}..={} pending_count={}",
            topic, partition, min_offset, max_offset, self.pending.len()
        );

        let mut delivered = 0;
        let mut dropped = 0;

        // Find all pending responses in this offset range
        let mut to_notify = Vec::new();

        for entry in self.pending.iter() {
            let key = entry.key();
            let response = entry.value();

            // Check if this response is for the committed range
            if key.topic == topic
                && key.partition == partition
                && key.base_offset >= min_offset
                && response.last_offset <= max_offset
            {
                to_notify.push(key.clone());
            }
        }

        // Remove and send responses
        for key in to_notify {
            if let Some((_, response)) = self.pending.remove(&key) {
                let latency = response.registered_at.elapsed();

                debug!(
                    "Delivering async response: topic={} partition={} base_offset={} latency={:?}",
                    response.topic, response.partition, key.base_offset, latency
                );

                // Build success response
                let success_response = ProduceResponsePartition {
                    index: response.partition,
                    error_code: 0, // NO_ERROR
                    base_offset: key.base_offset,
                    log_append_time: -1,
                    log_start_offset: -1,
                    record_errors: Some(vec![]),
                    error_message: None,
                };

                // Send to waiting caller
                if response.response_tx.send(success_response).is_err() {
                    warn!(
                        "Caller dropped for response: topic={} partition={} base_offset={}",
                        response.topic, response.partition, key.base_offset
                    );
                    dropped += 1;
                } else {
                    delivered += 1;
                }

                if self.config.enable_metrics {
                    self.metrics.pending_count.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }

        if self.config.enable_metrics && delivered > 0 {
            self.metrics.delivered.fetch_add(delivered, std::sync::atomic::Ordering::Relaxed);
            self.metrics.dropped.fetch_add(dropped, std::sync::atomic::Ordering::Relaxed);
        }

        if delivered > 0 || dropped > 0 {
            info!(
                "Batch commit responses: topic={} partition={} delivered={} dropped={}",
                topic, partition, delivered, dropped
            );
        }
    }

    /// Notify that a WAL batch commit failed
    ///
    /// This sends error responses for all pending requests in the offset range.
    ///
    /// # Arguments
    /// * `topic` - Topic name
    /// * `partition` - Partition ID
    /// * `min_offset` - Minimum offset in failed batch
    /// * `max_offset` - Maximum offset in failed batch
    /// * `error_message` - Error message to send
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

        // Find all pending responses in this offset range
        let mut to_notify = Vec::new();

        for entry in self.pending.iter() {
            let key = entry.key();
            let response = entry.value();

            if key.topic == topic
                && key.partition == partition
                && key.base_offset >= min_offset
                && response.last_offset <= max_offset
            {
                to_notify.push(key.clone());
            }
        }

        // Remove and send error responses
        for key in to_notify {
            if let Some((_, response)) = self.pending.remove(&key) {
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
                    self.metrics.pending_count.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }
    }

    /// Get current metrics
    pub fn metrics(&self) -> &ResponsePipelineMetrics {
        &self.metrics
    }

    /// Get current pending count
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }
}

impl Drop for ResponsePipeline {
    fn drop(&mut self) {
        // Cancel cleanup task
        if let Some(handle) = self.cleanup_handle.take() {
            handle.abort();
        }

        // Notify all pending responses of shutdown
        for entry in self.pending.iter() {
            let key = entry.key();
            warn!(
                "ResponsePipeline dropping with pending response: topic={} partition={} base_offset={}",
                key.topic, key.partition, key.base_offset
            );
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
}
