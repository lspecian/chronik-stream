//! Unified API Module (v2.2.22 - Phase 6)
//!
//! Consolidates all HTTP APIs onto a single port (default: 6092):
//! - SQL endpoints: `/_sql`, `/_sql/explain`, `/_sql/tables`
//! - Vector search: `/_vector/:topic/search`, `/_vector/:topic/stats`
//! - Admin API: `/admin/*` (cluster management)
//! - Schema Registry: `/subjects/*`, `/schemas/*`
//! - Search API: `/_search/*` (Elasticsearch-compatible)
//!
//! This replaces the previous multi-port approach where:
//! - Search API ran on port 6092
//! - Admin API ran on port 10001+node_id
//!
//! ## Configuration
//!
//! Set the port via environment variable:
//! ```bash
//! CHRONIK_UNIFIED_API_PORT=6092  # default
//! ```

pub mod sql_handler;
pub mod vector_handler;
pub mod admin_handler;
#[cfg(feature = "search")]
pub mod search_handler;

use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{debug, info};

use chronik_columnar::{
    ColumnarQueryEngine, HotDataBuffer, QueryEngineConfig, VectorIndexManager, VectorSearchService,
};
use chronik_common::metadata::traits::MetadataStore;
use chronik_embeddings::EmbeddingProvider;

pub use sql_handler::{SqlHandler, SqlRequest, SqlResponse};
pub use vector_handler::{
    VectorHandler, VectorSearchRequest, VectorSearchResponse,
    HybridSearchRequest, HybridSearchResponse,
};
pub use admin_handler::{create_admin_state, admin_router, create_admin_router_for_unified_api};

/// Unified API configuration
#[derive(Debug, Clone)]
pub struct UnifiedApiConfig {
    /// Port to listen on (default: 6092)
    pub port: u16,
    /// Bind address (default: 0.0.0.0)
    pub bind_addr: String,
    /// Enable SQL endpoints
    pub enable_sql: bool,
    /// Enable vector search endpoints
    pub enable_vector: bool,
    /// Enable admin endpoints
    pub enable_admin: bool,
    /// Enable search endpoints (Elasticsearch-compatible)
    pub enable_search: bool,
    /// Maximum query timeout in seconds
    pub query_timeout_secs: u64,
}

impl Default for UnifiedApiConfig {
    fn default() -> Self {
        Self {
            port: 6092,
            bind_addr: "0.0.0.0".to_string(),
            enable_sql: true,
            enable_vector: true,
            enable_admin: true,
            enable_search: true,
            query_timeout_secs: 30,
        }
    }
}

/// v2.2.23: Configuration for hot buffer (in-memory SQL queries)
#[derive(Debug, Clone)]
pub struct HotBufferConfig {
    /// Whether hot buffer is enabled
    pub enabled: bool,
    /// Maximum records to include in hot buffer per partition
    pub max_records_per_partition: usize,
    /// Refresh interval in milliseconds
    pub refresh_interval_ms: u64,
}

