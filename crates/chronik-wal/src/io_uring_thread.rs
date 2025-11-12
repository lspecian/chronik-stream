//! Dedicated io_uring thread for WAL writes (hybrid tokio + tokio-uring architecture)
//!
//! This module runs a separate OS thread with tokio-uring runtime for zero-copy WAL I/O,
//! while the main application uses regular tokio. Communication happens via crossbeam channels.
//!
//! Architecture:
//! - Main thread: Regular tokio runtime (multi-threaded)
//! - WAL thread: tokio-uring runtime (single-threaded, kernel-level async I/O)
//! - Communication: Crossbeam MPSC channels (thread-safe, wait-free)

#[cfg(all(target_os = "linux", feature = "async-io"))]
use std::path::PathBuf;
#[cfg(all(target_os = "linux", feature = "async-io"))]
use std::thread;
#[cfg(all(target_os = "linux", feature = "async-io"))]
use crossbeam::channel::{unbounded, Sender, Receiver};
#[cfg(all(target_os = "linux", feature = "async-io"))]
use bytes::Bytes;
#[cfg(all(target_os = "linux", feature = "async-io"))]
use crate::Result;
#[cfg(all(target_os = "linux", feature = "async-io"))]
use tracing::{info, warn, error};

#[cfg(all(target_os = "linux", feature = "async-io"))]
/// Command sent from main tokio thread to io_uring thread
enum IoUringCommand {
    Write {
        partition_key: String,
        data: Bytes,
        response: tokio::sync::oneshot::Sender<Result<()>>,
    },
    Sync {
        partition_key: String,
        response: tokio::sync::oneshot::Sender<Result<()>>,
    },
    CreateFile {
        partition_key: String,
        path: PathBuf,
        response: tokio::sync::oneshot::Sender<Result<()>>,
    },
    Shutdown,
}

#[cfg(all(target_os = "linux", feature = "async-io"))]
/// Handle to communicate with the io_uring thread from regular tokio runtime
#[derive(Clone)]
pub struct IoUringThreadHandle {
    cmd_tx: Sender<IoUringCommand>,
}

#[cfg(all(target_os = "linux", feature = "async-io"))]
impl std::fmt::Debug for IoUringThreadHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IoUringThreadHandle").finish()
    }
}

#[cfg(all(target_os = "linux", feature = "async-io"))]
impl IoUringThreadHandle {
    /// Spawn dedicated io_uring thread
    pub fn spawn() -> Result<Self> {
        let (cmd_tx, cmd_rx) = unbounded();

        thread::Builder::new()
            .name("wal-io_uring".to_string())
            .spawn(move || {
                info!("✨ Starting dedicated io_uring thread for WAL writes");

                // Run tokio-uring runtime in this thread
                tokio_uring::start(async move {
                    if let Err(e) = run_io_uring_loop(cmd_rx).await {
                        error!("io_uring thread error: {}", e);
                    }
                });

                info!("io_uring thread stopped");
            })?;

        Ok(Self { cmd_tx })
    }

    /// Write data to partition's WAL file
    pub async fn write(&self, partition_key: String, data: Bytes) -> Result<()> {
        let (tx, rx) = tokio::sync::oneshot::channel();

        self.cmd_tx.send(IoUringCommand::Write {
            partition_key,
            data,
            response: tx,
        }).map_err(|_| crate::WalError::IoError("io_uring thread died".into()))?;

        rx.await.map_err(|_| crate::WalError::IoError("io_uring response channel closed".into()))?
    }

    /// Fsync partition's WAL file
    pub async fn sync(&self, partition_key: String) -> Result<()> {
        let (tx, rx) = tokio::sync::oneshot::channel();

        self.cmd_tx.send(IoUringCommand::Sync {
            partition_key,
            response: tx,
        }).map_err(|_| crate::WalError::IoError("io_uring thread died".into()))?;

        rx.await.map_err(|_| crate::WalError::IoError("io_uring response channel closed".into()))?
    }

