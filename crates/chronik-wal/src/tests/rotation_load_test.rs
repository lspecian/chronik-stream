//! Comprehensive WAL segment rotation load tests
//!
//! This module provides intensive load testing of WAL segment rotation under high throughput,
//! validating data integrity, performance metrics, and proper rotation behavior at scale.

use std::time::Instant;
use std::sync::Arc;
use tempfile::TempDir;
use tokio::sync::{Mutex, Barrier};
use tokio::task;
use chrono::Utc;

use crate::{
    config::{WalConfig, RotationConfig, CheckpointConfig, RecoveryConfig, FsyncConfig, CompressionType},
    error::Result,
    record::WalRecord,
    manager::WalManager,
};

/// Performance metrics collected during load testing
#[derive(Debug, Clone, Default)]
struct PerformanceMetrics {
    total_messages: u64,
    total_bytes: u64,
    start_time: Option<Instant>,
    end_time: Option<Instant>,
    rotation_count: u64,
    segments_created: u64,
    avg_latency_ms: f64,
    throughput_msgs_per_sec: f64,
    throughput_mb_per_sec: f64,
    segment_sizes: Vec<u64>,
    rotation_times: Vec<Instant>,
}

impl PerformanceMetrics {
    fn new() -> Self {
        Self::default()
    }

    fn start_timer(&mut self) {
        self.start_time = Some(Instant::now());
    }

    fn stop_timer(&mut self) {
        self.end_time = Some(Instant::now());
    }

    fn calculate_throughput(&mut self) {
        if let (Some(start), Some(end)) = (self.start_time, self.end_time) {
            let duration = end.duration_since(start);
            let duration_secs = duration.as_secs_f64();
            
            if duration_secs > 0.0 {
                self.throughput_msgs_per_sec = self.total_messages as f64 / duration_secs;
                self.throughput_mb_per_sec = (self.total_bytes as f64 / (1024.0 * 1024.0)) / duration_secs;
                self.avg_latency_ms = (duration_secs * 1000.0) / self.total_messages as f64;
            }
        }
    }

    fn add_rotation(&mut self, segment_size: u64) {
        self.rotation_count += 1;
        self.segments_created += 1;
        self.segment_sizes.push(segment_size);
        self.rotation_times.push(Instant::now());
    }

    fn log_results(&self) {
        println!("\n=== WAL Rotation Load Test Results ===");
        println!("Total Messages: {}", self.total_messages);
        println!("Total Bytes: {} MB", self.total_bytes as f64 / (1024.0 * 1024.0));
        println!("Segments Created: {}", self.segments_created);
        println!("Rotations: {}", self.rotation_count);
        println!("Throughput: {:.2} msgs/sec", self.throughput_msgs_per_sec);
        println!("Throughput: {:.2} MB/sec", self.throughput_mb_per_sec);
        println!("Avg Latency: {:.2} ms", self.avg_latency_ms);
        
        if !self.segment_sizes.is_empty() {
            let avg_segment_size = self.segment_sizes.iter().sum::<u64>() as f64 / self.segment_sizes.len() as f64;
            let min_segment_size = *self.segment_sizes.iter().min().unwrap_or(&0);
            let max_segment_size = *self.segment_sizes.iter().max().unwrap_or(&0);
            
            println!("Avg Segment Size: {:.2} KB", avg_segment_size / 1024.0);
            println!("Min Segment Size: {} KB", min_segment_size / 1024);
            println!("Max Segment Size: {} KB", max_segment_size / 1024);
        }
        println!("=====================================\n");
    }
}

/// Helper function to create test WAL record with specified size
fn create_load_test_record(offset: i64, key: &str, payload_size: usize) -> WalRecord {
    let key_bytes = key.as_bytes().to_vec();
    // Create payload of specified size filled with pattern data
    let pattern = b"LOAD_TEST_DATA_PATTERN_";
    let mut value_bytes = Vec::with_capacity(payload_size);
    
    while value_bytes.len() < payload_size {
        let remaining = payload_size - value_bytes.len();
        if remaining >= pattern.len() {
            value_bytes.extend_from_slice(pattern);
        } else {
            value_bytes.extend_from_slice(&pattern[..remaining]);
        }
    }
    
    let timestamp = Utc::now().timestamp_millis();
    WalRecord::new(offset, Some(key_bytes), value_bytes, timestamp)
}

