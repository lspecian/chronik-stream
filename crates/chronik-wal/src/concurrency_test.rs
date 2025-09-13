//! Concurrency stress tests for WAL lock analysis
//!
//! This module provides stress tests to identify potential lock contention
//! and starvation issues in the WAL implementation under high concurrency.

use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Barrier;
use tokio::task::JoinSet;
use tracing::{info, warn, debug};
use parking_lot::Mutex;
use dashmap::DashMap;

use crate::{
    WalManager,
    WalConfig,
    WalRecord,
    error::Result,
};

/// Statistics for lock contention analysis
#[derive(Debug, Clone, Default)]
pub struct ContentionStats {
    pub total_operations: u64,
    pub successful_writes: u64,
    pub failed_writes: u64,
    pub rotation_count: u64,
    pub max_write_latency_ms: u64,
    pub avg_write_latency_ms: u64,
    pub lock_wait_time_ms: u64,
    pub contention_events: u64,
}

/// Per-thread operation metrics
#[derive(Debug, Clone)]
struct ThreadMetrics {
    thread_id: usize,
    operations: u64,
    total_latency_ms: u64,
    max_latency_ms: u64,
    lock_waits: u64,
}

/// Concurrency stress test configuration
#[derive(Debug, Clone)]
pub struct StressTestConfig {
    /// Number of concurrent producer threads
    pub producer_count: usize,
    /// Number of partitions to write to
    pub partition_count: usize,
    /// Number of records per producer
    pub records_per_producer: usize,
    /// Record size in bytes
    pub record_size: usize,
    /// Force rotation every N records (0 = natural rotation)
    pub force_rotation_interval: usize,
    /// Enable fairness checking
    pub check_fairness: bool,
    /// Test duration in seconds (0 = unlimited)
    pub duration_secs: u64,
}

impl Default for StressTestConfig {
    fn default() -> Self {
        Self {
            producer_count: 16,
            partition_count: 4,
            records_per_producer: 1000,
            record_size: 1024,
            force_rotation_interval: 100,
            check_fairness: true,
            duration_secs: 0,
        }
    }
}

/// Run concurrency stress test to identify lock contention
pub async fn run_stress_test(
    wal_config: WalConfig,
    test_config: StressTestConfig,
) -> Result<ContentionStats> {
    info!("Starting WAL concurrency stress test");
    info!("Producers: {}, Partitions: {}, Records/producer: {}", 
        test_config.producer_count,
        test_config.partition_count,
        test_config.records_per_producer
    );
    
    let wal_manager = WalManager::new(wal_config).await?;
    let wal_manager = Arc::new(Mutex::new(wal_manager));
    
    // Shared statistics
    let stats = Arc::new(Mutex::new(ContentionStats::default()));
    let thread_metrics = Arc::new(DashMap::new());
    
    // Synchronization barrier for all producers to start simultaneously
    let barrier = Arc::new(Barrier::new(test_config.producer_count));
    
    // Start time for duration-based tests
    let start_time = Instant::now();
    let test_duration = if test_config.duration_secs > 0 {
        Some(Duration::from_secs(test_config.duration_secs))
    } else {
        None
    };
    
    // Spawn producer tasks
    let mut tasks = JoinSet::new();
    
    for producer_id in 0..test_config.producer_count {
        let wal_manager = Arc::clone(&wal_manager);
        let stats = Arc::clone(&stats);
        let thread_metrics = Arc::clone(&thread_metrics);
        let barrier = Arc::clone(&barrier);
        let config = test_config.clone();
        
        tasks.spawn(async move {
            run_producer(
                producer_id,
                wal_manager,
                stats,
                thread_metrics,
                barrier,
                config,
                start_time,
                test_duration,
            ).await
        });
    }
    
    // If rotation interval is set, spawn rotation forcer
    if test_config.force_rotation_interval > 0 {
        let wal_manager = Arc::clone(&wal_manager);
        let stats = Arc::clone(&stats);
        let config = test_config.clone();
        
        tasks.spawn(async move {
            run_rotation_forcer(
                wal_manager,
                stats,
                config,
                start_time,
                test_duration,
            ).await
        });
    }
    
    // Wait for all producers to complete
    while let Some(result) = tasks.join_next().await {
        if let Err(e) = result {
            warn!("Producer task failed: {:?}", e);
        }
    }
    
    // Analyze fairness if requested
    if test_config.check_fairness {
        analyze_fairness(&thread_metrics);
    }
    
    // Calculate final statistics
    let final_stats = calculate_final_stats(&stats, &thread_metrics);
    
    info!("Stress test completed: {:?}", final_stats);
    
    Ok(final_stats)
}

