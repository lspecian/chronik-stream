//! WAL segment management

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use parking_lot::RwLock;
use bytes::BytesMut;
use chrono::Utc;
use tracing::{info, debug, instrument};

use crate::record::WalRecord;
use crate::error::Result;

/// Active WAL segment for writing
pub struct WalSegment {
    pub id: u64,
    pub path: PathBuf,
    pub base_offset: i64,
    pub last_offset: i64,
    pub size: u64,
    pub created_at: i64,
    pub buffer: Arc<RwLock<BytesMut>>,
    pub record_count: u64,
}

impl WalSegment {
    /// Create a new WAL segment
    #[instrument(fields(
        segment_id = id,
        base_offset = base_offset,
        segment_path = %path.display()
    ))]
    pub fn new(id: u64, base_offset: i64, path: PathBuf) -> Self {
        debug!("Creating new WAL segment");
        let segment = Self {
            id,
            path,
            base_offset,
            last_offset: base_offset - 1,
            size: 0,
            created_at: Utc::now().timestamp_millis(),
            buffer: Arc::new(RwLock::new(BytesMut::with_capacity(1024 * 1024))),
            record_count: 0,
        };
        
        info!("WAL segment created successfully");
        segment
    }
    
    /// Append a record to the segment
    #[instrument(skip(self, record), fields(
        segment_id = self.id,
        record_offset = record.offset,
        record_length = record.length,
        bytes_written = tracing::field::Empty,
        append_duration_us = tracing::field::Empty
    ))]
    pub async fn append(&mut self, record: WalRecord) -> Result<()> {
        let append_start = Instant::now();
        let record_size = record.length;
        
        let mut buffer = self.buffer.write();
        record.write_to(&mut buffer)?;
        
        self.last_offset = record.offset;
        self.size += record.length as u64;
        self.record_count += 1;
        
        let append_duration = append_start.elapsed();
        
        // Record metrics in span
        tracing::Span::current()
            .record("bytes_written", record_size)
            .record("append_duration_us", append_duration.as_micros() as u64);
        
        debug!(
            new_size = self.size,
            new_record_count = self.record_count,
            append_duration_us = append_duration.as_micros() as u64,
            "WAL record appended to segment"
        );
        
        Ok(())
    }
    
    /// Check if segment should be rotated
    #[instrument(skip(self), fields(
        segment_id = self.id,
        current_size = self.size,
        max_size = max_size,
        segment_age_ms = Utc::now().timestamp_millis() - self.created_at,
        max_age_ms = max_age_ms,
        should_rotate = tracing::field::Empty
    ))]
    pub fn should_rotate(&self, max_size: u64, max_age_ms: u64) -> bool {
        let should_rotate_size = self.size >= max_size;
        let age_ms = Utc::now().timestamp_millis() - self.created_at;
        let should_rotate_age = age_ms >= max_age_ms as i64;
        let should_rotate = should_rotate_size || should_rotate_age;
        
        // Record decision in span
        tracing::Span::current()
            .record("should_rotate", should_rotate);
        
        if should_rotate {
            debug!(
                rotation_reason = if should_rotate_size { "size_limit" } else { "age_limit" },
                "WAL segment rotation check: rotation needed"
            );
        }
        
        should_rotate
    }
    
    /// Seal the segment for rotation
    #[instrument(skip(self), fields(
        segment_id = self.id,
        final_size = self.size,
        final_record_count = self.record_count,
        bytes_written = tracing::field::Empty,
        fsync_duration_ms = tracing::field::Empty
    ))]
    pub async fn seal(self) -> Result<SealedSegment> {
        let buffer = self.buffer.read();
        let buffer_len = buffer.len();
        
        // Write buffer to disk with timing
        let fsync_start = Instant::now();
        tokio::fs::write(&self.path, &buffer[..]).await?;
        let fsync_duration = fsync_start.elapsed();
        
        // Record metrics in span
        tracing::Span::current()
            .record("bytes_written", buffer_len as u64)
            .record("fsync_duration_ms", fsync_duration.as_millis() as u64);
        
        info!(
            segment_path = %self.path.display(),
            bytes_written = buffer_len,
            fsync_duration_ms = fsync_duration.as_millis() as u64,
            "WAL segment sealed and written to disk"
        );
        
        Ok(SealedSegment {
            id: self.id,
            path: self.path,
            base_offset: self.base_offset,
            last_offset: self.last_offset,
            size: self.size,
            created_at: self.created_at,
            sealed_at: Utc::now().timestamp_millis(),
            record_count: self.record_count,
        })
    }
}

/// Sealed WAL segment (read-only)
#[derive(Debug, Clone)]
pub struct SealedSegment {
    pub id: u64,
    pub path: PathBuf,
    pub base_offset: i64,
    pub last_offset: i64,
    pub size: u64,
    pub created_at: i64,
    pub sealed_at: i64,
    pub record_count: u64,
}

impl SealedSegment {
    /// Check if segment contains the given offset
    #[instrument(skip(self), fields(
        segment_id = self.id,
        base_offset = self.base_offset,
        last_offset = self.last_offset,
        query_offset = offset,
        contains = tracing::field::Empty
    ))]
    pub fn contains_offset(&self, offset: i64) -> bool {
        let contains = offset >= self.base_offset && offset <= self.last_offset;
        
        // Record result in span
        tracing::Span::current()
            .record("contains", contains);
        
        debug!(
            contains = contains,
            "WAL segment offset containment check"
        );
        
        contains
    }
    
    /// Load segment from disk
    #[instrument(fields(
        segment_path = %path.display(),
        segment_id = tracing::field::Empty,
        base_offset = tracing::field::Empty,
        size_bytes = tracing::field::Empty
    ))]
    pub async fn load(path: &Path) -> Result<Self> {
        let load_start = Instant::now();
        // Parse segment metadata from filename
        // Format: wal_<id>_<base_offset>.log
        let filename = path.file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| crate::error::WalError::InvalidFormat(
                "Invalid segment filename".to_string()
            ))?;
        
        let parts: Vec<&str> = filename.trim_end_matches(".log").split('_').collect();
        if parts.len() != 3 || parts[0] != "wal" {
            return Err(crate::error::WalError::InvalidFormat(
                format!("Invalid segment filename format: {}", filename)
            ));
        }
        
        let id = parts[1].parse::<u64>()
            .map_err(|e| crate::error::WalError::InvalidFormat(e.to_string()))?;
        let base_offset = parts[2].parse::<i64>()
            .map_err(|e| crate::error::WalError::InvalidFormat(e.to_string()))?;
        
        let metadata = tokio::fs::metadata(path).await?;
        let size = metadata.len();
        let load_duration = load_start.elapsed();
        
        // Record metrics in span
        tracing::Span::current()
            .record("segment_id", id)
            .record("base_offset", base_offset)
            .record("size_bytes", size);
        
        info!(
            segment_id = id,
            base_offset = base_offset,
            size_bytes = size,
            load_duration_ms = load_duration.as_millis() as u64,
            "WAL segment loaded from disk"
        );
        
        Ok(Self {
            id,
            path: path.to_path_buf(),
            base_offset,
            last_offset: base_offset, // Will be updated during recovery
            size,
            created_at: 0,
            sealed_at: 0,
            record_count: 0,
        })
    }
}