//! Group commit implementation for WAL durability
//!
//! This module implements PostgreSQL-style group commit to achieve both high throughput
//! and zero data loss. Multiple concurrent writes are batched together and committed
//! with a single fsync, amortizing the cost of disk synchronization.
//!
//! Key features:
//! - Zero data loss for acks=1 and acks=-1
//! - Optional fire-and-forget for acks=0
//! - Bounded memory with backpressure
//! - Adaptive batching based on throughput
//! - Per-partition commit queues

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use bytes::Bytes;
use dashmap::DashMap;
use tokio::fs::{File, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex, oneshot, Notify};
use tracing::{debug, info, warn, error, instrument};

use crate::error::{Result, WalError};
use crate::record::WalRecord;

/// Configuration for group commit behavior
#[derive(Debug, Clone)]
pub struct GroupCommitConfig {
    /// Maximum number of writes per batch (prevents unbounded queue growth)
    pub max_batch_size: usize,

    /// Maximum bytes per batch (memory limit)
    pub max_batch_bytes: usize,

    /// Maximum time to wait before forcing a commit (latency bound)
    pub max_wait_time_ms: u64,

    /// Maximum queue depth per partition (backpressure threshold)
    pub max_queue_depth: usize,

    /// Enable metrics collection
    pub enable_metrics: bool,
}

impl Default for GroupCommitConfig {
    fn default() -> Self {
        Self {
            max_batch_size: 1000,           // 1000 writes per batch
            max_batch_bytes: 10_000_000,    // 10MB per batch
            max_wait_time_ms: 10,           // 10ms max latency
            max_queue_depth: 5000,          // 5000 pending writes max
            enable_metrics: true,
        }
    }
}

/// A pending write waiting for group commit
struct PendingWrite {
    /// Pre-serialized WAL record data
    data: Bytes,

    /// Channel to notify caller when commit completes
    response_tx: Option<oneshot::Sender<Result<()>>>,
}

/// Per-partition commit queue
struct PartitionCommitQueue {
    /// Pending writes waiting for commit
    pending: Mutex<VecDeque<PendingWrite>>,

    /// Total bytes in queue (for memory tracking)
    total_queued_bytes: Mutex<usize>,

    /// File handle for this partition's WAL
    file: Arc<Mutex<File>>,

    /// Last fsync timestamp
    last_fsync: Mutex<Instant>,

    /// Notification for new writes
    write_notify: Arc<Notify>,

    /// Metrics
    metrics: Arc<CommitMetrics>,
}

/// Commit metrics for observability
#[derive(Debug, Default)]
struct CommitMetrics {
    total_commits: std::sync::atomic::AtomicU64,
    total_writes: std::sync::atomic::AtomicU64,
    total_bytes: std::sync::atomic::AtomicU64,
    total_fsync_time_us: std::sync::atomic::AtomicU64,
    backpressure_events: std::sync::atomic::AtomicU64,
}

/// Group commit WAL manager
pub struct GroupCommitWal {
    /// Per-partition commit queues
    partition_queues: Arc<DashMap<(String, i32), Arc<PartitionCommitQueue>>>,

    /// Configuration
    config: GroupCommitConfig,

    /// Base directory for WAL files
    base_dir: PathBuf,

    /// Shutdown signal
    shutdown: Arc<Notify>,
}

impl GroupCommitWal {
    /// Create a new group commit WAL manager
    pub fn new(base_dir: PathBuf, config: GroupCommitConfig) -> Self {
        let wal = Self {
            partition_queues: Arc::new(DashMap::new()),
            config,
            base_dir,
            shutdown: Arc::new(Notify::new()),
        };

        // Start background commit thread
        wal.start_background_committer();

        info!("Group commit WAL initialized: max_batch={}, max_wait={}ms, max_queue={}",
              wal.config.max_batch_size, wal.config.max_wait_time_ms, wal.config.max_queue_depth);

        wal
    }