/// Run a single producer thread
async fn run_producer(
    producer_id: usize,
    wal_manager: Arc<Mutex<WalManager>>,
    stats: Arc<Mutex<ContentionStats>>,
    thread_metrics: Arc<DashMap<usize, ThreadMetrics>>,
    barrier: Arc<Barrier>,
    config: StressTestConfig,
    start_time: Instant,
    test_duration: Option<Duration>,
) -> Result<()> {
    debug!("Producer {} starting", producer_id);
    
    let mut metrics = ThreadMetrics {
        thread_id: producer_id,
        operations: 0,
        total_latency_ms: 0,
        max_latency_ms: 0,
        lock_waits: 0,
    };
    
    // Wait for all producers to be ready
    barrier.wait().await;
    
    let topic = format!("stress_test_topic_{}", producer_id % config.partition_count);
    let partition = (producer_id % config.partition_count) as i32;
    
    for record_num in 0..config.records_per_producer {
        // Check if duration limit exceeded
        if let Some(duration) = test_duration {
            if start_time.elapsed() > duration {
                break;
            }
        }
        
        // Create test record
        let record = WalRecord::new(
            record_num as i64,
            Some(format!("key_{}", record_num).into_bytes()),
            vec![0u8; config.record_size],
            chrono::Utc::now().timestamp_millis(),
        );
        
        // Measure write latency and lock acquisition time
        let write_start = Instant::now();
        let lock_start = Instant::now();
        
        // Acquire lock and write
        let lock_acquisition_time = lock_start.elapsed();
        
        if lock_acquisition_time > Duration::from_millis(10) {
            metrics.lock_waits += 1;
            stats.lock().contention_events += 1;
        }
        
        // Perform append with lock scoped properly
        let append_result = {
            let manager = wal_manager.clone();
            let topic_clone = topic.clone();
            let record_clone = record;
            tokio::task::spawn_blocking(move || {
                tokio::runtime::Handle::current().block_on(async move {
                    let mut mgr = manager.lock();
                    mgr.append(topic_clone, partition, vec![record_clone]).await
                })
            }).await.unwrap()
        };
        
        match append_result {
            Ok(_) => {
                stats.lock().successful_writes += 1;
            }
            Err(e) => {
                warn!("Producer {} write failed: {:?}", producer_id, e);
                stats.lock().failed_writes += 1;
            }
        }
        
        let write_latency = write_start.elapsed();
        let latency_ms = write_latency.as_millis() as u64;
        
        metrics.operations += 1;
        metrics.total_latency_ms += latency_ms;
        metrics.max_latency_ms = metrics.max_latency_ms.max(latency_ms);
        
        // Update global stats
        {
            let mut s = stats.lock();
            s.total_operations += 1;
            s.max_write_latency_ms = s.max_write_latency_ms.max(latency_ms);
            s.lock_wait_time_ms += lock_acquisition_time.as_millis() as u64;
        }
        
        // Small yield to increase contention likelihood
        if record_num % 10 == 0 {
            tokio::task::yield_now().await;
        }
    }
    
    thread_metrics.insert(producer_id, metrics);
    
    debug!("Producer {} completed", producer_id);
    Ok(())
}

/// Force periodic segment rotations to test rotation under load
async fn run_rotation_forcer(
    wal_manager: Arc<Mutex<WalManager>>,
    stats: Arc<Mutex<ContentionStats>>,
    config: StressTestConfig,
    start_time: Instant,
    test_duration: Option<Duration>,
) -> Result<()> {
    info!("Rotation forcer started, interval: {} records", config.force_rotation_interval);
    
    let mut rotation_count = 0;
    
    loop {
        // Check if duration limit exceeded
        if let Some(duration) = test_duration {
            if start_time.elapsed() > duration {
                break;
            }
        }
        
        // Wait for some operations to accumulate
        tokio::time::sleep(Duration::from_millis(100)).await;
        
        // Force flush which may trigger rotation
        let rotation_start = Instant::now();
        
        let flush_result = {
            let manager = wal_manager.lock();
            manager.flush_all().await
        };
        
        if let Err(e) = flush_result {
            warn!("Forced flush failed: {:?}", e);
        }
        
        let rotation_time = rotation_start.elapsed();
        if rotation_time > Duration::from_millis(50) {
            warn!("Slow rotation detected: {:?}", rotation_time);
        }
        
        rotation_count += 1;
        stats.lock().rotation_count += 1;
        
        // Check if we should stop
        if stats.lock().total_operations >= (config.producer_count * config.records_per_producer) as u64 {
            break;
        }
    }
    
    info!("Rotation forcer completed, total rotations: {}", rotation_count);
    Ok(())
}

