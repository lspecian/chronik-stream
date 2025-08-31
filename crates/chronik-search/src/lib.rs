//! Search node implementation for Chronik Stream.

pub mod api;
pub mod handlers;
pub mod indexer;
pub mod integration;
pub mod realtime_indexer;
pub mod json_pipeline;
pub mod aggregations;
pub mod cache;
pub mod client;
pub mod geo;

pub use api::SearchApi;
pub use indexer::{TantivyIndexer, IndexerConfig, SearchResult};
pub use integration::SearchIntegration;
pub use realtime_indexer::{
    RealtimeIndexer, RealtimeIndexerConfig, JsonDocument,
    FieldIndexingPolicy, FieldTypeHint, IndexingMetricsSnapshot
};
pub use json_pipeline::{JsonPipeline, JsonPipelineConfig, JsonPipelineBuilder};

/// Helper function to serve the search API app
pub async fn serve_app(
    listener: tokio::net::TcpListener,
    app: axum::Router,
) -> Result<(), std::io::Error> {
    axum::serve(listener, app).await
}