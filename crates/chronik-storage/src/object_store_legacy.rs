//! Object storage abstraction.

use async_trait::async_trait;
use chronik_common::{Bytes, Result, Error};
use opendal::{Operator, services};
use std::sync::Arc;

/// Configuration for object storage.
#[derive(Debug, Clone)]
pub struct ObjectStoreConfig {
    pub backend: ObjectStoreBackend,
    pub bucket: String,
    pub prefix: Option<String>,
    pub retry_attempts: u32,
    pub connection_timeout: std::time::Duration,
}

impl Default for ObjectStoreConfig {
    fn default() -> Self {
        Self {
            backend: ObjectStoreBackend::Local { path: "/tmp/chronik-storage".to_string() },
            bucket: "chronik".to_string(),
            prefix: None,
            retry_attempts: 3,
            connection_timeout: std::time::Duration::from_secs(10),
        }
    }
}

/// Supported object storage backends.
#[derive(Debug, Clone)]
pub enum ObjectStoreBackend {
    S3 { 
        endpoint: Option<String>, 
        region: String,
        access_key_id: String,
        secret_access_key: String,
    },
    Gcs {
        credential_path: Option<String>,
    },
    Azure {
        account_name: String,
        account_key: String,
    },
    Local { 
        path: String 
    },
}

/// Trait for object storage operations.
#[async_trait]
pub trait ObjectStore: Send + Sync {
    /// Put an object into storage.
    async fn put(&self, key: &str, data: Bytes) -> Result<()>;
    
    /// Get an object from storage.
    async fn get(&self, key: &str) -> Result<Bytes>;
    
    /// Get a byte range from an object.
    async fn get_range(&self, key: &str, offset: u64, length: u64) -> Result<Bytes>;
    
    /// Delete an object from storage.
    async fn delete(&self, key: &str) -> Result<()>;
    
    /// List objects with a given prefix.
    async fn list(&self, prefix: &str) -> Result<Vec<String>>;
    
    /// Check if an object exists.
    async fn exists(&self, key: &str) -> Result<bool>;
    
    /// Get object metadata.
    async fn head(&self, key: &str) -> Result<ObjectMetadata>;
}

/// Object metadata.
#[derive(Debug, Clone)]
pub struct ObjectMetadata {
    pub size: u64,
    pub last_modified: chrono::DateTime<chrono::Utc>,
    pub etag: Option<String>,
}

/// OpenDAL-based object store implementation.
pub struct OpenDalObjectStore {
    operator: Operator,
    prefix: Option<String>,
    retry_attempts: u32,
}

impl OpenDalObjectStore {
    /// Create a new object store from configuration.
    pub async fn new(config: ObjectStoreConfig) -> Result<Arc<dyn ObjectStore>> {
        let operator = match config.backend {
            ObjectStoreBackend::S3 { endpoint, region, access_key_id, secret_access_key } => {
                let mut builder = services::S3::default();
                builder.bucket(&config.bucket);
                builder.region(&region);
                builder.access_key_id(&access_key_id);
                builder.secret_access_key(&secret_access_key);
                
                if let Some(endpoint) = endpoint {
                    builder.endpoint(&endpoint);
                }
                
                Operator::new(builder)
                    .map_err(|e| Error::Storage(format!("Failed to create operator: {}", e)))?
                    .finish()
            }
            ObjectStoreBackend::Gcs { credential_path } => {
                let mut builder = services::Gcs::default();
                builder.bucket(&config.bucket);
                
                if let Some(path) = credential_path {
                    builder.credential_path(&path);
                }
                
                Operator::new(builder)
                    .map_err(|e| Error::Storage(format!("Failed to create operator: {}", e)))?
                    .finish()
            }
            ObjectStoreBackend::Azure { account_name, account_key } => {
                let mut builder = services::Azblob::default();
                builder.container(&config.bucket);
                builder.account_name(&account_name);
                builder.account_key(&account_key);
                
                Operator::new(builder)
                    .map_err(|e| Error::Storage(format!("Failed to create operator: {}", e)))?
                    .finish()
            }
            ObjectStoreBackend::Local { path } => {
                let mut builder = services::Fs::default();
                builder.root(&path);
                
                Operator::new(builder)
                    .map_err(|e| Error::Storage(format!("Failed to create operator: {}", e)))?
                    .finish()
            }
        };
        
        Ok(Arc::new(Self {
            operator,
            prefix: config.prefix,
            retry_attempts: config.retry_attempts,
        }))
    }
    
