//! Local filesystem backend implementation.

use async_trait::async_trait;
use bytes::Bytes;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;
use tokio::fs;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tracing::{debug, instrument};

use crate::object_store::{
    config::{ObjectStoreConfig, StorageBackend},
    errors::{ObjectStoreError, ObjectStoreResult},
    storage::{
        GetOptions, ListOptions, MultipartUpload, MultipartUploadPart, ObjectMetadata,
        ObjectStore, PutOptions,
    },
};

/// Local filesystem backend implementation
pub struct LocalBackend {
    root_path: PathBuf,
    prefix: Option<String>,
}

impl LocalBackend {
    /// Create a new local backend
    pub async fn new(config: ObjectStoreConfig) -> ObjectStoreResult<Self> {
        let root_path = match &config.backend {
            StorageBackend::Local { path } => PathBuf::from(path),
            _ => {
                return Err(ObjectStoreError::InvalidConfiguration {
                    message: "Expected Local backend configuration".to_string(),
                })
            }
        };

        // Ensure root directory exists
        if !root_path.exists() {
            fs::create_dir_all(&root_path)
                .await
                .map_err(|e| ObjectStoreError::InvalidConfiguration {
                    message: format!("Failed to create root directory: {}", e),
                })?;
        }

        debug!("Created local backend at path: {:?}", root_path);

        Ok(Self {
            root_path,
            prefix: config.prefix,
        })
    }

    /// Build the full path including prefix
    fn build_path(&self, key: &str) -> PathBuf {
        let mut path = self.root_path.clone();
        
        if let Some(prefix) = &self.prefix {
            path = path.join(prefix);
        }
        
        // Split key by '/' and build path
        for component in key.split('/') {
            if !component.is_empty() {
                path = path.join(component);
            }
        }
        
        path
    }

    /// Convert filesystem path back to key
    fn path_to_key(&self, path: &Path) -> ObjectStoreResult<String> {
        let relative_path = path.strip_prefix(&self.root_path)
            .map_err(|_| ObjectStoreError::Other("Invalid path".to_string()))?;

        let key = if let Some(prefix) = &self.prefix {
            relative_path
                .strip_prefix(prefix)
                .map_err(|_| ObjectStoreError::Other("Invalid prefixed path".to_string()))?
                .to_string_lossy()
                .to_string()
        } else {
            relative_path.to_string_lossy().to_string()
        };

        // Convert backslashes to forward slashes for consistency
        Ok(key.replace('\\', "/"))
    }

    /// Ensure parent directory exists
    async fn ensure_parent_dir(&self, path: &Path) -> ObjectStoreResult<()> {
        if let Some(parent) = path.parent() {
            if !parent.exists() {
                fs::create_dir_all(parent)
                    .await
                    .map_err(|e| ObjectStoreError::Other(format!("Failed to create directory: {}", e)))?;
            }
        }
        Ok(())
    }
    
    /// Recursively list files
    async fn list_recursive(
        &self,
        dir_path: &Path,
        prefix: &str,
        options: &ListOptions,
        objects: &mut Vec<ObjectMetadata>,
    ) -> ObjectStoreResult<()> {
        let mut entries = fs::read_dir(dir_path)
            .await
            .map_err(|e| ObjectStoreError::Other(format!("Failed to read directory {:?}: {}", dir_path, e)))?;

        while let Some(entry) = entries.next_entry()
            .await
            .map_err(|e| ObjectStoreError::Other(format!("Failed to read entry: {}", e)))?
        {
            let path = entry.path();
            
            if path.is_file() {
                if let Ok(key) = self.path_to_key(&path) {
                    if key.starts_with(prefix) {
                        let metadata = entry.metadata()
                            .await
                            .map_err(|e| ObjectStoreError::Other(format!("Failed to get metadata: {}", e)))?;

                        let last_modified = metadata.modified()
                            .map(|t| chrono::DateTime::<chrono::Utc>::from(t).timestamp_millis() as u64)
                            .unwrap_or_else(|_| chrono::Utc::now().timestamp_millis() as u64);

                        objects.push(ObjectMetadata {
                            key,
                            size: metadata.len(),
                            last_modified,
                            etag: None,
                            content_type: None,
                            content_encoding: None,
                            cache_control: None,
                            metadata: HashMap::new(),
                            storage_class: None,
                            encryption: None,
                            version_id: None,
                        });

                        if let Some(limit) = options.limit {
                            if objects.len() >= limit {
                                return Ok(());
                            }
                        }
                    }
                }
            } else if path.is_dir() {
                // Recursively search subdirectories
                Box::pin(self.list_recursive(&path, prefix, options, objects)).await?;
                
                if let Some(limit) = options.limit {
                    if objects.len() >= limit {
                        return Ok(());
                    }
                }
            }
        }

        Ok(())
    }
}

