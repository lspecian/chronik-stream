//! Write-Ahead Log (WAL) subsystem for Chronik Stream
//! 
//! Provides durability guarantees with zero message loss through
//! write-ahead logging, checkpointing, and fast recovery.

#![cfg_attr(not(feature = "wal"), allow(dead_code))]

pub mod error;
pub mod manager;
pub mod record;
pub mod segment;
pub mod config;
pub mod fsync;
pub mod async_io;
pub mod buffer_pool;
pub mod concurrency_test;

pub mod io;
pub mod checkpoint;
pub mod rotation;

// Future-ready modules
pub mod replication;
pub mod audit;
pub mod streaming;

#[cfg(test)]
mod tests;

pub use error::{WalError, Result};
pub use manager::WalManager;
pub use record::WalRecord;
pub use segment::{WalSegment, SealedSegment};
pub use config::{WalConfig, CompressionType, CheckpointConfig, RecoveryConfig, RotationConfig, FsyncConfig};
pub use checkpoint::{Checkpoint, CheckpointManager};
pub use fsync::{FsyncBatcher, FsyncStats};

use tracing::info;

/// Recover all WAL partitions from disk
pub async fn recover_all(config: &WalConfig) -> Result<RecoveryResult> {
    info!("Starting WAL recovery from {:?}", config.data_dir);
    
    let manager = WalManager::recover(config).await?;
    let result = manager.get_recovery_result();
    
    info!(
        "WAL recovery complete: {} records from {} partitions",
        result.total_records, result.partitions
    );
    
    Ok(result)
}

/// Recovery statistics
#[derive(Debug, Clone)]
pub struct RecoveryResult {
    pub total_records: usize,
    pub partitions: usize,
    pub corrupted_segments: usize,
    pub last_offsets: Vec<(String, i32, i64)>, // (topic, partition, offset)
}