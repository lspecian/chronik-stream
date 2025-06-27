//! Core object storage trait and implementations.

use async_trait::async_trait;
use bytes::Bytes;
use chronik_common::Result as ChronikResult;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::pin::Pin;
use tokio::io::{AsyncRead, AsyncWrite};
use tracing::instrument;

use crate::object_store::{
    config::ObjectStoreConfig,
    errors::{ObjectStoreError, ObjectStoreResult},
    retry::{retry_async_with_backoff, RetryConfig},
};

/// Core object storage trait
#[async_trait]
pub trait ObjectStore: Send + Sync {
    /// Put an object into storage
    async fn put(&self, key: &str, data: Bytes) -> ObjectStoreResult<()> {
        self.put_with_options(key, data, PutOptions::default()).await
    }
    
    /// Put an object with options
    async fn put_with_options(&self, key: &str, data: Bytes, options: PutOptions) -> ObjectStoreResult<()>;
    
    /// Get an object from storage
    async fn get(&self, key: &str) -> ObjectStoreResult<Bytes> {
        self.get_with_options(key, GetOptions::default()).await
    }
    
    /// Get an object with options
    async fn get_with_options(&self, key: &str, options: GetOptions) -> ObjectStoreResult<Bytes>;
    
    /// Get a byte range from an object
    async fn get_range(&self, key: &str, offset: u64, length: u64) -> ObjectStoreResult<Bytes> {
        let options = GetOptions {
            range: Some((offset, offset + length - 1)),
            ..Default::default()
        };
        self.get_with_options(key, options).await
    }
    
    /// Delete an object from storage
    async fn delete(&self, key: &str) -> ObjectStoreResult<()>;
    
    /// Delete multiple objects
    async fn delete_batch(&self, keys: &[String]) -> ObjectStoreResult<Vec<ObjectStoreResult<()>>>;
    
    /// List objects with a given prefix
    async fn list(&self, prefix: &str) -> ObjectStoreResult<Vec<ObjectMetadata>> {
        self.list_with_options(prefix, ListOptions::default()).await
    }
    
    /// List objects with options
    async fn list_with_options(&self, prefix: &str, options: ListOptions) -> ObjectStoreResult<Vec<ObjectMetadata>>;
    
    /// Check if an object exists
    async fn exists(&self, key: &str) -> ObjectStoreResult<bool>;
    
    /// Get object metadata
    async fn head(&self, key: &str) -> ObjectStoreResult<ObjectMetadata>;
    
    /// Copy an object within the same storage
    async fn copy(&self, from_key: &str, to_key: &str) -> ObjectStoreResult<()>;
    
    /// Start a multipart upload
    async fn start_multipart_upload(&self, key: &str) -> ObjectStoreResult<MultipartUpload>;
    
    /// Upload a part for multipart upload
    async fn upload_part(
        &self,
        upload: &MultipartUpload,
        part_number: u32,
        data: Bytes,
    ) -> ObjectStoreResult<MultipartUploadPart>;
    
    /// Complete a multipart upload
    async fn complete_multipart_upload(
        &self,
        upload: &MultipartUpload,
        parts: Vec<MultipartUploadPart>,
    ) -> ObjectStoreResult<()>;
    
    /// Abort a multipart upload
    async fn abort_multipart_upload(&self, upload: &MultipartUpload) -> ObjectStoreResult<()>;
    
    /// Get a pre-signed URL for object access
    async fn presign_get(&self, key: &str, expires_in: std::time::Duration) -> ObjectStoreResult<String>;
    
    /// Get a pre-signed URL for object upload
    async fn presign_put(&self, key: &str, expires_in: std::time::Duration) -> ObjectStoreResult<String>;
    
    /// Stream an object to a writer
    async fn stream_to_writer(
        &self,
        key: &str,
        writer: Pin<Box<dyn AsyncWrite + Send>>,
    ) -> ObjectStoreResult<u64>;
    
    /// Stream from a reader to an object
    async fn stream_from_reader(
        &self,
        key: &str,
        reader: Pin<Box<dyn AsyncRead + Send>>,
        content_length: Option<u64>,
    ) -> ObjectStoreResult<()>;
}

