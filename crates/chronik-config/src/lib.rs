//! Dynamic configuration management for Chronik Stream.
//!
//! This crate provides:
//! - Runtime configuration updates
//! - File watching for automatic reloads
//! - Multi-format support (YAML, TOML, JSON)
//! - Configuration validation
//! - Change notifications

pub mod manager;
pub mod provider;
pub mod watcher;
pub mod validation;
pub mod types;

pub use manager::{ConfigManager, ConfigUpdate};
pub use provider::{ConfigProvider, FileProvider, EnvironmentProvider};
pub use watcher::ConfigWatcher;
pub use validation::{ConfigValidator, ValidationError};
pub use types::{ConfigValue, ConfigSource};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    
    #[error("Parse error: {0}")]
    Parse(String),
    
    #[error("Validation error: {0}")]
    Validation(#[from] ValidationError),
    
    #[error("Watch error: {0}")]
    Watch(#[from] notify::Error),
    
    #[error("Config not found: {0}")]
    NotFound(String),
    
    #[error("Invalid config type: expected {expected}, got {actual}")]
    TypeMismatch { expected: String, actual: String },
}

pub type Result<T> = std::result::Result<T, ConfigError>;