/// Analyze fairness across producer threads
fn analyze_fairness(thread_metrics: &Arc<DashMap<usize, ThreadMetrics>>) {
    let mut total_ops = 0u64;
    let mut min_ops = u64::MAX;
    let mut max_ops = 0u64;
    let mut total_waits = 0u64;
    
    for entry in thread_metrics.iter() {
        let metrics = entry.value();
        total_ops += metrics.operations;
        min_ops = min_ops.min(metrics.operations);
        max_ops = max_ops.max(metrics.operations);
        total_waits += metrics.lock_waits;
    }
    
    let thread_count = thread_metrics.len();
    let avg_ops = if thread_count > 0 {
        total_ops / thread_count as u64
    } else {
        0
    };
    
    let fairness_ratio = if max_ops > 0 {
        min_ops as f64 / max_ops as f64
    } else {
        0.0
    };
    
    info!("Fairness Analysis:");
    info!("  Thread count: {}", thread_count);
    info!("  Min operations: {}", min_ops);
    info!("  Max operations: {}", max_ops);
    info!("  Avg operations: {}", avg_ops);
    info!("  Fairness ratio: {:.2}", fairness_ratio);
    info!("  Total lock waits: {}", total_waits);
    
    if fairness_ratio < 0.7 {
        warn!("Poor fairness detected! Some threads are being starved.");
        warn!("Consider using parking_lot::Mutex for better fairness.");
    }
}

/// Calculate final statistics from collected metrics
fn calculate_final_stats(
    stats: &Arc<Mutex<ContentionStats>>,
    thread_metrics: &Arc<DashMap<usize, ThreadMetrics>>,
) -> ContentionStats {
    let mut final_stats = stats.lock().clone();
    
    // Calculate average latency
    let mut total_latency = 0u64;
    let mut total_operations = 0u64;
    
    for entry in thread_metrics.iter() {
        let metrics = entry.value();
        total_latency += metrics.total_latency_ms;
        total_operations += metrics.operations;
    }
    
    if total_operations > 0 {
        final_stats.avg_write_latency_ms = total_latency / total_operations;
    }
    
    final_stats
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    
    #[tokio::test]
    async fn test_basic_stress_test() {
        let dir = tempdir().unwrap();
        let mut wal_config = WalConfig::default();
        wal_config.data_dir = dir.path().to_path_buf();
        
        let test_config = StressTestConfig {
            producer_count: 4,
            partition_count: 2,
            records_per_producer: 100,
            record_size: 512,
            force_rotation_interval: 50,
            check_fairness: true,
            duration_secs: 0,
        };
        
        let stats = run_stress_test(wal_config, test_config)
            .await
            .unwrap();
        
        assert_eq!(stats.successful_writes, 400);
        assert_eq!(stats.failed_writes, 0);
        assert!(stats.avg_write_latency_ms < 100);
    }
    
    #[tokio::test]
    async fn test_high_contention() {
        let dir = tempdir().unwrap();
        let mut wal_config = WalConfig::default();
        wal_config.data_dir = dir.path().to_path_buf();
        
        let test_config = StressTestConfig {
            producer_count: 16,
            partition_count: 1, // Single partition for maximum contention
            records_per_producer: 50,
            record_size: 256,
            force_rotation_interval: 0,
            check_fairness: true,
            duration_secs: 0,
        };
        
        let stats = run_stress_test(wal_config, test_config)
            .await
            .unwrap();
        
        // Under high contention, we should still complete all writes
        assert_eq!(stats.successful_writes, 800);
        
        // Check for contention events
        if stats.contention_events > 100 {
            println!("High contention detected: {} events", stats.contention_events);
        }
    }
    
    #[tokio::test]
    async fn test_duration_based() {
        let dir = tempdir().unwrap();
        let mut wal_config = WalConfig::default();
        wal_config.data_dir = dir.path().to_path_buf();
        
        let test_config = StressTestConfig {
            producer_count: 2,
            partition_count: 1,
            records_per_producer: 10000, // More than can complete in time
            record_size: 128,
            force_rotation_interval: 0,
            check_fairness: false,
            duration_secs: 1, // Run for 1 second only
        };
        
        let start = Instant::now();
        let stats = run_stress_test(wal_config, test_config)
            .await
            .unwrap();
        let duration = start.elapsed();
        
        // Should stop after approximately 1 second
        assert!(duration < Duration::from_secs(2));
        assert!(stats.successful_writes > 0);
        assert!(stats.successful_writes < 20000); // Didn't complete all
    }
}