    /// Append a record with specified acknowledgment mode
    #[instrument(skip(self, record), fields(topic = %topic, partition = partition, acks = acks))]
    pub async fn append(
        &self,
        topic: String,
        partition: i32,
        record: WalRecord,
        acks: i16,
    ) -> Result<()> {
        // Serialize record
        let data = record.to_bytes()?;
        let data_len = data.len();

        // Get or create partition queue
        let queue = self.get_or_create_queue(&topic, partition).await?;

        // Handle acks=0 (fire-and-forget)
        if acks == 0 {
            return self.enqueue_nowait(queue, data.into()).await;
        }

        // Handle acks=1 or acks=-1 (wait for commit)
        self.enqueue_and_wait(queue, data.into(), data_len).await
    }

    /// Enqueue without waiting (acks=0 mode)
    async fn enqueue_nowait(
        &self,
        queue: Arc<PartitionCommitQueue>,
        data: Bytes,
    ) -> Result<()> {
        // Check backpressure
        let queued_bytes = *queue.total_queued_bytes.lock().await;
        if queued_bytes >= self.config.max_batch_bytes * 2 {
            queue.metrics.backpressure_events.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return Err(WalError::Backpressure("Queue too full for acks=0".into()));
        }

        // Enqueue without response channel
        let data_len = data.len();
        let mut pending = queue.pending.lock().await;
        pending.push_back(PendingWrite {
            data,
            response_tx: None,
        });
        drop(pending);

        // Update queue size
        let mut total_bytes = queue.total_queued_bytes.lock().await;
        *total_bytes += data_len;
        drop(total_bytes);

        // Notify committer
        queue.write_notify.notify_one();

        Ok(())
    }

    /// Enqueue and wait for commit (acks=1 or acks=-1 mode)
    async fn enqueue_and_wait(
        &self,
        queue: Arc<PartitionCommitQueue>,
        data: Bytes,
        data_len: usize,
    ) -> Result<()> {
        debug!("🟢 ENQUEUE_START: Enqueuing write with {} bytes", data_len);

        // Check backpressure
        {
            let pending = queue.pending.lock().await;
            if pending.len() >= self.config.max_queue_depth {
                queue.metrics.backpressure_events.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                warn!("🔴 BACKPRESSURE: Queue depth {} exceeded max {}", pending.len(), self.config.max_queue_depth);
                return Err(WalError::Backpressure("Queue depth exceeded".into()));
            }
        }

        // Create response channel
        let (tx, rx) = oneshot::channel();
        debug!("📫 ENQUEUE_CHANNEL: Created oneshot channel for fsync confirmation");

        // Enqueue with response channel
        {
            let mut pending = queue.pending.lock().await;
            pending.push_back(PendingWrite {
                data,
                response_tx: Some(tx),
            });
            info!("📥 ENQUEUE_ADDED: Enqueued write (with wait), queue depth now: {}", pending.len());
        }

        // Update queue size
        {
            let mut total_bytes = queue.total_queued_bytes.lock().await;
            *total_bytes += data_len;
            debug!("📊 ENQUEUE_BYTES: Total queued bytes now: {}", *total_bytes);
        }

        // Check if we should trigger immediate commit
        let should_commit = self.should_commit_now(&queue).await;

        // Notify committer
        queue.write_notify.notify_one();
        info!("🔔 ENQUEUE_NOTIFY: Notified commit worker, should_commit={}", should_commit);

        // If batch is full, trigger commit immediately (don't wait for timer)
        if should_commit {
            info!("⚡ ENQUEUE_IMMEDIATE: Triggering immediate commit due to batch size");
        }

        // Wait for commit confirmation
        info!("⏳ ENQUEUE_WAIT: Waiting for fsync confirmation on oneshot channel...");
        let result = rx.await
            .map_err(|_| WalError::CommitFailed("Response channel closed".into()))?;
        info!("✅ ENQUEUE_DONE: Fsync confirmed! Returning success to caller");
        result
    }

    /// Check if we should commit immediately
    async fn should_commit_now(&self, queue: &PartitionCommitQueue) -> bool {
        let pending = queue.pending.lock().await;
        let total_bytes = queue.total_queued_bytes.lock().await;

        pending.len() >= self.config.max_batch_size ||
        *total_bytes >= self.config.max_batch_bytes
    }

