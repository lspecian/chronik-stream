//! Azure Blob Storage backend implementation.

use async_trait::async_trait;
use bytes::Bytes;
use opendal::{layers::RetryLayer, services::Azblob, Operator};
use std::pin::Pin;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use futures::TryStreamExt;
use std::collections::HashMap;

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
    operator: Operator,
    config: ObjectStoreConfig,
}

impl AzureBackend {
    /// Create a new Azure backend
    pub async fn new(config: ObjectStoreConfig) -> ObjectStoreResult<Self> {
        let mut builder = Azblob::default();
        
        // Configure container/bucket
        builder.container(&config.bucket);
        
        // Extract Azure-specific configuration
        match &config.backend {
            crate::object_store::config::StorageBackend::Azure { account_name, endpoint, .. } => {
                // Configure account name
                builder.account_name(account_name);
                
                // Configure endpoint if provided
                if let Some(endpoint) = endpoint {
                    builder.endpoint(endpoint);
                }
            }
            _ => {
                return Err(ObjectStoreError::InvalidConfiguration {
                    message: "Expected Azure backend configuration".to_string(),
                });
            }
        }
        
        // Configure authentication
        match &config.auth {
            crate::object_store::auth::AuthConfig::Azure(creds) => {
                use crate::object_store::auth::AzureCredentials;
                match creds {
                    AzureCredentials::AccountKey { account_key, .. } => {
                        builder.account_key(account_key);
                    }
                    AzureCredentials::SasToken { sas_token, .. } => {
                        builder.sas_token(sas_token);
                    }
                    AzureCredentials::ConnectionString { connection_string: _ } => {
                        // OpenDAL doesn't support connection strings directly
                        // Would need to parse it for account and key
                        return Err(ObjectStoreError::InvalidConfiguration {
                            message: "Connection string authentication not yet implemented".to_string(),
                        });
                    }
                    AzureCredentials::DefaultChain => {
                        // Use default Azure credential chain
                    }
                    _ => {
                        return Err(ObjectStoreError::InvalidConfiguration {
                            message: "Unsupported Azure authentication method".to_string(),
                        });
                    }
                }
            }
            _ => {
                return Err(ObjectStoreError::InvalidConfiguration {
                    message: "Azure backend requires Azure authentication".to_string(),
                });
            }
        }
        
        // Build operator with retry layer
        let operator = Operator::new(builder)
            .map_err(|e| ObjectStoreError::ConnectionError {
                backend: "azure".to_string(),
                details: e.to_string(),
            })?
            .layer(RetryLayer::new())
            .finish();
        
        Ok(Self { operator, config })
    }
}

#[async_trait]
impl ObjectStore for AzureBackend {
    async fn put_with_options(
        &self,
        key: &str,
        data: Bytes,
        _options: PutOptions,
    ) -> ObjectStoreResult<()> {
        // OpenDAL v0.45 uses a simpler API for writing
        self.operator.write(key, data).await
            .map_err(|e| ObjectStoreError::WriteError {
                key: key.to_string(),
                details: e.to_string(),
            })?;
        
        Ok(())
    }

    async fn get_with_options(&self, key: &str, options: GetOptions) -> ObjectStoreResult<Bytes> {
        // OpenDAL v0.45 uses read with range directly
        let data = if let Some((start, end)) = options.range {
            self.operator.read_with(key).range(start..=end).await
        } else {
            self.operator.read(key).await
        }.map_err(|e| match e.kind() {
            opendal::ErrorKind::NotFound => ObjectStoreError::NotFound {
                key: key.to_string(),
            },
            _ => ObjectStoreError::ReadError {
                key: key.to_string(),
                details: e.to_string(),
            },
        })?;
        
        Ok(data.into())
    }

    async fn delete(&self, key: &str) -> ObjectStoreResult<()> {
        self.operator.delete(key).await
            .map_err(|e| match e.kind() {
                opendal::ErrorKind::NotFound => ObjectStoreError::NotFound {
                    key: key.to_string(),
                },
                _ => ObjectStoreError::DeleteError {
                    key: key.to_string(),
                    details: e.to_string(),
                },
            })?;
        
        Ok(())
    }