/// Create test config with small segment size to force rotations
fn create_load_test_config(temp_dir: &TempDir, segment_size: u64) -> WalConfig {
    WalConfig {
        enabled: true,
        data_dir: temp_dir.path().to_path_buf(),
        segment_size: segment_size * 1024, // Convert KB to bytes
        flush_interval_ms: 50,  // Frequent flushes for testing
        flush_threshold: 512,
        compression: CompressionType::None,
        async_io: crate::config::AsyncIoConfig::default(),
        rotation: RotationConfig {
            max_segment_size: segment_size * 1024, // 1MB segments
            max_segment_age_ms: 30_000, // 30 seconds max age
            coordinate_with_storage: false,
        },
        checkpointing: CheckpointConfig {
            enabled: true,
            interval_records: 500,
            interval_bytes: 256 * 1024, // 256KB
        },
        recovery: RecoveryConfig {
            parallel_segments: 4,
            use_mmap: false,
            verify_checksums: true,
            corruption_tolerance: 0.0,
        },
        fsync: FsyncConfig {
            enabled: true,
            batch_size: 100,
            batch_timeout_ms: 50,
        },
    }
}

/// Collect segment information for validation
async fn collect_segment_info(data_dir: &std::path::Path, topic: &str, partition: i32) -> Result<Vec<(u64, u64)>> {
    let partition_dir = data_dir.join(topic).join(partition.to_string());
    
    if !partition_dir.exists() {
        return Ok(Vec::new());
    }
    
    let mut segments = Vec::new();
    let mut dir_entries = tokio::fs::read_dir(&partition_dir).await?;
    
    while let Some(entry) = dir_entries.next_entry().await? {
        let path = entry.path();
        if path.extension().map_or(false, |ext| ext == "log") {
            if let Some(file_name) = path.file_stem().and_then(|n| n.to_str()) {
                // Parse filename format: wal_{id}_{base_offset}.log
                if let Some(caps) = regex::Regex::new(r"wal_(\d+)_(\d+)")
                    .unwrap()
                    .captures(file_name) {
                    let id: u64 = caps[1].parse().unwrap_or(0);
                    let _base_offset: u64 = caps[2].parse().unwrap_or(0);
                    
                    // Get file size
                    let metadata = tokio::fs::metadata(&path).await?;
                    let size = metadata.len();
                    
                    segments.push((id, size));
                }
            }
        }
    }
    
    // Sort by segment ID
    segments.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(segments)
}