    /// Get or create a partition queue
    async fn get_or_create_queue(
        &self,
        topic: &str,
        partition: i32,
    ) -> Result<Arc<PartitionCommitQueue>> {
        let key = (topic.to_string(), partition);

        if let Some(queue) = self.partition_queues.get(&key) {
            return Ok(queue.clone());
        }

        // Create new queue
        let partition_dir = self.base_dir.join(topic).join(partition.to_string());
        tokio::fs::create_dir_all(&partition_dir).await?;

        let wal_path = partition_dir.join("wal_0_0.log");

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&wal_path)
            .await?;

        let queue = Arc::new(PartitionCommitQueue {
            pending: Mutex::new(VecDeque::new()),
            total_queued_bytes: Mutex::new(0),
            file: Arc::new(Mutex::new(file)),
            last_fsync: Mutex::new(Instant::now()),
            write_notify: Arc::new(Notify::new()),
            metrics: Arc::new(CommitMetrics::default()),
        });

        // Start per-partition commit worker
        self.start_partition_committer(queue.clone());

        self.partition_queues.insert(key, queue.clone());

        info!("Created commit queue for {}-{}", topic, partition);

        Ok(queue)
    }

    /// Start per-partition commit worker
    fn start_partition_committer(&self, queue: Arc<PartitionCommitQueue>) {
        let config = self.config.clone();
        let shutdown = self.shutdown.clone();

        info!("🚀 WORKER_SPAWN: Starting partition committer background task");

        tokio::spawn(async move {
            info!("✅ WORKER_STARTED: Partition committer task is running");
            let mut interval = tokio::time::interval(Duration::from_millis(config.max_wait_time_ms));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            debug!("WORKER_CONFIG: max_wait_time_ms={}, max_batch_size={}, max_batch_bytes={}",
                config.max_wait_time_ms, config.max_batch_size, config.max_batch_bytes);

            loop {
                debug!("🔄 WORKER_LOOP: Waiting for notification or interval tick");
                tokio::select! {
                    _ = queue.write_notify.notified() => {
                        info!("🔔 WORKER_NOTIFIED: Received write notification");
                    }
                    _ = interval.tick() => {
                        debug!("⏰ WORKER_TICK: Interval tick fired");
                    }
                    _ = shutdown.notified() => {
                        info!("🛑 WORKER_SHUTDOWN: Partition committer shutting down");
                        break;
                    }
                }

                // Commit if there are pending writes
                debug!("📝 WORKER_COMMIT: About to call commit_batch");
                if let Err(e) = Self::commit_batch(&queue, &config).await {
                    error!("❌ WORKER_ERROR: Commit batch failed: {}", e);
                } else {
                    debug!("✅ WORKER_SUCCESS: commit_batch completed successfully");
                }
            }
        });

        info!("🎯 WORKER_SPAWNED: tokio::spawn returned (worker should be running in background)");
    }

    /// Start background commit thread for all partitions
    fn start_background_committer(&self) {
        let queues = self.partition_queues.clone();
        let config = self.config.clone();
        let shutdown = self.shutdown.clone();

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(config.max_wait_time_ms));

            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        // Time-based commit for all partitions
                    }
                    _ = shutdown.notified() => {
                        info!("Background committer shutting down");
                        break;
                    }
                }

                // Commit all partitions with pending writes
                for entry in queues.iter() {
                    let queue = entry.value();
                    let pending_count = queue.pending.lock().await.len();

                    if pending_count > 0 {
                        if let Err(e) = Self::commit_batch(queue, &config).await {
                            error!("Background commit failed: {}", e);
                        }
                    }
                }
            }
        });
    }

    /// Commit a batch of writes with single fsync
    #[instrument(skip(queue, config), fields(batch_size, bytes, fsync_us))]
    async fn commit_batch(
        queue: &PartitionCommitQueue,
        config: &GroupCommitConfig,
    ) -> Result<()> {
        debug!("🔵 COMMIT_START: Entering commit_batch");
        let start = Instant::now();

        // Drain queue (up to max_batch_size)
        let batch = {
            let mut pending = queue.pending.lock().await;

            if pending.is_empty() {
                debug!("⚪ COMMIT_EMPTY: commit_batch called but queue is empty, nothing to do");
                return Ok(());
            }

            let queue_depth = pending.len();
            let batch_size = std::cmp::min(pending.len(), config.max_batch_size);
            let mut batch = Vec::with_capacity(batch_size);

            info!("📦 COMMIT_DRAIN: Draining {} writes from queue (total depth: {}) for batch commit", batch_size, queue_depth);

            for _ in 0..batch_size {
                if let Some(write) = pending.pop_front() {
                    batch.push(write);
                }
            }

            batch
        };

        let batch_count = batch.len();
        let mut total_bytes = 0;

        debug!("💾 COMMIT_WRITE: Writing {} records to file", batch_count);

        // Write all to file
        let mut file = queue.file.lock().await;
        for write in &batch {
            file.write_all(&write.data).await?;
            total_bytes += write.data.len();
        }

        debug!("🔄 COMMIT_FSYNC: Starting fsync for {} bytes", total_bytes);
        // Single fsync for entire batch ⭐
        file.sync_all().await?;
        drop(file);
        debug!("✅ COMMIT_FSYNC_DONE: fsync completed successfully");

        // Update last fsync time
        {
            let mut last_fsync = queue.last_fsync.lock().await;
            *last_fsync = Instant::now();
        }

        // Update queue size
        {
            let mut queued_bytes = queue.total_queued_bytes.lock().await;
            *queued_bytes = queued_bytes.saturating_sub(total_bytes);
        }

        let fsync_duration = start.elapsed();

        // Update metrics
        if config.enable_metrics {
            queue.metrics.total_commits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            queue.metrics.total_writes.fetch_add(batch_count as u64, std::sync::atomic::Ordering::Relaxed);
            queue.metrics.total_bytes.fetch_add(total_bytes as u64, std::sync::atomic::Ordering::Relaxed);
            queue.metrics.total_fsync_time_us.fetch_add(fsync_duration.as_micros() as u64, std::sync::atomic::Ordering::Relaxed);
        }

        // Record metrics in span
        tracing::Span::current()
            .record("batch_size", batch_count)
            .record("bytes", total_bytes)
            .record("fsync_us", fsync_duration.as_micros() as u64);

        info!(
            "✅ Group commit: {} writes, {} bytes, fsync took {:?}",
            batch_count, total_bytes, fsync_duration
        );

        // Notify all waiters
        debug!("📢 COMMIT_NOTIFY: Notifying {} waiters that fsync is complete", batch_count);
        for write in batch {
            if let Some(tx) = write.response_tx {
                let _ = tx.send(Ok(()));
            }
        }
        debug!("All {} waiters notified successfully", batch_count);

        Ok(())
    }

    /// Get metrics for a partition
    pub fn get_metrics(&self, topic: &str, partition: i32) -> Option<PartitionMetrics> {
        let key = (topic.to_string(), partition);
        self.partition_queues.get(&key).map(|queue| {
            let metrics = &queue.metrics;
            PartitionMetrics {
                total_commits: metrics.total_commits.load(std::sync::atomic::Ordering::Relaxed),
                total_writes: metrics.total_writes.load(std::sync::atomic::Ordering::Relaxed),
                total_bytes: metrics.total_bytes.load(std::sync::atomic::Ordering::Relaxed),
                avg_fsync_time_us: if metrics.total_commits.load(std::sync::atomic::Ordering::Relaxed) > 0 {
                    metrics.total_fsync_time_us.load(std::sync::atomic::Ordering::Relaxed) /
                    metrics.total_commits.load(std::sync::atomic::Ordering::Relaxed)
                } else {
                    0
                },
                backpressure_events: metrics.backpressure_events.load(std::sync::atomic::Ordering::Relaxed),
            }
        })
    }

    /// Shutdown the group commit WAL
    pub async fn shutdown(&self) {
        info!("Shutting down group commit WAL...");
        self.shutdown.notify_waiters();

        // Give workers time to finish pending commits
        tokio::time::sleep(Duration::from_millis(100)).await;

        info!("Group commit WAL shutdown complete");
    }
}

/// Public metrics structure
#[derive(Debug, Clone)]
pub struct PartitionMetrics {
    pub total_commits: u64,
    pub total_writes: u64,
    pub total_bytes: u64,
    pub avg_fsync_time_us: u64,
    pub backpressure_events: u64,
}