/// Object metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectMetadata {
    /// Object key
    pub key: String,
    
    /// Object size in bytes
    pub size: u64,
    
    /// Last modified timestamp (milliseconds since epoch)
    pub last_modified: u64,
    
    /// ETag (if available)
    pub etag: Option<String>,
    
    /// Content type
    pub content_type: Option<String>,
    
    /// Content encoding
    pub content_encoding: Option<String>,
    
    /// Cache control
    pub cache_control: Option<String>,
    
    /// User-defined metadata
    pub metadata: HashMap<String, String>,
    
    /// Storage class
    pub storage_class: Option<String>,
    
    /// Server-side encryption
    pub encryption: Option<String>,
    
    /// Version ID (for versioned buckets)
    pub version_id: Option<String>,
}

/// Options for put operations
#[derive(Debug, Clone, Default)]
pub struct PutOptions {
    /// Content type
    pub content_type: Option<String>,
    
    /// Content encoding
    pub content_encoding: Option<String>,
    
    /// Cache control
    pub cache_control: Option<String>,
    
    /// User-defined metadata
    pub metadata: Option<HashMap<String, String>>,
    
    /// Storage class
    pub storage_class: Option<String>,
    
    /// Server-side encryption
    pub encryption: Option<String>,
    
    /// Checksum to verify (MD5, SHA256, etc.)
    pub checksum: Option<String>,
    
    /// Whether to overwrite existing object
    pub overwrite: bool,
    
    /// Conditional headers
    pub if_none_match: Option<String>,
    pub if_match: Option<String>,
}

/// Options for get operations
#[derive(Debug, Clone, Default)]
pub struct GetOptions {
    /// Byte range (start, end) inclusive
    pub range: Option<(u64, u64)>,
    
    /// Conditional headers (timestamps in milliseconds since epoch)
    pub if_modified_since: Option<u64>,
    pub if_unmodified_since: Option<u64>,
    pub if_match: Option<String>,
    pub if_none_match: Option<String>,
    
    /// Version ID (for versioned buckets)
    pub version_id: Option<String>,
}

/// Options for list operations
#[derive(Debug, Clone, Default)]
pub struct ListOptions {
    /// Maximum number of objects to return
    pub limit: Option<usize>,
    
    /// Continuation token for pagination
    pub continuation_token: Option<String>,
    
    /// Delimiter for hierarchical listing
    pub delimiter: Option<String>,
    
    /// Include object metadata
    pub include_metadata: bool,
    
    /// Recursive listing
    pub recursive: bool,
}

/// Multipart upload state
#[derive(Debug, Clone)]
pub struct MultipartUpload {
    /// Upload ID
    pub upload_id: String,
    
    /// Object key
    pub key: String,
    
    /// Backend-specific data
    pub backend_data: HashMap<String, String>,
}

/// Multipart upload part
#[derive(Debug, Clone)]
pub struct MultipartUploadPart {
    /// Part number (1-based)
    pub part_number: u32,
    
    /// ETag of the uploaded part
    pub etag: String,
    
    /// Size of the part
    pub size: u64,
}

/// Factory function to create object store instances
pub async fn create_object_store(config: ObjectStoreConfig) -> ObjectStoreResult<Box<dyn ObjectStore>> {
    // Validate configuration
    config.validate().map_err(|msg| ObjectStoreError::InvalidConfiguration { message: msg })?;
    
    // Create the appropriate backend
    match &config.backend {
        crate::object_store::config::StorageBackend::S3 { .. } => {
            let backend = crate::object_store::backends::S3Backend::new(config).await?;
            Ok(Box::new(backend))
        }
        crate::object_store::config::StorageBackend::Gcs { .. } => {
            let backend = crate::object_store::backends::GcsBackend::new(config).await?;
            Ok(Box::new(backend))
        }
        crate::object_store::config::StorageBackend::Azure { .. } => {
            let backend = crate::object_store::backends::AzureBackend::new(config).await?;
            Ok(Box::new(backend))
        }
        crate::object_store::config::StorageBackend::Local { .. } => {
            let backend = crate::object_store::backends::LocalBackend::new(config).await?;
            Ok(Box::new(backend))
        }
    }
}

/// Wrapper that adds retry logic to any ObjectStore implementation
pub struct RetryableObjectStore<T> {
    inner: T,
    retry_config: RetryConfig,
}

impl<T> RetryableObjectStore<T>
where
    T: ObjectStore,
{
    pub fn new(inner: T, retry_config: RetryConfig) -> Self {
        Self { inner, retry_config }
    }
}

