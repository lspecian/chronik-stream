//! Error types for object storage operations.

use chronik_common::Error as ChronikError;
use thiserror::Error;

/// Result type for object storage operations
pub type ObjectStoreResult<T> = Result<T, ObjectStoreError>;

/// Comprehensive error types for object storage operations
#[derive(Error, Debug)]
pub enum ObjectStoreError {
    /// Object not found
    #[error("Object not found: {key}")]
    NotFound { key: String },

    /// Object already exists (for conditional operations)
    #[error("Object already exists: {key}")]
    AlreadyExists { key: String },

    /// Access denied (authentication/authorization failure)
    #[error("Access denied: {message}")]
    AccessDenied { message: String },

    /// Invalid configuration
    #[error("Invalid configuration: {message}")]
    InvalidConfiguration { message: String },

    /// Network/connectivity error
    #[error("Network error: {message}")]
    NetworkError { message: String },

    /// Service unavailable (temporary failure)
    #[error("Service unavailable: {message}")]
    ServiceUnavailable { message: String },

    /// Rate limited by storage service
    #[error("Rate limited: {message}")]
    RateLimited { message: String },

    /// Storage quota exceeded
    #[error("Storage quota exceeded: {message}")]
    QuotaExceeded { message: String },

    /// Invalid object key
    #[error("Invalid key: {key} - {reason}")]
    InvalidKey { key: String, reason: String },

    /// Serialization/deserialization error
    #[error("Serialization error: {message}")]
    SerializationError { message: String },

    /// Checksum mismatch
    #[error("Checksum mismatch for key: {key}")]
    ChecksumMismatch { key: String },

    /// Operation timeout
    #[error("Operation timeout: {operation}")]
    Timeout { operation: String },

    /// Retry exhausted
    #[error("Retry exhausted after {attempts} attempts: {last_error}")]
    RetryExhausted {
        attempts: u32,
        last_error: String,
    },

    /// Internal storage service error
    #[error("Internal storage error: {message}")]
    InternalError { message: String },

    /// Multipart upload error
    #[error("Multipart upload error: {message}")]
    MultipartUploadError { message: String },

    /// Unsupported operation
    #[error("Unsupported operation: {operation} for backend: {backend}")]
    UnsupportedOperation { operation: String, backend: String },

    /// I/O error
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// AWS SDK error
    #[error("AWS error: {0}")]
    Aws(String),

    /// OpenDAL error
    #[error("OpenDAL error: {0}")]
    OpenDal(String),

    /// Connection error
    #[error("Connection error for backend {backend}: {details}")]
    ConnectionError { backend: String, details: String },

    /// Write error
    #[error("Write error for key {key}: {details}")]
    WriteError { key: String, details: String },

    /// Read error
    #[error("Read error for key {key}: {details}")]
    ReadError { key: String, details: String },

    /// Delete error
    #[error("Delete error for key {key}: {details}")]
    DeleteError { key: String, details: String },

    /// List error
    #[error("List error for prefix {prefix}: {details}")]
    ListError { prefix: String, details: String },

    /// Access error
    #[error("Access error for key {key}, operation {operation}: {details}")]
    AccessError { key: String, operation: String, details: String },

    /// Copy error
    #[error("Copy error from {from_key} to {to_key}: {details}")]
    CopyError { from_key: String, to_key: String, details: String },

    /// Stream error
    #[error("Stream error for key {key}, operation {operation}: {details}")]
    StreamError { key: String, operation: String, details: String },

    /// Other errors
    #[error("Other error: {0}")]
    Other(String),
}

impl ObjectStoreError {
    /// Check if error is retryable
    pub fn is_retryable(&self) -> bool {
        match self {
            ObjectStoreError::NetworkError { .. } => true,
            ObjectStoreError::ServiceUnavailable { .. } => true,
            ObjectStoreError::RateLimited { .. } => true,
            ObjectStoreError::Timeout { .. } => true,
            ObjectStoreError::InternalError { .. } => true,
            ObjectStoreError::Io(_) => true,
            _ => false,
        }
    }

