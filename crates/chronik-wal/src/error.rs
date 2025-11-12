//! WAL error types

use std::io;
use thiserror::Error;

pub type Result<T> = std::result::Result<T, WalError>;

#[derive(Error, Debug)]
pub enum WalError {
    #[error("IO error: {0}")]
    Io(#[from] io::Error),
    
    #[error("Corrupt record at offset {offset}: {reason}")]
    CorruptRecord { offset: i64, reason: String },
    
    #[error("Checksum mismatch at offset {offset}: expected {expected}, got {actual}")]
    ChecksumMismatch {
        offset: i64,
        expected: u32,
        actual: u32,
    },
    
    #[error("Invalid WAL format: {0}")]
    InvalidFormat(String),
    
    #[error("Segment not found: {0}")]
    SegmentNotFound(String),
    
    #[error("Recovery failed: {0}")]
    RecoveryFailed(String),
    
    #[error("Rotation failed: {0}")]
    RotationFailed(String),
    
    #[error("Configuration error: {0}")]
    ConfigError(String),
    
    #[error("Serialization error: {0}")]
    SerializationError(#[from] bincode::Error),
    
    #[error("Channel send error")]
    ChannelSendError,
    
    #[error("WAL is sealed and cannot accept writes")]
    WalSealed,
    
    #[error("Offset out of range: {offset}")]
    OffsetOutOfRange { offset: i64 },
    
    #[error("JSON error: {0}")]
    JsonError(#[from] serde_json::Error),
    
    #[error("IO error: {0}")]
    IoError(String),
    
    #[error("Replication error: {0}")]
    ReplicationError(String),

    #[error("Backpressure: {0}")]
    Backpressure(String),

    #[error("Commit failed: {0}")]
    CommitFailed(String),

    #[error("Unsupported feature: {0}")]
    Unsupported(String),
}