#[async_trait]
impl ObjectStore for LocalBackend {
    #[instrument(skip(self, data))]
    async fn put_with_options(
        &self,
        key: &str,
        data: Bytes,
        _options: PutOptions,
    ) -> ObjectStoreResult<()> {
        let path = self.build_path(key);
        
        self.ensure_parent_dir(&path).await?;
        
        fs::write(&path, &data)
            .await
            .map_err(|e| ObjectStoreError::Other(format!("Failed to write file: {}", e)))?;

        debug!("Successfully wrote {} bytes to: {:?}", data.len(), path);
        Ok(())
    }

    #[instrument(skip(self))]
    async fn get_with_options(&self, key: &str, options: GetOptions) -> ObjectStoreResult<Bytes> {
        let path = self.build_path(key);

        if !path.exists() {
            return Err(ObjectStoreError::NotFound {
                key: key.to_string(),
            });
        }

        let data = if let Some((start, end)) = options.range {
            // Read specific range
            let file = fs::File::open(&path)
                .await
                .map_err(|e| ObjectStoreError::Other(format!("Failed to open file: {}", e)))?;

            let mut reader = tokio::io::BufReader::new(file);
            
            // Seek to start position
            use tokio::io::{AsyncSeekExt, SeekFrom};
            reader.seek(SeekFrom::Start(start))
                .await
                .map_err(|e| ObjectStoreError::Other(format!("Failed to seek: {}", e)))?;

            // Read the range
            let length = (end - start + 1) as usize;
            let mut buffer = vec![0u8; length];
            let bytes_read = reader.read(&mut buffer)
                .await
                .map_err(|e| ObjectStoreError::Other(format!("Failed to read range: {}", e)))?;
            
            // Truncate buffer to actual bytes read
            buffer.truncate(bytes_read);

            Bytes::from(buffer)
        } else {
            // Read entire file
            let data = fs::read(&path)
                .await
                .map_err(|e| ObjectStoreError::Other(format!("Failed to read file: {}", e)))?;
            Bytes::from(data)
        };

        debug!("Successfully read {} bytes from: {:?}", data.len(), path);
        Ok(data)
    }

    #[instrument(skip(self))]
    async fn delete(&self, key: &str) -> ObjectStoreResult<()> {
        let path = self.build_path(key);

        if !path.exists() {
            return Err(ObjectStoreError::NotFound {
                key: key.to_string(),
            });
        }

        fs::remove_file(&path)
            .await
            .map_err(|e| ObjectStoreError::Other(format!("Failed to delete file: {}", e)))?;

        debug!("Successfully deleted: {:?}", path);
        Ok(())
    }

    #[instrument(skip(self))]
    async fn delete_batch(&self, keys: &[String]) -> ObjectStoreResult<Vec<ObjectStoreResult<()>>> {
        let mut results = Vec::with_capacity(keys.len());

        for key in keys {
            let result = self.delete(key).await;
            results.push(result);
        }

        Ok(results)
    }

    #[instrument(skip(self))]
    async fn list_with_options(
        &self,
        prefix: &str,
        options: ListOptions,
    ) -> ObjectStoreResult<Vec<ObjectMetadata>> {
        let mut objects = Vec::new();
        
        // Start from root path and search recursively
        self.list_recursive(&self.root_path, prefix, &options, &mut objects).await?;

        debug!("Listed {} objects with prefix: {}", objects.len(), prefix);
        Ok(objects)
    }

    #[instrument(skip(self))]
    async fn exists(&self, key: &str) -> ObjectStoreResult<bool> {
        let path = self.build_path(key);
        Ok(path.exists() && path.is_file())
    }

