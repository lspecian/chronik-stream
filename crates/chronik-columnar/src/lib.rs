//! Chronik Columnar Storage
//!
//! This crate provides optional per-topic columnar storage using Apache Arrow and Parquet,
//! enabling SQL queries via DataFusion and high-performance analytics.
//!
//! ## Features
//!
//! - **Per-topic opt-in**: Enable columnar storage via topic configuration
//! - **Arrow/Parquet format**: Industry-standard columnar format
//! - **SQL queries**: Full SQL support via DataFusion
//! - **Predicate pushdown**: Efficient filtering at storage level
//! - **Arrow Flight**: High-performance streaming (optional feature)
//!
//! ## Topic Configuration
//!
//! Enable columnar storage for a topic:
//!
//! ```bash
//! kafka-topics.sh --create --topic orders \
//!   --config columnar.enabled=true \
//!   --config columnar.format=parquet \
//!   --config columnar.compression=zstd
//! ```
//!
//! ## Architecture
//!
//! Columnar processing happens in the background WalIndexer pipeline,
//! NOT during produce. This ensures zero impact on produce performance.
//!
//! ```text
//! Producer → WAL (fsync) → Response
//!                │
//!                ▼ (background, async)
//!           WalIndexer → Parquet files → Object Store
//! ```

pub mod config;
pub mod schema;
pub mod converter;
pub mod writer;
pub mod reader;
pub mod query_engine;
pub mod udfs;
pub mod topic_query;
pub mod vector_index;
pub mod hot_buffer;

#[cfg(feature = "flight")]
pub mod flight;

// Re-exports
pub use config::{ColumnarConfig, CompressionCodec, PartitioningStrategy};
pub use schema::{kafka_message_schema, KafkaSchemaBuilder};
pub use writer::ParquetSegmentWriter;
pub use reader::ParquetSegmentReader;
pub use query_engine::{ColumnarQueryEngine, QueryEngineConfig, QueryResult};
pub use topic_query::{TopicQueryService, QueryFilters, SegmentIndexProvider};
pub use vector_index::{
    VectorIndexManager, VectorIndexConfig, HnswIndexConfig, DistanceMetric,
    VectorEntry, EmbeddingPipeline, ProcessingStats, TopicIndexStats, PartitionIndexStats,
    VectorSearchService, VectorSearchResult, VectorSearchFilters,
};
pub use hot_buffer::{HotDataBuffer, HotBufferConfig, HotBufferStats};

// v2.2.22: Re-export datafusion for unified API
pub use datafusion;

// v2.2.22: Flight types (when feature enabled)
#[cfg(feature = "flight")]
pub use flight::{
    ChronikFlightService, FlightServiceConfig, TicketType, TicketParseError,
    vector_results_to_batch, vector_search_schema,
};
