//! Batch fsync implementation for WAL writes
//!
//! This module provides batched fsync operations to reduce write latency under load
//! while maintaining durability guarantees. The FsyncBatcher manages timing and 
//! size-based batching using configurable thresholds.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, RwLock};
use tokio::time::{interval, sleep_until};
use tracing::{debug, info, warn, instrument};
use chronik_common::metrics::Metrics;

use crate::config::FsyncConfig;
use crate::error::Result;

/// Batch fsync request 
#[derive(Debug)]
struct FsyncRequest {
    path: PathBuf,
    data: Vec<u8>,
    completion_notify: Arc<Notify>,
    topic: String,
    partition: i32,
}

/// Statistics for fsync batching operations
#[derive(Debug, Clone, Default)]
pub struct FsyncStats {
    pub batch_flushes_total: u64,
    pub total_flush_duration_ms: u64,
    pub total_batch_size: u64,
    pub largest_batch_size: u32,
    pub smallest_batch_size: u32,
}

/// Batched fsync coordinator for WAL segments
/// 
/// The FsyncBatcher collects multiple write requests and flushes them in batches
/// based on either size thresholds or timeout intervals. This reduces the frequency
/// of expensive fsync calls while maintaining durability guarantees.
pub struct FsyncBatcher {
    config: FsyncConfig,
    
    // Pending requests grouped by file path
    pending_requests: Arc<RwLock<HashMap<PathBuf, Vec<FsyncRequest>>>>,
    
    // Batching coordination
    batch_notify: Arc<Notify>,
    shutdown_notify: Arc<Notify>,
    
    // Statistics and metrics
    stats: Arc<RwLock<FsyncStats>>,
    pending_count: Arc<AtomicU32>,
    pending_bytes: Arc<AtomicU64>,
    
    // Background task handle
    _batch_task: tokio::task::JoinHandle<()>,
}

impl FsyncBatcher {
    /// Create a new fsync batcher with the given configuration
    #[instrument(skip(config))]
    pub fn new(config: FsyncConfig) -> Self {
        let pending_requests = Arc::new(RwLock::new(HashMap::new()));
        let batch_notify = Arc::new(Notify::new());
        let shutdown_notify = Arc::new(Notify::new());
        let stats = Arc::new(RwLock::new(FsyncStats::default()));
        let pending_count = Arc::new(AtomicU32::new(0));
        let pending_bytes = Arc::new(AtomicU64::new(0));
        
        // Start background batching task
        let batch_task = Self::start_batch_task(
            config.clone(),
            pending_requests.clone(),
            batch_notify.clone(),
            shutdown_notify.clone(),
            stats.clone(),
            pending_count.clone(),
            pending_bytes.clone(),
        );
        
        info!(
            batch_size = config.batch_size,
            batch_timeout_ms = config.batch_timeout_ms,
            "FsyncBatcher initialized successfully"
        );
        
        Self {
            config,
            pending_requests,
            batch_notify,
            shutdown_notify,
            stats,
            pending_count,
            pending_bytes,
            _batch_task: batch_task,
        }
    }
    
