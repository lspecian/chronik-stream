//! Comprehensive backup and restore functionality for Chronik Stream
//! 
//! This crate provides:
//! - Full and incremental backups
//! - Point-in-time recovery
//! - Data migration between versions
//! - Backup integrity validation
//! - Parallel backup/restore operations

pub mod manager;
pub mod metadata;
pub mod recovery;
pub mod migration;
pub mod validator;
pub mod scheduler;
pub mod compression;
pub mod encryption;

pub use manager::{BackupManager, BackupConfig};
pub use metadata::{BackupMetadata, MetadataBackup};
pub use recovery::{PointInTimeRecovery, RecoveryTarget, RecoveryResult};
pub use migration::{DataMigrator, MigrationStrategy, MigrationResult};
pub use validator::{BackupValidator, ValidationLevel, ValidationReport};
pub use scheduler::{BackupScheduler, Schedule, RetentionPolicy};
pub use compression::CompressionType;
pub use encryption::{EncryptionConfig, EncryptionType};

use thiserror::Error;

/// Backup error types
#[derive(Error, Debug)]
pub enum BackupError {
    #[error("Storage error: {0}")]
    Storage(String),
    
    #[error("Serialization error: {0}")]
    Serialization(String),
    
    #[error("Compression error: {0}")]
    Compression(String),
    
    #[error("Encryption error: {0}")]
    Encryption(String),
    
    #[error("Validation error: {0}")]
    Validation(String),
    
    #[error("Migration error: {0}")]
    Migration(String),
    
    #[error("Recovery error: {0}")]
    Recovery(String),
    
    #[error("Not found: {0}")]
    NotFound(String),
    
    #[error("Invalid backup: {0}")]
    InvalidBackup(String),
    
    #[error("Version mismatch: expected {expected}, got {actual}")]
    VersionMismatch { expected: String, actual: String },
    
    #[error("Checksum mismatch")]
    ChecksumMismatch,
    
    #[error("Internal error: {0}")]
    Internal(String),
}

pub type Result<T> = std::result::Result<T, BackupError>;

/// Convert various errors to BackupError
impl From<std::io::Error> for BackupError {
    fn from(e: std::io::Error) -> Self {
        BackupError::Storage(e.to_string())
    }
}

impl From<serde_json::Error> for BackupError {
    fn from(e: serde_json::Error) -> Self {
        BackupError::Serialization(e.to_string())
    }
}

impl From<bincode::Error> for BackupError {
    fn from(e: bincode::Error) -> Self {
        BackupError::Serialization(e.to_string())
    }
}