    async fn delete_batch(&self, keys: &[String]) -> ObjectStoreResult<Vec<ObjectStoreResult<()>>> {
        let futures = keys.iter().map(|key| async {
            self.delete(key).await
        });
        
        let results = futures::future::join_all(futures).await;
        Ok(results)
    }

    async fn list_with_options(
        &self,
        prefix: &str,
        options: ListOptions,
    ) -> ObjectStoreResult<Vec<ObjectMetadata>> {
        let mut lister = self.operator
            .lister(prefix)
            .await
            .map_err(|e| ObjectStoreError::ListError {
                prefix: prefix.to_string(),  
                details: e.to_string(),
            })?;
        
        let mut results = Vec::new();
        let mut count = 0;
        let max_results = options.limit.unwrap_or(usize::MAX);
        
        // Collect entries
        while let Some(entry) = lister.try_next().await
            .map_err(|e| ObjectStoreError::ListError {
                prefix: prefix.to_string(),
                details: e.to_string(),
            })? {
            if count >= max_results {
                break;
            }
            
            // Get metadata for the entry
            let metadata = entry.metadata();
            
            results.push(ObjectMetadata {
                key: entry.path().to_string(),
                size: metadata.content_length() as u64,
                last_modified: metadata.last_modified()
                    .map(|t| t.timestamp_millis() as u64)
                    .unwrap_or(0),
                etag: metadata.etag().map(|s| s.to_string()),
                content_type: metadata.content_type().map(|s| s.to_string()),
                content_encoding: None,
                cache_control: None,
                metadata: HashMap::new(),
                storage_class: None,
                encryption: None,
                version_id: None,
            });
            
            count += 1;
        }
        
        Ok(results)
    }

    async fn exists(&self, key: &str) -> ObjectStoreResult<bool> {
        let exists = self.operator.is_exist(key).await
            .map_err(|e| ObjectStoreError::AccessError {
                key: key.to_string(),
                operation: "exists".to_string(),
                details: e.to_string(),
            })?;
        
        Ok(exists)
    }

    async fn head(&self, key: &str) -> ObjectStoreResult<ObjectMetadata> {
        let stat = self.operator.stat(key).await
            .map_err(|e| match e.kind() {
                opendal::ErrorKind::NotFound => ObjectStoreError::NotFound {
                    key: key.to_string(),
                },
                _ => ObjectStoreError::AccessError {
                    key: key.to_string(),
                    operation: "head".to_string(),
                    details: e.to_string(),
                },
            })?;
        
        Ok(ObjectMetadata {
            key: key.to_string(),
            size: stat.content_length() as u64,
            last_modified: stat.last_modified()
                .map(|t| t.timestamp_millis() as u64)
                .unwrap_or(0),
            etag: stat.etag().map(|s| s.to_string()),
            content_type: stat.content_type().map(|s| s.to_string()),
            content_encoding: None,
            cache_control: None,
            metadata: HashMap::new(),
            storage_class: None,
            encryption: None,
            version_id: None,
        })
    }

    async fn copy(&self, from_key: &str, to_key: &str) -> ObjectStoreResult<()> {
        self.operator.copy(from_key, to_key).await
            .map_err(|e| ObjectStoreError::CopyError {
                from_key: from_key.to_string(),
                to_key: to_key.to_string(),
                details: e.to_string(),
            })?;
        
        Ok(())
    }

    async fn start_multipart_upload(&self, key: &str) -> ObjectStoreResult<MultipartUpload> {
        // Azure doesn't use explicit multipart uploads like S3
        // We'll return a simple upload ID for compatibility
        Ok(MultipartUpload {
            upload_id: uuid::Uuid::new_v4().to_string(),
            key: key.to_string(),
            backend_data: HashMap::new(),
        })
    }

    async fn upload_part(
        &self,
        upload: &MultipartUpload,
        part_number: u32,
        data: Bytes,
    ) -> ObjectStoreResult<MultipartUploadPart> {
        // For Azure, we'll store parts temporarily and combine them later
        let part_key = format!("{}.part{}", upload.key, part_number);
        self.put(&part_key, data.clone()).await?;
        
        Ok(MultipartUploadPart {
            part_number,
            etag: format!("part-{}", part_number),
            size: data.len() as u64,
        })
    }