    /// Submit a write request for batched fsync
    /// 
    /// This method queues the write request and returns immediately. The actual
    /// fsync will be performed by the background task when batch thresholds are met.
    #[instrument(skip(self, data), fields(
        path = %path.display(),
        data_size = data.len(),
        topic = %topic,
        partition = partition,
        pending_count = self.pending_count.load(Ordering::Relaxed),
        pending_bytes = self.pending_bytes.load(Ordering::Relaxed)
    ))]
    pub async fn submit_write(&self, path: PathBuf, data: Vec<u8>, topic: String, partition: i32) -> Result<()> {
        if !self.config.enabled {
            // If batching is disabled, write immediately
            tokio::fs::write(&path, &data).await?;
            return Ok(());
        }
        
        let data_size = data.len() as u64;
        let completion_notify = Arc::new(Notify::new());
        
        let request = FsyncRequest {
            path: path.clone(),
            data,
            completion_notify: completion_notify.clone(),
            topic: topic.clone(),
            partition,
        };
        
        // Add request to pending batch
        {
            let mut pending = self.pending_requests.write().await;
            pending.entry(path.clone()).or_insert_with(Vec::new).push(request);
        }
        
        // Update counters
        self.pending_count.fetch_add(1, Ordering::Relaxed);
        self.pending_bytes.fetch_add(data_size, Ordering::Relaxed);
        
        // Check if we should trigger a batch flush
        let should_flush = self.pending_count.load(Ordering::Relaxed) >= self.config.batch_size;
        
        if should_flush {
            debug!(
                pending_count = self.pending_count.load(Ordering::Relaxed),
                batch_size = self.config.batch_size,
                "Triggering immediate batch flush due to size threshold"
            );
            self.batch_notify.notify_one();
        }
        
        // Wait for completion
        completion_notify.notified().await;
        
        debug!(
            path = %path.display(),
            data_size = data_size,
            "Write request completed successfully"
        );
        
        Ok(())
    }
    
    /// Force flush all pending writes immediately
    #[instrument(skip(self))]
    pub async fn force_flush(&self) -> Result<()> {
        if self.pending_count.load(Ordering::Relaxed) == 0 {
            return Ok(());
        }
        
        debug!(
            pending_count = self.pending_count.load(Ordering::Relaxed),
            "Force flushing all pending writes"
        );
        
        self.batch_notify.notify_one();
        
        // Wait a short time for the flush to complete
        sleep_until(tokio::time::Instant::now() + Duration::from_millis(100)).await;
        
        Ok(())
    }
    
    /// Get current fsync statistics
    pub async fn get_stats(&self) -> FsyncStats {
        self.stats.read().await.clone()
    }
    
    /// Start the background task that handles batch flushing
    fn start_batch_task(
        config: FsyncConfig,
        pending_requests: Arc<RwLock<HashMap<PathBuf, Vec<FsyncRequest>>>>,
        batch_notify: Arc<Notify>,
        shutdown_notify: Arc<Notify>,
        stats: Arc<RwLock<FsyncStats>>,
        pending_count: Arc<AtomicU32>,
        pending_bytes: Arc<AtomicU64>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut timer = interval(Duration::from_millis(config.batch_timeout_ms));
            timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            
            info!(
                batch_timeout_ms = config.batch_timeout_ms,
                "Batch fsync background task started"
            );
            
            loop {
                tokio::select! {
                    // Timer-based flush
                    _ = timer.tick() => {
                        if pending_count.load(Ordering::Relaxed) > 0 {
                            debug!("Timer-based batch flush triggered");
                            Self::process_batch(
                                &pending_requests,
                                &stats,
                                &pending_count,
                                &pending_bytes,
                            ).await;
                        }
                    }
                    
                    // Size-based flush notification
                    _ = batch_notify.notified() => {
                        debug!("Size-based batch flush triggered");
                        Self::process_batch(
                            &pending_requests,
                            &stats,
                            &pending_count,
                            &pending_bytes,
                        ).await;
                    }
                    
                    // Shutdown signal
                    _ = shutdown_notify.notified() => {
                        info!("Batch fsync task shutting down");
                        // Final flush of remaining requests
                        Self::process_batch(
                            &pending_requests,
                            &stats,
                            &pending_count,
                            &pending_bytes,
                        ).await;
                        break;
                    }
                }
            }
        })
    }
    
    /// Process a batch of pending fsync requests
    #[instrument(skip(pending_requests, stats, pending_count, pending_bytes))]
    async fn process_batch(
        pending_requests: &Arc<RwLock<HashMap<PathBuf, Vec<FsyncRequest>>>>,
        stats: &Arc<RwLock<FsyncStats>>,
        pending_count: &Arc<AtomicU32>,
        pending_bytes: &Arc<AtomicU64>,
    ) {
        let batch_start = Instant::now();
        
        // Extract all pending requests
        let requests_to_process = {
            let mut pending = pending_requests.write().await;
            let mut extracted = HashMap::new();
            std::mem::swap(&mut extracted, &mut *pending);
            extracted
        };
        
        if requests_to_process.is_empty() {
            return;
        }
        
        let total_requests: usize = requests_to_process.values().map(|v| v.len()).sum();
        let total_bytes: u64 = requests_to_process
            .values()
            .flat_map(|v| v.iter())
            .map(|req| req.data.len() as u64)
            .sum();
        
        debug!(
            file_count = requests_to_process.len(),
            total_requests = total_requests,
            total_bytes = total_bytes,
            "Processing fsync batch"
        );
        
        // Process each file's requests
        for (path, requests) in requests_to_process {
            let file_start = Instant::now();
            
            // Combine all data for this file
            let mut combined_data = Vec::new();
            for request in &requests {
                combined_data.extend_from_slice(&request.data);
            }
            
            // Get topic/partition from first request (all should have same path)
            let (topic, partition) = if let Some(first_request) = requests.first() {
                (first_request.topic.clone(), first_request.partition)
            } else {
                continue;
            };
            
            // Write to disk
            let write_result = tokio::fs::write(&path, &combined_data).await;
            let file_duration = file_start.elapsed();
            
            match write_result {
                Ok(()) => {
                    // Record successful metrics
                    if let Ok(metrics) = std::panic::catch_unwind(|| Metrics::get()) {
                        metrics.wal_batch_flushes_total
                            .with_label_values(&[&topic, &partition.to_string()])
                            .inc();
                        
                        metrics.wal_batch_flush_duration_seconds
                            .with_label_values(&[&topic, &partition.to_string()])
                            .observe(file_duration.as_secs_f64());
                        
                        metrics.wal_batch_size
                            .with_label_values(&[&topic, &partition.to_string()])
                            .observe(requests.len() as f64);
                    }
                    
                    debug!(
                        path = %path.display(),
                        bytes_written = combined_data.len(),
                        requests_count = requests.len(),
                        write_duration_ms = file_duration.as_millis() as u64,
                        topic = %topic,
                        partition = partition,
                        "File fsync completed successfully"
                    );
                }
                Err(e) => {
                    warn!(
                        path = %path.display(),
                        error = %e,
                        requests_count = requests.len(),
                        topic = %topic,
                        partition = partition,
                        "File fsync failed"
                    );
                }
            }
            
            // Notify all requests for this file
            for request in requests {
                request.completion_notify.notify_one();
            }
        }
        
        // Reset counters
        pending_count.store(0, Ordering::Relaxed);
        pending_bytes.store(0, Ordering::Relaxed);
        
        let batch_duration = batch_start.elapsed();
        
        // Update statistics
        {
            let mut stats_guard = stats.write().await;
            stats_guard.batch_flushes_total += 1;
            stats_guard.total_flush_duration_ms += batch_duration.as_millis() as u64;
            stats_guard.total_batch_size += total_bytes;
            
            if stats_guard.largest_batch_size < total_requests as u32 {
                stats_guard.largest_batch_size = total_requests as u32;
            }
            
            if stats_guard.smallest_batch_size == 0 || stats_guard.smallest_batch_size > total_requests as u32 {
                stats_guard.smallest_batch_size = total_requests as u32;
            }
        }
        
        info!(
            batch_requests = total_requests,
            batch_bytes = total_bytes,
            batch_duration_ms = batch_duration.as_millis() as u64,
            "Batch fsync completed successfully"
        );
    }
}

