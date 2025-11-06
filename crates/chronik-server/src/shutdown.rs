//! Graceful shutdown coordination for Chronik Server
//!
//! This module provides signal handling and coordinated shutdown across
//! all server components, including Raft leadership transfer for zero-downtime
//! rolling restarts.

use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;
use tokio::signal;
use tracing::{info, warn};

/// Shutdown coordinator for the Chronik server
///
/// This coordinates graceful shutdown across all components:
/// 1. Accept shutdown signal (SIGTERM/SIGINT)
/// 2. Stop accepting new requests
/// 3. Drain in-flight requests
/// 4. Transfer Raft leadership (if Raft enabled)
/// 5. Flush WAL
/// 6. Shutdown components
pub struct ShutdownCoordinator {
    /// Shutdown signal channel (receivers can await shutdown)
    shutdown_tx: tokio::sync::broadcast::Sender<()>,

    /// Raft replica manager (if Raft is enabled)

    /// WAL manager for flushing
    wal_manager: Option<Arc<chronik_wal::WalManager>>,

    /// Shutdown timeout
    timeout: Duration,

    /// In-flight request counter
    in_flight: Arc<std::sync::atomic::AtomicU64>,

    /// Flag to stop accepting requests
    accepting_requests: Arc<std::sync::atomic::AtomicBool>,
}

impl ShutdownCoordinator {
    /// Create a new shutdown coordinator
    pub fn new() -> Self {
        let (shutdown_tx, _) = tokio::sync::broadcast::channel(1);

        Self {
            shutdown_tx,
            wal_manager: None,
            timeout: Duration::from_secs(30),
            in_flight: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            accepting_requests: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        }
    }

    /// Set Raft manager for leadership transfer

    /// Set WAL manager for flushing
    pub fn set_wal_manager(&mut self, wal_manager: Arc<chronik_wal::WalManager>) {
        self.wal_manager = Some(wal_manager);
    }

    /// Set shutdown timeout
    pub fn set_timeout(&mut self, timeout: Duration) {
        self.timeout = timeout;
    }

    /// Get a shutdown receiver
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<()> {
        self.shutdown_tx.subscribe()
    }

    /// Check if accepting new requests
    pub fn is_accepting_requests(&self) -> bool {
        self.accepting_requests.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Increment in-flight request counter
    pub fn request_started(&self) {
        self.in_flight.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Decrement in-flight request counter
    pub fn request_completed(&self) {
        self.in_flight.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Wait for shutdown signal and coordinate graceful shutdown
    pub async fn wait_for_signal_and_shutdown(self: Arc<Self>) -> Result<()> {
        // Wait for SIGTERM or SIGINT
        #[cfg(unix)]
        {
            let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate())?;
            let mut sigint = signal::unix::signal(signal::unix::SignalKind::interrupt())?;

            tokio::select! {
                _ = sigterm.recv() => {
                    info!("Received SIGTERM, initiating graceful shutdown");
                }
                _ = sigint.recv() => {
                    info!("Received SIGINT (Ctrl+C), initiating graceful shutdown");
                }
            }
        }

        #[cfg(not(unix))]
        {
            signal::ctrl_c().await?;
            info!("Received Ctrl+C, initiating graceful shutdown");
        }

        // Execute shutdown
        self.shutdown().await
    }

    /// Execute graceful shutdown
    ///
    /// This orchestrates the complete shutdown flow:
    /// 1. Stop accepting new requests
    /// 2. Drain in-flight requests
    /// 3. Transfer Raft leadership (if enabled)
    /// 4. Flush WAL
    /// 5. Signal shutdown to all components
    pub async fn shutdown(&self) -> Result<()> {
        let start = std::time::Instant::now();
        info!("Starting graceful shutdown sequence");

        // Step 1: Stop accepting new requests
        info!("Step 1/4: Stopping new request acceptance");
        self.accepting_requests.store(false, std::sync::atomic::Ordering::SeqCst);

        // Step 2: Drain in-flight requests
        info!("Step 2/4: Draining in-flight requests");
        let drain_timeout = Duration::from_secs(10);
        let drain_start = std::time::Instant::now();

        while drain_start.elapsed() < drain_timeout {
            let in_flight = self.in_flight.load(std::sync::atomic::Ordering::Relaxed);
            if in_flight == 0 {
                info!("All in-flight requests drained in {:?}", drain_start.elapsed());
                break;
            }

            info!("Waiting for {} in-flight requests to complete", in_flight);
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        let remaining = self.in_flight.load(std::sync::atomic::Ordering::Relaxed);
        if remaining > 0 {
            warn!("Drain timeout reached with {} in-flight requests remaining", remaining);
        }

        // Step 3: Flush WAL
        info!("Step 3/3: Flushing WAL to disk");
        if let Some(ref wal_manager) = self.wal_manager {
            if let Err(e) = wal_manager.flush_all().await {
                warn!("Failed to flush WAL: {}", e);
                // Continue anyway
            } else {
                info!("WAL flush complete");
            }
        }

        // Broadcast shutdown signal to all components
        let _ = self.shutdown_tx.send(());

        info!("Graceful shutdown complete in {:?}", start.elapsed());
        Ok(())
    }

    /// Transfer Raft leadership for all partitions where this node is leader

impl Default for ShutdownCoordinator {
    fn default() -> Self {
        Self::new()
    }
}
