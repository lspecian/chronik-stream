//! Segment retrieval and parsing
//!
//! Extracted from `fetch_from_segment()` to reduce complexity.
//! Handles segment retrieval from object store and parsing.

use chronik_common::Result;
use chronik_storage::{Segment, ObjectStoreTrait};
use std::sync::Arc;

/// Segment metadata information
#[derive(Debug, Clone)]
pub struct SegmentInfo {
    pub segment_id: String,
    pub object_key: String,
}

/// Segment reader
///
/// Retrieves and parses segments from object store.
pub struct SegmentReader;

impl SegmentReader {
    /// Read and parse segment from object store
    ///
    /// Complexity: < 10 (simple retrieval + parsing)
    pub async fn read_segment(
        object_store: &Arc<dyn ObjectStoreTrait>,
        segment_info: &SegmentInfo,
    ) -> Result<Segment> {
        tracing::info!("Fetching segment {} with key: {}",
            segment_info.segment_id, segment_info.object_key);

        // Debug: Log the full path being accessed
        tracing::warn!("SEGMENT→READ: Attempting to read segment from object store with key: {}",
            segment_info.object_key);

        // Read segment from storage using the correct Segment format (CHRN magic bytes)
        let segment_data = match object_store.get(&segment_info.object_key).await {
            Ok(data) => {
                tracing::info!("SEGMENT→READ: Successfully read segment {} ({} bytes)",
                    segment_info.object_key, data.len());
                data
            }
            Err(e) => {
                tracing::error!("SEGMENT→READ: Failed to read segment {}: {:?}",
                    segment_info.object_key, e);

                // Try to understand where the file should be
                tracing::error!("SEGMENT→DEBUG: The segment file was expected at key: {}",
                    segment_info.object_key);
                tracing::error!("SEGMENT→DEBUG: This typically maps to: ./data/segments/{}",
                    segment_info.object_key);

                return Err(e.into());
            }
        };

        // Parse using the Segment format - this is what SegmentWriter creates
        let segment = Segment::deserialize(segment_data)?;

        tracing::trace!("Successfully parsed segment v{} with {} records, raw_kafka: {} bytes, indexed: {} bytes",
            segment.header.version,
            segment.metadata.record_count,
            segment.raw_kafka_batches.len(),
            segment.indexed_records.len());

        Ok(segment)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_segment_info_creation() {
        let info = SegmentInfo {
            segment_id: "test-segment".to_string(),
            object_key: "segments/topic/partition/test-segment".to_string(),
        };

        assert_eq!(info.segment_id, "test-segment");
        assert!(info.object_key.contains("test-segment"));
    }
}
