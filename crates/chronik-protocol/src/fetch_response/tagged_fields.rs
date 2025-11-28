//! Tagged Fields Encoding
//!
//! Handles encoding of tagged fields for flexible Kafka protocol versions (v12+).
//! Complexity: < 10 per function

use crate::parser::Encoder;

/// Encoder for tagged fields in flexible protocol versions
pub struct TaggedFieldsEncoder;

impl TaggedFieldsEncoder {
    /// Encode empty tagged fields (v12+)
    ///
    /// Complexity: < 5 (single varint write with version check)
    ///
    /// Tagged fields allow for backward-compatible protocol extensions.
    /// Empty tagged fields are encoded as varint 0.
    pub fn encode_empty_tagged_fields(encoder: &mut Encoder, flexible: bool) {
        if flexible {
            encoder.write_unsigned_varint(0);
        }
    }

    /// Encode tagged fields at partition level
    ///
    /// Complexity: < 5
    pub fn encode_partition_tagged_fields(encoder: &mut Encoder, flexible: bool) {
        Self::encode_empty_tagged_fields(encoder, flexible);
    }

    /// Encode tagged fields at topic level
    ///
    /// Complexity: < 5
    pub fn encode_topic_tagged_fields(encoder: &mut Encoder, flexible: bool) {
        Self::encode_empty_tagged_fields(encoder, flexible);
    }

    /// Encode tagged fields at response level
    ///
    /// Complexity: < 5
    pub fn encode_response_tagged_fields(encoder: &mut Encoder, flexible: bool) {
        Self::encode_empty_tagged_fields(encoder, flexible);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::BytesMut;

    #[test]
    fn test_encode_tagged_fields_non_flexible() {
        let mut buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut buf);

        TaggedFieldsEncoder::encode_empty_tagged_fields(&mut encoder, false);

        // Non-flexible versions don't encode tagged fields
        assert_eq!(buf.len(), 0);
    }

    #[test]
    fn test_encode_tagged_fields_flexible() {
        let mut buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut buf);

        TaggedFieldsEncoder::encode_empty_tagged_fields(&mut encoder, true);

        // Flexible versions encode empty tagged fields as varint 0 (1 byte)
        assert_eq!(buf.len(), 1);
        assert_eq!(buf[0], 0);
    }

    #[test]
    fn test_encode_all_levels() {
        let mut buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut buf);

        TaggedFieldsEncoder::encode_partition_tagged_fields(&mut encoder, true);
        TaggedFieldsEncoder::encode_topic_tagged_fields(&mut encoder, true);
        TaggedFieldsEncoder::encode_response_tagged_fields(&mut encoder, true);

        // 3 levels × 1 byte each = 3 bytes
        assert_eq!(buf.len(), 3);
    }
}
