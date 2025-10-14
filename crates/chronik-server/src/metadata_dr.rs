//! Metadata Disaster Recovery Integration
//!
//! This module integrates the metadata uploader with the chronik-storage object store
//! to enable disaster recovery capabilities.

use chronik_common::metadata::{ObjectStoreAdapter, ObjectStoreImpl};
use chronik_storage::object_store::ObjectStore;
use std::sync::Arc;

/// Adapter implementation for chronik-storage's ObjectStore
pub struct ChronikObjectStoreAdapter {
    store: Arc<dyn ObjectStore>,
}

impl ChronikObjectStoreAdapter {
    pub fn new(store: Arc<dyn ObjectStore>) -> Self {
        Self { store }
    }
}

impl ObjectStoreImpl for ChronikObjectStoreAdapter {
    async fn put_blocking(
        &self,
        key: &str,
        data: bytes::Bytes,
    ) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.store
            .put(key, data)
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }

    async fn get_blocking(
        &self,
        key: &str,
    ) -> std::result::Result<bytes::Bytes, Box<dyn std::error::Error + Send + Sync>> {
        self.store
            .get(key)
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }

    async fn list_blocking(
        &self,
        prefix: &str,
    ) -> std::result::Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
        let metadata = self
            .store
            .list(prefix)
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

        Ok(metadata.iter().map(|m| m.key.clone()).collect())
    }

    async fn exists_blocking(
        &self,
        key: &str,
    ) -> std::result::Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        self.store
            .exists(key)
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }

    async fn delete_blocking(
        &self,
        key: &str,
    ) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.store
            .delete(key)
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }
}

/// Create metadata uploader from object store
pub fn create_metadata_uploader(
    object_store: Arc<dyn ObjectStore>,
    data_dir: &str,
    config: chronik_common::metadata::MetadataUploaderConfig,
) -> chronik_common::metadata::MetadataUploader {
    let adapter = Arc::new(ChronikObjectStoreAdapter::new(object_store));
    let object_store_adapter = Arc::new(ObjectStoreAdapter::new(adapter));

    chronik_common::metadata::MetadataUploader::new(
        config,
        object_store_adapter,
        data_dir,
    )
}
