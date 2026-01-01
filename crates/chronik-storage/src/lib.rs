//! Storage layer for Chronik Stream.

pub mod canonical_record;
pub mod tantivy_segment;
pub mod wal_indexer;
pub mod segment_index;
pub mod segment_compaction;
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
// v2.2.7 Phase 5: Deleted metadata_wal_adapter.rs (old WAL adapter, replaced by Raft)

pub use canonical_record::{
    CanonicalRecord, CanonicalRecordEntry, RecordHeader as CanonicalRecordHeader,
    CompressionType as CanonicalCompressionType, TimestampType,
};
pub use tantivy_segment::{
    TantivySegmentWriter, TantivySegmentReader, SegmentMetadata as TantivySegmentMetadata,
    SchemaFields as TantivySchemaFields,
};
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
    FusionMethod, HybridSearchResult, HnswIndex, RealHnswIndex, HnswConfig, HnswStats,
    VectorIndexFactory, ScoreFusion
};
pub use extended_segment::{
    ExtendedTansuSegment, ExtendedSegmentMetadata, ExtendedSegmentBuilder,
    VectorFieldConfig, HybridStats, SegmentMigration
};
// v2.2.7 Phase 5: Removed WalMetadataAdapter export (deleted file)
pub use wal_indexer::{
    WalIndexer, WalIndexerConfig, IndexingStats, TopicPartition as WalIndexerTopicPartition,
};
pub use segment_index::{
    SegmentIndex, SegmentMetadata as SegmentIndexMetadata, TopicPartition as SegmentIndexTopicPartition, IndexStats,
    ParquetSegmentMetadata, ParquetIndexStats, CombinedIndexStats,
};
pub use segment_compaction::{
    SegmentCompactor, CompactionConfig, CompactionStrategy, CompactionStats,
};