    /// Create new WAL file for partition
    pub async fn create_file(&self, partition_key: String, path: PathBuf) -> Result<()> {
        let (tx, rx) = tokio::sync::oneshot::channel();

        self.cmd_tx.send(IoUringCommand::CreateFile {
            partition_key,
            path,
            response: tx,
        }).map_err(|_| crate::WalError::IoError("io_uring thread died".into()))?;

        rx.await.map_err(|_| crate::WalError::IoError("io_uring response channel closed".into()))?
    }

    /// Shutdown io_uring thread gracefully
    pub fn shutdown(&self) {
        let _ = self.cmd_tx.send(IoUringCommand::Shutdown);
    }
}

#[cfg(all(target_os = "linux", feature = "async-io"))]
/// Main event loop running in io_uring thread
async fn run_io_uring_loop(cmd_rx: Receiver<IoUringCommand>) -> Result<()> {
    use tokio_uring::fs::File;
    use std::collections::HashMap;

    let mut files: HashMap<String, File> = HashMap::new();

    // Set WAL I/O priority in this thread
    if let Err(e) = crate::io_priority::set_wal_priority() {
        warn!("Failed to set WAL I/O priority: {}", e);
    }

    loop {
        // Receive command from main tokio thread (blocking)
        let cmd = match cmd_rx.recv() {
            Ok(cmd) => cmd,
            Err(_) => {
                info!("Command channel closed, shutting down io_uring thread");
                break;
            }
        };

        match cmd {
            IoUringCommand::Write { partition_key, data, response } => {
                let result = match files.get_mut(&partition_key) {
                    Some(file) => {
                        // Convert Bytes to Vec<u8> for io_uring's IoBuf
                        let buf = data.to_vec();
                        let mut offset = 0u64;
                        let mut remaining = buf;

                        loop {
                            let (res, buf_back) = file.write_at(remaining, offset).await;
                            match res {
                                Ok(n) if n > 0 => {
                                    offset += n as u64;
                                    if n == buf_back.len() {
                                        break Ok(());
                                    }
                                    remaining = buf_back[n..].to_vec();
                                }
                                Ok(_) => break Err(crate::WalError::Io(std::io::Error::new(
                                    std::io::ErrorKind::WriteZero,
                                    "failed to write whole buffer"
                                ))),
                                Err(e) => break Err(crate::WalError::Io(e)),
                            }
                        }
                    }
                    None => {
                        Err(crate::WalError::IoError(format!("File not found for partition: {}", partition_key)))
                    }
                };
                let _ = response.send(result);
            }

            IoUringCommand::Sync { partition_key, response } => {
                let result = match files.get(&partition_key) {
                    Some(file) => {
                        file.sync_all().await.map_err(|e| crate::WalError::Io(e))
                    }
                    None => {
                        Err(crate::WalError::IoError(format!("File not found for partition: {}", partition_key)))
                    }
                };
                let _ = response.send(result);
            }

            IoUringCommand::CreateFile { partition_key, path, response } => {
                let result = match File::create(&path).await {
                    Ok(file) => {
                        info!("✨ io_uring: Created WAL file for {}: {:?}", partition_key, path);
                        files.insert(partition_key, file);
                        Ok(())
                    }
                    Err(e) => Err(crate::WalError::Io(e))
                };
                let _ = response.send(result);
            }

            IoUringCommand::Shutdown => {
                info!("Received shutdown command");
                break;
            }
        }
    }

    info!("io_uring event loop finished");
    Ok(())
}

// Stub implementation for non-Linux or when async-io feature is disabled
#[cfg(not(all(target_os = "linux", feature = "async-io")))]
pub struct IoUringThreadHandle;

#[cfg(not(all(target_os = "linux", feature = "async-io")))]
impl IoUringThreadHandle {
    pub fn spawn() -> crate::Result<Self> {
        Err(crate::WalError::Unsupported(
            "io_uring requires Linux and async-io feature".into()
        ))
    }
}
