//! Comprehensive performance benchmarking suite for Chronik Stream
//! 
//! This crate provides benchmarks for:
//! - Ingest throughput and latency
//! - Search performance 
//! - End-to-end data flow
//! - Resource utilization
//! - Comparison with Kafka and Elasticsearch

pub mod benchmarks;
pub mod harness;
pub mod metrics;
pub mod scenarios;
pub mod utils;

pub use benchmarks::*;
pub use harness::*;
pub use metrics::*;
pub use scenarios::*;
pub use utils::*;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Benchmark configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkConfig {
    /// Number of threads to use
    pub threads: usize,
    /// Duration to run benchmark
    pub duration: Duration,
    /// Number of warmup iterations
    pub warmup_iterations: usize,
    /// Target throughput (messages per second)
    pub target_throughput: Option<u64>,
    /// Message size in bytes
    pub message_size: usize,
    /// Batch size for produce operations
    pub batch_size: usize,
    /// Number of topics
    pub topic_count: usize,
    /// Number of partitions per topic
    pub partitions_per_topic: usize,
}

impl Default for BenchmarkConfig {
    fn default() -> Self {
        Self {
            threads: num_cpus::get(),
            duration: Duration::from_secs(60),
            warmup_iterations: 100,
            target_throughput: None,
            message_size: 1024,
            batch_size: 100,
            topic_count: 1,
            partitions_per_topic: 3,
        }
    }
}

/// Benchmark results
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkResults {
    /// Benchmark name
    pub name: String,
    /// Throughput in messages per second
    pub throughput_msg_per_sec: f64,
    /// Throughput in MB per second
    pub throughput_mb_per_sec: f64,
    /// Average latency in milliseconds
    pub avg_latency_ms: f64,
    /// P50 latency in milliseconds
    pub p50_latency_ms: f64,
    /// P95 latency in milliseconds
    pub p95_latency_ms: f64,
    /// P99 latency in milliseconds
    pub p99_latency_ms: f64,
    /// Maximum latency in milliseconds
    pub max_latency_ms: f64,
    /// Total messages processed
    pub total_messages: u64,
    /// Total bytes processed
    pub total_bytes: u64,
    /// Test duration
    pub duration: Duration,
    /// Error count
    pub error_count: u64,
    /// CPU usage percentage
    pub cpu_usage_percent: f64,
    /// Memory usage in MB
    pub memory_usage_mb: f64,
    /// Additional metadata
    pub metadata: std::collections::HashMap<String, String>,
}

impl BenchmarkResults {
    /// Create new benchmark results
    pub fn new(name: String) -> Self {
        Self {
            name,
            throughput_msg_per_sec: 0.0,
            throughput_mb_per_sec: 0.0,
            avg_latency_ms: 0.0,
            p50_latency_ms: 0.0,
            p95_latency_ms: 0.0,
            p99_latency_ms: 0.0,
            max_latency_ms: 0.0,
            total_messages: 0,
            total_bytes: 0,
            duration: Duration::from_secs(0),
            error_count: 0,
            cpu_usage_percent: 0.0,
            memory_usage_mb: 0.0,
            metadata: std::collections::HashMap::new(),
        }
    }
    
    /// Calculate statistics from latency measurements
    pub fn calculate_stats(&mut self, latencies: &[Duration], total_bytes: u64) {
        if latencies.is_empty() {
            return;
        }
        
        self.total_messages = latencies.len() as u64;
        self.total_bytes = total_bytes;
        
        let latencies_ms: Vec<f64> = latencies.iter()
            .map(|d| d.as_secs_f64() * 1000.0)
            .collect();
        
        // Calculate throughput
        let duration_secs = self.duration.as_secs_f64();
        if duration_secs > 0.0 {
            self.throughput_msg_per_sec = self.total_messages as f64 / duration_secs;
            self.throughput_mb_per_sec = (self.total_bytes as f64 / 1024.0 / 1024.0) / duration_secs;
        }
        
        // Calculate latency statistics
        self.avg_latency_ms = latencies_ms.iter().sum::<f64>() / latencies_ms.len() as f64;
        
        let mut sorted_latencies = latencies_ms.clone();
        sorted_latencies.sort_by(|a, b| a.partial_cmp(b).unwrap());
        
        let len = sorted_latencies.len();
        if len > 0 {
            self.p50_latency_ms = sorted_latencies[len / 2];
            self.p95_latency_ms = sorted_latencies[(len as f64 * 0.95) as usize];
            self.p99_latency_ms = sorted_latencies[(len as f64 * 0.99) as usize];
            self.max_latency_ms = *sorted_latencies.last().unwrap();
        }
    }
    
    /// Print results in a formatted way
    pub fn print(&self) {
        println!("\n=== {} ===", self.name);
        println!("Throughput: {:.2} msg/sec, {:.2} MB/sec", 
                self.throughput_msg_per_sec, self.throughput_mb_per_sec);
        println!("Latency: avg={:.2}ms, p50={:.2}ms, p95={:.2}ms, p99={:.2}ms, max={:.2}ms",
                self.avg_latency_ms, self.p50_latency_ms, self.p95_latency_ms, 
                self.p99_latency_ms, self.max_latency_ms);
        println!("Total: {} messages, {} MB in {:?}",
                self.total_messages, self.total_bytes / 1024 / 1024, self.duration);
        if self.error_count > 0 {
            println!("Errors: {}", self.error_count);
        }
        println!("Resource usage: CPU={:.1}%, Memory={:.1}MB", 
                self.cpu_usage_percent, self.memory_usage_mb);
    }
}