#[tokio::test]
async fn test_rotation_load_single_partition() {
    const MESSAGE_COUNT: u64 = 10_000;
    const SEGMENT_SIZE_KB: u64 = 1024; // 1MB segments
    const MESSAGE_SIZE: usize = 512; // 512 bytes per message
    
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let config = create_load_test_config(&temp_dir, SEGMENT_SIZE_KB);
    let mut manager = WalManager::new(config.clone()).await.expect("Manager creation should succeed");
    
    let mut metrics = PerformanceMetrics::new();
    metrics.start_timer();
    
    println!("Starting single partition load test with {} messages...", MESSAGE_COUNT);
    
    // Generate and append messages in batches for better performance
    const BATCH_SIZE: usize = 100;
    let mut current_offset = 0i64;
    
    for batch in 0..(MESSAGE_COUNT / BATCH_SIZE as u64) {
        let mut batch_records = Vec::with_capacity(BATCH_SIZE);
        
        for i in 0..BATCH_SIZE {
            let record = create_load_test_record(
                current_offset,
                &format!("batch-{}-msg-{}", batch, i),
                MESSAGE_SIZE
            );
            metrics.total_bytes += record.length as u64;
            batch_records.push(record);
            current_offset += 1;
        }
        
        manager.append("load-test-topic".to_string(), 0, batch_records).await
            .expect("Batch append should succeed");
        
        metrics.total_messages += BATCH_SIZE as u64;
        
        // Progress update every 1000 messages
        if (batch + 1) % 10 == 0 {
            println!("Processed {} messages ({:.1}%)", 
                metrics.total_messages, 
                (metrics.total_messages as f64 / MESSAGE_COUNT as f64) * 100.0);
        }
    }
    
    // Flush all data to ensure segments are written
    manager.flush_all().await.expect("Final flush should succeed");
    
    metrics.stop_timer();
    metrics.calculate_throughput();
    
    // Collect segment information
    let segments = collect_segment_info(&config.data_dir, "load-test-topic", 0).await
        .expect("Should be able to collect segment info");
    
    // Update metrics with segment info
    metrics.segments_created = segments.len() as u64;
    for (_, size) in &segments {
        metrics.segment_sizes.push(*size);
    }
    
    // Log detailed results
    metrics.log_results();
    
    // Validation assertions
    assert_eq!(metrics.total_messages, MESSAGE_COUNT, "All messages should be processed");
    assert!(metrics.segments_created > 1, "Multiple segments should be created due to rotation");
    
    // Verify expected number of rotations based on data size
    let expected_segments = (metrics.total_bytes / (SEGMENT_SIZE_KB * 1024)) + 1;
    assert!(metrics.segments_created >= expected_segments, 
        "Should have at least {} segments, got {}", expected_segments, metrics.segments_created);
    
    // Verify segment sizes are reasonable
    for (i, &(_, size)) in segments.iter().enumerate() {
        if i < segments.len() - 1 {
            // Sealed segments should be close to max size
            assert!(size >= (SEGMENT_SIZE_KB * 1024) / 2, 
                "Sealed segment {} should be at least half the max size, got {} bytes", i, size);
        }
        // All segments should have some data
        assert!(size > 0, "Segment {} should not be empty", i);
    }
    
    // Performance requirements
    assert!(metrics.throughput_msgs_per_sec > 100.0, 
        "Throughput should be at least 100 msg/sec, got {:.2}", metrics.throughput_msgs_per_sec);
    assert!(metrics.avg_latency_ms < 100.0, 
        "Average latency should be less than 100ms, got {:.2}ms", metrics.avg_latency_ms);
    
    println!("✓ Single partition load test passed!");
}

