//! Compression utilities for backup data

use crate::{BackupError, Result};
use serde::{Deserialize, Serialize};

/// Supported compression types
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompressionType {
    /// No compression
    None,
    /// Gzip compression
    Gzip,
    /// Zstandard compression (recommended)
    Zstd,
    /// LZ4 compression (fast)
    Lz4,
}

impl CompressionType {
    /// Get file extension for this compression type
    pub fn extension(&self) -> &'static str {
        match self {
            Self::None => "",
            Self::Gzip => "gz",
            Self::Zstd => "zst",
            Self::Lz4 => "lz4",
        }
    }
    
    /// Get MIME type for this compression
    pub fn mime_type(&self) -> &'static str {
        match self {
            Self::None => "application/octet-stream",
            Self::Gzip => "application/gzip",
            Self::Zstd => "application/zstd",
            Self::Lz4 => "application/x-lz4",
        }
    }
}

impl Default for CompressionType {
    fn default() -> Self {
        Self::Zstd
    }
}

/// Compress data using the specified compression type
pub fn compress(data: &[u8], compression: CompressionType) -> Result<Vec<u8>> {
    match compression {
        CompressionType::None => Ok(data.to_vec()),
        CompressionType::Gzip => compress_gzip(data),
        CompressionType::Zstd => compress_zstd(data),
        CompressionType::Lz4 => compress_lz4(data),
    }
}

/// Decompress data using the specified compression type
pub fn decompress(data: &[u8], compression: CompressionType) -> Result<Vec<u8>> {
    match compression {
        CompressionType::None => Ok(data.to_vec()),
        CompressionType::Gzip => decompress_gzip(data),
        CompressionType::Zstd => decompress_zstd(data),
        CompressionType::Lz4 => decompress_lz4(data),
    }
}

/// Compress using gzip
fn compress_gzip(data: &[u8]) -> Result<Vec<u8>> {
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;
    
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(data)
        .map_err(|e| BackupError::Compression(format!("Gzip compression failed: {}", e)))?;
    
    encoder.finish()
        .map_err(|e| BackupError::Compression(format!("Gzip finalization failed: {}", e)))
}

/// Decompress using gzip
fn decompress_gzip(data: &[u8]) -> Result<Vec<u8>> {
    use flate2::read::GzDecoder;
    use std::io::Read;
    
    let mut decoder = GzDecoder::new(data);
    let mut decompressed = Vec::new();
    
    decoder.read_to_end(&mut decompressed)
        .map_err(|e| BackupError::Compression(format!("Gzip decompression failed: {}", e)))?;
    
    Ok(decompressed)
}

/// Compress using zstandard
fn compress_zstd(data: &[u8]) -> Result<Vec<u8>> {
    zstd::encode_all(data, 3) // Compression level 3 (good balance)
        .map_err(|e| BackupError::Compression(format!("Zstd compression failed: {}", e)))
}

/// Decompress using zstandard
fn decompress_zstd(data: &[u8]) -> Result<Vec<u8>> {
    zstd::decode_all(data)
        .map_err(|e| BackupError::Compression(format!("Zstd decompression failed: {}", e)))
}

/// Compress using LZ4
fn compress_lz4(data: &[u8]) -> Result<Vec<u8>> {
    // Use prepend_size=false since we're manually prepending the size
    let compressed = lz4::block::compress(data, None, false)
        .map_err(|e| BackupError::Compression(format!("LZ4 compression failed: {}", e)))?;
    
    // Prepend uncompressed size for decompression
    let mut result = Vec::with_capacity(8 + compressed.len());
    result.extend_from_slice(&(data.len() as u64).to_le_bytes());
    result.extend_from_slice(&compressed);
    
    Ok(result)
}

/// Decompress using LZ4
fn decompress_lz4(data: &[u8]) -> Result<Vec<u8>> {
    if data.len() < 8 {
        return Err(BackupError::Compression("LZ4 data too short".to_string()));
    }
    
    // Read uncompressed size
    let uncompressed_size = u64::from_le_bytes(data[0..8].try_into().unwrap()) as usize;
    let compressed_data = &data[8..];
    
    // LZ4 uses i32 for size, ensure we don't overflow
    if uncompressed_size > i32::MAX as usize {
        return Err(BackupError::Compression("LZ4 uncompressed size too large".to_string()));
    }
    
    let decompressed = lz4::block::decompress(compressed_data, Some(uncompressed_size as i32))
        .map_err(|e| BackupError::Compression(format!("LZ4 decompression failed: {}", e)))?;
    
    Ok(decompressed)
}

/// Compression statistics
#[derive(Debug, Clone)]
pub struct CompressionStats {
    /// Original size in bytes
    pub original_size: u64,
    /// Compressed size in bytes
    pub compressed_size: u64,
    /// Compression ratio (original/compressed)
    pub ratio: f64,
    /// Compression type used
    pub compression_type: CompressionType,
    /// Time taken to compress
    pub compression_time: std::time::Duration,
}

impl CompressionStats {
    /// Calculate compression stats
    pub fn new(
        original_size: u64,
        compressed_size: u64,
        compression_type: CompressionType,
        compression_time: std::time::Duration,
    ) -> Self {
        let ratio = if compressed_size > 0 {
            original_size as f64 / compressed_size as f64
        } else {
            1.0
        };
        
        Self {
            original_size,
            compressed_size,
            ratio,
            compression_type,
            compression_time,
        }
    }
    
    /// Get space saved as a percentage
    pub fn space_saved_percent(&self) -> f64 {
        if self.original_size > 0 {
            ((self.original_size - self.compressed_size) as f64 / self.original_size as f64) * 100.0
        } else {
            0.0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_compression_roundtrip() {
        // Use longer test data to ensure compression is effective
        let test_data = b"Hello, this is test data that should be compressed and decompressed correctly! \
                          This string is repeated multiple times to ensure there's enough redundancy for compression. \
                          Hello, this is test data that should be compressed and decompressed correctly! \
                          This string is repeated multiple times to ensure there's enough redundancy for compression. \
                          Hello, this is test data that should be compressed and decompressed correctly! \
                          This string is repeated multiple times to ensure there's enough redundancy for compression.";
        
        for compression_type in [
            CompressionType::None,
            CompressionType::Gzip,
            CompressionType::Zstd,
            CompressionType::Lz4,
        ] {
            let compressed = compress(test_data, compression_type).unwrap();
            let decompressed = decompress(&compressed, compression_type).unwrap();
            
            assert_eq!(test_data.to_vec(), decompressed,
                      "Compression roundtrip failed for {:?}", compression_type);
            
            // Verify compression actually reduces size (except for None)
            if compression_type != CompressionType::None {
                assert!(compressed.len() < test_data.len(),
                       "Compression didn't reduce size for {:?}", compression_type);
            }
        }
    }
    
    #[test]
    fn test_compression_stats() {
        let stats = CompressionStats::new(
            1000,
            250,
            CompressionType::Zstd,
            std::time::Duration::from_millis(10),
        );
        
        assert_eq!(stats.ratio, 4.0);
        assert_eq!(stats.space_saved_percent(), 75.0);
    }
}