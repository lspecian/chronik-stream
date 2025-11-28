//! Partition Encoding
//!
//! Handles encoding of individual partition responses in Fetch API.
//! Complexity: < 25 per function

use crate::parser::Encoder;
use crate::types::FetchResponsePartition;
use super::{VersionFieldsEncoder, RecordsEncoder, TaggedFieldsEncoder};
use tracing::trace;

/// Encoder for partition-level Fetch response data
pub struct PartitionEncoder;

impl PartitionEncoder {
    /// Encode a single partition response
    ///
    /// Complexity: < 25 (orchestrates version-specific encoding)
    pub fn encode_partition(
        encoder: &mut Encoder,
        partition: &FetchResponsePartition,
        version: i16,
        flexible: bool,
    ) {
        // Basic partition metadata (all versions)
        encoder.write_i32(partition.partition);
        encoder.write_i16(partition.error_code);
        encoder.write_i64(partition.high_watermark);

        trace!(
            "      Partition {}: error={}, hw={}",
            partition.partition,
            partition.error_code,
            partition.high_watermark
        );

        // Version-specific metadata (v4+)
        if version >= 4 {
            Self::encode_partition_metadata(encoder, partition, version);
        }

        // Preferred read replica (v11+)
        VersionFieldsEncoder::encode_preferred_read_replica(
            encoder,
            partition.preferred_read_replica,
            version,
        );

        // Records
        RecordsEncoder::encode_records(encoder, &partition.records, flexible);

        // Tagged fields for partition (v12+)
        TaggedFieldsEncoder::encode_partition_tagged_fields(encoder, flexible);
    }

    /// Encode version-specific partition metadata (v4+)
    ///
    /// Complexity: < 20 (version checks and metadata encoding)
    fn encode_partition_metadata(
        encoder: &mut Encoder,
        partition: &FetchResponsePartition,
        version: i16,
    ) {
        // Last stable offset (v4+)
        VersionFieldsEncoder::encode_last_stable_offset(
            encoder,
            partition.last_stable_offset,
            version,
        );

        // Log start offset (v5+)
        if version >= 5 {
            VersionFieldsEncoder::encode_log_start_offset(
                encoder,
                partition.log_start_offset,
                version,
            );
            trace!(
                "        lso={}, log_start={}",
                partition.last_stable_offset,
                partition.log_start_offset
            );
        }

        // Aborted transactions (v4+)
        let flexible = version >= 12;
        RecordsEncoder::encode_aborted_transactions(
            encoder,
            partition.aborted.as_ref(),
            version,
            flexible,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::BytesMut;

    fn create_test_partition() -> FetchResponsePartition {
        FetchResponsePartition {
            partition: 0,
            error_code: 0,
            high_watermark: 100,
            last_stable_offset: 95,
            log_start_offset: 0,
            preferred_read_replica: -1,
            records: vec![],
            aborted: None,
        }
    }

    #[test]
    fn test_encode_partition_v0() {
        let mut buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut buf);
        let partition = create_test_partition();

        PartitionEncoder::encode_partition(&mut encoder, &partition, 0, false);

        // v0: partition(4) + error(2) + hw(8) + records_length(4) = 18 bytes minimum
        assert!(buf.len() >= 18);
    }

    #[test]
    fn test_encode_partition_v4() {
        let mut buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut buf);
        let partition = create_test_partition();

        PartitionEncoder::encode_partition(&mut encoder, &partition, 4, false);

        // v4 adds: last_stable_offset(8) + aborted_txn_length(4) = +12 bytes
        assert!(buf.len() >= 30);
    }

    #[test]
    fn test_encode_partition_v12_flexible() {
        let mut buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut buf);
        let partition = create_test_partition();

        PartitionEncoder::encode_partition(&mut encoder, &partition, 12, true);

        // v12 uses varints and tagged fields
        assert!(buf.len() > 0);
    }
}
