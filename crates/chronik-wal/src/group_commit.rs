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
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
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

    /// Enable segment rotation (for S3 archival)
    pub enable_rotation: bool,

    /// Maximum segment size before rotation (bytes)
    pub rotation_size_bytes: u64,

    /// Maximum segment age before rotation (seconds)
    pub rotation_age_secs: u64,
}

/// State of a WAL segment in its lifecycle
#[derive(Debug, Clone, PartialEq)]
pub enum SegmentState {
    /// Currently accepting writes
    Active,
    /// Immutable, ready for indexing/archival (Quickwit's "Staged")
    Sealed,
    /// Uploaded to S3, local copy can be deleted (Quickwit's "Published")
    Archived,
}

/// Metadata about a WAL segment
#[derive(Debug, Clone)]
pub struct SegmentMetadata {
    pub segment_id: u64,
    pub created_at: Instant,
    pub size_bytes: u64,
    pub file_path: PathBuf,
    pub state: SegmentState,
}

/// Sealed segment info for archival (public API)
#[derive(Debug, Clone)]
pub struct SealedSegmentInfo {
    pub topic: String,
    pub partition: i32,
    pub segment_id: u64,
    pub file_path: PathBuf,
    pub size_bytes: u64,
    pub state: SegmentState,
    pub sealed_at: Instant,
}

impl Default for GroupCommitConfig {
    fn default() -> Self {
        // Check for profile override via environment
        if let Ok(profile) = std::env::var("CHRONIK_WAL_PROFILE") {
            return match profile.to_lowercase().as_str() {
                "low" | "small" | "container" => Self::low_resource(),
                "medium" | "balanced" => Self::medium_resource(),
                "high" | "aggressive" | "dedicated" => Self::high_resource(),
                "ultra" | "maximum" | "throughput" => Self::ultra_resource(),
                _ => {
                    warn!("Unknown CHRONIK_WAL_PROFILE '{}', using high profile", profile);
                    Self::high_resource()
                }
            };
        }

        // Default to high profile: good balance of throughput and latency
        Self::high_resource()
    }
}

impl GroupCommitConfig {
    /// Parse rotation size from environment variable or use default
    /// Supports: "100KB", "250MB", "1GB", or raw bytes "268435456"
    ///
    /// Example: CHRONIK_WAL_ROTATION_SIZE=100MB
    fn parse_rotation_size() -> u64 {
        match std::env::var("CHRONIK_WAL_ROTATION_SIZE") {
            Ok(val) => {
                let val = val.trim().to_uppercase();

                // Try to parse with unit suffix
                if val.ends_with("KB") {
                    val.trim_end_matches("KB").trim().parse::<u64>().unwrap_or(250) * 1024
                } else if val.ends_with("MB") {
                    val.trim_end_matches("MB").trim().parse::<u64>().unwrap_or(250) * 1024 * 1024
                } else if val.ends_with("GB") {
                    val.trim_end_matches("GB").trim().parse::<u64>().unwrap_or(1) * 1024 * 1024 * 1024
                } else {
                    // Try parsing as raw bytes
                    val.parse::<u64>().unwrap_or(250 * 1024 * 1024)
                }
            }
            Err(_) => 250 * 1024 * 1024,  // Default: 250MB
        }
    }

    /// Low resource profile (containers, small VMs: <= 1 CPU, < 512MB RAM)
    fn low_resource() -> Self {
        Self {
            max_batch_size: 500,            // 500 writes per batch
            max_batch_bytes: 5_000_000,     // 5MB per batch
            max_wait_time_ms: 20,           // 20ms latency
            max_queue_depth: 2_500,         // 2.5K queue depth
            enable_metrics: true,
            enable_rotation: true,          // Enable S3 archival
            rotation_size_bytes: Self::parse_rotation_size(),
            rotation_age_secs: 30 * 60,     // 30 minutes
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
            enable_rotation: true,          // Enable S3 archival
            rotation_size_bytes: Self::parse_rotation_size(),
            rotation_age_secs: 30 * 60,     // 30 minutes
        }
    }

