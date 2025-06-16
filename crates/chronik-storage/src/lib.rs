//! Storage layer for Chronik Stream.

pub mod index;
pub mod object_store;
pub mod record_batch;
pub mod segment;
pub mod segment_writer;
pub mod segment_reader;
pub mod chronik_segment;
pub mod chronik_segment_example;

pub use index::{IndexBuilder, Document, FieldType, FieldValue, SegmentSearcher, SearchHit};
pub use object_store::{
    ObjectStore as ObjectStoreTrait, ObjectStoreConfig, ObjectStoreFactory, ObjectMetadata,
    StorageBackend as ObjectStoreBackend, ChronikStorageAdapter, SegmentLocation
};
pub use record_batch::{RecordBatch, Record, RecordHeader, RecordBatchStats};
pub use segment::{Segment, SegmentBuilder};
pub use segment_writer::{SegmentWriter, SegmentWriterConfig};
pub use segment_reader::{SegmentReader, SegmentReaderConfig, FetchResult};
pub use chronik_segment::{ChronikSegment, ChronikSegmentBuilder, SegmentMetadata, CompressionType, BloomFilter};