//! WAL configuration

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalConfig {
    /// Enable WAL subsystem
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    
    /// Directory for WAL files
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    
    /// Maximum segment size in bytes
    #[serde(default = "default_segment_size")]
    pub segment_size: u64,
    
    /// Flush interval
    #[serde(default = "default_flush_interval")]
    pub flush_interval_ms: u64,
    
    /// Buffer size threshold for triggering flush
    #[serde(default = "default_flush_threshold")]
    pub flush_threshold: usize,
    
    /// Compression type
    #[serde(default)]
    pub compression: CompressionType,
    
    /// Checkpointing configuration
    #[serde(default)]
    pub checkpointing: CheckpointConfig,
    
    /// Recovery configuration
    #[serde(default)]
    pub recovery: RecoveryConfig,
    
    /// Rotation policy
    #[serde(default)]
    pub rotation: RotationConfig,
    
    /// Fsync configuration
    #[serde(default)]
    pub fsync: FsyncConfig,

    /// Async I/O configuration
    #[serde(default)]
    pub async_io: AsyncIoConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub enum CompressionType {
    #[default]
    None,
    #[cfg(feature = "wal-compression")]
    Zstd,
    Snappy,
    Lz4,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointConfig {
    /// Enable checkpointing
    #[serde(default = "default_checkpoint_enabled")]
    pub enabled: bool,
    
    /// Checkpoint every N records
    #[serde(default = "default_checkpoint_interval_records")]
    pub interval_records: u64,
    
    /// Or checkpoint every N bytes
    #[serde(default = "default_checkpoint_interval_bytes")]
    pub interval_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryConfig {
    /// Number of parallel segments to recover
    #[serde(default = "default_parallel_segments")]
    pub parallel_segments: usize,
    
    /// Use memory mapping for recovery
    #[serde(default = "default_use_mmap")]
    pub use_mmap: bool,
    
    /// Verify all checksums during recovery
    #[serde(default = "default_verify_checksums")]
    pub verify_checksums: bool,
    
    /// Acceptable corruption tolerance (0.0 - 1.0)
    #[serde(default = "default_corruption_tolerance")]
    pub corruption_tolerance: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RotationConfig {
    /// Maximum segment age before rotation
    #[serde(default = "default_max_segment_age_ms")]
    pub max_segment_age_ms: u64,
    
    /// Maximum segment size before rotation
    #[serde(default = "default_max_segment_size")]
    pub max_segment_size: u64,
    
    /// Coordinate with storage segment rotation
    #[serde(default = "default_coordinate_with_storage")]
    pub coordinate_with_storage: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsyncConfig {
    /// Enable fsync after writes
    #[serde(default = "default_fsync_enabled")]
    pub enabled: bool,
    
    /// Batch size for fsync operations
    #[serde(default = "default_fsync_batch_size")]
    pub batch_size: u32,
    
    /// Batch timeout in milliseconds
    #[serde(default = "default_fsync_batch_timeout_ms")]
    pub batch_timeout_ms: u64,
}

impl Default for WalConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            data_dir: default_data_dir(),
            segment_size: default_segment_size(),
            flush_interval_ms: default_flush_interval(),
            flush_threshold: default_flush_threshold(),
            compression: CompressionType::default(),
            checkpointing: CheckpointConfig::default(),
            recovery: RecoveryConfig::default(),
            rotation: RotationConfig::default(),
            fsync: FsyncConfig::default(),
            async_io: AsyncIoConfig::default(),
        }
    }
}

impl Default for CheckpointConfig {
    fn default() -> Self {
        Self {
            enabled: default_checkpoint_enabled(),
            interval_records: default_checkpoint_interval_records(),
            interval_bytes: default_checkpoint_interval_bytes(),
        }
    }
}

impl Default for RecoveryConfig {
    fn default() -> Self {
        Self {
            parallel_segments: default_parallel_segments(),
            use_mmap: default_use_mmap(),
            verify_checksums: default_verify_checksums(),
            corruption_tolerance: default_corruption_tolerance(),
        }
    }
}

impl Default for RotationConfig {
    fn default() -> Self {
        Self {
            max_segment_age_ms: default_max_segment_age_ms(),
            max_segment_size: default_max_segment_size(),
            coordinate_with_storage: default_coordinate_with_storage(),
        }
    }
}

impl Default for FsyncConfig {
    fn default() -> Self {
        Self {
            enabled: default_fsync_enabled(),
            batch_size: default_fsync_batch_size(),
            batch_timeout_ms: default_fsync_batch_timeout_ms(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AsyncIoConfig {
    /// Enable async I/O using io_uring on Linux
    #[serde(default = "default_use_io_uring")]
    pub use_io_uring: bool,
    
    /// Auto-detect best async I/O backend
    #[serde(default = "default_auto_detect")]
    pub auto_detect: bool,
}

impl Default for AsyncIoConfig {
    fn default() -> Self {
        Self {
            use_io_uring: default_use_io_uring(),
            auto_detect: default_auto_detect(),
        }
    }
}

impl WalConfig {
    pub fn flush_interval(&self) -> Duration {
        Duration::from_millis(self.flush_interval_ms)
    }
    
    pub fn max_segment_age(&self) -> Duration {
        Duration::from_millis(self.rotation.max_segment_age_ms)
    }
}

// Default value functions
fn default_enabled() -> bool { true } // WAL is always enabled now
fn default_data_dir() -> PathBuf { PathBuf::from("./data/wal") }
fn default_segment_size() -> u64 { 1024 * 1024 * 1024 } // 1GB
fn default_flush_interval() -> u64 { 100 } // 100ms
fn default_flush_threshold() -> usize { 1024 * 1024 } // 1MB
fn default_checkpoint_enabled() -> bool { true }
fn default_checkpoint_interval_records() -> u64 { 10000 }
fn default_checkpoint_interval_bytes() -> u64 { 100 * 1024 * 1024 } // 100MB
fn default_parallel_segments() -> usize { 4 }
fn default_use_mmap() -> bool { true }
fn default_verify_checksums() -> bool { true }
fn default_corruption_tolerance() -> f32 { 0.01 } // 1% tolerance
fn default_max_segment_age_ms() -> u64 { 30 * 60 * 1000 } // 30 minutes
fn default_max_segment_size() -> u64 { 1024 * 1024 * 1024 } // 1GB
fn default_coordinate_with_storage() -> bool { true }
fn default_fsync_enabled() -> bool { true }
fn default_fsync_batch_size() -> u32 { 100 }
fn default_fsync_batch_timeout_ms() -> u64 { 100 }
fn default_use_io_uring() -> bool { false } // Opt-in for now
fn default_auto_detect() -> bool { true } // Auto-detect by default