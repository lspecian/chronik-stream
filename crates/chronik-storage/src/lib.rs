//! Storage layer for Chronik Stream.

pub mod index;
pub mod object_store;
pub mod record_batch;
pub mod kafka_records;
pub mod segment;
pub mod segment_writer;
pub mod segment_reader;
pub mod chronik_segment;
pub mod chronik_segment_example;
pub mod checksum;
pub mod segment_checksum;
pub mod segment_cache;
pub mod optimized_segment;
pub mod vector_search;
pub mod extended_segment;
pub mod metadata_wal_adapter;

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
pub use checksum::{ChecksumUtils, ChecksumMode, StreamingChecksum};
pub use segment_checksum::{SegmentChecksumCalculator, SegmentChecksumVerifier, BatchChecksumVerifier};
pub use segment_cache::{SegmentCache, SegmentCacheConfig, CachedSegmentReader, CacheStats, EvictionPolicy};
pub use optimized_segment::{
    OptimizedTansuSegment, OptimizedSegmentHeader, ColumnarBatch, SegmentStats,
    CompressionType as OptimizedCompressionType
};
pub use vector_search::{
    VectorIndex, VectorIndexData, DistanceMetric, HybridSearchRequest, VectorQuery,
    FusionMethod, HybridSearchResult, HnswIndex, VectorIndexFactory, ScoreFusion
};
pub use extended_segment::{
    ExtendedTansuSegment, ExtendedSegmentMetadata, ExtendedSegmentBuilder,
    VectorFieldConfig, HybridStats, SegmentMigration
};
pub use metadata_wal_adapter::WalMetadataAdapter;