    /// High resource profile (dedicated servers: 4+ CPUs, 4GB+ RAM)
    /// Default profile: balanced throughput and reduced disk I/O with 100ms batching
    fn high_resource() -> Self {
        Self {
            max_batch_size: 10_000,         // 10K writes per batch
            max_batch_bytes: 50_000_000,    // 50MB per batch
            max_wait_time_ms: 100,          // 100ms latency (reduce disk I/O)
            max_queue_depth: 50_000,        // 50K queue depth
            enable_metrics: true,
            enable_rotation: true,          // Enable S3 archival
            rotation_size_bytes: Self::parse_rotation_size(),
            rotation_age_secs: 30 * 60,     // 30 minutes
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
            enable_rotation: true,          // Enable S3 archival
            rotation_size_bytes: 512 * 1024 * 1024,  // 512MB (larger for ultra)
            rotation_age_secs: 30 * 60,     // 30 minutes
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
            enable_rotation: true,
            rotation_size_bytes: 256 * 1024 * 1024,
            rotation_age_secs: 30 * 60,
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

    /// Current segment ID
    segment_id: Arc<AtomicU64>,

    /// Current segment creation time
    segment_created_at: Arc<Mutex<Instant>>,

    /// Current segment size in bytes
    segment_size_bytes: Arc<AtomicU64>,

    /// Topic name (for rotation)
    topic: String,

    /// Partition number (for rotation)
    partition: i32,
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

    /// Sealed segments ready for archival (topic:partition:segment_id → info)
    sealed_segments: Arc<DashMap<String, SealedSegmentInfo>>,

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
            sealed_segments: Arc::new(DashMap::new()),
            config,
            base_dir: base_dir.clone(),
            shutdown: Arc::new(Notify::new()),
        };

        // Discover existing sealed segments from filesystem
        wal.discover_sealed_segments();

        // Start background commit thread
        wal.start_background_committer();

        info!("Group commit WAL initialized: max_batch={}, max_wait={}ms, max_queue={}, rotation={}",
              wal.config.max_batch_size, wal.config.max_wait_time_ms, wal.config.max_queue_depth,
              if wal.config.enable_rotation { "enabled" } else { "disabled" });

        wal
    }