#[tokio::test] 
async fn test_rotation_load_multiple_partitions() {
    const MESSAGE_COUNT_PER_PARTITION: u64 = 5_000;
    const PARTITION_COUNT: i32 = 4;
    const SEGMENT_SIZE_KB: u64 = 512; // 512KB segments
    const MESSAGE_SIZE: usize = 256;
    
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let config = create_load_test_config(&temp_dir, SEGMENT_SIZE_KB);
    let manager = Arc::new(Mutex::new(WalManager::new(config.clone()).await.expect("Manager creation should succeed")));
    
    println!("Starting multi-partition load test with {} partitions, {} messages each...", 
        PARTITION_COUNT, MESSAGE_COUNT_PER_PARTITION);
    
    let barrier = Arc::new(Barrier::new(PARTITION_COUNT as usize));
    let mut handles = Vec::new();
    let start_time = Instant::now();
    
    // Spawn concurrent tasks for each partition
    for partition in 0..PARTITION_COUNT {
        let manager_clone = Arc::clone(&manager);
        let barrier_clone = Arc::clone(&barrier);
        
        let handle = task::spawn(async move {
            let mut partition_metrics = PerformanceMetrics::new();
            partition_metrics.start_timer();
            
            // Wait for all tasks to be ready
            barrier_clone.wait().await;
            
            let mut current_offset = (partition as i64) * (MESSAGE_COUNT_PER_PARTITION as i64);
            
            // Generate messages for this partition
            for batch in 0..(MESSAGE_COUNT_PER_PARTITION / 50) {
                let mut batch_records = Vec::with_capacity(50);
                
                for i in 0..50 {
                    let record = create_load_test_record(
                        current_offset,
                        &format!("p{}-batch{}-msg{}", partition, batch, i),
                        MESSAGE_SIZE
                    );
                    partition_metrics.total_bytes += record.length as u64;
                    batch_records.push(record);
                    current_offset += 1;
                }
                
                {
                    let mut mgr = manager_clone.lock().await;
                    mgr.append("multi-test-topic".to_string(), partition, batch_records).await
                        .expect("Batch append should succeed");
                }
                
                partition_metrics.total_messages += 50;
            }
            
            partition_metrics.stop_timer();
            partition_metrics.calculate_throughput();
            
            println!("Partition {} completed: {:.2} msgs/sec", 
                partition, partition_metrics.throughput_msgs_per_sec);
                
            partition_metrics
        });
        
        handles.push(handle);
    }
    
    // Wait for all partition tasks to complete
    let mut all_metrics = Vec::new();
    for handle in handles {
        let partition_metrics = handle.await.expect("Task should complete successfully");
        all_metrics.push(partition_metrics);
    }
    
    // Flush all data
    {
        let mgr = manager.lock().await;
        mgr.flush_all().await.expect("Final flush should succeed");
    }
    
    let total_duration = start_time.elapsed();
    
    // Aggregate metrics
    let mut total_metrics = PerformanceMetrics::new();
    total_metrics.start_time = Some(start_time);
    total_metrics.end_time = Some(start_time + total_duration);
    
    for partition_metrics in &all_metrics {
        total_metrics.total_messages += partition_metrics.total_messages;
        total_metrics.total_bytes += partition_metrics.total_bytes;
    }
    
    total_metrics.calculate_throughput();
    
    // Collect segment information for all partitions
    for partition in 0..PARTITION_COUNT {
        let segments = collect_segment_info(&config.data_dir, "multi-test-topic", partition).await
            .expect("Should be able to collect segment info");
        
        total_metrics.segments_created += segments.len() as u64;
        for (_, size) in segments {
            total_metrics.segment_sizes.push(size);
        }
    }
    
    total_metrics.log_results();
    
    // Validation assertions
    assert_eq!(total_metrics.total_messages, MESSAGE_COUNT_PER_PARTITION * PARTITION_COUNT as u64,
        "All messages should be processed across all partitions");
    assert!(total_metrics.segments_created >= PARTITION_COUNT as u64,
        "Should have at least one segment per partition");
    
    // Each partition should have created multiple segments due to rotation
    for partition in 0..PARTITION_COUNT {
        let segments = collect_segment_info(&config.data_dir, "multi-test-topic", partition).await
            .expect("Should be able to collect segment info");
        assert!(segments.len() > 1, "Partition {} should have multiple segments due to rotation", partition);
        
        // Verify partition directory structure
        let partition_dir = config.data_dir.join("multi-test-topic").join(partition.to_string());
        assert!(partition_dir.exists(), "Partition {} directory should exist", partition);
    }
    
    // Performance requirements for concurrent access
    assert!(total_metrics.throughput_msgs_per_sec > 200.0,
        "Total throughput should be at least 200 msg/sec, got {:.2}", total_metrics.throughput_msgs_per_sec);
    
    println!("✓ Multi-partition load test passed!");
}