    /// Build the full key including prefix.
    fn build_key(&self, key: &str) -> String {
        match &self.prefix {
            Some(prefix) => format!("{}/{}", prefix, key),
            None => key.to_string(),
        }
    }
}

#[async_trait]
impl ObjectStore for OpenDalObjectStore {
    async fn put(&self, key: &str, data: Bytes) -> Result<()> {
        let full_key = self.build_key(key);
        
        // Retry logic with exponential backoff
        let mut attempts = 0;
        loop {
            match self.operator.write(&full_key, data.clone()).await {
                Ok(_) => return Ok(()),
                Err(_e) if attempts < self.retry_attempts => {
                    attempts += 1;
                    let delay = std::time::Duration::from_millis(100 * 2u64.pow(attempts));
                    tokio::time::sleep(delay).await;
                    continue;
                }
                Err(e) => return Err(Error::Storage(format!("Failed to put object: {}", e))),
            }
        }
    }
    
    async fn get(&self, key: &str) -> Result<Bytes> {
        let full_key = self.build_key(key);
        
        match self.operator.read(&full_key).await {
            Ok(data) => Ok(Bytes::from(data.to_vec())),
            Err(e) => Err(Error::Storage(format!("Failed to get object: {}", e))),
        }
    }
    
    async fn get_range(&self, key: &str, offset: u64, length: u64) -> Result<Bytes> {
        let full_key = self.build_key(key);
        
        match self.operator.read_with(&full_key)
            .range(offset..offset + length)
            .await 
        {
            Ok(data) => Ok(Bytes::from(data.to_vec())),
            Err(e) => Err(Error::Storage(format!("Failed to get object range: {}", e))),
        }
    }
    
    async fn delete(&self, key: &str) -> Result<()> {
        let full_key = self.build_key(key);
        
        match self.operator.delete(&full_key).await {
            Ok(_) => Ok(()),
            Err(e) => Err(Error::Storage(format!("Failed to delete object: {}", e))),
        }
    }
    
    async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        let full_prefix = self.build_key(prefix);
        
        let mut keys = Vec::new();
        let entries = self.operator.list(&full_prefix).await
            .map_err(|e| Error::Storage(format!("Failed to list objects: {}", e)))?;
        
        for entry in entries {
            let name = entry.name();
            // Remove the prefix if present
            let key = if let Some(ref p) = self.prefix {
                name.strip_prefix(&format!("{}/", p)).unwrap_or(name)
            } else {
                name
            };
            keys.push(key.to_string());
        }
        
        Ok(keys)
    }
    
    async fn exists(&self, key: &str) -> Result<bool> {
        let full_key = self.build_key(key);
        
        match self.operator.is_exist(&full_key).await {
            Ok(exists) => Ok(exists),
            Err(e) => Err(Error::Storage(format!("Failed to check existence: {}", e))),
        }
    }
    
    async fn head(&self, key: &str) -> Result<ObjectMetadata> {
        let full_key = self.build_key(key);
        
        match self.operator.stat(&full_key).await {
            Ok(metadata) => Ok(ObjectMetadata {
                size: metadata.content_length(),
                last_modified: metadata.last_modified()
                    .unwrap_or_else(|| chrono::Utc::now()),
                etag: metadata.etag().map(|s| s.to_string()),
            }),
            Err(e) => Err(Error::Storage(format!("Failed to get metadata: {}", e))),
        }
    }
}