//! Kafka protocol compression support for message batching.
//! 
//! Supports Kafka compression codecs:
//! - None (0)
//! - Gzip (1)
//! - Snappy (2)
//! - LZ4 (3)
//! - Zstd (4)

use bytes::{Bytes, BytesMut};
use flate2::read::{GzDecoder, GzEncoder};
use flate2::Compression;
use std::io::Read;
use tracing::debug;

use chronik_common::{Result, Error};

/// Kafka compression codec types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionType {
    None = 0,
    Gzip = 1,
    Snappy = 2,
    Lz4 = 3,
    Zstd = 4,
}

impl CompressionType {
    /// Create from codec ID
    pub fn from_id(id: i8) -> Option<Self> {
        match id {
            0 => Some(CompressionType::None),
            1 => Some(CompressionType::Gzip),
            2 => Some(CompressionType::Snappy),
            3 => Some(CompressionType::Lz4),
            4 => Some(CompressionType::Zstd),
            _ => None,
        }
    }

    /// Get codec ID
    pub fn id(&self) -> i8 {
        *self as i8
    }

    /// Check if compression is enabled
    pub fn is_compressed(&self) -> bool {
        !matches!(self, CompressionType::None)
    }
}

/// Compression handler for Kafka messages
pub struct CompressionHandler;

impl CompressionHandler {
    /// Compress data using the specified codec
    pub fn compress(data: &[u8], codec: CompressionType) -> Result<Bytes> {
        match codec {
            CompressionType::None => Ok(Bytes::copy_from_slice(data)),
            CompressionType::Gzip => Self::compress_gzip(data),
            CompressionType::Snappy => Self::compress_snappy(data),
            CompressionType::Lz4 => Self::compress_lz4(data),
            CompressionType::Zstd => Self::compress_zstd(data),
        }
    }

    /// Decompress data using the specified codec
    pub fn decompress(data: &[u8], codec: CompressionType) -> Result<Bytes> {
        match codec {
            CompressionType::None => Ok(Bytes::copy_from_slice(data)),
            CompressionType::Gzip => Self::decompress_gzip(data),
            CompressionType::Snappy => Self::decompress_snappy(data),
            CompressionType::Lz4 => Self::decompress_lz4(data),
            CompressionType::Zstd => Self::decompress_zstd(data),
        }
    }

    /// Compress with Gzip
    fn compress_gzip(data: &[u8]) -> Result<Bytes> {
        debug!("Compressing {} bytes with gzip", data.len());
        let mut encoder = GzEncoder::new(data, Compression::default());
        let mut compressed = Vec::new();
        encoder
            .read_to_end(&mut compressed)
            .map_err(|e| Error::Protocol(format!("Gzip compression failed: {}", e)))?;
        debug!("Compressed to {} bytes", compressed.len());
        Ok(Bytes::from(compressed))
    }

    /// Decompress with Gzip
    fn decompress_gzip(data: &[u8]) -> Result<Bytes> {
        debug!("Decompressing {} bytes with gzip", data.len());
        let mut decoder = GzDecoder::new(data);
        let mut decompressed = Vec::new();
        decoder
            .read_to_end(&mut decompressed)
            .map_err(|e| Error::Protocol(format!("Gzip decompression failed: {}", e)))?;
        debug!("Decompressed to {} bytes", decompressed.len());
        Ok(Bytes::from(decompressed))
    }

    /// Compress with Snappy
    fn compress_snappy(_data: &[u8]) -> Result<Bytes> {
        // For now, return unsupported
        Err(Error::Protocol("Snappy compression not yet implemented".into()))
    }

    /// Decompress with Snappy
    fn decompress_snappy(_data: &[u8]) -> Result<Bytes> {
        // For now, return unsupported
        Err(Error::Protocol("Snappy decompression not yet implemented".into()))
    }

    /// Compress with LZ4
    fn compress_lz4(_data: &[u8]) -> Result<Bytes> {
        // For now, return unsupported
        Err(Error::Protocol("LZ4 compression not yet implemented".into()))
    }

    /// Decompress with LZ4
    fn decompress_lz4(_data: &[u8]) -> Result<Bytes> {
        // For now, return unsupported
        Err(Error::Protocol("LZ4 decompression not yet implemented".into()))
    }

    /// Compress with Zstd
    fn compress_zstd(_data: &[u8]) -> Result<Bytes> {
        // For now, return unsupported
        Err(Error::Protocol("Zstd compression not yet implemented".into()))
    }

    /// Decompress with Zstd
    fn decompress_zstd(_data: &[u8]) -> Result<Bytes> {
        // For now, return unsupported
        Err(Error::Protocol("Zstd decompression not yet implemented".into()))
    }
}

/// Message batch builder for efficient batching
pub struct MessageBatch {
    messages: Vec<Bytes>,
    compression: CompressionType,
    max_batch_size: usize,
    current_size: usize,
}

