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
        Self::auto_tune()
    }
}

impl GroupCommitConfig {
    /// Auto-tune configuration based on system resources
    ///
    /// Detects CPU and memory constraints, including:
    /// - K8s/Docker CPU limits (cgroup v1 and v2)
    /// - Available memory
    /// - Environment variable overrides
    pub fn auto_tune() -> Self {
        // Check for manual override via environment
        if let Ok(profile) = std::env::var("CHRONIK_WAL_PROFILE") {
            return match profile.to_lowercase().as_str() {
                "low" | "small" | "container" => Self::low_resource(),
                "medium" | "balanced" => Self::medium_resource(),
                "high" | "aggressive" | "dedicated" => Self::high_resource(),
                "ultra" | "maximum" | "throughput" => Self::ultra_resource(),
                _ => Self::detect_resources(),
            };
        }

        Self::detect_resources()
    }

    /// Detect actual available resources (handles K8s/Docker limits)
    fn detect_resources() -> Self {
        let cpu_limit = Self::detect_cpu_limit();
        let memory_bytes = Self::detect_memory_limit();

        // Convert memory to GB for easier comparison
        let memory_gb = memory_bytes as f64 / 1_073_741_824.0;

        // Choose profile based on BOTH CPU and memory
        // Use the more conservative constraint
        let cpu_profile = if cpu_limit <= 1.0 {
            0 // low
        } else if cpu_limit <= 4.0 {
            1 // medium
        } else if cpu_limit <= 16.0 {
            2 // high
        } else {
            3 // ultra (16+ CPUs)
        };

        let memory_profile = if memory_gb < 0.5 {
            0 // low (< 512MB)
        } else if memory_gb < 4.0 {
            1 // medium (< 4GB)
        } else if memory_gb < 16.0 {
            2 // high (< 16GB)
        } else {
            3 // ultra (>= 16GB)
        };

        // Use the more conservative profile
        let profile = cpu_profile.min(memory_profile);

        match profile {
            0 => Self::low_resource(),
            1 => Self::medium_resource(),
            2 => Self::high_resource(),
            _ => Self::ultra_resource(),
        }
    }

    /// Detect CPU limit (handles cgroups v1/v2 for K8s/Docker)
    fn detect_cpu_limit() -> f64 {
        // Try cgroup v2 first (newer K8s, Docker)
        if let Ok(cpu) = Self::read_cgroup_v2_cpu() {
            return cpu;
        }

        // Fall back to cgroup v1
        if let Ok(cpu) = Self::read_cgroup_v1_cpu() {
            return cpu;
        }

        // Fall back to thread::available_parallelism (bare metal)
        std::thread::available_parallelism()
            .map(|n| n.get() as f64)
            .unwrap_or(4.0)
    }

    /// Read cgroup v2 CPU limit (K8s 1.25+, modern Docker)
    fn read_cgroup_v2_cpu() -> std::result::Result<f64, std::io::Error> {
        use std::fs;

        // Read CPU max: "100000 100000" means 1 CPU (quota/period)
        let cpu_max = fs::read_to_string("/sys/fs/cgroup/cpu.max")?;
        let parts: Vec<&str> = cpu_max.trim().split_whitespace().collect();

        if parts.len() == 2 && parts[0] != "max" {
            let quota: f64 = parts[0].parse().unwrap_or(100_000.0);
            let period: f64 = parts[1].parse().unwrap_or(100_000.0);
            return Ok(quota / period);
        }

        Err(std::io::Error::new(std::io::ErrorKind::NotFound, "No CPU limit"))
    }

    /// Read cgroup v1 CPU limit (older K8s, Docker)
    fn read_cgroup_v1_cpu() -> std::result::Result<f64, std::io::Error> {
        use std::fs;

        let quota = fs::read_to_string("/sys/fs/cgroup/cpu/cpu.cfs_quota_us")?
            .trim()
            .parse::<i64>()
            .unwrap_or(-1);

        if quota > 0 {
            let period = fs::read_to_string("/sys/fs/cgroup/cpu/cpu.cfs_period_us")?
                .trim()
                .parse::<f64>()
                .unwrap_or(100_000.0);

            return Ok(quota as f64 / period);
        }

        Err(std::io::Error::new(std::io::ErrorKind::NotFound, "No CPU limit"))
    }

    /// Detect memory limit (handles cgroups v1/v2)
    fn detect_memory_limit() -> usize {
        // Try cgroup v2 first
        if let Ok(mem) = Self::read_cgroup_v2_memory() {
            return mem;
        }

        // Fall back to cgroup v1
        if let Ok(mem) = Self::read_cgroup_v1_memory() {
            return mem;
        }

        // Fall back to system memory (bare metal)
        #[cfg(target_os = "linux")]
        {
            if let Ok(info) = sys_info::mem_info() {
                return (info.total * 1024) as usize; // Convert KB to bytes
            }
        }

        // Conservative default: 2GB
        2_147_483_648
    }

