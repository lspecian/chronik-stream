//! Benchmark for async I/O implementations
//!
//! Compares performance between standard async I/O and io_uring

use crate::async_io::{AsyncFile, AsyncIoBackend};
use anyhow::Result;
use bytes::Bytes;
use std::time::Instant;
use tempfile::tempdir;
use tracing::info;

const WRITE_SIZE: usize = 4096; // 4KB writes
const NUM_WRITES: usize = 10000; // 10k writes = 40MB total
const NUM_ITERATIONS: usize = 3; // Run benchmark 3 times

/// Benchmark async write performance
pub async fn benchmark_writes(backend: AsyncIoBackend) -> Result<BenchmarkResult> {
    let dir = tempdir()?;
    let path = dir.path().join("bench.wal");
    
    let file = AsyncFile::open(&path, backend.clone()).await?;
    
    // Prepare data
    let data = Bytes::from(vec![0x42u8; WRITE_SIZE]);
    
    let mut total_duration = std::time::Duration::ZERO;
    let mut write_latencies = Vec::with_capacity(NUM_WRITES);
    
    for iteration in 0..NUM_ITERATIONS {
        info!("Starting iteration {} with {:?} backend", iteration + 1, backend);
        
        let iteration_start = Instant::now();
        
        for i in 0..NUM_WRITES {
            let offset = (i * WRITE_SIZE) as u64;
            let write_start = Instant::now();
            
            file.write_at(data.clone(), offset).await?;
            
            let write_duration = write_start.elapsed();
            write_latencies.push(write_duration.as_micros() as f64);
            
            // Periodic fsync
            if i % 100 == 99 {
                file.sync().await?;
            }
        }
        
        // Final fsync
        file.sync().await?;
        
        let iteration_duration = iteration_start.elapsed();
        total_duration += iteration_duration;
        
        info!(
            "Iteration {} completed in {:?}",
            iteration + 1,
            iteration_duration
        );
    }
    
    // Calculate statistics
    let avg_duration = total_duration / NUM_ITERATIONS as u32;
    let throughput_mbps = (WRITE_SIZE * NUM_WRITES) as f64 / avg_duration.as_secs_f64() / 1_000_000.0;
    
    write_latencies.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p50 = write_latencies[write_latencies.len() / 2];
    let p95 = write_latencies[write_latencies.len() * 95 / 100];
    let p99 = write_latencies[write_latencies.len() * 99 / 100];
    
    Ok(BenchmarkResult {
        backend,
        total_writes: NUM_WRITES * NUM_ITERATIONS,
        avg_duration,
        throughput_mbps,
        p50_latency_us: p50,
        p95_latency_us: p95,
        p99_latency_us: p99,
    })
}

/// Benchmark async read performance
pub async fn benchmark_reads(backend: AsyncIoBackend) -> Result<BenchmarkResult> {
    let dir = tempdir()?;
    let path = dir.path().join("bench.wal");
    
    let file = AsyncFile::open(&path, backend.clone()).await?;
    
    // First write data to read back
    let data = Bytes::from(vec![0x42u8; WRITE_SIZE]);
    for i in 0..NUM_WRITES {
        let offset = (i * WRITE_SIZE) as u64;
        file.write_at(data.clone(), offset).await?;
    }
    file.sync().await?;
    
    let mut total_duration = std::time::Duration::ZERO;
    let mut read_latencies = Vec::with_capacity(NUM_WRITES);
    
    for iteration in 0..NUM_ITERATIONS {
        info!("Starting read iteration {} with {:?} backend", iteration + 1, backend);
        
        let iteration_start = Instant::now();
        
        for i in 0..NUM_WRITES {
            let offset = (i * WRITE_SIZE) as u64;
            let read_start = Instant::now();
            
            let _ = file.read_at(offset, WRITE_SIZE).await?;
            
            let read_duration = read_start.elapsed();
            read_latencies.push(read_duration.as_micros() as f64);
        }
        
        let iteration_duration = iteration_start.elapsed();
        total_duration += iteration_duration;
        
        info!(
            "Read iteration {} completed in {:?}",
            iteration + 1,
            iteration_duration
        );
    }
    
    // Calculate statistics
    let avg_duration = total_duration / NUM_ITERATIONS as u32;
    let throughput_mbps = (WRITE_SIZE * NUM_WRITES) as f64 / avg_duration.as_secs_f64() / 1_000_000.0;
    
    read_latencies.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p50 = read_latencies[read_latencies.len() / 2];
    let p95 = read_latencies[read_latencies.len() * 95 / 100];
    let p99 = read_latencies[read_latencies.len() * 99 / 100];
    
    Ok(BenchmarkResult {
        backend,
        total_writes: 0, // This is a read benchmark
        avg_duration,
        throughput_mbps,
        p50_latency_us: p50,
        p95_latency_us: p95,
        p99_latency_us: p99,
    })
}

