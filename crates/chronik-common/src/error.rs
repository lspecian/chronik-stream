//! Error types for Chronik Stream.

use thiserror::Error;

/// Result type alias for Chronik Stream operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Main error type for Chronik Stream.
#[derive(Error, Debug)]
pub enum Error {
    /// I/O errors
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Serialization errors
    #[error("Serialization error: {0}")]
    Serialization(String),

    /// Storage errors
    #[error("Storage error: {0}")]
    Storage(String),

    /// Protocol errors
    #[error("Protocol error: {0}")]
    Protocol(String),
    
    /// Network errors
    #[error("Network error: {0}")]
    Network(String),

    /// Database errors
    #[error("Database error: {0}")]
    Database(String),

    /// Configuration errors
    #[error("Configuration error: {0}")]
    Configuration(String),

    /// Not found errors
    #[error("Not found: {0}")]
    NotFound(String),

    /// Invalid input errors
    #[error("Invalid input: {0}")]
    InvalidInput(String),

    /// Invalid segment errors
    #[error("Invalid segment: {0}")]
    InvalidSegment(String),

    /// Internal errors
    #[error("Internal error: {0}")]
    Internal(String),

    /// Other errors
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Serialization(e.to_string())
    }
}

