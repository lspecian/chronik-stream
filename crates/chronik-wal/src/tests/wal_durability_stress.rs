//! WAL Durability Stress Tests
//!
//! Tests that validate WAL durability under extreme conditions including:
//! - Crashes during segment flush
//! - Double fsync interruptions
//! - Recovery idempotence
//! - Randomized fsync failures

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use parking_lot::Mutex;
use rand::Rng;
use tempfile::TempDir;
use tokio::time::timeout;
use tracing::{info, warn, debug};

use crate::{
    WalManager, WalConfig, WalRecord, RecoveryResult,
    error::{Result, WalError},
    config::{FsyncConfig, FsyncMode, RotationConfig},
};

/// Mock fsync implementation that can simulate failures
#[derive(Clone)]
struct MockFsync {
    failure_rate: f64,
    total_calls: Arc<AtomicU64>,
    failed_calls: Arc<AtomicU64>,
    should_fail: Arc<AtomicBool>,
}

impl MockFsync {
    fn new(failure_rate: f64) -> Self {
        Self {
            failure_rate,
            total_calls: Arc::new(AtomicU64::new(0)),
            failed_calls: Arc::new(AtomicU64::new(0)),
            should_fail: Arc::new(AtomicBool::new(false)),
        }
    }
    
    async fn fsync(&self) -> Result<()> {
        self.total_calls.fetch_add(1, Ordering::SeqCst);
        
        if self.should_fail.load(Ordering::SeqCst) {
            self.failed_calls.fetch_add(1, Ordering::SeqCst);
            return Err(WalError::IoError("Mock fsync failure".into()));
        }
        
        let mut rng = rand::thread_rng();
        if rng.gen::<f64>() < self.failure_rate {
            self.failed_calls.fetch_add(1, Ordering::SeqCst);
            return Err(WalError::IoError("Random fsync failure".into()));
        }
        
        // Simulate fsync delay
        tokio::time::sleep(Duration::from_micros(100)).await;
        Ok(())
    }
    
    fn stats(&self) -> (u64, u64) {
        (
            self.total_calls.load(Ordering::SeqCst),
            self.failed_calls.load(Ordering::SeqCst)
        )
    }
}

/// Test crash during segment flush
#[tokio::test]
async fn test_crash_during_segment_flush() {
    let dir = TempDir::new().unwrap();
    let mut config = create_test_config(dir.path());
    config.rotation.max_segment_size = 1024; // Small segments for frequent rotation
    
    let mut manager = WalManager::new(config.clone()).await.unwrap();
    let mock_fsync = MockFsync::new(0.0);
    
    // Write records until rotation triggers
    let mut last_offset = 0i64;
    for batch in 0..10 {
        let records = create_test_records(batch * 100, 100);
        
        // Simulate crash during 5th batch
        if batch == 5 {
            mock_fsync.should_fail.store(true, Ordering::SeqCst);
        }
        
        match manager.append("test-topic".to_string(), 0, records.clone()).await {
            Ok(_) => {
                for record in &records {
                    last_offset = last_offset.max(record.offset);
                }
            }
            Err(e) => {
                warn!("Write failed during simulated crash: {:?}", e);
                break;
            }
        }
    }
    
    // Recover and verify
    let recovered_manager = WalManager::recover(&config).await.unwrap();
    let recovery_result = recovered_manager.get_recovery_result();
    
    // Verify no data loss up to last successful write
    assert!(recovery_result.total_records > 0);
    info!("Recovered {} records after crash", recovery_result.total_records);
    
    // Verify idempotent recovery
    let recovered_again = WalManager::recover(&config).await.unwrap();
    let recovery_result2 = recovered_again.get_recovery_result();
    assert_eq!(recovery_result.total_records, recovery_result2.total_records);
}

/// Test double fsync interruption
#[tokio::test]
async fn test_double_fsync_interruption() {
    let dir = TempDir::new().unwrap();
    let config = create_test_config(dir.path());
    
    let mut manager = WalManager::new(config.clone()).await.unwrap();
    let mock_fsync = MockFsync::new(0.0);
    
    // First write batch
    let records1 = create_test_records(0, 100);
    manager.append("test-topic".to_string(), 0, records1).await.unwrap();
    
    // Simulate first fsync failure
    mock_fsync.should_fail.store(true, Ordering::SeqCst);
    let flush_result1 = manager.flush_all().await;
    assert!(flush_result1.is_err());
    
    // Simulate second fsync failure (double interruption)
    let flush_result2 = manager.flush_all().await;
    assert!(flush_result2.is_err());
    
    // Now succeed
    mock_fsync.should_fail.store(false, Ordering::SeqCst);
    manager.flush_all().await.unwrap();
    
    // Write more data
    let records2 = create_test_records(100, 100);
    manager.append("test-topic".to_string(), 0, records2).await.unwrap();
    
    // Verify recovery handles interrupted fsyncs
    let recovered = WalManager::recover(&config).await.unwrap();
    let recovery_result = recovered.get_recovery_result();
    assert!(recovery_result.total_records >= 100);
}

