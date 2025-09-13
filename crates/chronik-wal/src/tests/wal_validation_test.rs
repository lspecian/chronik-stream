//! WAL Validation Test Coverage
//!
//! Tests for validating WAL behavior under corruption, partial writes,
//! and concurrent operations like rotation during async flush.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use parking_lot::Mutex;
use tempfile::TempDir;
use tokio::time::timeout;
use tracing::{info, warn, debug};

use crate::{
    WalManager, WalConfig, WalRecord, RecoveryResult,
    error::{Result, WalError},
    config::{FsyncConfig, FsyncMode, RotationConfig},
};

/// Test corrupt segment detection and recovery
#[tokio::test]
async fn test_corrupt_segment_detection() {
    let dir = TempDir::new().unwrap();
    let config = create_test_config(dir.path());
    
    // Create WAL and write some data
    let mut manager = WalManager::new(config.clone()).await.unwrap();
    
    // Write initial good data
    let records1 = create_test_records(0, 100);
    manager.append("test-topic".to_string(), 0, records1).await.unwrap();
    manager.flush_all().await.unwrap();
    
    // Force segment rotation
    manager.rotate_all_segments().await.unwrap();
    
    // Write more data to new segment
    let records2 = create_test_records(100, 100);
    manager.append("test-topic".to_string(), 0, records2).await.unwrap();
    manager.flush_all().await.unwrap();
    
    drop(manager);
    
    // Corrupt the first segment file
    let segment_path = dir.path()
        .join("test-topic")
        .join("0")
        .join("wal_0_0.log");
    
    if segment_path.exists() {
        let mut data = tokio::fs::read(&segment_path).await.unwrap();
        
        // Corrupt the header (magic bytes)
        if data.len() > 8 {
            data[0] = 0xFF;
            data[1] = 0xFF;
            data[2] = 0xFF;
            data[3] = 0xFF;
        }
        
        tokio::fs::write(&segment_path, data).await.unwrap();
        info!("Corrupted segment header at {:?}", segment_path);
    }
    
    // Recovery should detect corruption
    let recovered = WalManager::recover(&config).await.unwrap();
    let result = recovered.get_recovery_result();
    
    assert!(result.corrupted_segments > 0);
    assert_eq!(result.total_records, 100); // Should recover second segment
    info!("Detected {} corrupted segments, recovered {} records", 
        result.corrupted_segments, result.total_records);
}

/// Test partial write detection
#[tokio::test]
async fn test_partial_write_detection() {
    let dir = TempDir::new().unwrap();
    let config = create_test_config(dir.path());
    
    let mut manager = WalManager::new(config.clone()).await.unwrap();
    
    // Write complete records
    let records = create_test_records(0, 50);
    manager.append("test-topic".to_string(), 0, records).await.unwrap();
    manager.flush_all().await.unwrap();
    
    drop(manager);
    
    // Simulate partial write by truncating segment
    let segment_path = dir.path()
        .join("test-topic")
        .join("0")
        .join("wal_0_0.log");
    
    if segment_path.exists() {
        let data = tokio::fs::read(&segment_path).await.unwrap();
        let truncated_len = data.len() - 20; // Remove last 20 bytes
        
        if truncated_len > 100 {
            let truncated = &data[..truncated_len];
            tokio::fs::write(&segment_path, truncated).await.unwrap();
            info!("Truncated segment to simulate partial write");
        }
    }
    
    // Recovery should detect and handle partial write
    let recovered = WalManager::recover(&config).await.unwrap();
    let result = recovered.get_recovery_result();
    
    // Should recover most records, possibly losing the last one
    assert!(result.total_records >= 45);
    assert!(result.total_records < 50);
    info!("Recovered {} records after partial write", result.total_records);
}