    async fn complete_multipart_upload(
        &self,
        upload: &MultipartUpload,
        parts: Vec<MultipartUploadPart>,
    ) -> ObjectStoreResult<()> {
        // Combine all parts into final object
        let mut final_data = Vec::new();
        
        for part in &parts {
            let part_key = format!("{}.part{}", upload.key, part.part_number);
            let part_data = self.get(&part_key).await?;
            final_data.extend_from_slice(&part_data);
            // Clean up part file
            let _ = self.delete(&part_key).await;
        }
        
        // Write final object
        self.put(&upload.key, final_data.into()).await?;
        
        Ok(())
    }

    async fn abort_multipart_upload(&self, upload: &MultipartUpload) -> ObjectStoreResult<()> {
        // Clean up any uploaded parts
        for i in 1..=100 {
            let part_key = format!("{}.part{}", upload.key, i);
            if self.exists(&part_key).await? {
                let _ = self.delete(&part_key).await;
            } else {
                break;
            }
        }
        
        Ok(())
    }

    async fn presign_get(&self, key: &str, expires_in: Duration) -> ObjectStoreResult<String> {
        // Azure presigned URLs require SAS tokens
        // For now, return a placeholder URL
        let container = &self.config.bucket;
        let account = match &self.config.backend {
            crate::object_store::config::StorageBackend::Azure { account_name, .. } => account_name,
            _ => return Err(ObjectStoreError::InternalError {
                message: "Invalid backend configuration".to_string(),
            }),
        };
        let expires = chrono::Utc::now() + chrono::Duration::from_std(expires_in).unwrap();
        Ok(format!(
            "https://{}.blob.core.windows.net/{}/{}?se={}",
            account,
            container,
            key,
            expires.timestamp()
        ))
    }

    async fn presign_put(&self, key: &str, expires_in: Duration) -> ObjectStoreResult<String> {
        // Azure presigned URLs require SAS tokens
        // For now, return a placeholder URL
        let container = &self.config.bucket;
        let account = match &self.config.backend {
            crate::object_store::config::StorageBackend::Azure { account_name, .. } => account_name,
            _ => return Err(ObjectStoreError::InternalError {
                message: "Invalid backend configuration".to_string(),
            }),
        };
        let expires = chrono::Utc::now() + chrono::Duration::from_std(expires_in).unwrap();
        Ok(format!(
            "https://{}.blob.core.windows.net/{}/{}?sp=w&se={}",
            account,
            container,
            key,
            expires.timestamp()
        ))
    }

    async fn stream_to_writer(
        &self,
        key: &str,
        mut writer: Pin<Box<dyn AsyncWrite + Send>>,
    ) -> ObjectStoreResult<u64> {
        use tokio::io::AsyncWriteExt;
        
        let data = self.get(key).await?;
        let len = data.len() as u64;
        
        writer.write_all(&data).await
            .map_err(|e| ObjectStoreError::StreamError {
                key: key.to_string(),
                operation: "stream_to_writer".to_string(),
                details: e.to_string(),
            })?;
        
        writer.flush().await
            .map_err(|e| ObjectStoreError::StreamError {
                key: key.to_string(),
                operation: "stream_to_writer".to_string(),
                details: e.to_string(),
            })?;
        
        Ok(len)
    }

    async fn stream_from_reader(
        &self,
        key: &str,
        mut reader: Pin<Box<dyn AsyncRead + Send>>,
        _content_length: Option<u64>,
    ) -> ObjectStoreResult<()> {
        use tokio::io::AsyncReadExt;
        
        let mut buffer = Vec::new();
        reader.read_to_end(&mut buffer).await
            .map_err(|e| ObjectStoreError::StreamError {
                key: key.to_string(),
                operation: "stream_from_reader".to_string(),
                details: e.to_string(),
            })?;
        
        self.put(key, buffer.into()).await?;
        
        Ok(())
    }
}