    #[instrument(skip(self))]
    async fn head(&self, key: &str) -> ObjectStoreResult<ObjectMetadata> {
        let path = self.build_path(key);

        if !path.exists() {
            return Err(ObjectStoreError::NotFound {
                key: key.to_string(),
            });
        }

        let metadata = fs::metadata(&path)
            .await
            .map_err(|e| ObjectStoreError::Other(format!("Failed to get metadata: {}", e)))?;

        let last_modified = metadata.modified()
            .map(|t| chrono::DateTime::<chrono::Utc>::from(t).timestamp_millis() as u64)
            .unwrap_or_else(|_| chrono::Utc::now().timestamp_millis() as u64);

        Ok(ObjectMetadata {
            key: key.to_string(),
            size: metadata.len(),
            last_modified,
            etag: None,
            content_type: None,
            content_encoding: None,
            cache_control: None,
            metadata: HashMap::new(),
            storage_class: None,
            encryption: None,
            version_id: None,
        })
    }

    #[instrument(skip(self))]
    async fn copy(&self, from_key: &str, to_key: &str) -> ObjectStoreResult<()> {
        let from_path = self.build_path(from_key);
        let to_path = self.build_path(to_key);

        if !from_path.exists() {
            return Err(ObjectStoreError::NotFound {
                key: from_key.to_string(),
            });
        }

        self.ensure_parent_dir(&to_path).await?;

        fs::copy(&from_path, &to_path)
            .await
            .map_err(|e| ObjectStoreError::Other(format!("Failed to copy file: {}", e)))?;

        debug!("Successfully copied: {:?} -> {:?}", from_path, to_path);
        Ok(())
    }

