//! The error a cluster configuration check reports (`cluster.rs`).

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ValidationError {
    #[error("Required field missing: {0}")]
    RequiredFieldMissing(String),
    
    #[error("Invalid type for field {field}: expected {expected}, got {actual}")]
    InvalidType {
        field: String,
        expected: String,
        actual: String,
    },
    
    #[error("Value out of range for field {field}: {message}")]
    OutOfRange { field: String, message: String },
    
    #[error("Invalid format for field {field}: {message}")]
    InvalidFormat { field: String, message: String },
    
    #[error("Custom validation failed: {0}")]
    Custom(String),
    
    #[error("Validation errors: {0:?}")]
    Multiple(Vec<ValidationError>),
}
