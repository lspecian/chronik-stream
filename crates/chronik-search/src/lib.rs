//! Search node implementation for Chronik Stream.

pub mod api;
pub mod handlers;
pub mod indexer;
pub mod integration;
pub mod aggregations;
pub mod cache;
pub mod client;
pub mod geo;
pub mod hot_text_index;
pub mod cold_cache;
pub mod ranking;
pub mod corpus_stats;
pub mod sort;
pub mod filtered_query;
pub mod prefilter;
pub mod legacy_index;
pub mod topic_aggs;
pub mod result_cache;

pub use api::SearchApi;
pub use indexer::{TantivyIndexer, IndexerConfig, SearchResult};
pub use integration::SearchIntegration;

/// Helper function to serve the search API app
pub async fn serve_app(
    listener: tokio::net::TcpListener,
    app: axum::Router,
) -> Result<(), std::io::Error> {
    // Serves on the listener already bound by the caller.
    axum::serve(listener, app).await
}