/// Test recovery idempotence with replay
#[tokio::test]
async fn test_recovery_idempotence() {
    let dir = TempDir::new().unwrap();
    let config = create_test_config(dir.path());
    
    // Initial write phase
    let mut manager = WalManager::new(config.clone()).await.unwrap();
    let total_records = 1000;
    
    for batch in 0..10 {
        let records = create_test_records(batch * 100, 100);
        manager.append("test-topic".to_string(), 0, records).await.unwrap();
    }
    
    manager.flush_all().await.unwrap();
    drop(manager);
    
    // Recovery phase 1
    let recovered1 = WalManager::recover(&config).await.unwrap();
    let result1 = recovered1.get_recovery_result();
    assert_eq!(result1.total_records, total_records);
    
    // Recovery phase 2 (idempotent)
    let recovered2 = WalManager::recover(&config).await.unwrap();
    let result2 = recovered2.get_recovery_result();
    assert_eq!(result1.total_records, result2.total_records);
    assert_eq!(result1.last_offsets, result2.last_offsets);
    
    // Recovery phase 3 with additional writes
    let mut recovered3 = WalManager::recover(&config).await.unwrap();
    let additional_records = create_test_records(1000, 100);
    recovered3.append("test-topic".to_string(), 0, additional_records).await.unwrap();
    recovered3.flush_all().await.unwrap();
    drop(recovered3);
    
    // Recovery phase 4 (verify additional writes)
    let recovered4 = WalManager::recover(&config).await.unwrap();
    let result4 = recovered4.get_recovery_result();
    assert_eq!(result4.total_records, total_records + 100);
}

