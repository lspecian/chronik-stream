//! Cluster configuration for Chronik Stream: the TOML file a cluster node
//! starts from (`ClusterConfig`, `NodeConfig`), and its validation.

pub mod validation;
pub mod cluster;

pub use validation::ValidationError;
pub use cluster::{ClusterConfig, GossipConfig, NodeConfig};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Parse error: {0}")]
    Parse(String),

    #[error("Validation error: {0}")]
    Validation(#[from] ValidationError),

    #[error("Config not found: {0}")]
    NotFound(String),

    #[error("Invalid config type: expected {expected}, got {actual}")]
    TypeMismatch { expected: String, actual: String },
}

pub type Result<T> = std::result::Result<T, ConfigError>;
