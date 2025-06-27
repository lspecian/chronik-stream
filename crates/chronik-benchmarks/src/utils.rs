//! Benchmark utilities and helpers

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};
use tokio::time::timeout;
use tracing::{info, warn};

/// Test data generator for benchmarks
pub struct TestDataGenerator {
    seed: u64,
}

impl TestDataGenerator {
    /// Create a new test data generator with a seed
    pub fn new(seed: u64) -> Self {
        Self { seed }
    }
    
    /// Generate a test message of specified size
    pub fn generate_message(&mut self, size: usize) -> Vec<u8> {
        // Simple LCG for reproducible test data
        self.seed = self.seed.wrapping_mul(1103515245).wrapping_add(12345);
        
        let mut data = Vec::with_capacity(size);
        let base_content = format!(r#"{{"id": {}, "timestamp": "{}", "data": ""#, 
                                   self.seed, Utc::now().to_rfc3339());
        
        data.extend_from_slice(base_content.as_bytes());
        
        // Fill remaining space with repeated pattern
        let pattern = b"abcdefghijklmnopqrstuvwxyz0123456789";
        let mut remaining = size.saturating_sub(data.len()).saturating_sub(2); // Reserve space for closing
        
        while remaining > 0 {
            let chunk_size = remaining.min(pattern.len());
            data.extend_from_slice(&pattern[..chunk_size]);
            remaining -= chunk_size;
        }
        
        data.extend_from_slice(b"\"}");
        data.truncate(size); // Ensure exact size
        
        data
    }
    
    /// Generate a batch of messages
    pub fn generate_batch(&mut self, count: usize, message_size: usize) -> Vec<Vec<u8>> {
        (0..count)
            .map(|_| self.generate_message(message_size))
            .collect()
    }
    
    /// Generate JSON test data
    pub fn generate_json_message(&mut self, fields: &[&str]) -> serde_json::Value {
        self.seed = self.seed.wrapping_mul(1103515245).wrapping_add(12345);
        
        let mut json = serde_json::json!({
            "id": self.seed,
            "timestamp": Utc::now().to_rfc3339(),
            "sequence": self.seed % 1000000,
        });
        
        // Add requested fields with random data
        for field in fields {
            let value = match *field {
                "user_id" => serde_json::Value::Number((self.seed % 100000).into()),
                "session_id" => serde_json::Value::String(format!("session_{}", self.seed % 10000)),
                "event_type" => {
                    let events = ["login", "logout", "purchase", "view", "click"];
                    serde_json::Value::String(events[(self.seed % events.len() as u64) as usize].to_string())
                },
                "amount" => serde_json::Value::Number(serde_json::Number::from_f64((self.seed % 10000) as f64 / 100.0).unwrap()),
                "status" => {
                    let statuses = ["success", "pending", "failed"];
                    serde_json::Value::String(statuses[(self.seed % statuses.len() as u64) as usize].to_string())
                },
                _ => serde_json::Value::String(format!("value_{}", self.seed % 1000)),
            };
            
            json[field] = value;
            self.seed = self.seed.wrapping_mul(1103515245).wrapping_add(12345);
        }
        
        json
    }
}

/// Latency tracker for measuring operation timings
pub struct LatencyTracker {
    measurements: Vec<Duration>,
    start_time: Option<Instant>,
}

impl LatencyTracker {
    /// Create a new latency tracker
    pub fn new() -> Self {
        Self {
            measurements: Vec::new(),
            start_time: None,
        }
    }
    
    /// Start measuring an operation
    pub fn start(&mut self) {
        self.start_time = Some(Instant::now());
    }
    
    /// End measuring and record the latency
    pub fn end(&mut self) -> Option<Duration> {
        if let Some(start) = self.start_time.take() {
            let latency = start.elapsed();
            self.measurements.push(latency);
            Some(latency)
        } else {
            None
        }
    }
    
    /// Measure the duration of an async operation
    pub async fn measure<F, T>(&mut self, operation: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        self.start();
        let result = operation.await;
        self.end();
        result
    }
    
    /// Measure the duration of an async operation with timeout
    pub async fn measure_with_timeout<F, T>(&mut self, operation: F, timeout_duration: Duration) -> Result<T>
    where
        F: std::future::Future<Output = T>,
        T: Send,
    {
        self.start();
        let result = timeout(timeout_duration, operation).await
            .map_err(|_| anyhow::anyhow!("Operation timed out"));
        self.end();
        result
    }
    
    /// Get all recorded measurements
    pub fn measurements(&self) -> &[Duration] {
        &self.measurements
    }
    
    /// Clear all measurements
    pub fn clear(&mut self) {
        self.measurements.clear();
    }
    
    /// Calculate percentile from measurements
    pub fn percentile(&self, p: f64) -> Option<Duration> {
        if self.measurements.is_empty() {
            return None;
        }
        
        let mut sorted = self.measurements.clone();
        sorted.sort();
        
        let index = ((sorted.len() as f64) * p / 100.0) as usize;
        sorted.get(index.min(sorted.len() - 1)).copied()
    }
}

/// Rate limiter for controlling operation throughput
pub struct RateLimiter {
    target_rate: f64, // operations per second
    interval: Duration,
    last_op: Option<Instant>,
}