    /// Check if error is permanent (not retryable)
    pub fn is_permanent(&self) -> bool {
        !self.is_retryable()
    }

    /// Get retry delay suggestion in milliseconds
    pub fn retry_delay_ms(&self) -> Option<u64> {
        match self {
            ObjectStoreError::RateLimited { .. } => Some(5000),
            ObjectStoreError::ServiceUnavailable { .. } => Some(1000),
            ObjectStoreError::NetworkError { .. } => Some(500),
            ObjectStoreError::Timeout { .. } => Some(2000),
            _ => None,
        }
    }
}

impl From<ObjectStoreError> for ChronikError {
    fn from(err: ObjectStoreError) -> Self {
        match err {
            ObjectStoreError::NotFound { key } => ChronikError::NotFound(key),
            ObjectStoreError::InvalidConfiguration { message } => ChronikError::Configuration(message),
            ObjectStoreError::SerializationError { message } => ChronikError::Serialization(message),
            ObjectStoreError::Io(io_err) => ChronikError::Io(io_err),
            other => ChronikError::Storage(other.to_string()),
        }
    }
}

impl From<opendal::Error> for ObjectStoreError {
    fn from(err: opendal::Error) -> Self {
        match err.kind() {
            opendal::ErrorKind::NotFound => ObjectStoreError::NotFound {
                key: "unknown".to_string(),
            },
            opendal::ErrorKind::PermissionDenied => ObjectStoreError::AccessDenied {
                message: err.to_string(),
            },
            opendal::ErrorKind::RateLimited => ObjectStoreError::RateLimited {
                message: err.to_string(),
            },
            opendal::ErrorKind::Unsupported => ObjectStoreError::UnsupportedOperation {
                operation: "unknown".to_string(),
                backend: "opendal".to_string(),
            },
            _ => ObjectStoreError::OpenDal(err.to_string()),
        }
    }
}

impl From<aws_sdk_s3::error::SdkError<aws_sdk_s3::operation::get_object::GetObjectError>> for ObjectStoreError {
    fn from(err: aws_sdk_s3::error::SdkError<aws_sdk_s3::operation::get_object::GetObjectError>) -> Self {
        match err {
            aws_sdk_s3::error::SdkError::ServiceError(service_err) => {
                match service_err.err() {
                    aws_sdk_s3::operation::get_object::GetObjectError::NoSuchKey(_) => {
                        ObjectStoreError::NotFound {
                            key: "unknown".to_string(),
                        }
                    }
                    _ => ObjectStoreError::Aws(format!("{:?}", service_err)),
                }
            }
            _ => ObjectStoreError::Aws(err.to_string()),
        }
    }
}

/// Helper macro for converting errors with context
#[macro_export]
macro_rules! storage_error {
    ($variant:ident, $($key:ident = $value:expr),*) => {
        $crate::object_store::ObjectStoreError::$variant {
            $($key: $value.into()),*
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_retryability() {
        assert!(ObjectStoreError::NetworkError {
            message: "timeout".to_string()
        }.is_retryable());

        assert!(!ObjectStoreError::NotFound {
            key: "test".to_string()
        }.is_retryable());

        assert!(ObjectStoreError::RateLimited {
            message: "throttled".to_string()
        }.is_retryable());
    }

    #[test]
    fn test_retry_delay() {
        let rate_limit_error = ObjectStoreError::RateLimited {
            message: "throttled".to_string()
        };
        assert_eq!(rate_limit_error.retry_delay_ms(), Some(5000));

        let not_found_error = ObjectStoreError::NotFound {
            key: "test".to_string()
        };
        assert_eq!(not_found_error.retry_delay_ms(), None);
    }

    #[test]
    fn test_error_conversion_to_chronik_error() {
        let storage_error = ObjectStoreError::NotFound {
            key: "test-key".to_string()
        };
        
        let chronik_error: ChronikError = storage_error.into();
        match chronik_error {
            ChronikError::NotFound(key) => assert_eq!(key, "test-key"),
            _ => panic!("Expected NotFound error"),
        }
    }
}