    #[instrument(skip(self))]
    async fn start_multipart_upload(&self, key: &str) -> ObjectStoreResult<MultipartUpload> {
        // For local storage, we'll simulate multipart uploads by creating a temporary directory
        let temp_dir = std::env::temp_dir().join(format!("multipart_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&temp_dir)
            .await
            .map_err(|e| ObjectStoreError::MultipartUploadError {
                message: format!("Failed to create temp directory: {}", e),
            })?;

        let upload_id = temp_dir.to_string_lossy().to_string();

        debug!("Started multipart upload for key: {} ({})", key, upload_id);

        Ok(MultipartUpload {
            upload_id,
            key: key.to_string(),
            backend_data: HashMap::new(),
        })
    }

    #[instrument(skip(self, data))]
    async fn upload_part(
        &self,
        upload: &MultipartUpload,
        part_number: u32,
        data: Bytes,
    ) -> ObjectStoreResult<MultipartUploadPart> {
        let temp_dir = PathBuf::from(&upload.upload_id);
        let part_path = temp_dir.join(format!("part_{:05}", part_number));

        fs::write(&part_path, &data)
            .await
            .map_err(|e| ObjectStoreError::MultipartUploadError {
                message: format!("Failed to write part: {}", e),
            })?;

        // Generate a simple etag (not cryptographically secure, just for tracking)
        let etag = format!("{:x}", data.len());

        debug!("Uploaded part {} for upload {}: {} bytes", part_number, upload.upload_id, data.len());

        Ok(MultipartUploadPart {
            part_number,
            etag,
            size: data.len() as u64,
        })
    }

    #[instrument(skip(self))]
    async fn complete_multipart_upload(
        &self,
        upload: &MultipartUpload,
        parts: Vec<MultipartUploadPart>,
    ) -> ObjectStoreResult<()> {
        let temp_dir = PathBuf::from(&upload.upload_id);
        let final_path = self.build_path(&upload.key);

        self.ensure_parent_dir(&final_path).await?;

        // Combine all parts in order
        let mut final_data = Vec::new();
        for part in parts {
            let part_path = temp_dir.join(format!("part_{:05}", part.part_number));
            let part_data = fs::read(&part_path)
                .await
                .map_err(|e| ObjectStoreError::MultipartUploadError {
                    message: format!("Failed to read part {}: {}", part.part_number, e),
                })?;
            final_data.extend(part_data);
        }

        // Write combined data
        fs::write(&final_path, final_data)
            .await
            .map_err(|e| ObjectStoreError::MultipartUploadError {
                message: format!("Failed to write final file: {}", e),
            })?;

        // Clean up temp directory
        let _ = fs::remove_dir_all(&temp_dir).await;

        debug!("Completed multipart upload for key: {} ({})", upload.key, upload.upload_id);
        Ok(())
    }

    #[instrument(skip(self))]
    async fn abort_multipart_upload(&self, upload: &MultipartUpload) -> ObjectStoreResult<()> {
        let temp_dir = PathBuf::from(&upload.upload_id);
        
        // Clean up temp directory
        let _ = fs::remove_dir_all(&temp_dir).await;

        debug!("Aborted multipart upload for key: {} ({})", upload.key, upload.upload_id);
        Ok(())
    }

    #[instrument(skip(self))]
    async fn presign_get(&self, _key: &str, _expires_in: Duration) -> ObjectStoreResult<String> {
        Err(ObjectStoreError::UnsupportedOperation {
            operation: "presign_get".to_string(),
            backend: "local".to_string(),
        })
    }

    #[instrument(skip(self))]
    async fn presign_put(&self, _key: &str, _expires_in: Duration) -> ObjectStoreResult<String> {
        Err(ObjectStoreError::UnsupportedOperation {
            operation: "presign_put".to_string(),
            backend: "local".to_string(),
        })
    }

    #[instrument(skip(self, writer))]
    async fn stream_to_writer(
        &self,
        key: &str,
        mut writer: Pin<Box<dyn AsyncWrite + Send>>,
    ) -> ObjectStoreResult<u64> {
        let path = self.build_path(key);

        if !path.exists() {
            return Err(ObjectStoreError::NotFound {
                key: key.to_string(),
            });
        }

        let mut file = fs::File::open(&path)
            .await
            .map_err(|e| ObjectStoreError::Other(format!("Failed to open file: {}", e)))?;

        let bytes_copied = tokio::io::copy(&mut file, &mut writer)
            .await
            .map_err(|e| ObjectStoreError::Other(format!("Failed to copy data: {}", e)))?;

        writer.flush().await.map_err(|e| ObjectStoreError::Io(e))?;

        debug!("Streamed {} bytes from: {:?}", bytes_copied, path);
        Ok(bytes_copied)
    }

    #[instrument(skip(self, reader))]
    async fn stream_from_reader(
        &self,
        key: &str,
        mut reader: Pin<Box<dyn AsyncRead + Send>>,
        _content_length: Option<u64>,
    ) -> ObjectStoreResult<()> {
        let path = self.build_path(key);
        
        self.ensure_parent_dir(&path).await?;

        let mut file = fs::File::create(&path)
            .await
            .map_err(|e| ObjectStoreError::Other(format!("Failed to create file: {}", e)))?;

        let bytes_copied = tokio::io::copy(&mut reader, &mut file)
            .await
            .map_err(|e| ObjectStoreError::Other(format!("Failed to copy data: {}", e)))?;

        file.flush().await.map_err(|e| ObjectStoreError::Io(e))?;

        debug!("Streamed {} bytes to: {:?}", bytes_copied, path);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_local_backend_basic_operations() {
        let temp_dir = TempDir::new().unwrap();
        
        let config = ObjectStoreConfig {
            backend: StorageBackend::Local {
                path: temp_dir.path().to_string_lossy().to_string(),
            },
            bucket: "test".to_string(),
            prefix: None,
            ..ObjectStoreConfig::default()
        };

        let backend = LocalBackend::new(config).await.unwrap();

        // Test put and get
        let key = "test/file.txt";
        let data = Bytes::from("Hello, World!");
        
        backend.put_with_options(key, data.clone(), PutOptions::default()).await.unwrap();
        let retrieved = backend.get_with_options(key, GetOptions::default()).await.unwrap();
        assert_eq!(retrieved, data);

        // Test exists
        assert!(backend.exists(key).await.unwrap());
        assert!(!backend.exists("nonexistent").await.unwrap());

        // Test head
        let metadata = backend.head(key).await.unwrap();
        assert_eq!(metadata.size, data.len() as u64);

        // Test delete
        backend.delete(key).await.unwrap();
        assert!(!backend.exists(key).await.unwrap());
    }

    #[tokio::test]
    async fn test_local_backend_with_prefix() {
        let temp_dir = TempDir::new().unwrap();
        
        let config = ObjectStoreConfig {
            backend: StorageBackend::Local {
                path: temp_dir.path().to_string_lossy().to_string(),
            },
            bucket: "test".to_string(),
            prefix: Some("prefix".to_string()),
            ..ObjectStoreConfig::default()
        };

        let backend = LocalBackend::new(config).await.unwrap();

        let key = "file.txt";
        let data = Bytes::from("Test data");
        
        backend.put_with_options(key, data.clone(), PutOptions::default()).await.unwrap();
        let retrieved = backend.get_with_options(key, GetOptions::default()).await.unwrap();
        assert_eq!(retrieved, data);

        // Check that the file was actually created in the prefixed path
        let expected_path = temp_dir.path().join("prefix").join("file.txt");
        assert!(expected_path.exists());
    }
}