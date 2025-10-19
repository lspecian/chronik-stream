//! Error types for Raft storage and consensus operations

use std::io;
use thiserror::Error;

/// Result type for Raft operations
pub type Result<T> = std::result::Result<T, RaftError>;

/// Errors that can occur during Raft storage and consensus operations
#[derive(Debug, Error)]
pub enum RaftError {
    /// Entry not found
    #[error("Entry not found at index {0}")]
    EntryNotFound(u64),

    /// Invalid index range
    #[error("Invalid index range: {start}..{end}")]
    InvalidRange { start: u64, end: u64 },

    /// Serialization error
    #[error("Serialization error: {0}")]
    SerializationError(String),

    /// I/O error
    #[error("I/O error: {0}")]
    IoError(#[from] io::Error),

    /// Storage error
    #[error("Storage error: {0}")]
    StorageError(String),

    /// Configuration error
    #[error("Configuration error: {0}")]
    Config(String),

    /// gRPC transport error
    #[error("gRPC error: {0}")]
    Grpc(#[from] tonic::Status),

    /// Error from the underlying Raft library
    #[error("Raft protocol error: {0}")]
    Raft(#[from] raft::Error),

    /// Generic error from anyhow
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl From<bincode::Error> for RaftError {
    fn from(e: bincode::Error) -> Self {
        RaftError::SerializationError(e.to_string())
    }
}

impl From<RaftError> for tonic::Status {
    fn from(e: RaftError) -> Self {
        match e {
            RaftError::Config(msg) => tonic::Status::invalid_argument(msg),
            _ => tonic::Status::internal(e.to_string()),
        }
    }
}