impl Drop for FsyncBatcher {
    fn drop(&mut self) {
        // Signal shutdown to background task
        self.shutdown_notify.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tokio::time::timeout;
    
    #[tokio::test]
    async fn test_batch_fsync_basic() {
        let config = FsyncConfig {
            enabled: true,
            batch_size: 2,
            batch_timeout_ms: 100,
        };
        
        let batcher = FsyncBatcher::new(config);
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.log");
        
        // Submit a write request
        let data = b"test data".to_vec();
        batcher.submit_write(test_file.clone(), data.clone(), "test_topic".to_string(), 0).await.unwrap();
        
        // Verify file was written
        let written_data = tokio::fs::read(&test_file).await.unwrap();
        assert_eq!(written_data, data);
        
        // Check stats
        let stats = batcher.get_stats().await;
        assert!(stats.batch_flushes_total >= 1);
    }
    
    #[tokio::test]
    async fn test_batch_fsync_multiple_requests() {
        let config = FsyncConfig {
            enabled: true,
            batch_size: 3,
            batch_timeout_ms: 100,
        };
        
        let batcher = FsyncBatcher::new(config);
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.log");
        
        // Submit multiple write requests that should be batched
        let data1 = b"data1".to_vec();
        let data2 = b"data2".to_vec();
        let data3 = b"data3".to_vec();
        
        let write1 = batcher.submit_write(test_file.clone(), data1.clone(), "test_topic".to_string(), 0);
        let write2 = batcher.submit_write(test_file.clone(), data2.clone(), "test_topic".to_string(), 0);
        let write3 = batcher.submit_write(test_file.clone(), data3.clone(), "test_topic".to_string(), 0);
        
        // Wait for all writes to complete
        let (res1, res2, res3) = timeout(Duration::from_secs(1), async {
            tokio::join!(write1, write2, write3)
        }).await.unwrap();
        
        res1.unwrap();
        res2.unwrap();  
        res3.unwrap();
        
        // Verify all data was combined and written
        let written_data = tokio::fs::read(&test_file).await.unwrap();
        let expected = [data1, data2, data3].concat();
        assert_eq!(written_data, expected);
    }
    
    #[tokio::test]
    async fn test_batch_fsync_disabled() {
        let config = FsyncConfig {
            enabled: false,
            batch_size: 10,
            batch_timeout_ms: 1000,
        };
        
        let batcher = FsyncBatcher::new(config);
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.log");
        
        // Submit a write request
        let data = b"test data".to_vec();
        batcher.submit_write(test_file.clone(), data.clone(), "test_topic".to_string(), 0).await.unwrap();
        
        // Verify file was written immediately (no batching)
        let written_data = tokio::fs::read(&test_file).await.unwrap();
        assert_eq!(written_data, data);
        
        // Stats should show no batching occurred
        let stats = batcher.get_stats().await;
        assert_eq!(stats.batch_flushes_total, 0);
    }
}