    /// Discover sealed segments from filesystem (called on startup)
    fn discover_sealed_segments(&self) {
        use std::fs;

        if !self.config.enable_rotation {
            return;
        }

        let mut discovered = 0;

        // Scan data/wal/topic/partition/ directories
        if let Ok(topic_dirs) = fs::read_dir(&self.base_dir) {
            for topic_entry in topic_dirs.flatten() {
                if let Ok(topic_name) = topic_entry.file_name().into_string() {
                    // Skip non-directories or __meta
                    if topic_name.starts_with("__") || !topic_entry.path().is_dir() {
                        continue;
                    }

                    // Scan partition directories
                    if let Ok(partition_dirs) = fs::read_dir(topic_entry.path()) {
                        for partition_entry in partition_dirs.flatten() {
                            if let Ok(partition_str) = partition_entry.file_name().into_string() {
                                if let Ok(partition) = partition_str.parse::<i32>() {
                                    // Scan WAL files in this partition
                                    if let Ok(files) = fs::read_dir(partition_entry.path()) {
                                        for file_entry in files.flatten() {
                                            let file_name = file_entry.file_name();
                                            let file_name_str = file_name.to_string_lossy();

                                            // Match pattern: wal_PARTITION_SEGMENT.log
                                            if file_name_str.starts_with("wal_") && file_name_str.ends_with(".log") {
                                                // Extract segment_id from filename
                                                if let Some(segment_str) = file_name_str.strip_prefix("wal_").and_then(|s| s.strip_suffix(".log")) {
                                                    let parts: Vec<&str> = segment_str.split('_').collect();
                                                    if parts.len() == 2 {
                                                        if let Ok(segment_id) = parts[1].parse::<u64>() {
                                                            // Get file metadata
                                                            if let Ok(metadata) = file_entry.metadata() {
                                                                let size_bytes = metadata.len();

                                                                // Only consider sealed segments (>= rotation size or not the highest segment_id)
                                                                // For now, consider all segments except segment_0 as potentially sealed
                                                                if segment_id > 0 || size_bytes >= self.config.rotation_size_bytes {
                                                                    let sealed_key = format!("{}:{}:{}", topic_name, partition, segment_id);
                                                                    self.sealed_segments.insert(
                                                                        sealed_key,
                                                                        SealedSegmentInfo {
                                                                            topic: topic_name.clone(),
                                                                            partition,
                                                                            segment_id,
                                                                            file_path: file_entry.path(),
                                                                            size_bytes,
                                                                            state: SegmentState::Sealed,
                                                                            sealed_at: Instant::now(),
                                                                        },
                                                                    );
                                                                    discovered += 1;
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        if discovered > 0 {
            info!("Discovered {} sealed segments from filesystem", discovered);
        }
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

        // Use partition-specific naming for consistency with rotation
        // Format: wal_{partition}_{segment_id}.log
        let wal_path = partition_dir.join(format!("wal_{}_0.log", partition));

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
            segment_id: Arc::new(AtomicU64::new(0)),
            segment_created_at: Arc::new(Mutex::new(Instant::now())),
            segment_size_bytes: Arc::new(AtomicU64::new(0)),
            topic: topic.to_string(),
            partition,
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
        let sealed_segments = self.sealed_segments.clone();
        let base_dir = self.base_dir.clone();

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
                if let Err(e) = Self::commit_batch(&queue, &config, &sealed_segments, &base_dir).await {
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
        let sealed_segments = self.sealed_segments.clone();
        let base_dir = self.base_dir.clone();

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
                        if let Err(e) = Self::commit_batch(queue, &config, &sealed_segments, &base_dir).await {
                            error!("Background commit failed: {}", e);
                        }
                    }
                }
            }
        });
    }

    /// Commit a batch of writes with single fsync
    #[instrument(skip(queue, config, sealed_segments, base_dir), fields(batch_size, bytes, fsync_us))]
    async fn commit_batch(
        queue: &PartitionCommitQueue,
        config: &GroupCommitConfig,
        sealed_segments: &Arc<DashMap<String, SealedSegmentInfo>>,
        base_dir: &Path,
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

        // Update segment size
        queue.segment_size_bytes.fetch_add(total_bytes as u64, Ordering::Relaxed);

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

        // Check if we should seal and rotate segment
        if let Err(e) = Self::check_and_seal_segment(queue, config, sealed_segments, base_dir).await {
            error!("Failed to check/seal segment: {}", e);
            // Don't fail the commit, just log the error
        }

        Ok(())
    }

    /// Check if current segment should be sealed and rotate if needed
    async fn check_and_seal_segment(
        queue: &PartitionCommitQueue,
        config: &GroupCommitConfig,
        sealed_segments: &Arc<DashMap<String, SealedSegmentInfo>>,
        base_dir: &Path,
    ) -> Result<()> {
        // Skip if rotation disabled
        if !config.enable_rotation {
            return Ok(());
        }

        let current_size = queue.segment_size_bytes.load(Ordering::Relaxed);
        let segment_age = {
            let created_at = queue.segment_created_at.lock().await;
            created_at.elapsed()
        };

        let should_seal = current_size >= config.rotation_size_bytes
            || segment_age.as_secs() >= config.rotation_age_secs;

        if !should_seal {
            return Ok(());
        }

        // Seal current segment
        let old_segment_id = queue.segment_id.load(Ordering::Relaxed);
        let old_file_path = base_dir
            .join(&queue.topic)
            .join(queue.partition.to_string())
            .join(format!("wal_{}_{}.log", queue.partition, old_segment_id));

        info!(
            "🔒 Sealing segment {}/{} segment_id={} (size={} bytes, age={:?})",
            queue.topic, queue.partition, old_segment_id, current_size, segment_age
        );

        // Close old file
        {
            let file = queue.file.lock().await;
            file.sync_all().await?;
            drop(file); // Explicitly drop to close
        }

        // Record sealed segment info
        let sealed_key = format!("{}:{}:{}", queue.topic, queue.partition, old_segment_id);
        sealed_segments.insert(
            sealed_key,
            SealedSegmentInfo {
                topic: queue.topic.clone(),
                partition: queue.partition,
                segment_id: old_segment_id,
                file_path: old_file_path.clone(),
                size_bytes: current_size,
                state: SegmentState::Sealed,
                sealed_at: Instant::now(),
            },
        );

        // Rotate to new segment
        let new_segment_id = old_segment_id + 1;
        queue.segment_id.store(new_segment_id, Ordering::Relaxed);

        let new_file_path = base_dir
            .join(&queue.topic)
            .join(queue.partition.to_string())
            .join(format!("wal_{}_{}.log", queue.partition, new_segment_id));

        // Open new file
        let new_file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&new_file_path)
            .await?;

        *queue.file.lock().await = new_file;

        // Reset segment tracking
        *queue.segment_created_at.lock().await = Instant::now();
        queue.segment_size_bytes.store(0, Ordering::Relaxed);

        info!(
            "✅ Rotated to new segment {}/{} segment_id={}",
            queue.topic, queue.partition, new_segment_id
        );

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

    /// Get list of sealed segments ready for archival
    pub fn get_sealed_segments(&self) -> Vec<SealedSegmentInfo> {
        self.sealed_segments
            .iter()
            .filter(|entry| entry.value().state == SegmentState::Sealed)
            .map(|entry| entry.value().clone())
            .collect()
    }

    /// Mark segment as archived after successful S3 upload
    pub fn mark_segment_archived(&self, topic: &str, partition: i32, segment_id: u64) {
        let key = format!("{}:{}:{}", topic, partition, segment_id);
        if let Some(mut entry) = self.sealed_segments.get_mut(&key) {
            entry.state = SegmentState::Archived;
            info!("Marked segment {}/{} segment_id={} as Archived", topic, partition, segment_id);
        }
    }

    /// Remove archived segment (called after S3 upload and local cleanup)
    pub fn remove_segment(&self, topic: &str, partition: i32, segment_id: u64) -> Result<()> {
        let key = format!("{}:{}:{}", topic, partition, segment_id);

        if let Some((_, segment_info)) = self.sealed_segments.remove(&key) {
            // Delete local file if configured
            if segment_info.state == SegmentState::Archived {
                if let Err(e) = std::fs::remove_file(&segment_info.file_path) {
                    warn!("Failed to delete archived segment file {:?}: {}", segment_info.file_path, e);
                } else {
                    info!("Deleted archived segment file {:?}", segment_info.file_path);
                }
            }
        }

        Ok(())
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
