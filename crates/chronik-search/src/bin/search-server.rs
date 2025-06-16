//! Search server binary for Chronik Stream.

use chronik_search::{SearchApi, SearchIntegration, IndexerConfig};
use std::sync::Arc;
use tokio::net::TcpListener;
use tower_http::trace::TraceLayer;
use tracing::{info, error};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Debug, Clone)]
struct ServerConfig {
    /// Server address to bind to
    pub addr: String,
    /// Indexer configuration
    pub indexer: IndexerConfig,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            addr: "0.0.0.0:9200".to_string(),
            indexer: IndexerConfig::default(),
        }
    }
}

#[tokio::main]
async fn main() {
    // Initialize tracing
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "search_server=debug,chronik_search=debug,tower_http=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    // Load configuration (in production, this would come from CLI args or config file)
    let config = ServerConfig::default();
    
    info!("Starting Chronik Stream Search Server");
    info!("Listening on: {}", config.addr);

    // Create search API
    let api = match SearchApi::new() {
        Ok(api) => Arc::new(api),
        Err(e) => {
            error!("Failed to create search API: {}", e);
            return;
        }
    };

    // Create search integration
    let mut integration = match SearchIntegration::new(api.clone()).await {
        Ok(integration) => integration,
        Err(e) => {
            error!("Failed to create search integration: {}", e);
            return;
        }
    };

    // Initialize Kafka indexer
    if let Err(e) = integration.init_kafka_indexer(config.indexer).await {
        error!("Failed to initialize Kafka indexer: {}", e);
        // Continue anyway - the API can still work without the Kafka indexer
    }

    // Create router
    let app = api.router()
        .layer(TraceLayer::new_for_http());

    // Create TCP listener
    let listener = match TcpListener::bind(&config.addr).await {
        Ok(listener) => listener,
        Err(e) => {
            error!("Failed to bind to {}: {}", config.addr, e);
            return;
        }
    };

    info!("Search server ready to accept connections");
    info!("API endpoints:");
    info!("  Health: GET /health");
    info!("  Metrics: GET /metrics");
    info!("  Search all: GET|POST /_search");
    info!("  Search index: GET|POST /{{index}}/_search");
    info!("  Index document: POST|PUT /{{index}}/_doc/{{id}}");
    info!("  Get document: GET /{{index}}/_doc/{{id}}");
    info!("  Delete document: DELETE /{{index}}/_doc/{{id}}");
    info!("  Create index: PUT /{{index}}");
    info!("  Delete index: DELETE /{{index}}");
    info!("  Get mapping: GET /{{index}}/_mapping");
    info!("  List indices: GET /_cat/indices");

    // Run server
    if let Err(e) = axum::serve(listener, app).await {
        error!("Server error: {}", e);
    }
}