#[async_trait]
impl<T> ObjectStore for RetryableObjectStore<T>
where
    T: ObjectStore,
{
    #[instrument(skip(self, data))]
    async fn put_with_options(&self, key: &str, data: Bytes, options: PutOptions) -> ObjectStoreResult<()> {
        retry_async_with_backoff(
            self.retry_config.clone(),
            "put_with_options",
            || self.inner.put_with_options(key, data.clone(), options.clone()),
        ).await
    }
    
    #[instrument(skip(self))]
    async fn get_with_options(&self, key: &str, options: GetOptions) -> ObjectStoreResult<Bytes> {
        retry_async_with_backoff(
            self.retry_config.clone(),
            "get_with_options",
            || self.inner.get_with_options(key, options.clone()),
        ).await
    }
    
    #[instrument(skip(self))]
    async fn delete(&self, key: &str) -> ObjectStoreResult<()> {
        retry_async_with_backoff(
            self.retry_config.clone(),
            "delete",
            || self.inner.delete(key),
        ).await
    }
    
    #[instrument(skip(self))]
    async fn delete_batch(&self, keys: &[String]) -> ObjectStoreResult<Vec<ObjectStoreResult<()>>> {
        retry_async_with_backoff(
            self.retry_config.clone(),
            "delete_batch",
            || self.inner.delete_batch(keys),
        ).await
    }
    
    #[instrument(skip(self))]
    async fn list_with_options(&self, prefix: &str, options: ListOptions) -> ObjectStoreResult<Vec<ObjectMetadata>> {
        retry_async_with_backoff(
            self.retry_config.clone(),
            "list_with_options",
            || self.inner.list_with_options(prefix, options.clone()),
        ).await
    }
    
    #[instrument(skip(self))]
    async fn exists(&self, key: &str) -> ObjectStoreResult<bool> {
        retry_async_with_backoff(
            self.retry_config.clone(),
            "exists",
            || self.inner.exists(key),
        ).await
    }
    
    #[instrument(skip(self))]
    async fn head(&self, key: &str) -> ObjectStoreResult<ObjectMetadata> {
        retry_async_with_backoff(
            self.retry_config.clone(),
            "head",
            || self.inner.head(key),
        ).await
    }
    
    #[instrument(skip(self))]
    async fn copy(&self, from_key: &str, to_key: &str) -> ObjectStoreResult<()> {
        retry_async_with_backoff(
            self.retry_config.clone(),
            "copy",
            || self.inner.copy(from_key, to_key),
        ).await
    }
    
    #[instrument(skip(self))]
    async fn start_multipart_upload(&self, key: &str) -> ObjectStoreResult<MultipartUpload> {
        retry_async_with_backoff(
            self.retry_config.clone(),
            "start_multipart_upload",
            || self.inner.start_multipart_upload(key),
        ).await
    }
    
    #[instrument(skip(self, data))]
    async fn upload_part(
        &self,
        upload: &MultipartUpload,
        part_number: u32,
        data: Bytes,
    ) -> ObjectStoreResult<MultipartUploadPart> {
        retry_async_with_backoff(
            self.retry_config.clone(),
            "upload_part",
            || self.inner.upload_part(upload, part_number, data.clone()),
        ).await
    }
    
    #[instrument(skip(self))]
    async fn complete_multipart_upload(
        &self,
        upload: &MultipartUpload,
        parts: Vec<MultipartUploadPart>,
    ) -> ObjectStoreResult<()> {
        retry_async_with_backoff(
            self.retry_config.clone(),
            "complete_multipart_upload",
            || self.inner.complete_multipart_upload(upload, parts.clone()),
        ).await
    }
    
    #[instrument(skip(self))]
    async fn abort_multipart_upload(&self, upload: &MultipartUpload) -> ObjectStoreResult<()> {
        retry_async_with_backoff(
            self.retry_config.clone(),
            "abort_multipart_upload",
            || self.inner.abort_multipart_upload(upload),
        ).await
    }
    
    #[instrument(skip(self))]
    async fn presign_get(&self, key: &str, expires_in: std::time::Duration) -> ObjectStoreResult<String> {
        // Pre-signing typically doesn't need retry as it's not a network operation
        self.inner.presign_get(key, expires_in).await
    }
    
    #[instrument(skip(self))]
    async fn presign_put(&self, key: &str, expires_in: std::time::Duration) -> ObjectStoreResult<String> {
        // Pre-signing typically doesn't need retry as it's not a network operation
        self.inner.presign_put(key, expires_in).await
    }
    
    #[instrument(skip(self, writer))]
    async fn stream_to_writer(
        &self,
        key: &str,
        writer: Pin<Box<dyn AsyncWrite + Send>>,
    ) -> ObjectStoreResult<u64> {
        // Stream operations are not easily retryable due to ownership issues
        // Retry logic would need to be implemented at a higher level
        self.inner.stream_to_writer(key, writer).await
    }
    
    #[instrument(skip(self, reader))]
    async fn stream_from_reader(
        &self,
        key: &str,
        reader: Pin<Box<dyn AsyncRead + Send>>,
        content_length: Option<u64>,
    ) -> ObjectStoreResult<()> {
        // Stream operations are not easily retryable due to ownership issues
        // Retry logic would need to be implemented at a higher level
        self.inner.stream_from_reader(key, reader, content_length).await
    }
}

