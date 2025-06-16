//! Enhanced Object Storage Abstraction Layer for Chronik Stream.
//!
//! This module provides a comprehensive abstraction over various object storage
//! backends including AWS S3, Google Cloud Storage, Azure Blob Storage, and
//! local filesystem. It includes features like:
//!
//! - Unified async API across all storage backends
//! - Configurable retry logic with exponential backoff
//! - Connection pooling and performance optimization
//! - Multiple authentication methods
//! - Integration with ChronikSegment format
//! - Comprehensive error handling
//! - Support for S3-compatible storage (MinIO, etc.)

pub mod auth;
pub mod backends;
pub mod config;
pub mod errors;
pub mod metrics;
pub mod retry;
pub mod storage;
pub mod chronik_integration;

pub use auth::{AuthConfig, S3Credentials, GcsCredentials, AzureCredentials};
pub use backends::{S3Backend, GcsBackend, AzureBackend, LocalBackend};
pub use config::{ObjectStoreConfig, StorageBackend, ConnectionConfig, PerformanceConfig};
pub use errors::{ObjectStoreError, ObjectStoreResult};
pub use metrics::{ObjectStoreMetrics, OperationMetrics};
pub use retry::{RetryConfig, ExponentialBackoff};
pub use storage::{
    ObjectStore, ObjectMetadata, PutOptions, GetOptions, ListOptions,
    MultipartUpload, MultipartUploadPart,
};
pub use chronik_integration::{ChronikStorageAdapter, SegmentLocation, SegmentCache};

// Re-export for backward compatibility
pub use storage::{ObjectStore as ObjectStoreTrait};

/// Default object store implementation factory
pub struct ObjectStoreFactory;

impl ObjectStoreFactory {
    /// Create a new object store instance from configuration
    pub async fn create(config: ObjectStoreConfig) -> ObjectStoreResult<Box<dyn ObjectStore>> {
        storage::create_object_store(config).await
    }
}