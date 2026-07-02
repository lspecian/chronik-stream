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
pub mod query_handler;
pub mod query_router;
#[cfg(feature = "search")]
pub mod search_handler;

// AM-1.7: Agent Memory endpoints (/memory/v1/*).
pub mod memory;
pub mod memory_types;

use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{debug, info};

use chronik_columnar::{
    ColumnarQueryEngine, HotDataBuffer, QueryEngineConfig, VectorIndexManager, VectorSearchService,
};
use chronik_common::metadata::traits::MetadataStore;
use chronik_storage::wal_indexer::WalIndexer;
use chronik_embeddings::EmbeddingProvider;
use chronik_embeddings::reranker::RerankerProvider;

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
    /// v2.4.0: SearchApi for full-text search via query orchestrator
    #[cfg(feature = "search")]
    pub search_api: Option<Arc<chronik_search::SearchApi>>,
    /// v2.4.0: Shared profile store for ranking profiles (across all requests)
    pub profile_store: Arc<chronik_query::profiles::ProfileStore>,
    /// v2.4.0: Shared feature logger for query training data
    pub feature_logger: Arc<chronik_query::features::FeatureLogger>,
    /// v2.4.1: Query embedding cache (SV-1) — shared across all vector search requests
    pub embedding_cache: Option<Arc<chronik_columnar::QueryEmbeddingCache>>,
    /// v2.3.1: WalIndexer for vector embedding backfill
    pub wal_indexer: Option<Arc<WalIndexer>>,
    /// VO-4: Optional reranker provider for cross-encoder re-ranking after HNSW retrieval
    pub reranker: Option<Arc<dyn RerankerProvider>>,
    /// Distributed query router for cluster-mode scatter-gather fan-out.
    /// None in single-node mode — queries execute locally only.
    pub query_router: Option<Arc<query_router::QueryRouter>>,
    /// HP-2.5: Shared hot vector index for NRT ANN search.
    /// When present, `/_vector/:topic/search` merges hot hits with cold HNSW.
    pub hot_vector_index: Option<Arc<chronik_columnar::hot_vector_index::HotVectorIndex>>,

    // ───────────────────────── AM-1.7: Agent Memory ─────────────────────────
    /// Per-namespace [`chronik_memory::Memory`] cache. When `None`, the
    /// `/memory/v1/*` endpoints respond 503 `service_unavailable`. Wire it
    /// from `main.rs` after reading `CHRONIK_MEMORY_KAFKA` + `CHRONIK_MEMORY_API`.
    pub memory_registry: Option<Arc<chronik_memory::MemoryRegistry>>,
    /// Optional text generator for `synthesize: true` recall. Typically an
    /// Anthropic Haiku 4.5 client wired from `CHRONIK_MEMORY_SYNTHESIS_PROVIDER`.
    pub memory_text_generator: Option<Arc<dyn chronik_memory::TextGenerator>>,
    /// When `true`, `/memory/v1/*` requires `X-Tenant-Id` + `X-API-Key` headers.
    /// In Phase 1 passthrough this only checks presence; with [`Self::memory_tenants`]
    /// populated (AM-2.5), the handlers validate `(tenant_id, api_key)` against
    /// the registry and reject cross-namespace access.
    /// Toggle via `CHRONIK_MEMORY_REQUIRE_AUTH=true`.
    pub memory_require_auth: bool,
    /// AM-2.5: Tenant registry for full multi-tenant auth/authorization.
    /// When empty (default), handlers run in passthrough mode (only the
    /// header-presence check from `memory_require_auth` applies). When
    /// populated, handlers validate `(X-Tenant-Id, X-API-Key)` against the
    /// registry on every request and reject cross-namespace access with 403.
    pub memory_tenants: Option<Arc<chronik_memory::TenantRegistry>>,
    /// AM-2.5: Per-tenant Prometheus counters exposed at
    /// `GET /memory/v1/metrics`. Cheap to clone (`Arc` interior). When
    /// `None`, handlers skip the increment and the metrics endpoint returns
    /// an empty payload with just the # HELP/TYPE headers.
    pub memory_tenant_metrics: Option<Arc<chronik_memory::TenantMetrics>>,
    /// AM-2.6: In-memory `memory_id → Source` index used by
    /// `GET /memory/v1/{memory_id}/source`. Populated by
    /// [`chronik_memory::spawn_memory_index_consumer`] tailing every
    /// `mem.{type}.{tenant}` topic. When `None`, the source endpoint
    /// returns 503.
    pub memory_index: Option<Arc<chronik_memory::MemoryIndex>>,
    /// AM-2.5: Per-tenant token-bucket rate limiter. When present AND
    /// [`Self::memory_tenants`] is populated, every write / recall consumes
    /// tokens from the caller's `TenantQuotas.{ingest_msgs_per_sec,
    /// recall_qps}` bucket. When either is `None`, requests pass through
    /// (unlimited). Denied requests get `429 Too Many Requests` with a
    /// `Retry-After`-shaped message body.
    pub memory_rate_limiter: Option<Arc<chronik_memory::RateLimiter>>,
    /// AM-2.3: Synchronous compaction runner for
    /// `POST /memory/v1/compact`. Object-safe so the state doesn't
    /// leak the [`chronik_memory::compaction::CompactionController`]
    /// embedder generic. When `None`, the endpoint returns 503.
    pub memory_compaction: Option<Arc<dyn chronik_memory::CompactionRunner>>,
    /// AM-2.5: Per-tenant storage byte counter + quota gate. When
    /// present AND [`Self::memory_tenants`] is populated, every write
    /// consults the tracker via
    /// [`StorageTracker::try_reserve`](chronik_memory::StorageTracker::try_reserve)
    /// against the caller's `TenantQuotas.storage_bytes`. Writes over
    /// quota get `413 Payload Too Large`. When either is `None`, writes
    /// pass through without accounting.
    pub memory_storage_tracker: Option<Arc<chronik_memory::StorageTracker>>,
    /// AM-3.4: Provenance-graph index backing
    /// `GET /memory/v1/{memory_id}/lineage`. Populated by a consumer
    /// that tails typed-memory topics. When `None`, the endpoint
    /// returns 503.
    pub memory_lineage: Option<Arc<chronik_memory::LineageIndex>>,
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
            #[cfg(feature = "search")]
            search_api: None,
            profile_store: Arc::new(chronik_query::profiles::ProfileStore::new()),
            feature_logger: Arc::new(chronik_query::features::FeatureLogger::new()),
            embedding_cache: None,
            wal_indexer: None,
            reranker: None,
            query_router: None,
            hot_vector_index: None,
            memory_registry: None,
            memory_text_generator: None,
            memory_require_auth: false,
            memory_tenants: None,
            memory_rate_limiter: None,
            memory_index: None,
            memory_tenant_metrics: None,
            memory_compaction: None,
            memory_storage_tracker: None,
            memory_lineage: None,
        }
    }

    /// AM-1.7: Attach the per-namespace `Memory` cache for the
    /// `/memory/v1/*` endpoints. Without this, those endpoints reply
    /// `503 service_unavailable`.
    pub fn with_memory_registry(
        mut self,
        registry: Arc<chronik_memory::MemoryRegistry>,
    ) -> Self {
        self.memory_registry = Some(registry);
        self
    }

    /// AM-1.7: Attach a text generator used when callers request
    /// `synthesize: true` on `/memory/v1/recall`.
    pub fn with_memory_text_generator(
        mut self,
        gen: Arc<dyn chronik_memory::TextGenerator>,
    ) -> Self {
        self.memory_text_generator = Some(gen);
        self
    }

    /// AM-1.7: Require `X-Tenant-Id` + `X-API-Key` on every `/memory/v1/*`
    /// request. Toggle via `CHRONIK_MEMORY_REQUIRE_AUTH=true`.
    pub fn with_memory_require_auth(mut self, require: bool) -> Self {
        self.memory_require_auth = require;
        self
    }

    /// AM-2.5: Attach a populated tenant registry. When present, handlers
    /// validate every request's `(X-Tenant-Id, X-API-Key)` against the
    /// registry and reject cross-namespace access with 403.
    pub fn with_memory_tenants(
        mut self,
        tenants: Arc<chronik_memory::TenantRegistry>,
    ) -> Self {
        self.memory_tenants = Some(tenants);
        self
    }

    /// AM-2.5: Attach a per-tenant token-bucket rate limiter. Only takes
    /// effect when combined with [`Self::with_memory_tenants`] — otherwise
    /// handlers can't identify the caller's quotas.
    pub fn with_memory_rate_limiter(
        mut self,
        limiter: Arc<chronik_memory::RateLimiter>,
    ) -> Self {
        self.memory_rate_limiter = Some(limiter);
        self
    }

    /// AM-2.6: Attach the `memory_id → Source` index used by the
    /// `GET /memory/v1/{memory_id}/source` provenance endpoint. When
    /// missing, the endpoint replies `503 service_unavailable`.
    pub fn with_memory_index(
        mut self,
        index: Arc<chronik_memory::MemoryIndex>,
    ) -> Self {
        self.memory_index = Some(index);
        self
    }

    /// AM-2.5: Attach the per-tenant metric registry. When missing,
    /// handlers skip the increment and `GET /memory/v1/metrics` returns
    /// an empty payload.
    pub fn with_memory_tenant_metrics(
        mut self,
        metrics: Arc<chronik_memory::TenantMetrics>,
    ) -> Self {
        self.memory_tenant_metrics = Some(metrics);
        self
    }

    /// AM-2.3: Attach the synchronous compaction runner. When missing,
    /// `POST /memory/v1/compact` returns `503 service_unavailable`.
    pub fn with_memory_compaction(
        mut self,
        runner: Arc<dyn chronik_memory::CompactionRunner>,
    ) -> Self {
        self.memory_compaction = Some(runner);
        self
    }

    /// AM-2.5: Attach the per-tenant storage byte counter. When missing,
    /// writes are not accounted (passthrough).
    pub fn with_memory_storage_tracker(
        mut self,
        tracker: Arc<chronik_memory::StorageTracker>,
    ) -> Self {
        self.memory_storage_tracker = Some(tracker);
        self
    }

    /// AM-3.4: Attach the lineage / provenance-graph index. Without
    /// it, `GET /memory/v1/{memory_id}/lineage` returns 503.
    pub fn with_memory_lineage(
        mut self,
        idx: Arc<chronik_memory::LineageIndex>,
    ) -> Self {
        self.memory_lineage = Some(idx);
        self
    }

    /// HP-2.5: Attach the shared hot vector index for NRT ANN search.
    pub fn with_hot_vector_index(
        mut self,
        index: Arc<chronik_columnar::hot_vector_index::HotVectorIndex>,
    ) -> Self {
        self.hot_vector_index = Some(index);
        self
    }

    /// Set the WalIndexer for vector backfill support
    pub fn with_wal_indexer(mut self, indexer: Arc<WalIndexer>) -> Self {
        self.wal_indexer = Some(indexer);
        self
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

    /// v2.4.0: Set the SearchApi for full-text search
    ///
    /// When set, the query orchestrator's `execute_text()` will search
    /// Tantivy indices (both in-memory and WAL-created) for matching documents.
    #[cfg(feature = "search")]
    pub fn with_search_api(mut self, api: Arc<chronik_search::SearchApi>) -> Self {
        self.search_api = Some(api);
        self
    }

    /// VO-4: Set the reranker provider for cross-encoder re-ranking.
    ///
    /// When set, vector search endpoints can apply re-ranking to improve
    /// result quality. Overretrieves k * overretrieval_factor candidates,
    /// scores them with a cross-encoder, and returns top-k.
    pub fn with_reranker(mut self, reranker: Arc<dyn RerankerProvider>) -> Self {
        self.reranker = Some(reranker);
        self
    }

    /// Set the distributed query router for cluster-mode fan-out.
    ///
    /// When set, query endpoints automatically fan out to peer nodes
    /// and merge results. See docs/DISTRIBUTED_QUERY_LAYER.md.
    pub fn with_query_router(mut self, router: Arc<query_router::QueryRouter>) -> Self {
        self.query_router = Some(router);
        self
    }

    /// v2.4.1: Set the query embedding cache (SV-1).
    ///
    /// When set, vector search queries check the cache before calling the
    /// embedding provider, reducing repeated query latency from ~372ms to ~1-5ms.
    pub fn with_embedding_cache(mut self, cache: Arc<chronik_columnar::QueryEmbeddingCache>) -> Self {
        self.embedding_cache = Some(cache);
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
            .route("/_vector/topics", get(vector_handler::list_topics))
            .route("/_vector/:topic/backfill", post(vector_handler::backfill))
            .route("/_vector/:topic/index", delete(vector_handler::clear_vector_index));
    }

    // Unified query endpoint (always enabled — orchestrates across all backends)
    router = router
        .route("/_query", post(query_handler::handle_query))
        .route("/_query/capabilities", get(query_handler::handle_capabilities))
        .route("/_query/profiles", get(query_handler::handle_profiles));

    // AM-1.7: Agent Memory endpoints (always registered; handlers return 503
    // when memory_registry is None on UnifiedApiState).
    router = router
        .route("/memory/v1/ingest", post(memory::ingest))
        .route("/memory/v1/remember", post(memory::remember))
        .route("/memory/v1/forget", post(memory::forget))
        .route("/memory/v1/recall", post(memory::recall))
        .route("/memory/v1/feedback", post(memory::feedback))
        .route("/memory/v1/health", get(memory::health))
        .route("/memory/v1/metrics", get(memory::metrics))
        .route("/memory/v1/admin/init-namespace", post(memory::admin_init_namespace))
        .route("/memory/v1/compact", post(memory::compact))
        .route("/memory/v1/recall/stream", post(memory::recall_stream))
        .route("/memory/v1/:memory_id/source", get(memory::source))
        .route("/memory/v1/:memory_id/lineage", get(memory::lineage));

    // Add shared state for SQL/Vector/Query/Memory endpoints
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
#[derive(Debug, Serialize, Deserialize)]
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
    info!("  Query endpoints:  /_query, /_query/capabilities, /_query/profiles");
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

/// Start the unified API HTTP server with a pre-built state.
///
/// Use this when you need to configure the state manually (e.g., to inject
/// the SearchApi for the query orchestrator's text search).
pub async fn start_unified_api_with_state(
    state: UnifiedApiState,
    search_router: Option<Router>,
    admin_router: Option<Router>,
) -> anyhow::Result<tokio::task::JoinHandle<()>> {
    let port = std::env::var("CHRONIK_UNIFIED_API_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(state.config.port);

    let bind_addr = std::env::var("CHRONIK_UNIFIED_API_BIND")
        .unwrap_or_else(|_| state.config.bind_addr.clone());

    let search_enabled = search_router.is_some();
    let admin_enabled = admin_router.is_some();

    info!("Starting Unified API HTTP server on {}:{}", bind_addr, port);
    info!("  Query endpoints:  /_query, /_query/capabilities, /_query/profiles");
    if state.query_engine.is_some() {
        info!("  SQL endpoints:    /_sql, /_sql/explain, /_sql/tables");
    }
    if state.vector_index_manager.is_some() {
        info!("  Vector endpoints: /_vector/:topic/search, /_vector/:topic/stats, /_vector/:topic/hybrid");
    }
    if search_enabled {
        info!("  Search endpoints: /_search, /:index/_search, /:index/_doc/:id");
    }
    if admin_enabled {
        info!("  Admin endpoints:  /admin/status, /admin/add-node, /admin/remove-node");
        info!("  Schema Registry:  /subjects/*, /schemas/ids/*, /config/*");
    }
    info!("  Health endpoint:  /health");

    let app = create_router_full(state, search_router, admin_router);
    let addr: std::net::SocketAddr = format!("{}:{}", bind_addr, port).parse()?;

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

    #[tokio::test]
    async fn test_unified_api_state_builder() {
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
        let body = hyper::body::to_bytes(response.into_body()).await.unwrap();
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

        // Returns OK with empty topics list when no vector manager configured
        // (list_topics gracefully handles missing vector_index_manager)
        let response = app
            .oneshot(Request::builder().uri("/_vector/topics").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
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
