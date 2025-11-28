//! Frame reading and parsing
//!
//! Extracted from `handle_connection()` to reduce complexity.
//! Handles WAL replication frame protocol: header reading, heartbeat handling, complete frame buffering.

use anyhow::Result;
use bytes::{Buf, Bytes, BytesMut};
use dashmap::DashMap;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::time::{timeout, Duration};
use tracing::{debug, warn};

use super::super::wal_replication::{WalReplicationRecord, deserialize_wal_frame};

/// Magic number for WAL frames ('WA' in hex)
const FRAME_MAGIC: u16 = 0x5741;

/// Magic number for heartbeat frames ('HB' in hex)
const HEARTBEAT_MAGIC: u16 = 0x4842;

/// Frame reader state
#[derive(Debug)]
pub enum FrameReadResult {
    /// Complete WAL record frame with heartbeat tracking
    WalRecord {
        record: WalReplicationRecord,
        partition_key: (String, i32),
    },
    /// Heartbeat frame consumed
    Heartbeat,
    /// Connection closed gracefully
    ConnectionClosed,
    /// Need to continue reading (timeout, incomplete data)
    Continue,
}

/// Frame reader
///
/// Handles low-level frame protocol operations.
pub struct FrameReader;

impl FrameReader {
    /// Read frame header from stream with timeout
    ///
    /// Complexity: < 15 (buffer reading with timeout handling)
    pub async fn read_header(
        stream: &mut TcpStream,
        buffer: &mut BytesMut,
    ) -> Result<Option<()>> {
        if buffer.len() >= 8 {
            return Ok(Some(())); // Already have header
        }

        // Need more data for header
        let read_timeout = timeout(Duration::from_secs(60), stream.read_buf(buffer)).await;

        match read_timeout {
            Ok(Ok(0)) => {
                // Connection closed
                debug!("WAL receiver: Connection closed by peer");
                Ok(None)
            }
            Ok(Ok(n)) => {
                debug!("WAL receiver: Read {} bytes", n);
                Ok(Some(()))
            }
            Ok(Err(e)) => {
                Err(anyhow::anyhow!("Read error: {}", e))
            }
            Err(_) => {
                // Timeout - check for shutdown
                if buffer.is_empty() {
                    warn!("WAL receiver: No data received for 60s, closing connection");
                    Ok(None)
                } else {
                    Ok(Some(())) // Continue with existing buffer
                }
            }
        }
    }

    /// Parse frame header and determine frame type
    ///
    /// Complexity: < 10 (simple header parsing)
    pub fn parse_header(buffer: &BytesMut) -> Option<(u16, usize)> {
        if buffer.len() < 8 {
            return None;
        }

        let magic = u16::from_be_bytes([buffer[0], buffer[1]]);
        let total_length = u32::from_be_bytes([buffer[4], buffer[5], buffer[6], buffer[7]]) as usize;
        let frame_size = 8 + total_length; // header + payload

        Some((magic, frame_size))
    }

    /// Handle heartbeat frame
    ///
    /// Complexity: < 5 (simple buffer consumption)
    pub fn handle_heartbeat(buffer: &mut BytesMut) -> bool {
        if buffer.len() >= 8 {
            buffer.advance(8);
            debug!("WAL receiver: Received heartbeat");
            true
        } else {
            false
        }
    }

    /// Read complete frame from stream
    ///
    /// Complexity: < 15 (buffered reading with timeout)
    pub async fn read_complete_frame(
        stream: &mut TcpStream,
        buffer: &mut BytesMut,
        frame_size: usize,
        shutdown: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<Bytes> {
        // Wait until we have the complete frame
        while buffer.len() < frame_size && !shutdown.load(std::sync::atomic::Ordering::Relaxed) {
            match timeout(Duration::from_secs(30), stream.read_buf(buffer)).await {
                Ok(Ok(0)) => {
                    return Err(anyhow::anyhow!("Connection closed before complete frame received"));
                }
                Ok(Ok(n)) => {
                    debug!("WAL receiver: Read {} more bytes (total: {})", n, buffer.len());
                }
                Ok(Err(e)) => {
                    return Err(anyhow::anyhow!("Read error: {}", e));
                }
                Err(_) => {
                    return Err(anyhow::anyhow!("Timeout waiting for complete frame"));
                }
            }
        }

        // Split and return frame bytes
        Ok(buffer.split_to(frame_size).freeze())
    }

    /// Try to read and parse next frame from stream
    ///
    /// Complexity: < 25 (orchestrates header read, frame read, deserialization)
    pub async fn read_next_frame(
        stream: &mut TcpStream,
        buffer: &mut BytesMut,
        shutdown: &std::sync::Arc<std::sync::atomic::AtomicBool>,
        last_heartbeat: &std::sync::Arc<DashMap<(String, i32), std::time::Instant>>,
    ) -> Result<FrameReadResult> {
        // Read header if needed
        match Self::read_header(stream, buffer).await? {
            None => return Ok(FrameReadResult::ConnectionClosed),
            Some(_) => {}
        }

        // Try to parse frame header
        let (magic, frame_size) = match Self::parse_header(buffer) {
            Some(header) => header,
            None => return Ok(FrameReadResult::Continue), // Need more data
        };

        // Handle heartbeat frames
        if magic == HEARTBEAT_MAGIC {
            Self::handle_heartbeat(buffer);
            return Ok(FrameReadResult::Heartbeat);
        }

        // Validate frame magic
        if magic != FRAME_MAGIC {
            return Err(anyhow::anyhow!("Invalid frame magic: 0x{:04x}", magic));
        }

        // Read complete frame
        let frame_bytes = Self::read_complete_frame(stream, buffer, frame_size, shutdown).await?;

        // Deserialize WAL record
        let wal_record = deserialize_wal_frame(frame_bytes)?;

        // Track last heartbeat for this partition
        let partition_key = (wal_record.topic.clone(), wal_record.partition);
        last_heartbeat.insert(partition_key.clone(), std::time::Instant::now());

        Ok(FrameReadResult::WalRecord {
            record: wal_record,
            partition_key,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_header_insufficient_data() {
        let buffer = BytesMut::from(&[0x57, 0x41, 0x00, 0x01][..]);
        assert!(FrameReader::parse_header(&buffer).is_none());
    }

    #[test]
    fn test_parse_header_complete() {
        let mut buffer = BytesMut::new();
        buffer.extend_from_slice(&[
            0x57, 0x41, // FRAME_MAGIC
            0x00, 0x01, // version
            0x00, 0x00, 0x00, 0x64, // length = 100
        ]);

        let result = FrameReader::parse_header(&buffer);
        assert!(result.is_some());
        let (magic, frame_size) = result.unwrap();
        assert_eq!(magic, FRAME_MAGIC);
        assert_eq!(frame_size, 108); // 8 header + 100 payload
    }

    #[test]
    fn test_heartbeat_magic_detection() {
        let mut buffer = BytesMut::new();
        buffer.extend_from_slice(&[
            0x48, 0x42, // HEARTBEAT_MAGIC
            0x00, 0x01, // version
            0x00, 0x00, 0x00, 0x00, // length = 0
        ]);

        let (magic, _) = FrameReader::parse_header(&buffer).unwrap();
        assert_eq!(magic, HEARTBEAT_MAGIC);
    }
}