impl Default for HotBufferConfig {
    fn default() -> Self {
        Self {
            enabled: std::env::var("CHRONIK_HOT_BUFFER_ENABLED")
                .map(|v| v.to_lowercase() == "true")
                .unwrap_or(true),  // Enabled by default
            max_records_per_partition: std::env::var("CHRONIK_HOT_BUFFER_MAX_RECORDS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(100_000),
            refresh_interval_ms: std::env::var("CHRONIK_HOT_BUFFER_REFRESH_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(1000),
        }
    }
}

/// Shared state for the Unified API
#[derive(Clone)]
pub struct UnifiedApiState {
    /// SQL query engine
    pub query_engine: Option<Arc<ColumnarQueryEngine>>,
    /// Vector index manager
    pub vector_index_manager: Option<Arc<VectorIndexManager>>,
    /// Embedding provider for query embedding
    pub embedding_provider: Option<Arc<dyn EmbeddingProvider>>,
    /// Metadata store
    pub metadata_store: Arc<dyn MetadataStore>,
    /// Configuration
    pub config: UnifiedApiConfig,
    /// v2.2.23: WAL manager for hot buffer (in-memory SQL queries)
    /// Enables <1s query latency by reading recent records from WAL
    pub wal_manager: Option<Arc<chronik_wal::WalManager>>,
    /// v2.2.23: Hot buffer configuration
    pub hot_buffer_config: HotBufferConfig,
    /// v2.2.23: Hot data buffer for sub-second SQL queries
    pub hot_buffer: Option<Arc<HotDataBuffer>>,
}

impl UnifiedApiState {
    /// Create a new unified API state
    pub fn new(
        metadata_store: Arc<dyn MetadataStore>,
        config: UnifiedApiConfig,
    ) -> Self {
        Self {
            query_engine: None,
            vector_index_manager: None,
            embedding_provider: None,
            metadata_store,
            config,
            wal_manager: None,
            hot_buffer_config: HotBufferConfig::default(),
            hot_buffer: None,
        }
    }

    /// Set the query engine
    pub fn with_query_engine(mut self, engine: Arc<ColumnarQueryEngine>) -> Self {
        self.query_engine = Some(engine);
        self
    }

    /// Set the vector index manager
    pub fn with_vector_index_manager(mut self, manager: Arc<VectorIndexManager>) -> Self {
        self.vector_index_manager = Some(manager);
        self
    }

    /// Set the embedding provider
    pub fn with_embedding_provider(mut self, provider: Arc<dyn EmbeddingProvider>) -> Self {
        self.embedding_provider = Some(provider);
        self
    }

    /// v2.2.23: Set the WAL manager for hot buffer queries
    ///
    /// When set, SQL queries can read recent data directly from WAL
    /// for sub-second query latency (data available before Parquet files are created).
    pub fn with_wal_manager(mut self, manager: Arc<chronik_wal::WalManager>) -> Self {
        self.wal_manager = Some(manager);
        self
    }

    /// v2.2.23: Set the hot buffer configuration
    pub fn with_hot_buffer_config(mut self, config: HotBufferConfig) -> Self {
        self.hot_buffer_config = config;
        self
    }

    /// v2.2.23: Set the hot data buffer
    ///
    /// The hot buffer enables sub-second SQL queries by reading recent data
    /// directly from WAL before it's written to Parquet files.
    pub fn with_hot_buffer(mut self, buffer: Arc<HotDataBuffer>) -> Self {
        self.hot_buffer = Some(buffer);
        self
    }
}

/// Create the unified API router
pub fn create_router(state: UnifiedApiState) -> Router {
    create_router_full(state, None, None)
}

/// Create the unified API router with optional search API integration
///
/// The search router uses its own state (Arc<SearchApi>), so it's passed
/// as a separate router that gets merged into the unified API.
pub fn create_router_with_search(
    state: UnifiedApiState,
    search_router: Option<Router>,
) -> Router {
    create_router_full(state, search_router, None)
}

/// Create the unified API router with optional search and admin API integration
///
/// This is the full-featured router creation function that supports:
/// - SQL endpoints (via UnifiedApiState)
/// - Vector search endpoints (via UnifiedApiState)
/// - Search API (Elasticsearch-compatible, via separate Router with Arc<SearchApi>)
/// - Admin API + Schema Registry (via separate Router with AdminApiState)
///
/// Each API component uses its own state type, so they're passed as separate routers
/// that get merged into the unified API.
pub fn create_router_full(
    state: UnifiedApiState,
    search_router: Option<Router>,
    admin_router: Option<Router>,
) -> Router {
    let mut router = Router::new();

    // Health check endpoint (always enabled)
    router = router.route("/health", get(health_check));

    // SQL endpoints
    if state.config.enable_sql {
        router = router
            .route("/_sql", post(sql_handler::execute_sql))
            .route("/_sql/explain", post(sql_handler::explain_sql))
            .route("/_sql/tables", get(sql_handler::list_tables))
            .route("/_sql/describe/:table", get(sql_handler::describe_table));
    }

    // Vector search endpoints
    if state.config.enable_vector {
        router = router
            .route("/_vector/:topic/search", post(vector_handler::search))
            .route("/_vector/:topic/search_by_vector", post(vector_handler::search_by_vector))
            .route("/_vector/:topic/hybrid", post(vector_handler::hybrid_search))
            .route("/_vector/:topic/stats", get(vector_handler::get_stats))
            .route("/_vector/topics", get(vector_handler::list_topics));
    }

    // Add shared state for SQL/Vector endpoints
    let mut router = router.with_state(state);

    // Merge search router if provided
    // Search endpoints: /_search, /:index/_search, /:index/_doc/:id, etc.
    if let Some(search) = search_router {
        router = router.merge(search);
    }

    // Merge admin router if provided
    // Admin endpoints: /admin/*, Schema Registry: /subjects/*, /schemas/*, /config/*
    if let Some(admin) = admin_router {
        router = router.merge(admin);
    }

    router
}

/// Health check response
#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub version: String,
    pub sql_enabled: bool,
    pub vector_enabled: bool,
    pub search_enabled: bool,
    pub admin_enabled: bool,
}

/// Health check handler
async fn health_check(State(state): State<UnifiedApiState>) -> impl IntoResponse {
    let response = HealthResponse {
        status: "ok".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sql_enabled: state.query_engine.is_some(),
        vector_enabled: state.vector_index_manager.is_some(),
        search_enabled: state.config.enable_search,
        admin_enabled: state.config.enable_admin,
    };
    Json(response)
}

/// Start the unified API HTTP server
///
/// This function spawns a background tokio task that runs the HTTP server
/// on the configured port (default: 6092).
///
/// # Arguments
/// * `metadata_store` - Metadata store for topic information
/// * `query_engine` - Optional SQL query engine
/// * `vector_index_manager` - Optional vector index manager
/// * `embedding_provider` - Optional embedding provider for text-based vector search
/// * `config` - Unified API configuration
///
/// # Returns
/// A JoinHandle that can be used to await server shutdown
pub async fn start_unified_api(
    metadata_store: Arc<dyn MetadataStore>,
    query_engine: Option<Arc<ColumnarQueryEngine>>,
    vector_index_manager: Option<Arc<VectorIndexManager>>,
    embedding_provider: Option<Arc<dyn EmbeddingProvider>>,
    config: UnifiedApiConfig,
) -> anyhow::Result<tokio::task::JoinHandle<()>> {
    start_unified_api_with_search(
        metadata_store,
        query_engine,
        vector_index_manager,
        embedding_provider,
        None,
        config,
    )
    .await
}

/// Start the unified API HTTP server with search integration
///
/// This function spawns a background tokio task that runs the HTTP server
/// on the configured port (default: 6092), including the search API endpoints.
///
/// # Arguments
/// * `metadata_store` - Metadata store for topic information
/// * `query_engine` - Optional SQL query engine
/// * `vector_index_manager` - Optional vector index manager
/// * `embedding_provider` - Optional embedding provider for text-based vector search
/// * `search_router` - Optional search API router (Elasticsearch-compatible endpoints)
/// * `config` - Unified API configuration
///
/// # Returns
/// A JoinHandle that can be used to await server shutdown
pub async fn start_unified_api_with_search(
    metadata_store: Arc<dyn MetadataStore>,
    query_engine: Option<Arc<ColumnarQueryEngine>>,
    vector_index_manager: Option<Arc<VectorIndexManager>>,
    embedding_provider: Option<Arc<dyn EmbeddingProvider>>,
    search_router: Option<Router>,
    config: UnifiedApiConfig,
) -> anyhow::Result<tokio::task::JoinHandle<()>> {
    start_unified_api_full(
        metadata_store,
        query_engine,
        vector_index_manager,
        embedding_provider,
        search_router,
        None, // admin_router
        None, // hot_buffer - v2.2.23
        config,
    )
    .await
}

/// Start the unified API HTTP server with all integrations
///
/// This function spawns a background tokio task that runs the HTTP server
/// on the configured port (default: 6092), including all API components:
/// - SQL endpoints
/// - Vector search endpoints
/// - Search API (Elasticsearch-compatible)
/// - Admin API + Schema Registry
///
/// # Arguments
/// * `metadata_store` - Metadata store for topic information
/// * `query_engine` - Optional SQL query engine
/// * `vector_index_manager` - Optional vector index manager
/// * `embedding_provider` - Optional embedding provider for text-based vector search
/// * `search_router` - Optional search API router (Elasticsearch-compatible endpoints)
/// * `admin_router` - Optional admin API router (cluster management + Schema Registry)
/// * `hot_buffer` - Optional hot buffer for sub-second SQL queries on recent data (v2.2.23)
/// * `config` - Unified API configuration
///
/// # Returns
/// A JoinHandle that can be used to await server shutdown
pub async fn start_unified_api_full(
    metadata_store: Arc<dyn MetadataStore>,
    query_engine: Option<Arc<ColumnarQueryEngine>>,
    vector_index_manager: Option<Arc<VectorIndexManager>>,
    embedding_provider: Option<Arc<dyn EmbeddingProvider>>,
    search_router: Option<Router>,
    admin_router: Option<Router>,
    hot_buffer: Option<Arc<chronik_columnar::HotDataBuffer>>,
    config: UnifiedApiConfig,
) -> anyhow::Result<tokio::task::JoinHandle<()>> {
    let port = std::env::var("CHRONIK_UNIFIED_API_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(config.port);

    let bind_addr = std::env::var("CHRONIK_UNIFIED_API_BIND")
        .unwrap_or_else(|_| config.bind_addr.clone());

    let search_enabled = search_router.is_some();
    let admin_enabled = admin_router.is_some();

    let config = UnifiedApiConfig {
        port,
        bind_addr: bind_addr.clone(),
        enable_search: search_enabled,
        enable_admin: admin_enabled,
        ..config
    };

    let state = UnifiedApiState::new(metadata_store, config);
    let state = if let Some(engine) = query_engine {
        info!("Unified API: SQL query engine enabled");
        state.with_query_engine(engine)
    } else {
        state
    };
    let state = if let Some(manager) = vector_index_manager {
        info!("Unified API: Vector search enabled");
        state.with_vector_index_manager(manager)
    } else {
        state
    };
    let state = if let Some(provider) = embedding_provider {
        info!("Unified API: Embedding provider '{}' enabled", provider.name());
        state.with_embedding_provider(provider)
    } else {
        state
    };
    // v2.2.23: Hot buffer for sub-second SQL queries
    let state = if let Some(hb) = hot_buffer {
        info!("Unified API: Hot buffer enabled for sub-second SQL latency");
        state.with_hot_buffer(hb)
    } else {
        state
    };

    if search_enabled {
        info!("Unified API: Search (Elasticsearch-compatible) enabled");
    }
    if admin_enabled {
        info!("Unified API: Admin API + Schema Registry enabled");
    }

    let app = create_router_full(state, search_router, admin_router);
    let addr: std::net::SocketAddr = format!("{}:{}", bind_addr, port).parse()?;

    info!("Starting Unified API HTTP server on {}", addr);
    info!("  SQL endpoints:    /_sql, /_sql/explain, /_sql/tables");
    info!("  Vector endpoints: /_vector/:topic/search, /_vector/:topic/stats, /_vector/:topic/hybrid");
    if search_enabled {
        info!("  Search endpoints: /_search, /:index/_search, /:index/_doc/:id");
    }
    if admin_enabled {
        info!("  Admin endpoints:  /admin/status, /admin/add-node, /admin/remove-node");
        info!("  Schema Registry:  /subjects/*, /schemas/ids/*, /config/*");
    }
    info!("  Health endpoint:  /health");

    let handle = tokio::spawn(async move {
        if let Err(e) = axum::Server::bind(&addr)
            .serve(app.into_make_service())
            .await
        {
            tracing::error!("Unified API server error: {}", e);
        }
    });

    Ok(handle)
}

/// Get the unified API port from environment or default
pub fn get_unified_api_port() -> u16 {
    std::env::var("CHRONIK_UNIFIED_API_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(6092)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use tower::ServiceExt; // for oneshot

    /// Create a mock metadata store for testing
    fn create_mock_metadata_store() -> Arc<dyn MetadataStore> {
        use chronik_common::metadata::InMemoryMetadataStore;
        Arc::new(InMemoryMetadataStore::new())
    }

    #[test]
    fn test_config_default() {
        let config = UnifiedApiConfig::default();
        assert_eq!(config.port, 6092);
        assert_eq!(config.bind_addr, "0.0.0.0");
        assert!(config.enable_sql);
        assert!(config.enable_vector);
        assert!(config.enable_search);
        assert!(config.enable_admin);
    }

    #[test]
    fn test_config_custom() {
        let config = UnifiedApiConfig {
            port: 8080,
            bind_addr: "127.0.0.1".to_string(),
            enable_sql: false,
            enable_vector: true,
            enable_admin: false,
            enable_search: true,
            query_timeout_secs: 60,
        };
        assert_eq!(config.port, 8080);
        assert_eq!(config.bind_addr, "127.0.0.1");
        assert!(!config.enable_sql);
        assert!(config.enable_vector);
        assert!(!config.enable_admin);
        assert!(config.enable_search);
        assert_eq!(config.query_timeout_secs, 60);
    }

    #[test]
    fn test_unified_api_state_builder() {
        let metadata_store = create_mock_metadata_store();
        let config = UnifiedApiConfig::default();

        let state = UnifiedApiState::new(metadata_store.clone(), config.clone());

        // State should start without optional components
        assert!(state.query_engine.is_none());
        assert!(state.vector_index_manager.is_none());
        assert!(state.embedding_provider.is_none());

        // Config should be preserved
        assert_eq!(state.config.port, 6092);
    }

    #[tokio::test]
    async fn test_health_endpoint() {
        let metadata_store = create_mock_metadata_store();
        let config = UnifiedApiConfig::default();
        let state = UnifiedApiState::new(metadata_store, config);

        let app = create_router(state);

        let response = app
            .oneshot(Request::builder().uri("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        // Parse response body
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let health: HealthResponse = serde_json::from_slice(&body).unwrap();

        assert_eq!(health.status, "ok");
        assert!(!health.sql_enabled); // No query engine provided
        assert!(!health.vector_enabled); // No vector manager provided
    }

    #[tokio::test]
    async fn test_sql_endpoint_disabled_without_engine() {
        let metadata_store = create_mock_metadata_store();
        let config = UnifiedApiConfig::default();
        let state = UnifiedApiState::new(metadata_store, config);

        let app = create_router(state);

        // SQL endpoint should return SERVICE_UNAVAILABLE without query engine
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/_sql")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"query": "SELECT 1"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn test_vector_endpoint_disabled_without_manager() {
        let metadata_store = create_mock_metadata_store();
        let config = UnifiedApiConfig::default();
        let state = UnifiedApiState::new(metadata_store, config);

        let app = create_router(state);

        // Vector endpoint should return SERVICE_UNAVAILABLE without manager
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/_vector/test-topic/search")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"query": "test"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn test_sql_tables_endpoint() {
        let metadata_store = create_mock_metadata_store();
        let config = UnifiedApiConfig::default();
        let state = UnifiedApiState::new(metadata_store, config);

        let app = create_router(state);

        // Should return SERVICE_UNAVAILABLE without query engine
        let response = app
            .oneshot(Request::builder().uri("/_sql/tables").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn test_vector_topics_endpoint() {
        let metadata_store = create_mock_metadata_store();
        let config = UnifiedApiConfig::default();
        let state = UnifiedApiState::new(metadata_store, config);

        let app = create_router(state);

        // Should return SERVICE_UNAVAILABLE without vector manager
        let response = app
            .oneshot(Request::builder().uri("/_vector/topics").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn test_router_with_disabled_features() {
        let metadata_store = create_mock_metadata_store();
        let config = UnifiedApiConfig {
            enable_sql: false,
            enable_vector: false,
            ..Default::default()
        };
        let state = UnifiedApiState::new(metadata_store, config);

        let app = create_router(state);

        // Health should still work
        let response = app
            .clone()
            .oneshot(Request::builder().uri("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // SQL endpoint should return 404 (not mounted)
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/_sql")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"query": "SELECT 1"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn test_get_unified_api_port_default() {
        // Clear env var to test default
        std::env::remove_var("CHRONIK_UNIFIED_API_PORT");
        assert_eq!(get_unified_api_port(), 6092);
    }

    #[test]
    fn test_health_response_serialization() {
        let response = HealthResponse {
            status: "ok".to_string(),
            version: "2.2.22".to_string(),
            sql_enabled: true,
            vector_enabled: false,
            search_enabled: true,
            admin_enabled: true,
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("\"status\":\"ok\""));
        assert!(json.contains("\"sql_enabled\":true"));
        assert!(json.contains("\"vector_enabled\":false"));
    }
}
