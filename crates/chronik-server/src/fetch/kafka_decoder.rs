//! Raw Kafka batch decoder
//!
//! Extracted from `fetch_from_segment()` to reduce complexity.
//! Handles raw Kafka batch decoding with CRC preservation.

use chronik_common::Result;
use chronik_storage::kafka_records::KafkaRecordBatch;
use chronik_storage::{Record, Segment};

/// Raw Kafka batch decoder
///
/// Decodes raw Kafka batches from segments (CRC-preserving format).
pub struct RawKafkaBatchDecoder;

impl RawKafkaBatchDecoder {
    /// Decode raw Kafka batches from segment
    ///
    /// Complexity: < 20 (multi-batch decode with offset tracking)
    pub fn decode_raw_kafka_batches(segment: &Segment) -> Result<Vec<Record>> {
        if segment.raw_kafka_batches.is_empty() {
            return Ok(vec![]);
        }

        // ALWAYS prefer raw Kafka batches when available (preserves CRC from original produce request)
        // Dual storage (v2) has both indexed + raw, but raw has correct CRC
        tracing::info!("Using raw Kafka batches for fetch (CRC-preserving format, {} bytes)",
            segment.raw_kafka_batches.len());

        let mut all_records = Vec::new();
        let mut cursor_pos = 0;
        let total_len = segment.raw_kafka_batches.len();
        let mut batch_count = 0;
        let mut current_absolute_offset = segment.metadata.base_offset;

        tracing::info!(
            "SEGMENT→KAFKA: Starting multi-batch decode from raw_kafka_batches ({} bytes, base_offset={})",
            total_len,
            current_absolute_offset
        );

        while cursor_pos < total_len {
            // Kafka batches are self-describing - decode will use batch_length from header
            match KafkaRecordBatch::decode(&segment.raw_kafka_batches[cursor_pos..]) {
                Ok((kafka_batch, bytes_consumed)) => {
                    // CRITICAL: Use segment metadata's base_offset, NOT client's base_offset from Kafka batch header
                    // The client's base_offset in the Kafka batch header is often incorrect (e.g., 1 instead of 0)
                    // We must use the segment's actual offsets which are tracked server-side
                    let records: Vec<Record> = kafka_batch.records.into_iter().enumerate().map(|(i, kr)| {
                        Record {
                            offset: current_absolute_offset + i as i64,
                            timestamp: kafka_batch.header.base_timestamp + kr.timestamp_delta,
                            key: kr.key.map(|k| k.to_vec()),
                            value: kr.value.map(|v| v.to_vec()).unwrap_or_default(),
                            headers: kr.headers.into_iter().map(|h| {
                                (h.key, h.value.map(|v| v.to_vec()).unwrap_or_default())
                            }).collect(),
                        }
                    }).collect();

                    let batch_records = records.len();
                    tracing::debug!(
                        "Decoded batch {}: {} records (offsets {}-{})",
                        batch_count + 1,
                        batch_records,
                        current_absolute_offset,
                        current_absolute_offset + batch_records as i64 - 1
                    );

                    // Increment absolute offset for next batch
                    current_absolute_offset += batch_records as i64;

                    all_records.extend(records);

                    // Advance cursor using bytes_consumed from Kafka decode
                    cursor_pos += bytes_consumed;

                    // Safety check
                    if bytes_consumed == 0 {
                        tracing::error!("SEGMENT→KAFKA: Zero bytes consumed, breaking to avoid infinite loop");
                        break;
                    }

                    batch_count += 1;
                }
                Err(e) => {
                    // Expected to fail when we run out of complete batches
                    tracing::info!(
                        "SEGMENT→KAFKA: Finished decoding at position {} ({} bytes remaining): {}",
                        cursor_pos,
                        total_len - cursor_pos,
                        e
                    );
                    break;
                }
            }
        }

        tracing::info!(
            "SEGMENT→KAFKA: Decoded {} batches with {} total records from raw_kafka_batches",
            batch_count,
            all_records.len()
        );

        Ok(all_records)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chronik_storage::SegmentMetadata;

    #[test]
    fn test_empty_raw_batches_returns_empty() {
        let segment = Segment {
            header: chronik_storage::SegmentHeader {
                magic: *b"CHRN",
                version: 3,
                checksum: 0,
            },
            metadata: SegmentMetadata {
                base_offset: 0,
                last_offset: 0,
                record_count: 0,
                timestamp: 0,
            },
            indexed_records: vec![],
            raw_kafka_batches: vec![],
        };

        let result = RawKafkaBatchDecoder::decode_raw_kafka_batches(&segment);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().len(), 0);
    }
}