impl MessageBatch {
    /// Create a new message batch
    pub fn new(compression: CompressionType) -> Self {
        Self {
            messages: Vec::new(),
            compression,
            max_batch_size: 1024 * 1024, // 1MB default
            current_size: 0,
        }
    }

    /// Create with custom max batch size
    pub fn with_max_size(compression: CompressionType, max_batch_size: usize) -> Self {
        Self {
            messages: Vec::new(),
            compression,
            max_batch_size,
            current_size: 0,
        }
    }

    /// Add a message to the batch
    pub fn add(&mut self, message: Bytes) -> Result<bool> {
        let message_size = message.len();
        
        // Check if adding this message would exceed max size
        if self.current_size + message_size > self.max_batch_size {
            return Ok(false);
        }

        self.messages.push(message);
        self.current_size += message_size;
        Ok(true)
    }

    /// Check if batch is empty
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// Get number of messages in batch
    pub fn len(&self) -> usize {
        self.messages.len()
    }

    /// Get current batch size
    pub fn size(&self) -> usize {
        self.current_size
    }

    /// Check if batch is full
    pub fn is_full(&self) -> bool {
        self.current_size >= self.max_batch_size
    }

    /// Build the final batch with compression
    pub fn build(self) -> Result<Bytes> {
        if self.messages.is_empty() {
            return Ok(Bytes::new());
        }

        // Concatenate all messages
        let mut buf = BytesMut::with_capacity(self.current_size);
        for message in self.messages {
            buf.extend_from_slice(&message);
        }

        let data = buf.freeze();

        // Apply compression if needed
        if self.compression.is_compressed() {
            CompressionHandler::compress(&data, self.compression)
        } else {
            Ok(data)
        }
    }

    /// Clear the batch
    pub fn clear(&mut self) {
        self.messages.clear();
        self.current_size = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compression_type() {
        assert_eq!(CompressionType::from_id(0), Some(CompressionType::None));
        assert_eq!(CompressionType::from_id(1), Some(CompressionType::Gzip));
        assert_eq!(CompressionType::from_id(2), Some(CompressionType::Snappy));
        assert_eq!(CompressionType::from_id(3), Some(CompressionType::Lz4));
        assert_eq!(CompressionType::from_id(4), Some(CompressionType::Zstd));
        assert_eq!(CompressionType::from_id(5), None);

        assert_eq!(CompressionType::None.id(), 0);
        assert_eq!(CompressionType::Gzip.id(), 1);

        assert!(!CompressionType::None.is_compressed());
        assert!(CompressionType::Gzip.is_compressed());
    }

    #[test]
    fn test_gzip_compression() {
        let data = b"Hello, this is a test message that should be compressed!";
        let compressed = CompressionHandler::compress(data, CompressionType::Gzip).unwrap();
        
        // Compressed should be different and likely smaller
        assert_ne!(&compressed[..], data);
        
        // Decompress and verify
        let decompressed = CompressionHandler::decompress(&compressed, CompressionType::Gzip).unwrap();
        assert_eq!(&decompressed[..], data);
    }

    #[test]
    fn test_no_compression() {
        let data = b"Hello, this is a test message!";
        let result = CompressionHandler::compress(data, CompressionType::None).unwrap();
        assert_eq!(&result[..], data);
        
        let decompressed = CompressionHandler::decompress(&result, CompressionType::None).unwrap();
        assert_eq!(&decompressed[..], data);
    }

    #[test]
    fn test_message_batch() {
        let mut batch = MessageBatch::new(CompressionType::None);
        
        assert!(batch.is_empty());
        assert_eq!(batch.len(), 0);
        assert_eq!(batch.size(), 0);

        // Add messages
        let msg1 = Bytes::from("message1");
        let msg2 = Bytes::from("message2");
        
        assert!(batch.add(msg1.clone()).unwrap());
        assert!(batch.add(msg2.clone()).unwrap());
        
        assert!(!batch.is_empty());
        assert_eq!(batch.len(), 2);
        assert_eq!(batch.size(), 16); // 8 + 8

        // Build batch
        let result = batch.build().unwrap();
        assert_eq!(&result[..], b"message1message2");
    }

    #[test]
    fn test_message_batch_size_limit() {
        let mut batch = MessageBatch::with_max_size(CompressionType::None, 10);
        
        let msg1 = Bytes::from("12345");
        let msg2 = Bytes::from("67890");
        let msg3 = Bytes::from("X");
        
        assert!(batch.add(msg1).unwrap());
        assert!(batch.add(msg2).unwrap());
        assert!(!batch.add(msg3).unwrap()); // Would exceed limit
        
        assert_eq!(batch.len(), 2);
        assert_eq!(batch.size(), 10);
    }
}