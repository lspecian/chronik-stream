//! Version-Specific Field Encoding
//!
//! Handles encoding of version-specific header fields in Fetch responses.
//! Complexity: < 15 per function

use crate::parser::Encoder;
use tracing::trace;

/// Encoder for version-specific response header fields
pub struct VersionFieldsEncoder;

impl VersionFieldsEncoder {
    /// Encode throttle time (v1+)
    ///
    /// Complexity: < 5 (single field with version check)
    pub fn encode_throttle_time(encoder: &mut Encoder, throttle_time_ms: i32, version: i16) {
        if version >= 1 {
            encoder.write_i32(throttle_time_ms);
            trace!("  throttle_time_ms: {}", throttle_time_ms);
        }
    }

    /// Encode error code and session ID (v7+)
    ///
    /// Complexity: < 5 (two fields with version check)
    pub fn encode_error_and_session(
        encoder: &mut Encoder,
        error_code: i16,
        session_id: i32,
        version: i16,
    ) {
        if version >= 7 {
            encoder.write_i16(error_code);
            encoder.write_i32(session_id);
            trace!("  error_code: {}, session_id: {}", error_code, session_id);
        }
    }

    /// Encode last stable offset (v4+)
    ///
    /// Complexity: < 5
    pub fn encode_last_stable_offset(encoder: &mut Encoder, offset: i64, version: i16) {
        if version >= 4 {
            encoder.write_i64(offset);
        }
    }

    /// Encode log start offset (v5+)
    ///
    /// Complexity: < 5
    pub fn encode_log_start_offset(encoder: &mut Encoder, offset: i64, version: i16) {
        if version >= 5 {
            encoder.write_i64(offset);
            trace!("        log_start_offset: {}", offset);
        }
    }

    /// Encode preferred read replica (v11+)
    ///
    /// Complexity: < 5
    pub fn encode_preferred_read_replica(encoder: &mut Encoder, replica: i32, version: i16) {
        if version >= 11 {
            encoder.write_i32(replica);
            trace!("        preferred_read_replica: {}", replica);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::BytesMut;

    #[test]
    fn test_encode_throttle_time_v0() {
        let mut buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut buf);

        VersionFieldsEncoder::encode_throttle_time(&mut encoder, 100, 0);

        // v0 doesn't include throttle time
        assert_eq!(buf.len(), 0);
    }

    #[test]
    fn test_encode_throttle_time_v1() {
        let mut buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut buf);

        VersionFieldsEncoder::encode_throttle_time(&mut encoder, 100, 1);

        // v1+ includes throttle time (4 bytes)
        assert_eq!(buf.len(), 4);
    }

    #[test]
    fn test_encode_error_and_session_v6() {
        let mut buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut buf);

        VersionFieldsEncoder::encode_error_and_session(&mut encoder, 0, 42, 6);

        // v6 doesn't include error/session
        assert_eq!(buf.len(), 0);
    }

    #[test]
    fn test_encode_error_and_session_v7() {
        let mut buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut buf);

        VersionFieldsEncoder::encode_error_and_session(&mut encoder, 0, 42, 7);

        // v7+ includes error (2 bytes) + session (4 bytes) = 6 bytes
        assert_eq!(buf.len(), 6);
    }
}