/// Create a retryable object store wrapper
pub fn with_retry<T>(store: T, retry_config: RetryConfig) -> RetryableObjectStore<T>
where
    T: ObjectStore,
{
    RetryableObjectStore::new(store, retry_config)
}

/// Utility functions
impl ObjectMetadata {
    /// Create basic metadata
    pub fn new(key: String, size: u64) -> Self {
        Self {
            key,
            size,
            last_modified: chrono::Utc::now().timestamp_millis() as u64,
            etag: None,
            content_type: None,
            content_encoding: None,
            cache_control: None,
            metadata: HashMap::new(),
            storage_class: None,
            encryption: None,
            version_id: None,
        }
    }
    
    /// Check if object is recently modified
    pub fn is_recently_modified(&self, threshold_millis: u64) -> bool {
        let now = chrono::Utc::now().timestamp_millis() as u64;
        now - self.last_modified < threshold_millis
    }
}

impl PutOptions {
    /// Create options for JSON content
    pub fn json() -> Self {
        Self {
            content_type: Some("application/json".to_string()),
            ..Default::default()
        }
    }
    
    /// Create options for binary content
    pub fn binary() -> Self {
        Self {
            content_type: Some("application/octet-stream".to_string()),
            ..Default::default()
        }
    }
    
    /// Set content type
    pub fn with_content_type(mut self, content_type: impl Into<String>) -> Self {
        self.content_type = Some(content_type.into());
        self
    }
    
    /// Set metadata
    pub fn with_metadata(mut self, metadata: HashMap<String, String>) -> Self {
        self.metadata = Some(metadata);
        self
    }
    
    /// Add metadata entry
    pub fn add_metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata.get_or_insert_with(HashMap::new).insert(key.into(), value.into());
        self
    }
}

impl ListOptions {
    /// Create options for recursive listing
    pub fn recursive() -> Self {
        Self {
            recursive: true,
            ..Default::default()
        }
    }
    
    /// Set limit
    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = Some(limit);
        self
    }
    
    /// Set delimiter
    pub fn with_delimiter(mut self, delimiter: impl Into<String>) -> Self {
        self.delimiter = Some(delimiter.into());
        self
    }
}

/// Convert from Chronik Result to ObjectStore Result
pub fn from_chronik_result<T>(result: ChronikResult<T>) -> ObjectStoreResult<T> {
    result.map_err(|e| ObjectStoreError::Other(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_put_options_builder() {
        let options = PutOptions::json()
            .with_content_type("application/json; charset=utf-8")
            .add_metadata("author", "test")
            .add_metadata("version", "1.0");
        
        assert_eq!(options.content_type, Some("application/json; charset=utf-8".to_string()));
        assert!(options.metadata.is_some());
        
        let metadata = options.metadata.unwrap();
        assert_eq!(metadata.get("author"), Some(&"test".to_string()));
        assert_eq!(metadata.get("version"), Some(&"1.0".to_string()));
    }

    #[test]
    fn test_list_options_builder() {
        let options = ListOptions::recursive()
            .with_limit(100)
            .with_delimiter("/");
        
        assert!(options.recursive);
        assert_eq!(options.limit, Some(100));
        assert_eq!(options.delimiter, Some("/".to_string()));
    }

    #[test]
    fn test_object_metadata_recent_modification() {
        let mut metadata = ObjectMetadata::new("test".to_string(), 1024);
        // Set last modified to 5 minutes ago
        metadata.last_modified = (chrono::Utc::now() - chrono::Duration::minutes(5)).timestamp_millis() as u64;
        
        // 1 hour = 3600000 milliseconds
        assert!(metadata.is_recently_modified(3600000));
        // 1 minute = 60000 milliseconds
        assert!(!metadata.is_recently_modified(60000));
    }
}