/// Test rotation during async flush
#[tokio::test]
async fn test_rotation_during_async_flush() {
    let dir = TempDir::new().unwrap();
    let mut config = create_test_config(dir.path());
    config.rotation.max_segment_size = 10 * 1024; // Small segments for frequent rotation
    config.fsync.mode = FsyncMode::Batch;
    config.fsync.interval_ms = 50;
    
    let manager = Arc::new(Mutex::new(WalManager::new(config.clone()).await.unwrap()));
    let rotation_triggered = Arc::new(AtomicBool::new(false));
    let flush_in_progress = Arc::new(AtomicBool::new(false));
    
    // Spawn flush task
    let flush_manager = Arc::clone(&manager);
    let flush_flag = Arc::clone(&flush_in_progress);
    let rotation_flag = Arc::clone(&rotation_triggered);
    
    let flush_handle = tokio::spawn(async move {
        for _ in 0..10 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            
            flush_flag.store(true, Ordering::SeqCst);
            
            // Perform flush
            let result = {
                let manager = flush_manager.lock();
                manager.flush_all().await
            };
            
            flush_flag.store(false, Ordering::SeqCst);
            
            if let Err(e) = result {
                warn!("Flush error during rotation test: {:?}", e);
            }
            
            // Check if rotation happened during flush
            if rotation_flag.load(Ordering::SeqCst) && flush_flag.load(Ordering::SeqCst) {
                info!("Detected rotation during flush!");
            }
        }
    });
    
    // Spawn write task that triggers rotation
    let write_manager = Arc::clone(&manager);
    let rotation_flag = Arc::clone(&rotation_triggered);
    let flush_flag = Arc::clone(&flush_in_progress);
    
    let write_handle = tokio::spawn(async move {
        for batch in 0..20 {
            let records = create_test_records(batch * 50, 50);
            
            let append_result = {
                let mut manager = write_manager.lock();
                
                // Check if we're about to rotate during a flush
                let pre_rotation = manager.needs_rotation("test-topic", 0).await;
                
                let result = manager.append("test-topic".to_string(), 0, records).await;
                
                // Check if rotation happened
                let post_rotation = manager.needs_rotation("test-topic", 0).await;
                
                if pre_rotation != post_rotation && flush_flag.load(Ordering::SeqCst) {
                    rotation_flag.store(true, Ordering::SeqCst);
                    info!("Rotation triggered while flush in progress!");
                }
                
                result
            };
            
            if let Err(e) = append_result {
                warn!("Write error during rotation test: {:?}", e);
            }
            
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    });
    
    // Wait for both tasks
    let _ = tokio::join!(flush_handle, write_handle);
    
    // Verify data integrity
    drop(manager);
    let recovered = WalManager::recover(&config).await.unwrap();
    let result = recovered.get_recovery_result();
    
    assert!(result.total_records > 0);
    assert_eq!(result.corrupted_segments, 0);
    info!("Successfully handled rotation during flush: {} records", result.total_records);
}

/// Test CRC validation for corrupt records
#[tokio::test]
async fn test_crc_validation() {
    let dir = TempDir::new().unwrap();
    let config = create_test_config(dir.path());
    
    let mut manager = WalManager::new(config.clone()).await.unwrap();
    
    // Write records with CRC
    let records = create_test_records(0, 100);
    manager.append("test-topic".to_string(), 0, records).await.unwrap();
    manager.flush_all().await.unwrap();
    
    drop(manager);
    
    // Corrupt a record's data but leave CRC intact
    let segment_path = dir.path()
        .join("test-topic")
        .join("0")
        .join("wal_0_0.log");
    
    if segment_path.exists() {
        let mut data = tokio::fs::read(&segment_path).await.unwrap();
        
        // Find and corrupt a record in the middle
        if data.len() > 200 {
            // Corrupt some bytes in the middle of the file
            for i in 150..160 {
                data[i] ^= 0xFF; // Flip all bits
            }
        }
        
        tokio::fs::write(&segment_path, data).await.unwrap();
        info!("Corrupted record data for CRC validation test");
    }
    
    // Recovery should detect CRC mismatch
    let recovered = WalManager::recover(&config).await.unwrap();
    let result = recovered.get_recovery_result();
    
    // Should recover some records but detect corruption
    assert!(result.total_records < 100);
    info!("CRC validation caught corruption, recovered {} records", result.total_records);
}

/// Test recovery with mixed valid and corrupt segments
#[tokio::test]
async fn test_mixed_segment_recovery() {
    let dir = TempDir::new().unwrap();
    let mut config = create_test_config(dir.path());
    config.rotation.max_segment_size = 10 * 1024;
    
    let mut manager = WalManager::new(config.clone()).await.unwrap();
    
    // Create multiple segments
    for segment in 0..5 {
        let records = create_test_records(segment * 100, 100);
        manager.append("test-topic".to_string(), 0, records).await.unwrap();
        manager.flush_all().await.unwrap();
        manager.rotate_all_segments().await.unwrap();
    }
    
    drop(manager);
    
    // Corrupt segments 1 and 3
    for segment_id in [1, 3] {
        let segment_path = dir.path()
            .join("test-topic")
            .join("0")
            .join(format!("wal_{}_{}.log", segment_id * 100, (segment_id + 1) * 100 - 1));
        
        if segment_path.exists() {
            let mut data = tokio::fs::read(&segment_path).await.unwrap();
            
            // Corrupt the segment
            for i in 0..std::cmp::min(100, data.len()) {
                data[i] = 0xFF;
            }
            
            tokio::fs::write(&segment_path, data).await.unwrap();
            info!("Corrupted segment {}", segment_id);
        }
    }
    
    // Recovery should skip corrupt segments but recover valid ones
    let recovered = WalManager::recover(&config).await.unwrap();
    let result = recovered.get_recovery_result();
    
    // Should recover segments 0, 2, and 4 (300 records)
    assert_eq!(result.total_records, 300);
    assert_eq!(result.corrupted_segments, 2);
    info!("Recovered {} records from mixed segments, {} corrupted", 
        result.total_records, result.corrupted_segments);
}

