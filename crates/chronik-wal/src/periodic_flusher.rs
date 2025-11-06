//! Periodic background WAL flusher
//!
//! This module implements a simple periodic background task that flushes
//! the WAL to disk at regular intervals. This eliminates the fsync bottleneck
//! in the produce path while maintaining durability guarantees.
//!
//! Design:
//! - Simple background task that calls `flush_all()` every N milliseconds
//! - No locks held during produce (append is already buffered)
//! - No confirmation channels or waiting (fire-and-forget)
//! - Kafka-compatible semantics (acks=-1 returns immediately, data fsynced within flush interval)

use crate::{WalManager, WalConfig};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::time::interval;
use tracing::{info, error, debug};

/// Configuration for periodic background flushing
#[derive(Debug, Clone)]
pub struct PeriodicFlusherConfig {
    /// Enable periodic flushing (default: true)
    pub enabled: bool,

    /// Flush interval in milliseconds (default: 50ms)
    /// Trade-off: Lower = better durability, Higher = better throughput
    pub interval_ms: u64,
}

impl Default for PeriodicFlusherConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_ms: 50, // 50ms is Kafka-standard for batch.linger.ms
        }
    }
}

/// Spawn a background task that periodically flushes the WAL
///
/// This task runs indefinitely until the Arc<WalManager> is dropped.
/// It simply calls `flush_all()` at regular intervals.
///
/// # Arguments
/// * `wal_manager` - Shared WAL manager instance
/// * `config` - Periodic flusher configuration
///
/// # Returns
/// * Handle to the background task (can be used for shutdown)
pub fn spawn_periodic_flusher(
    wal_manager: Arc<WalManager>,
    config: PeriodicFlusherConfig,
) -> tokio::task::JoinHandle<()> {
    info!("Starting periodic WAL flusher (interval={}ms)", config.interval_ms);

    tokio::spawn(async move {
        let mut flush_timer = interval(Duration::from_millis(config.interval_ms));
        let mut total_flushes: u64 = 0;
        let mut total_flush_time_ms: u64 = 0;

        loop {
            flush_timer.tick().await;

            let flush_start = std::time::Instant::now();

            // Flush all partitions (v1.3.47+: direct call, no lock needed)
            let result = wal_manager.flush_all().await;

            let flush_duration = flush_start.elapsed();
            total_flushes += 1;
            total_flush_time_ms += flush_duration.as_millis() as u64;

            match result {
                Ok(_) => {
                    debug!("Periodic WAL flush completed in {:?}", flush_duration);

                    // Log statistics every 1000 flushes
                    if total_flushes % 1000 == 0 {
                        let avg_flush_time_ms = total_flush_time_ms / total_flushes;
                        info!("Periodic flusher statistics: total_flushes={}, avg_flush_time={}ms",
                            total_flushes, avg_flush_time_ms);
                    }
                }
                Err(e) => {
                    error!("Periodic WAL flush failed after {:?}: {}", flush_duration, e);
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::WalConfig;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_periodic_flusher() {
        let temp_dir = TempDir::new().unwrap();
        let mut config = WalConfig::default();
        config.data_dir = temp_dir.path().to_path_buf();

        let manager = WalManager::new(config).await.unwrap();
        let manager_arc = Arc::new(manager);

        let flusher_config = PeriodicFlusherConfig {
            enabled: true,
            interval_ms: 10, // Fast for testing
        };

        let handle = spawn_periodic_flusher(manager_arc.clone(), flusher_config);

        // Let it run for a bit
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Abort the task
        handle.abort();
    }
}
