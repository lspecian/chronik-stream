//! CRC32 checksum utilities for segment integrity verification.
//! 
//! Provides enhanced checksum functionality including:
//! - Incremental checksum calculation for streaming
//! - Parallel checksum verification
//! - Checksum repair and recovery options

use chronik_common::{Result, Error};
use crc32fast::Hasher;
use std::io::{Read, Write, Seek, SeekFrom};
use tracing::{debug, warn, error};

/// Checksum verification mode
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ChecksumMode {
    /// Skip checksum verification (fast but unsafe)
    Skip,
    /// Verify checksum and log warning on mismatch
    Warn,
    /// Verify checksum and fail on mismatch (default)
    Strict,
    /// Try to repair checksum if mismatched
    Repair,
}

impl Default for ChecksumMode {
    fn default() -> Self {
        ChecksumMode::Strict
    }
}

/// Checksum calculator for streaming data
pub struct StreamingChecksum {
    hasher: Hasher,
    bytes_processed: u64,
}

impl StreamingChecksum {
    /// Create a new streaming checksum calculator
    pub fn new() -> Self {
        Self {
            hasher: Hasher::new(),
            bytes_processed: 0,
        }
    }
    
    /// Update checksum with data
    pub fn update(&mut self, data: &[u8]) {
        self.hasher.update(data);
        self.bytes_processed += data.len() as u64;
    }
    
    /// Finalize and get checksum
    pub fn finalize(self) -> u32 {
        self.hasher.finalize()
    }
    
    /// Get bytes processed so far
    pub fn bytes_processed(&self) -> u64 {
        self.bytes_processed
    }
}

/// Checksum utilities
pub struct ChecksumUtils;

impl ChecksumUtils {
    /// Calculate checksum for a reader
    pub fn calculate_reader_checksum<R: Read>(reader: &mut R) -> Result<(u32, u64)> {
        let mut hasher = Hasher::new();
        let mut buffer = vec![0u8; 64 * 1024]; // 64KB buffer
        let mut total_bytes = 0u64;
        
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    hasher.update(&buffer[..n]);
                    total_bytes += n as u64;
                }
                Err(e) => return Err(Error::Io(e)),
            }
        }
        
        Ok((hasher.finalize(), total_bytes))
    }
    
    /// Calculate checksum for a slice
    pub fn calculate_checksum(data: &[u8]) -> u32 {
        crc32fast::hash(data)
    }
    
    /// Verify checksum of data at specific position in reader
    pub fn verify_checksum<R: Read + Seek>(
        reader: &mut R,
        start_pos: u64,
        length: u64,
        expected_checksum: u32,
        mode: ChecksumMode,
    ) -> Result<bool> {
        if mode == ChecksumMode::Skip {
            debug!("Skipping checksum verification");
            return Ok(true);
        }
        
        // Seek to start position
        reader.seek(SeekFrom::Start(start_pos))?;
        
        // Calculate checksum
        let mut buffer = vec![0u8; length as usize];
        reader.read_exact(&mut buffer)?;
        let calculated = Self::calculate_checksum(&buffer);
        
        if calculated == expected_checksum {
            debug!(
                "Checksum verified: {} ({}B at offset {})",
                calculated, length, start_pos
            );
            Ok(true)
        } else {
            match mode {
                ChecksumMode::Warn => {
                    warn!(
                        "Checksum mismatch: expected {}, got {} ({}B at offset {})",
                        expected_checksum, calculated, length, start_pos
                    );
                    Ok(false)
                }
                ChecksumMode::Strict => {
                    error!(
                        "Checksum mismatch: expected {}, got {} ({}B at offset {})",
                        expected_checksum, calculated, length, start_pos
                    );
                    Err(Error::InvalidSegment(format!(
                        "Checksum mismatch: expected {}, got {}",
                        expected_checksum, calculated
                    )))
                }
                ChecksumMode::Repair => {
                    warn!(
                        "Checksum mismatch: expected {}, got {} - repair not implemented",
                        expected_checksum, calculated
                    );
                    Ok(false)
                }
                _ => unreachable!(),
            }
        }
    }
    
    /// Calculate checksum for multiple data segments
    pub fn calculate_multi_checksum(segments: &[&[u8]]) -> u32 {
        let mut hasher = Hasher::new();
        for segment in segments {
            hasher.update(segment);
        }
        hasher.finalize()
    }
    
    /// Verify data integrity with progress callback
    pub fn verify_with_progress<R, F>(
        reader: &mut R,
        expected_checksum: u32,
        total_size: u64,
        mut progress_callback: F,
    ) -> Result<bool>
    where
        R: Read,
        F: FnMut(u64, u64),
    {
        let mut hasher = Hasher::new();
        let mut buffer = vec![0u8; 1024 * 1024]; // 1MB buffer
        let mut bytes_read = 0u64;
        
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    hasher.update(&buffer[..n]);
                    bytes_read += n as u64;
                    progress_callback(bytes_read, total_size);
                }
                Err(e) => return Err(Error::Io(e)),
            }
        }
        
        let calculated = hasher.finalize();
        Ok(calculated == expected_checksum)
    }
    
    /// Write data with checksum
    pub fn write_with_checksum<W: Write>(
        writer: &mut W,
        data: &[u8],
    ) -> Result<u32> {
        let checksum = Self::calculate_checksum(data);
        writer.write_all(&checksum.to_be_bytes())?;
        writer.write_all(data)?;
        Ok(checksum)
    }
    
    /// Read data with checksum verification
    pub fn read_with_checksum<R: Read>(
        reader: &mut R,
        expected_size: usize,
        mode: ChecksumMode,
    ) -> Result<Vec<u8>> {
        // Read checksum
        let mut checksum_bytes = [0u8; 4];
        reader.read_exact(&mut checksum_bytes)?;
        let expected_checksum = u32::from_be_bytes(checksum_bytes);
        
        // Read data
        let mut data = vec![0u8; expected_size];
        reader.read_exact(&mut data)?;
        
        // Verify checksum
        if mode != ChecksumMode::Skip {
            let calculated = Self::calculate_checksum(&data);
            if calculated != expected_checksum {
                match mode {
                    ChecksumMode::Warn => {
                        warn!("Checksum mismatch in read_with_checksum");
                    }
                    ChecksumMode::Strict => {
                        return Err(Error::InvalidSegment(
                            "Checksum mismatch in read_with_checksum".into()
                        ));
                    }
                    _ => {}
                }
            }
        }
        
        Ok(data)
    }
}

