//! Adapter between chronik-storage ObjectStore and metadata uploader ObjectStoreInterface

use async_trait::async_trait;
use std::sync::Arc;

use super::metadata_uploader::ObjectStoreInterface;
use super::{MetadataError, Result};

/// Adapter to use chronik-storage's ObjectStore with metadata uploader
///
/// This bridges the ObjectStore trait from chronik-storage with the
/// ObjectStoreInterface trait needed by MetadataUploader, avoiding
/// circular dependencies.
pub struct ObjectStoreAdapter {
    /// The underlying object store from chronik-storage
    /// We use dynamic dispatch to avoid circular dependencies
    inner: Arc<dyn ObjectStoreInternal>,
}

/// Internal trait that matches chronik-storage's ObjectStore interface
#[async_trait]
trait ObjectStoreInternal: Send + Sync {
    async fn put(&self, key: &str, data: bytes::Bytes) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>>;
    async fn get(&self, key: &str) -> std::result::Result<bytes::Bytes, Box<dyn std::error::Error + Send + Sync>>;
    async fn list(&self, prefix: &str) -> std::result::Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>>;
    async fn exists(&self, key: &str) -> std::result::Result<bool, Box<dyn std::error::Error + Send + Sync>>;
    async fn delete(&self, key: &str) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>>;
}

/// Wrapper to adapt any type implementing the storage ObjectStore trait
struct StorageObjectStoreWrapper<T> {
    inner: Arc<T>,
}

impl ObjectStoreAdapter {
    /// Create a new adapter from a chronik-storage ObjectStore
    ///
    /// This accepts any type that implements the required methods, avoiding
    /// the need to depend directly on chronik-storage types.
    pub fn new<T>(object_store: Arc<T>) -> Self
    where
        T: Send + Sync + 'static,
        T: ObjectStoreImpl,
    {
        Self {
            inner: Arc::new(StorageObjectStoreWrapper {
                inner: object_store,
            }),
        }
    }
}

/// Trait for types that can be used as object stores
///
/// This mirrors the chronik-storage ObjectStore interface without requiring
/// a direct dependency.
pub trait ObjectStoreImpl: Send + Sync {
    fn put_blocking(
        &self,
        key: &str,
        data: bytes::Bytes,
    ) -> impl std::future::Future<Output = std::result::Result<(), Box<dyn std::error::Error + Send + Sync>>> + Send;

    fn get_blocking(
        &self,
        key: &str,
    ) -> impl std::future::Future<Output = std::result::Result<bytes::Bytes, Box<dyn std::error::Error + Send + Sync>>> + Send;

    fn list_blocking(
        &self,
        prefix: &str,
    ) -> impl std::future::Future<Output = std::result::Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>>> + Send;

    fn exists_blocking(
        &self,
        key: &str,
    ) -> impl std::future::Future<Output = std::result::Result<bool, Box<dyn std::error::Error + Send + Sync>>> + Send;

    fn delete_blocking(
        &self,
        key: &str,
    ) -> impl std::future::Future<Output = std::result::Result<(), Box<dyn std::error::Error + Send + Sync>>> + Send;
}

#[async_trait]
impl<T: ObjectStoreImpl + Send + Sync + 'static> ObjectStoreInternal for StorageObjectStoreWrapper<T> {
    async fn put(&self, key: &str, data: bytes::Bytes) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.inner.put_blocking(key, data).await
    }

    async fn get(&self, key: &str) -> std::result::Result<bytes::Bytes, Box<dyn std::error::Error + Send + Sync>> {
        self.inner.get_blocking(key).await
    }

    async fn list(&self, prefix: &str) -> std::result::Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
        self.inner.list_blocking(prefix).await
    }

    async fn exists(&self, key: &str) -> std::result::Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        self.inner.exists_blocking(key).await
    }

    async fn delete(&self, key: &str) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.inner.delete_blocking(key).await
    }
}

#[async_trait]
impl ObjectStoreInterface for ObjectStoreAdapter {
    async fn put(&self, key: &str, data: bytes::Bytes) -> Result<()> {
        self.inner
            .put(key, data)
            .await
            .map_err(|e| MetadataError::StorageError(format!("Object store put failed: {}", e)))
    }

    async fn get(&self, key: &str) -> Result<bytes::Bytes> {
        self.inner
            .get(key)
            .await
            .map_err(|e| MetadataError::StorageError(format!("Object store get failed: {}", e)))
    }

    async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        self.inner
            .list(prefix)
            .await
            .map_err(|e| MetadataError::StorageError(format!("Object store list failed: {}", e)))
    }

    async fn exists(&self, key: &str) -> Result<bool> {
        self.inner
            .exists(key)
            .await
            .map_err(|e| MetadataError::StorageError(format!("Object store exists failed: {}", e)))
    }

    async fn delete(&self, key: &str) -> Result<()> {
        self.inner
            .delete(key)
            .await
            .map_err(|e| MetadataError::StorageError(format!("Object store delete failed: {}", e)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tokio::sync::Mutex;

    struct MockStore {
        data: Arc<Mutex<HashMap<String, bytes::Bytes>>>,
    }

    impl MockStore {
        fn new() -> Self {
            Self {
                data: Arc::new(Mutex::new(HashMap::new())),
            }
        }
    }

    impl ObjectStoreImpl for MockStore {
        async fn put_blocking(&self, key: &str, data: bytes::Bytes) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
            self.data.lock().await.insert(key.to_string(), data);
            Ok(())
        }

        async fn get_blocking(&self, key: &str) -> std::result::Result<bytes::Bytes, Box<dyn std::error::Error + Send + Sync>> {
            self.data
                .lock()
                .await
                .get(key)
                .cloned()
                .ok_or_else(|| "Not found".into())
        }

        async fn list_blocking(&self, prefix: &str) -> std::result::Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
            let data = self.data.lock().await;
            Ok(data
                .keys()
                .filter(|k| k.starts_with(prefix))
                .cloned()
                .collect())
        }

        async fn exists_blocking(&self, key: &str) -> std::result::Result<bool, Box<dyn std::error::Error + Send + Sync>> {
            Ok(self.data.lock().await.contains_key(key))
        }

        async fn delete_blocking(&self, key: &str) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
            self.data.lock().await.remove(key);
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_adapter() {
        let store = Arc::new(MockStore::new());
        let adapter = ObjectStoreAdapter::new(store);

        // Test put and get
        let data = bytes::Bytes::from("test data");
        adapter.put("test-key", data.clone()).await.unwrap();

        let retrieved = adapter.get("test-key").await.unwrap();
        assert_eq!(retrieved, data);

        // Test list
        adapter.put("test-key-2", bytes::Bytes::from("data2")).await.unwrap();
        let keys = adapter.list("test").await.unwrap();
        assert_eq!(keys.len(), 2);

        // Test exists
        assert!(adapter.exists("test-key").await.unwrap());
        assert!(!adapter.exists("missing").await.unwrap());

        // Test delete
        adapter.delete("test-key").await.unwrap();
        assert!(!adapter.exists("test-key").await.unwrap());
    }
}