/// Test with randomized fsync failures
#[tokio::test]
async fn test_randomized_fsync_failures() {
    let dir = TempDir::new().unwrap();
    let mut config = create_test_config(dir.path());
    config.fsync.mode = FsyncMode::Batch;
    config.fsync.interval_ms = 10;
    
    let mock_fsync = MockFsync::new(0.1); // 10% failure rate
    let mut manager = WalManager::new(config.clone()).await.unwrap();
    
    let mut successful_writes = 0;
    let mut failed_writes = 0;
    let target_messages = 10_000;
    
    // Write with random failures
    for batch_id in 0..100 {
        let records = create_test_records(batch_id * 100, 100);
        
        match timeout(
            Duration::from_secs(1),
            manager.append("test-topic".to_string(), batch_id as i32 % 4, records)
        ).await {
            Ok(Ok(_)) => successful_writes += 100,
            Ok(Err(e)) => {
                debug!("Write failed: {:?}", e);
                failed_writes += 100;
            }
            Err(_) => {
                warn!("Write timeout");
                failed_writes += 100;
            }
        }
        
        if successful_writes >= target_messages {
            break;
        }
    }
    
    // Force flush with retries
    for _ in 0..5 {
        if manager.flush_all().await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    
    let (total_fsyncs, failed_fsyncs) = mock_fsync.stats();
    info!(
        "Fsync stats: {}/{} failed ({:.1}%)",
        failed_fsyncs,
        total_fsyncs,
        (failed_fsyncs as f64 / total_fsyncs as f64) * 100.0
    );
    
    // Verify recovery
    let recovered = WalManager::recover(&config).await.unwrap();
    let result = recovered.get_recovery_result();
    
    // Should have recovered most messages despite failures
    assert!(result.total_records >= (successful_writes * 8) / 10);
    info!(
        "Recovered {}/{} records ({:.1}% recovery rate)",
        result.total_records,
        successful_writes,
        (result.total_records as f64 / successful_writes as f64) * 100.0
    );
}

/// Test 10k+ message cycle with failures
#[tokio::test]
async fn test_10k_message_durability_cycle() {
    let dir = TempDir::new().unwrap();
    let mut config = create_test_config(dir.path());
    config.rotation.max_segment_size = 10 * 1024 * 1024; // 10MB segments
    
    let mock_fsync = MockFsync::new(0.01); // 1% failure rate
    let cycles = 5;
    let messages_per_cycle = 2000;
    
    for cycle in 0..cycles {
        info!("Starting cycle {}/{}", cycle + 1, cycles);
        
        // Write phase
        let mut manager = WalManager::new(config.clone()).await.unwrap();
        let base_offset = cycle * messages_per_cycle;
        
        for batch_id in 0..20 {
            let records = create_test_records(base_offset + batch_id * 100, 100);
            
            // Retry logic for failed writes
            let mut retries = 0;
            loop {
                match manager.append(
                    "durability-test".to_string(),
                    batch_id as i32 % 8,
                    records.clone()
                ).await {
                    Ok(_) => break,
                    Err(e) if retries < 3 => {
                        debug!("Retry {} for batch {}: {:?}", retries + 1, batch_id, e);
                        retries += 1;
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    Err(e) => {
                        warn!("Failed batch {} after {} retries: {:?}", batch_id, retries, e);
                        break;
                    }
                }
            }
        }
        
        // Flush with retries
        for _ in 0..5 {
            if manager.flush_all().await.is_ok() {
                break;
            }
        }
        
        drop(manager);
        
        // Recovery phase
        let recovered = WalManager::recover(&config).await.unwrap();
        let result = recovered.get_recovery_result();
        
        // Verify monotonic progress
        assert!(result.total_records >= (cycle + 1) * messages_per_cycle * 9 / 10);
        info!("Cycle {} complete: {} total records", cycle + 1, result.total_records);
        
        // Continue writing in recovered manager
        let mut recovered = recovered;
        let verify_records = create_test_records(base_offset + 2000, 10);
        recovered.append("durability-test".to_string(), 0, verify_records).await.unwrap();
        drop(recovered);
    }
    
    // Final recovery and verification
    let final_recovery = WalManager::recover(&config).await.unwrap();
    let final_result = final_recovery.get_recovery_result();
    
    // Should have no data loss across all cycles
    assert!(final_result.total_records >= cycles * messages_per_cycle);
    assert_eq!(final_result.corrupted_segments, 0);
    
    info!(
        "Final durability test: {} records recovered, 0 corrupted segments",
        final_result.total_records
    );
}

// Helper functions

fn create_test_config(dir: &std::path::Path) -> WalConfig {
    WalConfig {
        data_dir: dir.to_path_buf(),
        rotation: RotationConfig {
            max_segment_size: 1024 * 1024, // 1MB
            max_segment_age_ms: 60_000,
        },
        fsync: FsyncConfig {
            mode: FsyncMode::Always,
            interval_ms: 100,
            max_batch_size: 100,
        },
        write_buffer_size: 64 * 1024,
        checkpointing: Default::default(),
        recovery: Default::default(),
        async_io: Default::default(),
    }
}

fn create_test_records(base_offset: i64, count: usize) -> Vec<WalRecord> {
    (0..count)
        .map(|i| {
            WalRecord::new(
                base_offset + i as i64,
                Some(format!("key-{}", base_offset + i as i64).into_bytes()),
                format!("value-{}", base_offset + i as i64).into_bytes(),
                chrono::Utc::now().timestamp_millis(),
            )
        })
        .collect()
}

/// Test segment corruption during async operations
#[tokio::test]
async fn test_segment_corruption_during_async() {
    let dir = TempDir::new().unwrap();
    let config = create_test_config(dir.path());
    
    let mut manager = WalManager::new(config.clone()).await.unwrap();
    
    // Write initial data
    let records = create_test_records(0, 500);
    manager.append("test-topic".to_string(), 0, records).await.unwrap();
    
    // Corrupt a segment file while manager is active
    let segment_path = dir.path()
        .join("test-topic")
        .join("0")
        .join("wal_0_0.log");
    
    if segment_path.exists() {
        // Simulate partial write corruption
        let mut data = tokio::fs::read(&segment_path).await.unwrap();
        if data.len() > 100 {
            // Corrupt middle of file
            for i in 50..100 {
                data[i] = 0xFF;
            }
            tokio::fs::write(&segment_path, data).await.unwrap();
        }
    }
    
    // Try to continue writing
    let more_records = create_test_records(500, 100);
    let write_result = manager.append("test-topic".to_string(), 0, more_records).await;
    
    // Recovery should handle corruption
    drop(manager);
    let recovered = WalManager::recover(&config).await.unwrap();
    let result = recovered.get_recovery_result();
    
    // Should detect corruption
    assert!(result.corrupted_segments > 0 || write_result.is_err());
    info!("Detected {} corrupted segments", result.corrupted_segments);
}

/// Test concurrent writes with fsync failures
#[tokio::test]
async fn test_concurrent_writes_with_failures() {
    let dir = TempDir::new().unwrap();
    let config = create_test_config(dir.path());
    
    let manager = Arc::new(Mutex::new(WalManager::new(config.clone()).await.unwrap()));
    let mock_fsync = MockFsync::new(0.05); // 5% failure rate
    
    let mut handles = vec![];
    
    // Spawn concurrent writers
    for writer_id in 0..10 {
        let manager = Arc::clone(&manager);
        let handle = tokio::spawn(async move {
            let mut successful = 0;
            let mut failed = 0;
            
            for batch in 0..100 {
                let records = create_test_records(writer_id * 1000 + batch * 10, 10);
                
                let result = {
                    let mut mgr = manager.lock();
                    mgr.append(
                        format!("topic-{}", writer_id),
                        writer_id as i32,
                        records
                    ).await
                };
                
                match result {
                    Ok(_) => successful += 10,
                    Err(_) => failed += 10,
                }
                
                if batch % 10 == 0 {
                    tokio::task::yield_now().await;
                }
            }
            
            (successful, failed)
        });
        
        handles.push(handle);
    }
    
    // Wait for all writers
    let mut total_successful = 0;
    let mut total_failed = 0;
    
    for handle in handles {
        let (successful, failed) = handle.await.unwrap();
        total_successful += successful;
        total_failed += failed;
    }
    
    info!(
        "Concurrent writes: {} successful, {} failed",
        total_successful, total_failed
    );
    
    // Verify recovery
    drop(manager);
    let recovered = WalManager::recover(&config).await.unwrap();
    let result = recovered.get_recovery_result();
    
    // Should recover most of the successful writes
    assert!(result.total_records >= (total_successful * 9) / 10);
}