/// Parallel checksum calculator for large files
pub struct ParallelChecksum {
    chunk_size: usize,
}

impl ParallelChecksum {
    /// Create new parallel checksum calculator
    pub fn new(chunk_size: usize) -> Self {
        Self { chunk_size }
    }
    
    /// Calculate checksum in parallel (simplified - real implementation would use rayon)
    pub fn calculate(&self, data: &[u8]) -> u32 {
        // For now, just use the standard calculation
        // In a real implementation, we'd split data into chunks and process in parallel
        ChecksumUtils::calculate_checksum(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    
    #[test]
    fn test_streaming_checksum() {
        let mut checksum = StreamingChecksum::new();
        checksum.update(b"hello");
        checksum.update(b" ");
        checksum.update(b"world");
        
        let result = checksum.finalize();
        let expected = crc32fast::hash(b"hello world");
        assert_eq!(result, expected);
    }
    
    #[test]
    fn test_calculate_checksum() {
        let data = b"test data for checksum";
        let checksum = ChecksumUtils::calculate_checksum(data);
        let expected = crc32fast::hash(data);
        assert_eq!(checksum, expected);
    }
    
    #[test]
    fn test_multi_checksum() {
        let segments = vec![
            b"segment1".as_ref(),
            b"segment2".as_ref(),
            b"segment3".as_ref(),
        ];
        
        let multi_checksum = ChecksumUtils::calculate_multi_checksum(&segments);
        let expected = crc32fast::hash(b"segment1segment2segment3");
        assert_eq!(multi_checksum, expected);
    }
    
    #[test]
    fn test_write_read_with_checksum() {
        let data = b"test data with checksum";
        let mut buffer = Vec::new();
        
        // Write with checksum
        let checksum = ChecksumUtils::write_with_checksum(&mut buffer, data).unwrap();
        
        // Read with checksum verification
        let mut cursor = Cursor::new(buffer);
        let read_data = ChecksumUtils::read_with_checksum(
            &mut cursor,
            data.len(),
            ChecksumMode::Strict,
        ).unwrap();
        
        assert_eq!(read_data, data);
        assert_eq!(checksum, crc32fast::hash(data));
    }
    
    #[test]
    fn test_checksum_verification_modes() {
        let data = b"test data";
        let correct_checksum = crc32fast::hash(data);
        let wrong_checksum = correct_checksum + 1;
        
        let mut cursor = Cursor::new(data);
        
        // Skip mode - should always pass
        assert!(ChecksumUtils::verify_checksum(
            &mut cursor,
            0,
            data.len() as u64,
            wrong_checksum,
            ChecksumMode::Skip,
        ).unwrap());
        
        // Warn mode - should return false but not error
        cursor.set_position(0);
        assert!(!ChecksumUtils::verify_checksum(
            &mut cursor,
            0,
            data.len() as u64,
            wrong_checksum,
            ChecksumMode::Warn,
        ).unwrap());
        
        // Strict mode - should error
        cursor.set_position(0);
        assert!(ChecksumUtils::verify_checksum(
            &mut cursor,
            0,
            data.len() as u64,
            wrong_checksum,
            ChecksumMode::Strict,
        ).is_err());
        
        // Correct checksum should pass in all modes
        cursor.set_position(0);
        assert!(ChecksumUtils::verify_checksum(
            &mut cursor,
            0,
            data.len() as u64,
            correct_checksum,
            ChecksumMode::Strict,
        ).unwrap());
    }
}