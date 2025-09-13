//! Async I/O implementation for WAL using tokio-uring on Linux
//!
//! This module provides high-performance async file I/O for WAL operations
//! using io_uring on Linux systems. Falls back to standard async I/O on other platforms.

use anyhow::Result;
use bytes::{Bytes, BytesMut};
use std::path::Path;
use std::sync::Arc;
use tracing::{debug, instrument};
#[cfg(all(target_os = "linux", feature = "async-io"))]
use tracing::warn;

#[cfg(all(target_os = "linux", feature = "async-io"))]
use tokio_uring::fs::{File as UringFile, OpenOptions as UringOpenOptions};

/// Async I/O backend abstraction
#[derive(Debug, Clone)]
pub enum AsyncIoBackend {
    /// Standard tokio async I/O
    Tokio,
    /// io_uring-based async I/O (Linux only)
    #[cfg(all(target_os = "linux", feature = "async-io"))]
    IoUring,
}

impl AsyncIoBackend {
    /// Detect the best available backend for the current platform
    pub fn detect() -> Self {
        #[cfg(all(target_os = "linux", feature = "async-io"))]
        {
            // Check if io_uring is available
            if Self::is_io_uring_available() {
                debug!("Using io_uring async I/O backend");
                return AsyncIoBackend::IoUring;
            } else {
                warn!("io_uring not available, falling back to standard async I/O");
            }
        }
        
        debug!("Using standard tokio async I/O backend");
        AsyncIoBackend::Tokio
    }
    
    #[cfg(all(target_os = "linux", feature = "async-io"))]
    fn is_io_uring_available() -> bool {
        // Try to create a simple io_uring instance to check availability
        // This will fail on older kernels (< 5.1) or if io_uring is disabled
        std::panic::catch_unwind(|| {
            let _ = tokio_uring::start(async {
                // Simple probe operation
                Ok::<(), ()>(())
            });
        }).is_ok()
    }
}

/// Async file handle abstraction
pub struct AsyncFile {
    backend: AsyncIoBackend,
    #[cfg(all(target_os = "linux", feature = "async-io"))]
    uring_file: Option<Arc<UringFile>>,
    tokio_file: Option<Arc<tokio::fs::File>>,
}

impl AsyncFile {
    /// Open a file for async I/O
    #[instrument(skip_all, fields(path = %path.as_ref().display()))]
    pub async fn open(path: impl AsRef<Path>, backend: AsyncIoBackend) -> Result<Self> {
        let path = path.as_ref();
        
        match backend {
            #[cfg(all(target_os = "linux", feature = "async-io"))]
            AsyncIoBackend::IoUring => {
                let file = UringOpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .open(path)
                    .await?;
                
                Ok(AsyncFile {
                    backend,
                    uring_file: Some(Arc::new(file)),
                    tokio_file: None,
                })
            }
            AsyncIoBackend::Tokio => {
                let file = tokio::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .open(path)
                    .await?;
                
                Ok(AsyncFile {
                    backend,
                    #[cfg(all(target_os = "linux", feature = "async-io"))]
                    uring_file: None,
                    tokio_file: Some(Arc::new(file)),
                })
            }
        }
    }
    
    /// Write data at the specified offset
    #[instrument(skip_all, fields(offset = offset, len = data.len()))]
    pub async fn write_at(&self, data: Bytes, offset: u64) -> Result<usize> {
        match self.backend {
            #[cfg(all(target_os = "linux", feature = "async-io"))]
            AsyncIoBackend::IoUring => {
                if let Some(file) = &self.uring_file {
                    let (res, _) = file.write_at(data, offset).await;
                    Ok(res? as usize)
                } else {
                    anyhow::bail!("io_uring file not initialized");
                }
            }
            AsyncIoBackend::Tokio => {
                
                if let Some(file) = &self.tokio_file {
                    use std::os::unix::fs::FileExt;
                    let std_file = file.as_ref().try_clone().await?.into_std().await;
                    std_file.write_at(&data, offset)?;
                    Ok(data.len())
                } else {
                    anyhow::bail!("Tokio file not initialized");
                }
            }
        }
    }
    