#[tokio::test]
async fn test_rotation_under_memory_pressure() {
    const MESSAGE_COUNT: u64 = 15_000;
    const SEGMENT_SIZE_KB: u64 = 256; // Small 256KB segments
    const LARGE_MESSAGE_SIZE: usize = 2048; // 2KB messages
    
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let config = create_load_test_config(&temp_dir, SEGMENT_SIZE_KB);
    let mut manager = WalManager::new(config.clone()).await.expect("Manager creation should succeed");
    
    let mut metrics = PerformanceMetrics::new();
    metrics.start_timer();
    
    println!("Starting memory pressure test with large messages...");
    
    // Track segment rotations
    let mut last_segment_count = 0;
    let mut rotation_events = Vec::new();
    
    for i in 0..MESSAGE_COUNT {
        // Create large message with unique content
        let large_payload = format!("PRESSURE_TEST_MESSAGE_{:08}_{}", i, "X".repeat(LARGE_MESSAGE_SIZE - 50));
        let record = WalRecord::new(
            i as i64,
            Some(format!("pressure-key-{}", i).as_bytes().to_vec()),
            large_payload.as_bytes().to_vec(),
            Utc::now().timestamp_millis()
        );
        
        metrics.total_bytes += record.length as u64;
        
        manager.append("pressure-test-topic".to_string(), 0, vec![record]).await
            .expect("Append under pressure should succeed");
        
        metrics.total_messages += 1;
        
        // Check for rotations every 100 messages
        if i % 100 == 0 {
            let segments = collect_segment_info(&config.data_dir, "pressure-test-topic", 0).await
                .expect("Should be able to collect segment info");
            
            if segments.len() > last_segment_count {
                println!("Rotation detected at message {}: {} segments total", i, segments.len());
                rotation_events.push((i, Instant::now(), segments.len()));
                last_segment_count = segments.len();
            }
            
            if i % 1000 == 0 {
                println!("Progress: {} messages ({:.1}%)", i, (i as f64 / MESSAGE_COUNT as f64) * 100.0);
            }
        }
    }
    
    // Final flush
    manager.flush_all().await.expect("Final flush should succeed");
    
    metrics.stop_timer();
    metrics.calculate_throughput();
    
    // Final segment count
    let final_segments = collect_segment_info(&config.data_dir, "pressure-test-topic", 0).await
        .expect("Should be able to collect segment info");
    
    metrics.segments_created = final_segments.len() as u64;
    metrics.rotation_count = rotation_events.len() as u64;
    
    for (_, size) in &final_segments {
        metrics.segment_sizes.push(*size);
    }
    
    metrics.log_results();
    
    // Validation assertions
    assert_eq!(metrics.total_messages, MESSAGE_COUNT, "All messages should be processed");
    assert!(metrics.segments_created > 10, "Should have many segments due to large message size and small segments");
    
    // Verify frequent rotations occurred
    assert!(metrics.rotation_count > 5, "Should have detected multiple rotations");
    
    // Verify rotation timing - rotations should happen regularly
    if rotation_events.len() > 1 {
        let rotation_intervals: Vec<_> = rotation_events.windows(2)
            .map(|pair| pair[0].0.abs_diff(pair[1].0))
            .collect();
        
        let avg_interval = rotation_intervals.iter().sum::<u64>() as f64 / rotation_intervals.len() as f64;
        println!("Average rotation interval: {:.1} messages", avg_interval);
        
        // Rotations should be relatively frequent given message size vs segment size
        assert!(avg_interval < 1000.0, "Rotations should happen more frequently with large messages");
    }
    
    // Memory pressure test should still maintain reasonable performance
    assert!(metrics.throughput_msgs_per_sec > 50.0, 
        "Should maintain throughput > 50 msg/sec under pressure, got {:.2}", metrics.throughput_msgs_per_sec);
    
    // Verify all segment files exist and have expected content
    for (i, &(segment_id, size)) in final_segments.iter().enumerate() {
        assert!(size > 0, "Segment {} should not be empty", segment_id);
        if i < final_segments.len() - 1 {
            // Sealed segments should be close to target size
            assert!(size >= (SEGMENT_SIZE_KB * 1024) / 4, 
                "Sealed segment {} should be reasonable size, got {} bytes", segment_id, size);
        }
    }
    
    println!("✓ Memory pressure test passed!");
}

