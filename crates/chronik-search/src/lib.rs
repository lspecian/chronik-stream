//! Search node implementation for Chronik Stream.

pub mod api;
pub mod handlers;
pub mod indexer;
pub mod integration;
pub mod realtime_indexer;
pub mod json_pipeline;

pub use api::SearchApi;
pub use indexer::{TantivyIndexer, IndexerConfig, SearchResult};
pub use integration::SearchIntegration;
pub use realtime_indexer::{
    RealtimeIndexer, RealtimeIndexerConfig, JsonDocument,
    FieldIndexingPolicy, FieldTypeHint, IndexingMetricsSnapshot
};
pub use json_pipeline::{JsonPipeline, JsonPipelineConfig, JsonPipelineBuilder};