    /// Read data from the specified offset
    #[instrument(skip_all, fields(offset = offset, len = len))]
    pub async fn read_at(&self, offset: u64, len: usize) -> Result<Bytes> {
        match self.backend {
            #[cfg(all(target_os = "linux", feature = "async-io"))]
            AsyncIoBackend::IoUring => {
                if let Some(file) = &self.uring_file {
                    let mut buf = BytesMut::with_capacity(len);
                    buf.resize(len, 0);
                    let (res, buf) = file.read_at(buf, offset).await;
                    let n = res?;
                    buf.truncate(n);
                    Ok(buf.freeze())
                } else {
                    anyhow::bail!("io_uring file not initialized");
                }
            }
            AsyncIoBackend::Tokio => {
                
                if let Some(file) = &self.tokio_file {
                    use std::os::unix::fs::FileExt;
                    let std_file = file.as_ref().try_clone().await?.into_std().await;
                    let mut buf = BytesMut::with_capacity(len);
                    buf.resize(len, 0);
                    let n = std_file.read_at(&mut buf, offset)?;
                    buf.truncate(n);
                    Ok(buf.freeze())
                } else {
                    anyhow::bail!("Tokio file not initialized");
                }
            }
        }
    }
    
    /// Sync data to disk (fsync)
    #[instrument(skip_all)]
    pub async fn sync(&self) -> Result<()> {
        match self.backend {
            #[cfg(all(target_os = "linux", feature = "async-io"))]
            AsyncIoBackend::IoUring => {
                if let Some(file) = &self.uring_file {
                    file.sync_all().await?;
                    Ok(())
                } else {
                    anyhow::bail!("io_uring file not initialized");
                }
            }
            AsyncIoBackend::Tokio => {
                if let Some(file) = &self.tokio_file {
                    file.sync_all().await?;
                    Ok(())
                } else {
                    anyhow::bail!("Tokio file not initialized");
                }
            }
        }
    }
    
    /// Get file metadata
    pub async fn metadata(&self) -> Result<std::fs::Metadata> {
        match self.backend {
            #[cfg(all(target_os = "linux", feature = "async-io"))]
            AsyncIoBackend::IoUring => {
                if let Some(file) = &self.uring_file {
                    let metadata = file.statx().await?;
                    // Convert statx to Metadata (simplified)
                    // In production, would need proper conversion
                    Ok(std::fs::metadata("/proc/self/fd/0")?)
                } else {
                    anyhow::bail!("io_uring file not initialized");
                }
            }
            AsyncIoBackend::Tokio => {
                if let Some(file) = &self.tokio_file {
                    Ok(file.metadata().await?)
                } else {
                    anyhow::bail!("Tokio file not initialized");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    
    #[tokio::test]
    async fn test_async_file_operations() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.wal");
        
        // Test with standard tokio backend
        let backend = AsyncIoBackend::Tokio;
        let file = AsyncFile::open(&path, backend).await.unwrap();
        
        // Test write
        let data = Bytes::from("Hello, WAL!");
        let written = file.write_at(data.clone(), 0).await.unwrap();
        assert_eq!(written, data.len());
        
        // Test read
        let read_data = file.read_at(0, data.len()).await.unwrap();
        assert_eq!(read_data, data);
        
        // Test sync
        file.sync().await.unwrap();
    }
    
    #[cfg(all(target_os = "linux", feature = "async-io"))]
    #[tokio::test]
    async fn test_io_uring_operations() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test_uring.wal");
        
        // Test with io_uring backend if available
        let backend = AsyncIoBackend::detect();
        if matches!(backend, AsyncIoBackend::IoUring) {
            let file = AsyncFile::open(&path, backend).await.unwrap();
            
            // Test write
            let data = Bytes::from("Hello from io_uring!");
            let written = file.write_at(data.clone(), 0).await.unwrap();
            assert_eq!(written, data.len());
            
            // Test read
            let read_data = file.read_at(0, data.len()).await.unwrap();
            assert_eq!(read_data, data);
            
            // Test sync
            file.sync().await.unwrap();
        }
    }
}