    /// Read cgroup v2 memory limit
    fn read_cgroup_v2_memory() -> std::result::Result<usize, std::io::Error> {
        use std::fs;

        let mem_max = fs::read_to_string("/sys/fs/cgroup/memory.max")?;
        let mem_max = mem_max.trim();

        if mem_max != "max" {
            return Ok(mem_max.parse().unwrap_or(2_147_483_648));
        }

        Err(std::io::Error::new(std::io::ErrorKind::NotFound, "No memory limit"))
    }

    /// Read cgroup v1 memory limit
    fn read_cgroup_v1_memory() -> std::result::Result<usize, std::io::Error> {
        use std::fs;

        let mem_limit = fs::read_to_string("/sys/fs/cgroup/memory/memory.limit_in_bytes")?;
        Ok(mem_limit.trim().parse().unwrap_or(2_147_483_648))
    }

    /// Low resource profile (containers, small VMs: <= 1 CPU, < 512MB RAM)
    fn low_resource() -> Self {
        Self {
            max_batch_size: 500,            // 500 writes per batch
            max_batch_bytes: 5_000_000,     // 5MB per batch
            max_wait_time_ms: 20,           // 20ms latency
            max_queue_depth: 2_500,         // 2.5K queue depth
            enable_metrics: true,
        }
    }

    /// Medium resource profile (typical servers: 2-4 CPUs, 512MB-4GB RAM)
    fn medium_resource() -> Self {
        Self {
            max_batch_size: 2_000,          // 2K writes per batch
            max_batch_bytes: 15_000_000,    // 15MB per batch
            max_wait_time_ms: 10,           // 10ms latency
            max_queue_depth: 10_000,        // 10K queue depth
            enable_metrics: true,
        }
    }

    /// High resource profile (dedicated servers: 4+ CPUs, 4GB+ RAM)
    fn high_resource() -> Self {
        Self {
            max_batch_size: 5_000,          // 5K writes per batch
            max_batch_bytes: 25_000_000,    // 25MB per batch
            max_wait_time_ms: 5,            // 5ms latency (low-latency optimized)
            max_queue_depth: 25_000,        // 25K queue depth
            enable_metrics: true,
        }
    }

    /// Ultra resource profile (maximum throughput: 16+ CPUs, 16GB+ RAM)
    /// Uses 100ms batching window for massive throughput at cost of higher latency
    /// Trade-off: ~100ms p99 latency for potentially 2-3x higher throughput
    fn ultra_resource() -> Self {
        Self {
            max_batch_size: 20_000,         // 20K writes per batch (4x high profile)
            max_batch_bytes: 100_000_000,   // 100MB per batch (4x high profile)
            max_wait_time_ms: 100,          // 100ms latency (20x high profile - maximum batching)
            max_queue_depth: 100_000,       // 100K queue depth (4x high profile)
            enable_metrics: true,
        }
    }

    /// Create custom configuration
    pub fn custom(
        max_batch_size: usize,
        max_batch_bytes: usize,
        max_wait_time_ms: u64,
        max_queue_depth: usize,
    ) -> Self {
        Self {
            max_batch_size,
            max_batch_bytes,
            max_wait_time_ms,
            max_queue_depth,
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
    ///
    /// Elastic buffer for acks=0 with high threshold
    /// - Monitor queue size but DON'T block/error for acks=0 (fire-and-forget semantics)
    /// - Queue grows elastically during bursts, background committer drains at its own pace
    /// - Warn only if queue exceeds 10x batch size (very large accumulation)
    /// - Ultra profile: 10MB batch size × 10 = 100MB warning threshold
    /// - This prevents false "message loss" from backpressure errors
    async fn enqueue_nowait(
        &self,
        queue: Arc<PartitionCommitQueue>,
        data: Bytes,
    ) -> Result<()> {
        let data_len = data.len();

        // Monitor backpressure but don't block - elastic buffer with high threshold
        {
            let queued_bytes = *queue.total_queued_bytes.lock().await;
            if queued_bytes >= self.config.max_batch_bytes * 10 {
                queue.metrics.backpressure_events.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                warn!(
                    "acks=0 queue growing large: {} MB (elastic threshold {} MB), continuing",
                    queued_bytes / 1_000_000,
                    (self.config.max_batch_bytes * 10) / 1_000_000
                );
            }
        }

        // Enqueue without response channel
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
