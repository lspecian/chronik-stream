//! Record filtering
//!
//! Extracted from `fetch_from_segment()` to reduce complexity.
//! Handles offset range and max_bytes filtering.

use chronik_storage::Record;

/// Record filter
///
/// Filters records by offset range and max_bytes limits.
pub struct RecordFilter;

impl RecordFilter {
    /// Filter records by offset range and max_bytes
    ///
    /// Complexity: < 10 (simple iteration with size tracking)
    pub fn filter_by_range(
        records: Vec<Record>,
        start_offset: i64,
        end_offset: i64,
        max_bytes: i32,
    ) -> Vec<Record> {
        if records.is_empty() {
            tracing::warn!("SEGMENT→EMPTY: No records to filter");
            return vec![];
        }

        let mut filtered_records = Vec::new();
        let mut bytes_fetched = 0;

        for record in records {
            if record.offset >= start_offset && record.offset < end_offset {
                let record_size = record.value.len() +
                    record.key.as_ref().map(|k| k.len()).unwrap_or(0) + 24;

                if bytes_fetched + record_size > max_bytes as usize && !filtered_records.is_empty() {
                    break;
                }

                filtered_records.push(record);
                bytes_fetched += record_size;
            }
        }

        tracing::info!("Returning {} records after filtering for offset range {}-{}",
            filtered_records.len(), start_offset, end_offset);

        filtered_records
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_record(offset: i64, value_size: usize) -> Record {
        Record {
            offset,
            timestamp: 0,
            key: None,
            value: vec![0u8; value_size],
            headers: vec![],
        }
    }

    #[test]
    fn test_filter_by_offset_range() {
        let records = vec![
            create_test_record(0, 10),
            create_test_record(1, 10),
            create_test_record(2, 10),
            create_test_record(3, 10),
        ];

        let filtered = RecordFilter::filter_by_range(records, 1, 3, 1000);

        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[0].offset, 1);
        assert_eq!(filtered[1].offset, 2);
    }

    #[test]
    fn test_filter_by_max_bytes() {
        let records = vec![
            create_test_record(0, 100),
            create_test_record(1, 100),
            create_test_record(2, 100),
        ];

        // Each record: 100 (value) + 24 (overhead) = 124 bytes
        // max_bytes = 150 should only return 1 record
        let filtered = RecordFilter::filter_by_range(records, 0, 10, 150);

        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].offset, 0);
    }

    #[test]
    fn test_filter_empty_records() {
        let records = vec![];

        let filtered = RecordFilter::filter_by_range(records, 0, 10, 1000);

        assert_eq!(filtered.len(), 0);
    }
}
