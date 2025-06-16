//! Azure Blob Storage backend implementation.

use async_trait::async_trait;
use bytes::Bytes;
use std::pin::Pin;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::object_store::{
    config::ObjectStoreConfig,
    errors::{ObjectStoreError, ObjectStoreResult},
    storage::{
        GetOptions, ListOptions, MultipartUpload, MultipartUploadPart, ObjectMetadata,
        ObjectStore, PutOptions,
    },
};

/// Azure Blob Storage backend implementation
pub struct AzureBackend {
    _config: ObjectStoreConfig,
}

impl AzureBackend {
    /// Create a new Azure backend
    pub async fn new(config: ObjectStoreConfig) -> ObjectStoreResult<Self> {
        // TODO: Implement Azure backend using azure-storage-blobs crate
        Ok(Self { _config: config })
    }
}

#[async_trait]
impl ObjectStore for AzureBackend {
    async fn put_with_options(
        &self,
        _key: &str,
        _data: Bytes,
        _options: PutOptions,
    ) -> ObjectStoreResult<()> {
        Err(ObjectStoreError::UnsupportedOperation {
            operation: "put".to_string(),
            backend: "azure".to_string(),
        })
    }

    async fn get_with_options(&self, _key: &str, _options: GetOptions) -> ObjectStoreResult<Bytes> {
        Err(ObjectStoreError::UnsupportedOperation {
            operation: "get".to_string(),
            backend: "azure".to_string(),
        })
    }

    async fn delete(&self, _key: &str) -> ObjectStoreResult<()> {
        Err(ObjectStoreError::UnsupportedOperation {
            operation: "delete".to_string(),
            backend: "azure".to_string(),
        })
    }

    async fn delete_batch(&self, _keys: &[String]) -> ObjectStoreResult<Vec<ObjectStoreResult<()>>> {
        Err(ObjectStoreError::UnsupportedOperation {
            operation: "delete_batch".to_string(),
            backend: "azure".to_string(),
        })
    }

    async fn list_with_options(
        &self,
        _prefix: &str,
        _options: ListOptions,
    ) -> ObjectStoreResult<Vec<ObjectMetadata>> {
        Err(ObjectStoreError::UnsupportedOperation {
            operation: "list".to_string(),
            backend: "azure".to_string(),
        })
    }

    async fn exists(&self, _key: &str) -> ObjectStoreResult<bool> {
        Err(ObjectStoreError::UnsupportedOperation {
            operation: "exists".to_string(),
            backend: "azure".to_string(),
        })
    }

    async fn head(&self, _key: &str) -> ObjectStoreResult<ObjectMetadata> {
        Err(ObjectStoreError::UnsupportedOperation {
            operation: "head".to_string(),
            backend: "azure".to_string(),
        })
    }

    async fn copy(&self, _from_key: &str, _to_key: &str) -> ObjectStoreResult<()> {
        Err(ObjectStoreError::UnsupportedOperation {
            operation: "copy".to_string(),
            backend: "azure".to_string(),
        })
    }

    async fn start_multipart_upload(&self, _key: &str) -> ObjectStoreResult<MultipartUpload> {
        Err(ObjectStoreError::UnsupportedOperation {
            operation: "start_multipart_upload".to_string(),
            backend: "azure".to_string(),
        })
    }

    async fn upload_part(
        &self,
        _upload: &MultipartUpload,
        _part_number: u32,
        _data: Bytes,
    ) -> ObjectStoreResult<MultipartUploadPart> {
        Err(ObjectStoreError::UnsupportedOperation {
            operation: "upload_part".to_string(),
            backend: "azure".to_string(),
        })
    }

    async fn complete_multipart_upload(
        &self,
        _upload: &MultipartUpload,
        _parts: Vec<MultipartUploadPart>,
    ) -> ObjectStoreResult<()> {
        Err(ObjectStoreError::UnsupportedOperation {
            operation: "complete_multipart_upload".to_string(),
            backend: "azure".to_string(),
        })
    }

    async fn abort_multipart_upload(&self, _upload: &MultipartUpload) -> ObjectStoreResult<()> {
        Err(ObjectStoreError::UnsupportedOperation {
            operation: "abort_multipart_upload".to_string(),
            backend: "azure".to_string(),
        })
    }

    async fn presign_get(&self, _key: &str, _expires_in: Duration) -> ObjectStoreResult<String> {
        Err(ObjectStoreError::UnsupportedOperation {
            operation: "presign_get".to_string(),
            backend: "azure".to_string(),
        })
    }

    async fn presign_put(&self, _key: &str, _expires_in: Duration) -> ObjectStoreResult<String> {
        Err(ObjectStoreError::UnsupportedOperation {
            operation: "presign_put".to_string(),
            backend: "azure".to_string(),
        })
    }

    async fn stream_to_writer(
        &self,
        _key: &str,
        _writer: Pin<Box<dyn AsyncWrite + Send>>,
    ) -> ObjectStoreResult<u64> {
        Err(ObjectStoreError::UnsupportedOperation {
            operation: "stream_to_writer".to_string(),
            backend: "azure".to_string(),
        })
    }

    async fn stream_from_reader(
        &self,
        _key: &str,
        _reader: Pin<Box<dyn AsyncRead + Send>>,
        _content_length: Option<u64>,
    ) -> ObjectStoreResult<()> {
        Err(ObjectStoreError::UnsupportedOperation {
            operation: "stream_from_reader".to_string(),
            backend: "azure".to_string(),
        })
    }
}