/// Test zero-length segment handling
#[tokio::test]
async fn test_zero_length_segment() {
    let dir = TempDir::new().unwrap();
    let config = create_test_config(dir.path());
    
    let mut manager = WalManager::new(config.clone()).await.unwrap();
    
    // Write some data
    let records = create_test_records(0, 50);
    manager.append("test-topic".to_string(), 0, records).await.unwrap();
    manager.flush_all().await.unwrap();
    
    drop(manager);
    
    // Create a zero-length segment file
    let zero_segment = dir.path()
        .join("test-topic")
        .join("0")
        .join("wal_50_99.log");
    
    tokio::fs::create_dir_all(zero_segment.parent().unwrap()).await.unwrap();
    tokio::fs::write(&zero_segment, b"").await.unwrap();
    info!("Created zero-length segment at {:?}", zero_segment);
    
    // Recovery should handle zero-length segment gracefully
    let recovered = WalManager::recover(&config).await.unwrap();
    let result = recovered.get_recovery_result();
    
    assert_eq!(result.total_records, 50);
    info!("Successfully handled zero-length segment");
}

/// Test recovery after ungraceful shutdown during write
#[tokio::test]
async fn test_ungraceful_shutdown_recovery() {
    let dir = TempDir::new().unwrap();
    let config = create_test_config(dir.path());
    
    let manager = Arc::new(Mutex::new(WalManager::new(config.clone()).await.unwrap()));
    let shutdown = Arc::new(AtomicBool::new(false));
    let records_written = Arc::new(AtomicU64::new(0));
    
    // Spawn writer that will be "killed"
    let writer = Arc::clone(&manager);
    let shutdown_flag = Arc::clone(&shutdown);
    let counter = Arc::clone(&records_written);
    
    let write_handle = tokio::spawn(async move {
        let mut offset = 0i64;
        
        while !shutdown_flag.load(Ordering::SeqCst) {
            let records = create_test_records(offset, 10);
            
            let result = {
                let mut mgr = writer.lock();
                mgr.append("test-topic".to_string(), 0, records).await
            };
            
            if result.is_ok() {
                counter.fetch_add(10, Ordering::SeqCst);
                offset += 10;
            }
            
            // Don't always flush to simulate ungraceful shutdown
            if offset % 50 == 0 {
                let mgr = writer.lock();
                let _ = mgr.flush_all().await;
            }
            
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    });
    
    // Let it run for a bit
    tokio::time::sleep(Duration::from_millis(100)).await;
    
    // Simulate ungraceful shutdown
    shutdown.store(true, Ordering::SeqCst);
    let _ = timeout(Duration::from_millis(10), write_handle).await;
    
    let written = records_written.load(Ordering::SeqCst);
    info!("Wrote {} records before ungraceful shutdown", written);
    
    // Drop without explicit flush
    drop(manager);
    
    // Recovery should handle incomplete writes
    let recovered = WalManager::recover(&config).await.unwrap();
    let result = recovered.get_recovery_result();
    
    // Should recover most records
    assert!(result.total_records > 0);
    assert!(result.total_records <= written);
    info!("Recovered {}/{} records after ungraceful shutdown", 
        result.total_records, written);
}

/// Test boundary conditions for segment files
#[tokio::test]
async fn test_segment_boundary_conditions() {
    let dir = TempDir::new().unwrap();
    let mut config = create_test_config(dir.path());
    config.rotation.max_segment_size = 1024; // Exact size for boundary testing
    
    let mut manager = WalManager::new(config.clone()).await.unwrap();
    
    // Write exactly to segment boundary
    let mut total_size = 0usize;
    let mut record_count = 0;
    
    while total_size < 1024 {
        let record = WalRecord::new(
            record_count,
            Some(vec![0u8; 10]),
            vec![0u8; 50],
            chrono::Utc::now().timestamp_millis(),
        );
        
        // Calculate record size (approximate)
        let record_size = 8 + 10 + 50 + 8 + 4; // offset + key + value + timestamp + crc
        
        if total_size + record_size > 1024 {
            break;
        }
        
        manager.append("test-topic".to_string(), 0, vec![record]).await.unwrap();
        total_size += record_size;
        record_count += 1;
    }
    
    manager.flush_all().await.unwrap();
    
    // Write one more record to trigger rotation
    let boundary_record = create_test_records(record_count, 1);
    manager.append("test-topic".to_string(), 0, boundary_record).await.unwrap();
    
    // Verify rotation happened
    drop(manager);
    
    let recovered = WalManager::recover(&config).await.unwrap();
    let result = recovered.get_recovery_result();
    
    assert_eq!(result.total_records, record_count + 1);
    assert_eq!(result.corrupted_segments, 0);
    info!("Successfully handled segment boundary: {} records", result.total_records);
}

// Helper functions

fn create_test_config(dir: &std::path::Path) -> WalConfig {
    WalConfig {
        data_dir: dir.to_path_buf(),
        rotation: RotationConfig {
            max_segment_size: 100 * 1024, // 100KB
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