impl RateLimiter {
    /// Create a new rate limiter
    pub fn new(target_rate: f64) -> Self {
        Self {
            target_rate,
            interval: Duration::from_secs_f64(1.0 / target_rate),
            last_op: None,
        }
    }
    
    /// Wait if necessary to maintain the target rate
    pub async fn wait(&mut self) {
        let now = Instant::now();
        
        if let Some(last) = self.last_op {
            let elapsed = now.duration_since(last);
            if elapsed < self.interval {
                let sleep_duration = self.interval - elapsed;
                tokio::time::sleep(sleep_duration).await;
            }
        }
        
        self.last_op = Some(Instant::now());
    }
    
    /// Check if an operation should be allowed (non-blocking)
    pub fn try_acquire(&mut self) -> bool {
        let now = Instant::now();
        
        if let Some(last) = self.last_op {
            let elapsed = now.duration_since(last);
            if elapsed >= self.interval {
                self.last_op = Some(now);
                true
            } else {
                false
            }
        } else {
            self.last_op = Some(now);
            true
        }
    }
}

/// Progress reporter for long-running benchmarks
pub struct ProgressReporter {
    total: u64,
    current: u64,
    start_time: Instant,
    last_report: Instant,
    report_interval: Duration,
}

impl ProgressReporter {
    /// Create a new progress reporter
    pub fn new(total: u64) -> Self {
        let now = Instant::now();
        Self {
            total,
            current: 0,
            start_time: now,
            last_report: now,
            report_interval: Duration::from_secs(10),
        }
    }
    
    /// Update progress and report if necessary
    pub fn update(&mut self, increment: u64) {
        self.current += increment;
        
        let now = Instant::now();
        if now.duration_since(self.last_report) >= self.report_interval {
            self.report();
            self.last_report = now;
        }
    }
    
    /// Force a progress report
    pub fn report(&self) {
        let elapsed = self.start_time.elapsed();
        let percentage = if self.total > 0 {
            (self.current as f64 / self.total as f64) * 100.0
        } else {
            0.0
        };
        
        let rate = if elapsed.as_secs() > 0 {
            self.current as f64 / elapsed.as_secs_f64()
        } else {
            0.0
        };
        
        let eta = if rate > 0.0 && self.current < self.total {
            let remaining = self.total - self.current;
            Duration::from_secs_f64(remaining as f64 / rate)
        } else {
            Duration::from_secs(0)
        };
        
        info!("Progress: {}/{} ({:.1}%) - {:.0} ops/sec - ETA: {:?}", 
              self.current, self.total, percentage, rate, eta);
    }
    
    /// Report completion
    pub fn complete(&self) {
        let elapsed = self.start_time.elapsed();
        let rate = if elapsed.as_secs() > 0 {
            self.current as f64 / elapsed.as_secs_f64()
        } else {
            0.0
        };
        
        info!("Completed {} operations in {:?} ({:.0} ops/sec)", 
              self.current, elapsed, rate);
    }
}

/// Environment information collector
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvironmentInfo {
    pub hostname: String,
    pub os: String,
    pub cpu_cores: usize,
    pub total_memory_mb: u64,
    pub rust_version: String,
    pub chronik_version: String,
    pub timestamp: DateTime<Utc>,
}

impl EnvironmentInfo {
    /// Collect current environment information
    pub fn collect() -> Self {
        Self {
            hostname: hostname::get()
                .unwrap_or_else(|_| "unknown".into())
                .to_string_lossy()
                .to_string(),
            os: std::env::consts::OS.to_string(),
            cpu_cores: num_cpus::get(),
            total_memory_mb: Self::get_total_memory_mb(),
            rust_version: Self::get_rust_version(),
            chronik_version: env!("CARGO_PKG_VERSION").to_string(),
            timestamp: Utc::now(),
        }
    }
    
    fn get_total_memory_mb() -> u64 {
        // Try to get memory info from /proc/meminfo on Linux
        if cfg!(target_os = "linux") {
            if let Ok(meminfo) = std::fs::read_to_string("/proc/meminfo") {
                for line in meminfo.lines() {
                    if line.starts_with("MemTotal:") {
                        if let Some(kb) = line.split_whitespace().nth(1) {
                            if let Ok(kb_val) = kb.parse::<u64>() {
                                return kb_val / 1024; // Convert KB to MB
                            }
                        }
                    }
                }
            }
        }
        
        // Fallback estimate based on system
        8192 // 8GB default
    }
    
    fn get_rust_version() -> String {
        std::env::var("RUSTC_VERSION")
            .unwrap_or_else(|_| "unknown".to_string())
    }
}

/// Add num_cpus dependency
#[cfg(not(feature = "num_cpus"))]
pub mod num_cpus {
    pub fn get() -> usize {
        std::thread::available_parallelism()
            .map(|p| p.get())
            .unwrap_or(4)
    }
}

#[cfg(feature = "num_cpus")]
pub use num_cpus;

/// Add hostname dependency
#[cfg(not(feature = "hostname"))]
pub mod hostname {
    use std::ffi::OsString;
    
    pub fn get() -> Result<OsString, std::io::Error> {
        Ok(OsString::from("localhost"))
    }
}

#[cfg(feature = "hostname")]
pub use hostname;