#[derive(Debug)]
pub struct BenchmarkResult {
    pub backend: AsyncIoBackend,
    pub total_writes: usize,
    pub avg_duration: std::time::Duration,
    pub throughput_mbps: f64,
    pub p50_latency_us: f64,
    pub p95_latency_us: f64,
    pub p99_latency_us: f64,
}

impl BenchmarkResult {
    pub fn print_summary(&self) {
        println!("\n=== Benchmark Results for {:?} ===", self.backend);
        println!("Average duration: {:?}", self.avg_duration);
        println!("Throughput: {:.2} MB/s", self.throughput_mbps);
        println!("Latency P50: {:.1} μs", self.p50_latency_us);
        println!("Latency P95: {:.1} μs", self.p95_latency_us);
        println!("Latency P99: {:.1} μs", self.p99_latency_us);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[tokio::test]
    async fn test_benchmark_tokio() {
        let result = benchmark_writes(AsyncIoBackend::Tokio).await.unwrap();
        result.print_summary();
        
        assert!(result.throughput_mbps > 0.0);
        assert!(result.p50_latency_us > 0.0);
    }
    
    #[cfg(all(target_os = "linux", feature = "async-io"))]
    #[tokio::test]
    async fn test_benchmark_io_uring() {
        if AsyncIoBackend::detect() == AsyncIoBackend::IoUring {
            let result = benchmark_writes(AsyncIoBackend::IoUring).await.unwrap();
            result.print_summary();
            
            assert!(result.throughput_mbps > 0.0);
            assert!(result.p50_latency_us > 0.0);
        }
    }
    
    #[tokio::test]
    async fn compare_backends() {
        println!("\n=== WAL Async I/O Performance Comparison ===\n");
        
        // Benchmark standard tokio
        println!("Testing standard Tokio async I/O...");
        let tokio_write = benchmark_writes(AsyncIoBackend::Tokio).await.unwrap();
        tokio_write.print_summary();
        
        let tokio_read = benchmark_reads(AsyncIoBackend::Tokio).await.unwrap();
        println!("\nRead benchmark:");
        tokio_read.print_summary();
        
        // Benchmark io_uring if available
        #[cfg(all(target_os = "linux", feature = "async-io"))]
        {
            let backend = AsyncIoBackend::detect();
            if matches!(backend, AsyncIoBackend::IoUring) {
                println!("\nTesting io_uring async I/O...");
                let uring_write = benchmark_writes(AsyncIoBackend::IoUring).await.unwrap();
                uring_write.print_summary();
                
                let uring_read = benchmark_reads(AsyncIoBackend::IoUring).await.unwrap();
                println!("\nRead benchmark:");
                uring_read.print_summary();
                
                // Compare results
                println!("\n=== Performance Comparison ===");
                let write_speedup = uring_write.throughput_mbps / tokio_write.throughput_mbps;
                let read_speedup = uring_read.throughput_mbps / tokio_read.throughput_mbps;
                let write_latency_reduction = 1.0 - (uring_write.p50_latency_us / tokio_write.p50_latency_us);
                
                println!("Write throughput improvement: {:.1}x", write_speedup);
                println!("Read throughput improvement: {:.1}x", read_speedup);
                println!("Write latency reduction: {:.1}%", write_latency_reduction * 100.0);
                
                if write_speedup > 1.2 {
                    println!("✅ io_uring provides significant performance improvement!");
                } else {
                    println!("ℹ️  io_uring performance is comparable to standard async I/O");
                }
            } else {
                println!("\n⚠️  io_uring not available on this system");
            }
        }
        
        #[cfg(not(all(target_os = "linux", feature = "async-io")))]
        {
            println!("\n⚠️  io_uring benchmark not available (Linux-only with async-io feature)");
        }
    }
}