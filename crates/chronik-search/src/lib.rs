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

pub use api::SearchApi;
pub use indexer::{TantivyIndexer, IndexerConfig, SearchResult};
pub use integration::SearchIntegration;

/// Helper function to serve the search API app
pub async fn serve_app(
    listener: tokio::net::TcpListener,
    app: axum::Router,
) -> Result<(), std::io::Error> {
    // Convert tokio TcpListener to std::net::TcpListener for hyper
    // CRITICAL: Do NOT bind again - listener is already bound!
    let std_listener = listener.into_std()?;
    std_listener.set_nonblocking(true)?;

    // In axum 0.6, use from_tcp to reuse the existing listener
    axum::Server::from_tcp(std_listener)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?
        .serve(app.into_make_service())
        .await
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
}