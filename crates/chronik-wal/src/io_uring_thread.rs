//! Dedicated io_uring thread for WAL writes (hybrid tokio + tokio-uring architecture)
//!
//! This module runs a separate OS thread with tokio-uring runtime for zero-copy WAL I/O,
//! while the main application uses regular tokio. Communication happens via crossbeam channels.
//!
//! Architecture:
//! - Main thread: Regular tokio runtime (multi-threaded)
//! - WAL thread: tokio-uring runtime (single-threaded, kernel-level async I/O)
//! - Communication: async tokio MPSC channel
//!
//! The channel is deliberately `tokio::sync::mpsc` and not `crossbeam`. This loop
//! runs *inside* `tokio_uring::start`, so a blocking receive parks the only thread
//! that can reap io_uring completions — and crossbeam's blocking receive spins on
//! `sched_yield` first, which measured 200,507 yields per 12s against 2,014 on the
//! standard-I/O path. An async receive lets the runtime drive completions while it
//! waits.

#[cfg(all(target_os = "linux", feature = "async-io"))]
use std::path::PathBuf;
#[cfg(all(target_os = "linux", feature = "async-io"))]
use std::thread;
#[cfg(all(target_os = "linux", feature = "async-io"))]
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender as Sender, UnboundedReceiver as Receiver};
#[cfg(all(target_os = "linux", feature = "async-io"))]
use bytes::Bytes;
#[cfg(all(target_os = "linux", feature = "async-io"))]
use crate::Result;
#[cfg(all(target_os = "linux", feature = "async-io"))]
use tracing::{info, warn, error};

/// `Bytes` as an io_uring buffer, so a WAL write does not have to copy.
///
/// The previous path did `data.to_vec()` on every write because `IoBuf` needs an
/// owned buffer — a memcpy of the whole batch on the hot path, discarding the
/// refcounting `Bytes` exists for.
///
/// # Safety
///
/// `IoBuf` requires that the pointer stays valid while the runtime owns the value,
/// *even if the value is moved*. `Bytes` is a refcounted handle to a heap
/// allocation: moving the handle does not move the bytes, and holding it keeps the
/// allocation alive for as long as the operation runs.
#[cfg(all(target_os = "linux", feature = "async-io"))]
struct BytesBuf(Bytes);

