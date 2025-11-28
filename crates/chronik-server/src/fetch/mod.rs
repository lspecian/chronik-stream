//! Fetch handler module
//!
//! Extracted from `fetch_from_segment()` to reduce complexity from 198 to <25 per function.
//! Handles 3-tier storage fetch logic with version-specific segment format support.
//!
//! **Module Structure:**
//! - `segment_reader` - Segment retrieval and parsing from object store
//! - `indexed_decoder` - Indexed records decoder (v2/v3 format)
//! - `kafka_decoder` - Raw Kafka batch decoder (CRC-preserving)
//! - `record_filter` - Offset range and max_bytes filtering
//!
//! **Supported Formats:**
//! - Segment v2: Indexed records without length prefixes
//! - Segment v3+: Indexed records with u32 length prefixes
//! - Raw Kafka batches: CRC-preserving original format
//!
//! **Refactoring Goal:**
//! Extract 298-line monolithic `fetch_from_segment()` into focused, testable modules.

pub mod segment_reader;
pub mod indexed_decoder;
pub mod kafka_decoder;
pub mod record_filter;

// Re-export key types for convenience
pub use segment_reader::SegmentReader;
pub use indexed_decoder::IndexedRecordDecoder;
pub use kafka_decoder::RawKafkaBatchDecoder;
pub use record_filter::RecordFilter;
