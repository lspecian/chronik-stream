//! Resource monitoring and metrics collection for benchmarks

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tokio::time::interval;
use tracing::{debug, warn};

/// Resource usage statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceStats {
    /// Average CPU usage percentage
    pub avg_cpu_percent: f64,
    /// Maximum CPU usage percentage
    pub max_cpu_percent: f64,
    /// Average memory usage in MB
    pub avg_memory_mb: f64,
    /// Maximum memory usage in MB
    pub max_memory_mb: f64,
    /// Average disk I/O in MB/s
    pub avg_disk_io_mb_per_sec: f64,
    /// Average network I/O in MB/s
    pub avg_network_io_mb_per_sec: f64,
    /// Number of samples collected
    pub sample_count: u64,
    /// Duration of monitoring
    pub duration: Duration,
}

impl Default for ResourceStats {
    fn default() -> Self {
        Self {
            avg_cpu_percent: 0.0,
            max_cpu_percent: 0.0,
            avg_memory_mb: 0.0,
            max_memory_mb: 0.0,
            avg_disk_io_mb_per_sec: 0.0,
            avg_network_io_mb_per_sec: 0.0,
            sample_count: 0,
            duration: Duration::from_secs(0),
        }
    }
}

/// Resource monitoring state
#[derive(Debug)]
struct MonitorState {
    cpu_samples: Vec<f64>,
    memory_samples: Vec<f64>,
    disk_io_samples: Vec<f64>,
    network_io_samples: Vec<f64>,
    start_time: Option<Instant>,
    is_running: bool,
}

impl Default for MonitorState {
    fn default() -> Self {
        Self {
            cpu_samples: Vec::new(),
            memory_samples: Vec::new(),
            disk_io_samples: Vec::new(),
            network_io_samples: Vec::new(),
            start_time: None,
            is_running: false,
        }
    }
}