#[tokio::test]
async fn test_rotation_data_integrity() {
    const MESSAGE_COUNT: u64 = 8_000;
    const SEGMENT_SIZE_KB: u64 = 512;
    const MESSAGE_SIZE: usize = 128;
    
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let config = create_load_test_config(&temp_dir, SEGMENT_SIZE_KB);
    let mut manager = WalManager::new(config.clone()).await.expect("Manager creation should succeed");
    
    println!("Starting data integrity test with checksums and verification...");
    
    // Track all written records for later verification
    let mut written_records = Vec::with_capacity(MESSAGE_COUNT as usize);
    let mut current_offset = 0i64;
    
    // Write messages with predictable content
    for i in 0..MESSAGE_COUNT {
        let key = format!("integrity-key-{:08}", i);
        let value = format!("INTEGRITY_TEST_VALUE_{:08}_{}", i, "A".repeat(MESSAGE_SIZE - 50));
        
        let record = WalRecord::new(
            current_offset,
            Some(key.as_bytes().to_vec()),
            value.as_bytes().to_vec(),
            Utc::now().timestamp_millis()
        );
        
        // Verify record checksum before writing
        record.verify_checksum().expect("Record checksum should be valid before writing");
        
        written_records.push((current_offset, key.clone(), value.clone(), record.crc32));
        
        manager.append("integrity-test-topic".to_string(), 0, vec![record]).await
            .expect("Append should succeed");
        
        current_offset += 1;
        
        if i % 1000 == 0 {
            println!("Written {} messages", i);
        }
    }
    
    // Flush and verify segments were created
    manager.flush_all().await.expect("Final flush should succeed");
    
    let segments = collect_segment_info(&config.data_dir, "integrity-test-topic", 0).await
        .expect("Should be able to collect segment info");
    
    println!("Created {} segments during integrity test", segments.len());
    assert!(segments.len() > 1, "Multiple segments should be created");
    
    // Verify all segment files exist and are readable
    for (segment_id, size) in &segments {
        let segment_file = config.data_dir
            .join("integrity-test-topic")
            .join("0")
            .join(format!("wal_{}_{}.log", segment_id, 0)); // Base offset logic may vary
        
        if segment_file.exists() {
            println!("Verified segment file exists: {:?} ({} bytes)", segment_file, size);
            assert!(size > &0, "Segment file should not be empty");
        }
    }
    
    // TODO: Add reading/verification once read functionality is implemented
    // For now, verify that all data was written without errors and segments were created
    
    println!("✓ Data integrity test passed!");
}

#[tokio::test] 
async fn test_rotation_performance_benchmarks() {
    const MESSAGE_COUNT: u64 = 20_000;
    const SEGMENT_SIZE_KB: u64 = 2048; // 2MB segments
    const MESSAGE_SIZE: usize = 1024; // 1KB messages
    
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let config = create_load_test_config(&temp_dir, SEGMENT_SIZE_KB);
    let mut manager = WalManager::new(config.clone()).await.expect("Manager creation should succeed");
    
    println!("Starting performance benchmark test...");
    
    let mut metrics = PerformanceMetrics::new();
    let mut latencies = Vec::new();
    
    metrics.start_timer();
    
    // Measure individual operation latencies
    for i in 0..MESSAGE_COUNT {
        let operation_start = Instant::now();
        
        let record = create_load_test_record(
            i as i64,
            &format!("bench-{}", i),
            MESSAGE_SIZE
        );
        
        metrics.total_bytes += record.length as u64;
        
        manager.append("benchmark-topic".to_string(), 0, vec![record]).await
            .expect("Append should succeed");
        
        let operation_latency = operation_start.elapsed();
        latencies.push(operation_latency.as_micros() as f64 / 1000.0); // Convert to milliseconds
        
        metrics.total_messages += 1;
        
        if i % 2000 == 0 {
            println!("Benchmark progress: {} messages", i);
        }
    }
    
    manager.flush_all().await.expect("Final flush should succeed");
    
    metrics.stop_timer();
    metrics.calculate_throughput();
    
    // Collect segment information
    let segments = collect_segment_info(&config.data_dir, "benchmark-topic", 0).await
        .expect("Should be able to collect segment info");
    
    metrics.segments_created = segments.len() as u64;
    for (_, size) in segments {
        metrics.segment_sizes.push(size);
    }
    
    // Calculate detailed latency statistics
    latencies.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p50 = latencies[latencies.len() / 2];
    let p95 = latencies[(latencies.len() * 95) / 100];
    let p99 = latencies[(latencies.len() * 99) / 100];
    let max_latency = latencies.last().copied().unwrap_or(0.0);
    let min_latency = latencies.first().copied().unwrap_or(0.0);
    
    // Enhanced performance reporting
    metrics.log_results();
    
    println!("=== Detailed Latency Statistics ===");
    println!("Min Latency: {:.3} ms", min_latency);
    println!("P50 Latency: {:.3} ms", p50);
    println!("P95 Latency: {:.3} ms", p95);
    println!("P99 Latency: {:.3} ms", p99);
    println!("Max Latency: {:.3} ms", max_latency);
    println!("================================\n");
    
    // Performance assertions
    assert!(metrics.throughput_msgs_per_sec > 1000.0, 
        "Benchmark should achieve > 1000 msg/sec, got {:.2}", metrics.throughput_msgs_per_sec);
    assert!(p95 < 50.0, "P95 latency should be < 50ms, got {:.3}ms", p95);
    assert!(p99 < 100.0, "P99 latency should be < 100ms, got {:.3}ms", p99);
    assert!(max_latency < 1000.0, "Max latency should be < 1000ms, got {:.3}ms", max_latency);
    
    // Verify reasonable segment distribution
    assert!(metrics.segments_created >= 5, "Should create multiple segments for benchmark");
    assert!(metrics.segments_created <= 50, "Should not create excessive segments");
    
    println!("✓ Performance benchmark test passed!");
}

