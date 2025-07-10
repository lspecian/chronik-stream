//! Kafka protocol frame handling for request/response framing.
//! 
//! The Kafka protocol uses length-prefixed messages:
//! - Request: [Length: i32][RequestMessage]
//! - Response: [Length: i32][ResponseMessage]

use bytes::{Buf, BufMut, Bytes, BytesMut};
use tokio_util::codec::{Decoder, Encoder};
use tracing::{debug, trace};

use chronik_common::{Result, Error};

/// Maximum frame size (100MB) to prevent OOM attacks
const MAX_FRAME_SIZE: usize = 100 * 1024 * 1024;

/// Minimum frame size - Kafka allows very small frames
/// API versions response can be as small as 6 bytes (correlation ID + error code)
const MIN_FRAME_SIZE: usize = 6;

/// Kafka protocol frame decoder/encoder
pub struct KafkaFrameCodec {
    /// Maximum allowed frame size
    max_frame_size: usize,
}

impl KafkaFrameCodec {
    /// Create a new frame codec with default settings
    pub fn new() -> Self {
        Self {
            max_frame_size: MAX_FRAME_SIZE,
        }
    }

    /// Create a new frame codec with custom max frame size
    pub fn with_max_frame_size(max_frame_size: usize) -> Self {
        Self { max_frame_size }
    }
}

impl Default for KafkaFrameCodec {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder for KafkaFrameCodec {
    type Item = Bytes;
    type Error = Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>> {
        // Need at least 4 bytes for the length prefix
        if src.len() < 4 {
            trace!("Not enough data for length prefix, have {} bytes", src.len());
            return Ok(None);
        }

        // Peek at the length without consuming
        let mut length_bytes = [0u8; 4];
        length_bytes.copy_from_slice(&src[..4]);
        let length = i32::from_be_bytes(length_bytes) as usize;

        // Validate frame size
        if length < MIN_FRAME_SIZE {
            return Err(Error::Protocol(format!(
                "Frame size {} is below minimum {}",
                length, MIN_FRAME_SIZE
            )));
        }

        if length > self.max_frame_size {
            return Err(Error::Protocol(format!(
                "Frame size {} exceeds maximum {}",
                length, self.max_frame_size
            )));
        }

        // Check if we have the complete frame
        if src.len() < 4 + length {
            trace!(
                "Waiting for complete frame, have {} bytes, need {}",
                src.len(),
                4 + length
            );
            // Reserve capacity for the complete frame
            src.reserve(4 + length - src.len());
            return Ok(None);
        }

        // We have a complete frame
        debug!("Decoding frame of {} bytes", length);

        // Skip the length prefix
        src.advance(4);

        // Extract the frame data
        let frame = src.split_to(length).freeze();

        Ok(Some(frame))
    }
}

impl Encoder<Bytes> for KafkaFrameCodec {
    type Error = Error;

    fn encode(&mut self, item: Bytes, dst: &mut BytesMut) -> Result<()> {
        let length = item.len();

        // Validate frame size
        if length < MIN_FRAME_SIZE {
            return Err(Error::Protocol(format!(
                "Frame size {} is below minimum {}",
                length, MIN_FRAME_SIZE
            )));
        }

        if length > self.max_frame_size {
            return Err(Error::Protocol(format!(
                "Frame size {} exceeds maximum {}",
                length, self.max_frame_size
            )));
        }

        debug!("Encoding frame of {} bytes", length);

        // Reserve space for length prefix and data
        dst.reserve(4 + length);

        // Write length prefix
        dst.put_i32(length as i32);

        // Write frame data
        dst.put(item);

        Ok(())
    }
}

/// Frame-level message for batching multiple requests/responses
#[derive(Debug, Clone)]
pub struct Frame {
    /// The frame data
    pub data: Bytes,
    /// Optional correlation ID extracted from header
    pub correlation_id: Option<i32>,
}

impl Frame {
    /// Create a new frame
    pub fn new(data: Bytes) -> Self {
        Self {
            data,
            correlation_id: None,
        }
    }

    /// Create a frame with correlation ID
    pub fn with_correlation_id(data: Bytes, correlation_id: i32) -> Self {
        Self {
            data,
            correlation_id: Some(correlation_id),
        }
    }

    /// Get the frame size (excluding length prefix)
    pub fn size(&self) -> usize {
        self.data.len()
    }

    /// Extract correlation ID from request frame (if possible)
    pub fn extract_correlation_id(&mut self) -> Option<i32> {
        if self.correlation_id.is_some() {
            return self.correlation_id;
        }

        // Try to extract from frame data
        // Request header: api_key(2) + api_version(2) + correlation_id(4) + client_id_len(2)
        if self.data.len() >= 8 {
            let mut data = self.data.clone();
            data.advance(4); // Skip api_key and api_version
            let correlation_id = data.get_i32();
            self.correlation_id = Some(correlation_id);
            Some(correlation_id)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_frame_codec_decode() {
        let mut codec = KafkaFrameCodec::new();
        let mut buf = BytesMut::new();

        // Test incomplete length prefix
        buf.put_u8(0);
        buf.put_u8(0);
        assert!(codec.decode(&mut buf).unwrap().is_none());

        // Complete length prefix but no data
        buf.put_u8(0);
        buf.put_u8(20); // Length = 20
        assert!(codec.decode(&mut buf).unwrap().is_none());

        // Add complete frame data
        let data = vec![0u8; 20];
        buf.put_slice(&data);

        let frame = codec.decode(&mut buf).unwrap().unwrap();
        assert_eq!(frame.len(), 20);
        assert_eq!(buf.len(), 0); // All consumed
    }

    #[test]
    fn test_frame_codec_encode() {
        let mut codec = KafkaFrameCodec::new();
        let mut buf = BytesMut::new();

        let data = vec![0u8; 100];
        let frame = Bytes::from(data);

        codec.encode(frame, &mut buf).unwrap();

        // Check length prefix
        assert_eq!(buf.len(), 104); // 4 bytes length + 100 bytes data
        let length = buf.get_i32();
        assert_eq!(length, 100);
    }

    #[test]
    fn test_frame_size_validation() {
        let mut codec = KafkaFrameCodec::new();
        let mut buf = BytesMut::new();

        // Test frame too small
        let small_frame = Bytes::from(vec![0u8; 10]);
        assert!(codec.encode(small_frame, &mut buf).is_err());

        // Test frame too large
        let codec = KafkaFrameCodec::with_max_frame_size(1000);
        let large_frame = Bytes::from(vec![0u8; 2000]);
        let mut codec = codec;
        assert!(codec.encode(large_frame, &mut buf).is_err());
    }

    #[test]
    fn test_extract_correlation_id() {
        // Create a mock request frame
        let mut data = BytesMut::new();
        data.put_i16(3); // api_key = Metadata
        data.put_i16(12); // api_version
        data.put_i32(12345); // correlation_id
        data.put_i16(0); // client_id length (null)

        let mut frame = Frame::new(data.freeze());
        assert_eq!(frame.extract_correlation_id(), Some(12345));
        assert_eq!(frame.correlation_id, Some(12345));
    }
}