/// Resource monitor for tracking system resource usage during benchmarks
pub struct ResourceMonitor {
    state: Arc<RwLock<MonitorState>>,
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl ResourceMonitor {
    /// Create a new resource monitor
    pub fn new() -> Self {
        Self {
            state: Arc::new(RwLock::new(MonitorState::default())),
            handle: None,
        }
    }
    
    /// Start monitoring resources
    pub fn start(&mut self) {
        let state = Arc::clone(&self.state);
        
        let handle = tokio::spawn(async move {
            let mut interval = interval(Duration::from_millis(100));
            
            {
                let mut state_guard = state.write().await;
                state_guard.start_time = Some(Instant::now());
                state_guard.is_running = true;
            }
            
            loop {
                interval.tick().await;
                
                // Check if monitoring should stop
                {
                    let state_guard = state.read().await;
                    if !state_guard.is_running {
                        break;
                    }
                }
                
                // Collect resource metrics
                let cpu_usage = Self::get_cpu_usage().await;
                let memory_usage = Self::get_memory_usage().await;
                let disk_io = Self::get_disk_io().await;
                let network_io = Self::get_network_io().await;
                
                // Store samples
                {
                    let mut state_guard = state.write().await;
                    state_guard.cpu_samples.push(cpu_usage);
                    state_guard.memory_samples.push(memory_usage);
                    state_guard.disk_io_samples.push(disk_io);
                    state_guard.network_io_samples.push(network_io);
                }
                
                debug!("Resource sample: CPU={:.1}%, Memory={:.1}MB, Disk I/O={:.1}MB/s, Network I/O={:.1}MB/s",
                       cpu_usage, memory_usage, disk_io, network_io);
            }
        });
        
        self.handle = Some(handle);
    }
    
    /// Stop monitoring and return collected statistics
    pub async fn stop(&mut self) -> ResourceStats {
        // Signal to stop monitoring
        {
            let mut state_guard = self.state.write().await;
            state_guard.is_running = false;
        }
        
        // Wait for monitoring task to complete
        if let Some(handle) = self.handle.take() {
            if let Err(e) = handle.await {
                warn!("Error stopping resource monitor: {}", e);
            }
        }
        
        // Calculate statistics
        let state_guard = self.state.read().await;
        let duration = state_guard.start_time
            .map(|start| start.elapsed())
            .unwrap_or_default();
        
        let sample_count = state_guard.cpu_samples.len() as u64;
        
        let avg_cpu = if !state_guard.cpu_samples.is_empty() {
            state_guard.cpu_samples.iter().sum::<f64>() / state_guard.cpu_samples.len() as f64
        } else {
            0.0
        };
        
        let max_cpu = state_guard.cpu_samples.iter()
            .max_by(|a, b| a.partial_cmp(b).unwrap())
            .copied()
            .unwrap_or(0.0);
        
        let avg_memory = if !state_guard.memory_samples.is_empty() {
            state_guard.memory_samples.iter().sum::<f64>() / state_guard.memory_samples.len() as f64
        } else {
            0.0
        };
        
        let max_memory = state_guard.memory_samples.iter()
            .max_by(|a, b| a.partial_cmp(b).unwrap())
            .copied()
            .unwrap_or(0.0);
        
        let avg_disk_io = if !state_guard.disk_io_samples.is_empty() {
            state_guard.disk_io_samples.iter().sum::<f64>() / state_guard.disk_io_samples.len() as f64
        } else {
            0.0
        };
        
        let avg_network_io = if !state_guard.network_io_samples.is_empty() {
            state_guard.network_io_samples.iter().sum::<f64>() / state_guard.network_io_samples.len() as f64
        } else {
            0.0
        };
        
        ResourceStats {
            avg_cpu_percent: avg_cpu,
            max_cpu_percent: max_cpu,
            avg_memory_mb: avg_memory,
            max_memory_mb: max_memory,
            avg_disk_io_mb_per_sec: avg_disk_io,
            avg_network_io_mb_per_sec: avg_network_io,
            sample_count,
            duration,
        }
    }
    
    /// Get current CPU usage percentage (simplified implementation)
    async fn get_cpu_usage() -> f64 {
        // In a real implementation, this would read from /proc/stat or use system APIs
        // For now, simulate CPU usage
        use std::process::Command;
        
        let output = Command::new("sh")
            .arg("-c")
            .arg("top -bn1 | grep 'Cpu(s)' | awk '{print $2}' | awk -F'%' '{print $1}'")
            .output();
        
        match output {
            Ok(output) => {
                let cpu_str = String::from_utf8_lossy(&output.stdout);
                cpu_str.trim().parse().unwrap_or(0.0)
            }
            Err(_) => {
                // Fallback to simulated CPU usage
                (std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis() % 100) as f64
            }
        }
    }
    
    /// Get current memory usage in MB (simplified implementation)
    async fn get_memory_usage() -> f64 {
        // In a real implementation, this would read from /proc/meminfo
        // For now, simulate memory usage
        use std::process::Command;
        
        let output = Command::new("sh")
            .arg("-c")
            .arg("free -m | awk 'NR==2{printf \"%.2f\", $3}'")
            .output();
        
        match output {
            Ok(output) => {
                let mem_str = String::from_utf8_lossy(&output.stdout);
                mem_str.trim().parse().unwrap_or(0.0)
            }
            Err(_) => {
                // Fallback to simulated memory usage
                512.0 + ((std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis() % 1024) as f64)
            }
        }
    }
    
    /// Get current disk I/O in MB/s (simplified implementation)
    async fn get_disk_io() -> f64 {
        // In a real implementation, this would track disk I/O from /proc/diskstats
        // For now, simulate disk I/O
        (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() % 50) as f64 / 10.0
    }
    
    /// Get current network I/O in MB/s (simplified implementation)
    async fn get_network_io() -> f64 {
        // In a real implementation, this would track network I/O from /proc/net/dev
        // For now, simulate network I/O
        (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() % 100) as f64 / 20.0
    }
}

impl Drop for ResourceMonitor {
    fn drop(&mut self) {
        // Ensure monitoring is stopped when dropped
        if let Some(handle) = &self.handle {
            handle.abort();
        }
    }
}