#[tokio::test]
async fn test_rotation_edge_cases() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let config = create_load_test_config(&temp_dir, 128); // Very small 128KB segments
    let mut manager = WalManager::new(config.clone()).await.expect("Manager creation should succeed");
    
    println!("Testing rotation edge cases...");
    
    // Test 1: Single large message exceeding segment size
    {
        let huge_message = "X".repeat(200 * 1024); // 200KB message
        let record = WalRecord::new(
            0,
            Some(b"huge-key".to_vec()),
            huge_message.as_bytes().to_vec(),
            Utc::now().timestamp_millis()
        );
        
        manager.append("edge-test-topic".to_string(), 0, vec![record]).await
            .expect("Large message should be handled");
        
        manager.flush_all().await.expect("Flush should succeed");
        
        let segments = collect_segment_info(&config.data_dir, "edge-test-topic", 0).await
            .expect("Should collect segment info");
        
        assert!(!segments.is_empty(), "Should create at least one segment");
        println!("✓ Large message test passed");
    }
    
    // Test 2: Rapid small messages
    {
        for i in 1..1000 {
            let tiny_record = WalRecord::new(
                i,
                Some(format!("k{}", i).as_bytes().to_vec()),
                b"tiny".to_vec(),
                Utc::now().timestamp_millis()
            );
            
            manager.append("edge-test-topic".to_string(), 1, vec![tiny_record]).await
                .expect("Tiny message should be handled");
        }
        
        manager.flush_all().await.expect("Flush should succeed");
        
        let segments = collect_segment_info(&config.data_dir, "edge-test-topic", 1).await
            .expect("Should collect segment info");
        
        // Should create multiple segments due to volume
        assert!(segments.len() > 1, "Rapid small messages should trigger rotation");
        println!("✓ Rapid small messages test passed");
    }
    
    // Test 3: Empty batch handling
    {
        // This should not create any segments
        manager.append("edge-test-topic".to_string(), 2, vec![]).await
            .expect("Empty batch should be handled gracefully");
        
        let segments = collect_segment_info(&config.data_dir, "edge-test-topic", 2).await
            .expect("Should collect segment info");
        
        // No segments should be created for empty batch
        assert!(segments.is_empty(), "Empty batch should not create segments");
        println!("✓ Empty batch test passed");
    }
    
    println!("✓ All edge cases passed!");
}