#[cfg(all(target_os = "linux", feature = "async-io"))]
unsafe impl tokio_uring::buf::IoBuf for BytesBuf {
    fn stable_ptr(&self) -> *const u8 {
        self.0.as_ptr()
    }
    fn bytes_init(&self) -> usize {
        self.0.len()
    }
    fn bytes_total(&self) -> usize {
        self.0.len()
    }
}

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
        let (cmd_tx, cmd_rx) = unbounded_channel();

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
async fn run_io_uring_loop(mut cmd_rx: Receiver<IoUringCommand>) -> Result<()> {
    use tokio_uring::fs::File;
    use std::collections::HashMap;

    let mut files: HashMap<String, File> = HashMap::new();
    // Track current file offset for append-mode WAL writes
    let mut file_offsets: HashMap<String, u64> = HashMap::new();

    // Set WAL I/O priority in this thread
    if let Err(e) = crate::io_priority::set_wal_priority() {
        warn!("Failed to set WAL I/O priority: {}", e);
    }

    loop {
        // BATCHED PARALLEL FSYNC: Drain all pending commands and process Sync operations in parallel

        // Step 1: Await the first command. This yields to the runtime instead of
        // parking the thread, so io_uring completions are reaped while idle.
        let first_cmd = match cmd_rx.recv().await {
            Some(cmd) => cmd,
            None => {
                info!("Command channel closed, shutting down io_uring thread");
                break;
            }
        };

        // Step 2: Drain everything else already queued (non-blocking), so one
        // pass through the loop serves a whole burst.
        let mut all_cmds = vec![first_cmd];
        while let Ok(cmd) = cmd_rx.try_recv() {
            all_cmds.push(cmd);
        }

        // Step 3: Separate Sync commands from others
        let mut sync_cmds = Vec::new();
        let mut other_cmds = Vec::new();

        for cmd in all_cmds {
            match cmd {
                IoUringCommand::Sync { .. } => sync_cmds.push(cmd),
                _ => other_cmds.push(cmd),
            }
        }

        // Step 4: Process all Sync commands in PARALLEL using futures::join_all
        if !sync_cmds.is_empty() {
            use futures::future::join_all;

            let sync_futures: Vec<_> = sync_cmds.into_iter().map(|cmd| {
                if let IoUringCommand::Sync { partition_key, response } = cmd {
                    let file_ref = files.get(&partition_key);
                    async move {
                        let result = match file_ref {
                            Some(file) => {
                                file.sync_all().await.map_err(|e| crate::WalError::Io(e))
                            }
                            None => {
                                Err(crate::WalError::IoError(format!("File not found for partition: {}", partition_key)))
                            }
                        };
                        let _ = response.send(result);
                    }
                } else {
                    unreachable!()
                }
            }).collect();

            // All fsyncs execute in parallel here!
            join_all(sync_futures).await;
        }

        // Step 5a: Writes, concurrent ACROSS partitions but strictly ordered
        // WITHIN one. Previously every write in the burst was awaited one at a
        // time in this loop, so a batch of N writes cost N sequential round trips
        // through the kernel no matter which files they touched.
        //
        // Ordering is preserved by assigning each write its byte offset up front,
        // in arrival order, per partition — so a partition's writes land where
        // they would have anyway, and no two futures contend for `file_offsets`
        // across an await.
        let mut write_cmds = Vec::new();
        let mut rest_cmds = Vec::new();
        for cmd in other_cmds {
            match cmd {
                IoUringCommand::Write { .. } => write_cmds.push(cmd),
                other => rest_cmds.push(other),
            }
        }

        if !write_cmds.is_empty() {
            use futures::future::join_all;

            // Group by partition, preserving arrival order within each.
            let mut by_partition: HashMap<String, Vec<(Bytes, tokio::sync::oneshot::Sender<Result<()>>)>> =
                HashMap::new();
            for cmd in write_cmds {
                if let IoUringCommand::Write { partition_key, data, response } = cmd {
                    by_partition.entry(partition_key).or_default().push((data, response));
                }
            }

            let mut partition_futures = Vec::new();
            for (partition_key, writes) in by_partition {
                let Some(file) = files.get(&partition_key) else {
                    for (_, response) in writes {
                        let _ = response.send(Err(crate::WalError::IoError(format!(
                            "File not found for partition: {}",
                            partition_key
                        ))));
                    }
                    continue;
                };

                // Reserve the byte range for this partition's whole burst now, so
                // the offset map is consistent before any await point.
                let start = file_offsets.get(&partition_key).copied().unwrap_or(0);
                let total: u64 = writes.iter().map(|(d, _)| d.len() as u64).sum();
                file_offsets.insert(partition_key.clone(), start + total);

                partition_futures.push(async move {
                    let mut offset = start;
                    for (data, response) in writes {
                        let len = data.len() as u64;
                        let result = write_all_at(file, data, offset).await;
                        offset += len;
                        let _ = response.send(result);
                    }
                });
            }

            join_all(partition_futures).await;
        }

        // Step 5b: Everything else (CreateFile, Shutdown), in order.
        for cmd in rest_cmds {
            match cmd {
                IoUringCommand::Write { .. } => unreachable!("writes handled above"),

                IoUringCommand::CreateFile { partition_key, path, response } => {
                    let result = match File::create(&path).await {
                        Ok(file) => {
                            info!("✨ io_uring: Created WAL file for {}: {:?}", partition_key, path);
                            files.insert(partition_key.clone(), file);
                            // Initialize offset to 0 for new file
                            file_offsets.insert(partition_key, 0);
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

                IoUringCommand::Sync { .. } => {
                    // Already processed in parallel batch above
                    unreachable!("Sync commands should be processed in parallel batch")
                }
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

/// Write a whole `Bytes` at `offset`, resubmitting on a short write.
///
/// Uses [`BytesBuf`] so the payload is submitted to the kernel directly instead of
/// being copied into a fresh `Vec` per write. A short write resubmits the
/// remainder as a zero-copy `Bytes` slice rather than reallocating the tail.
#[cfg(all(target_os = "linux", feature = "async-io"))]
async fn write_all_at(file: &tokio_uring::fs::File, data: Bytes, offset: u64) -> Result<()> {
    let mut remaining = data;
    let mut pos = offset;
    while !remaining.is_empty() {
        let len = remaining.len();
        let (res, buf) = file.write_at(BytesBuf(remaining), pos).await;
        match res {
            Ok(0) => {
                return Err(crate::WalError::Io(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "io_uring wrote zero bytes",
                )))
            }
            Ok(n) => {
                pos += n as u64;
                remaining = buf.0.slice(n..); // refcount bump, no copy
                debug_assert!(n <= len);
            }
            Err(e) => return Err(crate::WalError::Io(e)),
        }
    }
    Ok(())
}
