//! Indexed records decoder
//!
//! Extracted from `fetch_from_segment()` to reduce complexity.
//! Handles v2/v3 indexed records format with offset adjustment.

use chronik_common::Result;
use chronik_protocol::records::RecordBatch;
use chronik_storage::{Record, Segment};

/// Indexed record decoder
///
/// Decodes indexed records from segments with v2/v3 format support.
pub struct IndexedRecordDecoder;

impl IndexedRecordDecoder {
    /// Convert protocol Record to storage Record
    fn convert_record(protocol_record: chronik_protocol::records::Record) -> Record {
        Record {
            offset: protocol_record.offset,
            timestamp: protocol_record.timestamp,
            key: protocol_record.key.map(|b| b.to_vec()),
            value: protocol_record.value.map(|b| b.to_vec()).unwrap_or_default(),
            headers: protocol_record.headers.into_iter()
                .map(|h| (h.key, h.value.map(|v| v.to_vec()).unwrap_or_default()))
                .collect(),
        }
    }

    /// Decode indexed records from segment
    ///
    /// Complexity: < 25 (multi-batch decode with version-specific logic)
    pub fn decode_indexed_records(segment: &Segment) -> Result<Vec<Record>> {
        let mut all_records: Vec<Record> = Vec::new();
        let mut batch_count = 0;
        let mut cursor_pos = 0;
        let total_len = segment.indexed_records.len();

        // Check segment version to determine format
        let is_v3_format = segment.header.version >= 3;

        tracing::info!(
            "SEGMENT→DECODE: Starting multi-batch decode from indexed_records ({} bytes, v{} format)",
            total_len, segment.header.version
        );

        while cursor_pos < total_len {
            // v3 format: Read length prefix first
            let batch_data_start = if is_v3_format {
                // Need at least 4 bytes for length prefix
                if cursor_pos + 4 > total_len {
                    tracing::info!("SEGMENT→DECODE: Not enough bytes for length prefix at position {}", cursor_pos);
                    break;
                }

                // Read u32 length prefix (big-endian)
                let batch_len = u32::from_be_bytes([
                    segment.indexed_records[cursor_pos],
                    segment.indexed_records[cursor_pos + 1],
                    segment.indexed_records[cursor_pos + 2],
                    segment.indexed_records[cursor_pos + 3],
                ]) as usize;

                tracing::debug!("SEGMENT→V3: Batch {} has length prefix {} bytes", batch_count + 1, batch_len);

                // Move cursor past length prefix
                cursor_pos + 4
            } else {
                // v2 format: No length prefix, try to decode from current position
                cursor_pos
            };

            // Calculate end position for this batch
            let batch_data_end = if is_v3_format {
                let batch_len = u32::from_be_bytes([
                    segment.indexed_records[cursor_pos],
                    segment.indexed_records[cursor_pos + 1],
                    segment.indexed_records[cursor_pos + 2],
                    segment.indexed_records[cursor_pos + 3],
                ]) as usize;
                batch_data_start + batch_len
            } else {
                total_len  // For v2, decode will determine the end
            };

            // Ensure we have enough data
            if batch_data_end > total_len {
                tracing::error!(
                    "SEGMENT→ERROR: Batch extends beyond segment bounds (pos={}, end={}, total={})",
                    batch_data_start, batch_data_end, total_len
                );
                break;
            }

            // Decode the batch
            use bytes::Bytes;
            let batch_data = Bytes::copy_from_slice(&segment.indexed_records[batch_data_start..batch_data_end]);

            match RecordBatch::decode(batch_data) {
                Ok(batch) => {
                    let batch_records = batch.records.len();
                    let bytes_in_batch = batch_data_end - batch_data_start;

                    tracing::info!(
                        "SEGMENT→BATCH {}: Decoded {} records, {} bytes at position {}",
                        batch_count + 1,
                        batch_records,
                        bytes_in_batch,
                        cursor_pos
                    );

                    // Convert protocol records to storage records
                    all_records.extend(batch.records.into_iter().map(Self::convert_record));

                    // Advance cursor to next batch
                    cursor_pos = batch_data_end;
                    batch_count += 1;
                }
                Err(e) => {
                    if is_v3_format {
                        // v3: Length prefix told us exact size, decode should not fail
                        tracing::error!(
                            "SEGMENT→ERROR: Failed to decode v3 batch {} at position {}: {}",
                            batch_count + 1, cursor_pos, e
                        );
                        break;
                    } else {
                        // v2: Expected to fail after last batch (no length prefix to know when to stop)
                        tracing::info!(
                            "SEGMENT→DECODE: Finished v2 format at position {} ({} bytes remaining): {}",
                            cursor_pos,
                            total_len - cursor_pos,
                            e
                        );
                        break;
                    }
                }
            }
        }

        tracing::info!(
            "SEGMENT→COMPLETE: Decoded {} batches with {} total records from indexed_records",
            batch_count,
            all_records.len()
        );

        // Adjust offsets if needed
        Self::adjust_record_offsets(&mut all_records[..], segment.metadata.base_offset);

        Ok(all_records)
    }

    /// Adjust record offsets from relative to absolute
    ///
    /// Complexity: < 10 (simple offset adjustment)
    fn adjust_record_offsets(records: &mut [Record], base_offset: i64) {
        if base_offset > 0 && !records.is_empty() {
            // Check if offsets need adjustment (if first record offset is small, likely relative)
            let first_offset = records[0].offset;
            if first_offset < base_offset {
                tracing::info!(
                    "SEGMENT→ADJUST: Adjusting {} record offsets by base_offset={} (first record offset was {})",
                    records.len(),
                    base_offset,
                    first_offset
                );
                for record in records.iter_mut() {
                    record.offset += base_offset;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_adjust_record_offsets_when_needed() {
        let mut records = vec![
            Record {
                offset: 0,
                timestamp: 0,
                key: None,
                value: vec![],
                headers: vec![],
            },
            Record {
                offset: 1,
                timestamp: 0,
                key: None,
                value: vec![],
                headers: vec![],
            },
        ];

        IndexedRecordDecoder::adjust_record_offsets(&mut records, 100);

        assert_eq!(records[0].offset, 100);
        assert_eq!(records[1].offset, 101);
    }

    #[test]
    fn test_adjust_record_offsets_when_not_needed() {
        let mut records = vec![
            Record {
                offset: 100,
                timestamp: 0,
                key: None,
                value: vec![],
                headers: vec![],
            },
        ];

        IndexedRecordDecoder::adjust_record_offsets(&mut records, 100);

        assert_eq!(records[0].offset, 100); // Should not change
    }
}
