//! REST API for search operations.

use chronik_common::{Result, Error};
use axum::{
    routing::{delete, get, post, put},
    Router,
};
use dashmap::DashMap;
use prometheus::{register_counter_vec, register_histogram_vec, CounterVec, HistogramVec};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::Arc,
};
use tantivy::{
    schema::{Schema, TEXT, STORED, STRING, NumericOptions},
    Index, IndexReader, IndexWriter,
};
use tower_http::cors::CorsLayer;

use crate::handlers::{
    health_check, metrics_handler, search_all, search_index, index_document,
    get_document, delete_document, create_index, delete_index, get_mapping, cat_indices,
};

/// Search API server.
pub struct SearchApi {
    /// Map of index name to Tantivy index
    pub indices: Arc<DashMap<String, IndexState>>,
    /// Metrics
    metrics: ApiMetrics,
    /// Query cache
    pub cache: Arc<crate::cache::QueryCache>,
    /// WAL Indexer for querying WAL-created indices (optional for backwards compatibility)
    wal_indexer: Option<Arc<chronik_storage::WalIndexer>>,
    /// Base path for WAL-created Tantivy indices
    index_base_path: Option<String>,
}

/// State for a single index
pub struct IndexState {
    pub index: Index,
    pub writer: Arc<tokio::sync::RwLock<IndexWriter>>,
    pub reader: IndexReader,
    pub schema: Schema,
    pub mapping: IndexMapping,
}

/// API metrics
struct ApiMetrics {
    requests_total: CounterVec,
    request_duration_seconds: HistogramVec,
    errors_total: CounterVec,
}

impl ApiMetrics {
    fn new() -> Result<Self> {
        let requests_total = register_counter_vec!(
            "search_api_requests_total",
            "Total number of API requests",
            &["method", "endpoint", "status"]
        ).map_err(|e| Error::Internal(format!("Failed to register metric: {}", e)))?;

        let request_duration_seconds = register_histogram_vec!(
            "search_api_request_duration_seconds",
            "API request duration in seconds",
            &["method", "endpoint"]
        ).map_err(|e| Error::Internal(format!("Failed to register metric: {}", e)))?;

        let errors_total = register_counter_vec!(
            "search_api_errors_total",
            "Total number of API errors",
            &["method", "endpoint", "error_type"]
        ).map_err(|e| Error::Internal(format!("Failed to register metric: {}", e)))?;

        Ok(Self {
            requests_total,
            request_duration_seconds,
            errors_total,
        })
    }
}

/// Index mapping (Elasticsearch compatible)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexMapping {
    pub properties: HashMap<String, FieldMapping>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldMapping {
    #[serde(rename = "type")]
    pub field_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub analyzer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<bool>,
}

/// Search request (Elasticsearch compatible)
#[derive(Debug, Serialize, Deserialize)]
pub struct SearchRequest {
    #[serde(default)]
    pub query: Option<QueryDsl>,
    #[serde(default = "default_size")]
    pub size: usize,
    #[serde(default)]
    pub from: usize,
    #[serde(default)]
    pub sort: Option<Vec<SortClause>>,
    #[serde(default)]
    pub _source: Option<SourceFilter>,
    pub aggs: Option<serde_json::Value>,
    pub aggregations: Option<serde_json::Value>,
    #[serde(default)]
    pub highlight: Option<HighlightConfig>,
}

fn default_size() -> usize {
    10
}

/// Highlight configuration
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct HighlightConfig {
    #[serde(default)]
    pub fields: HashMap<String, HighlightField>,
    #[serde(default = "default_pre_tags")]
    pub pre_tags: Vec<String>,
    #[serde(default = "default_post_tags")]
    pub post_tags: Vec<String>,
    #[serde(default = "default_fragment_size")]
    pub fragment_size: usize,
    #[serde(default = "default_number_of_fragments")]
    pub number_of_fragments: usize,
    #[serde(default)]
    pub require_field_match: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct HighlightField {
    #[serde(default)]
    pub fragment_size: Option<usize>,
    #[serde(default)]
    pub number_of_fragments: Option<usize>,
    #[serde(default)]
    pub no_match_size: Option<usize>,
}

fn default_pre_tags() -> Vec<String> {
    vec!["<em>".to_string()]
}

fn default_post_tags() -> Vec<String> {
    vec!["</em>".to_string()]
}

fn default_fragment_size() -> usize {
    150
}

fn default_number_of_fragments() -> usize {
    3
}

/// Query DSL (Elasticsearch compatible)
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryDsl {
    Match(MatchQuery),
    Term(TermQueryDsl),
    Range(RangeQueryDsl),
    Bool(BoolQuery),
    MatchAll(MatchAllQuery),
    GeoDistance(GeoDistanceQuery),
    GeoBoundingBox(GeoBoundingBoxQuery),
    GeoPolygon(GeoPolygonQuery),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MatchQuery {
    #[serde(flatten)]
    pub field_value: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TermQueryDsl {
    #[serde(flatten)]
    pub field_value: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RangeQueryDsl {
    #[serde(flatten)]
    pub field_range: HashMap<String, RangeClause>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RangeClause {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gte: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gt: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lte: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lt: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BoolQuery {
    #[serde(default)]
    pub must: Vec<QueryDsl>,
    #[serde(default)]
    pub should: Vec<QueryDsl>,
    #[serde(default)]
    pub must_not: Vec<QueryDsl>,
    #[serde(default)]
    pub filter: Vec<QueryDsl>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MatchAllQuery {}

#[derive(Debug, Serialize, Deserialize)]
pub struct GeoDistanceQuery {
    pub field: String,
    pub distance: String,
    #[serde(flatten)]
    pub center: serde_json::Value,
    #[serde(default)]
    pub distance_type: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GeoBoundingBoxQuery {
    pub field: String,
    pub top_left: serde_json::Value,
    pub bottom_right: serde_json::Value,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GeoPolygonQuery {
    pub field: String,
    pub points: Vec<serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SortClause {
    Field(String),
    Object(HashMap<String, SortOrder>),
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SortOrder {
    Asc,
    Desc,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SourceFilter {
    Bool(bool),
    Fields(Vec<String>),
}

/// Search response (Elasticsearch compatible)
#[derive(Debug, Clone, Serialize)]
pub struct SearchResponse {
    pub took: u64,
    pub timed_out: bool,
    pub _shards: ShardInfo,
    pub hits: HitsInfo,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aggregations: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ShardInfo {
    pub total: u32,
    pub successful: u32,
    pub skipped: u32,
    pub failed: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct HitsInfo {
    pub total: TotalHits,
    pub max_score: Option<f32>,
    pub hits: Vec<Hit>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TotalHits {
    pub value: u64,
    pub relation: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Hit {
    pub _index: String,
    pub _id: String,
    pub _score: Option<f32>,
    pub _source: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub highlight: Option<HashMap<String, Vec<String>>>,
}

/// Index document request
#[derive(Debug, Deserialize)]
pub struct IndexDocumentRequest {
    #[serde(flatten)]
    pub document: serde_json::Value,
}

/// Index document response
#[derive(Debug, Serialize)]
pub struct IndexDocumentResponse {
    pub _index: String,
    pub _id: String,
    pub _version: u64,
    pub result: String,
    pub _shards: ShardInfo,
    pub _seq_no: u64,
    pub _primary_term: u64,
}

/// Get document response
#[derive(Debug, Serialize)]
pub struct GetDocumentResponse {
    pub _index: String,
    pub _id: String,
    pub _version: u64,
    pub _seq_no: u64,
    pub _primary_term: u64,
    pub found: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub _source: Option<serde_json::Value>,
}

/// Delete document response
#[derive(Debug, Serialize)]
pub struct DeleteDocumentResponse {
    pub _index: String,
    pub _id: String,
    pub _version: u64,
    pub result: String,
    pub _shards: ShardInfo,
    pub _seq_no: u64,
    pub _primary_term: u64,
}

/// Cat indices response
#[derive(Debug, Serialize)]
pub struct CatIndexInfo {
    pub health: String,
    pub status: String,
    pub index: String,
    pub uuid: String,
    pub pri: u32,
    pub rep: u32,
    #[serde(rename = "docs.count")]
    pub docs_count: u64,
    #[serde(rename = "docs.deleted")]
    pub docs_deleted: u64,
    #[serde(rename = "store.size")]
    pub store_size: String,
    #[serde(rename = "pri.store.size")]
    pub pri_store_size: String,
}

/// Error response (Elasticsearch compatible)
#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    pub error: ErrorInfo,
    pub status: u16,
}

#[derive(Debug, Serialize)]
pub struct ErrorInfo {
    #[serde(rename = "type")]
    pub error_type: String,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caused_by: Option<Box<ErrorInfo>>,
}

impl SearchApi {
    /// Create a new search API (without WAL indexer integration).
    pub fn new() -> Result<Self> {
        let cache_config = crate::cache::CacheConfig::default();
        Ok(Self {
            indices: Arc::new(DashMap::new()),
            metrics: ApiMetrics::new()?,
            cache: Arc::new(crate::cache::QueryCache::new(cache_config)),
            wal_indexer: None,
            index_base_path: None,
        })
    }

    /// Create a new search API with WAL indexer integration.
    /// The index_base_path parameter specifies where to find WAL-created Tantivy indices.
    pub fn new_with_wal_indexer(wal_indexer: Arc<chronik_storage::WalIndexer>, index_base_path: String) -> Result<Self> {
        let cache_config = crate::cache::CacheConfig::default();
        Ok(Self {
            indices: Arc::new(DashMap::new()),
            metrics: ApiMetrics::new()?,
            cache: Arc::new(crate::cache::QueryCache::new(cache_config)),
            wal_indexer: Some(wal_indexer),
            index_base_path: Some(index_base_path),
        })
    }

    /// Get the WAL indexer's index base path for querying WAL-created indices
    pub fn get_index_base_path(&self) -> Option<&str> {
        self.index_base_path.as_deref()
    }

    /// Create router for the API (standalone mode)
    pub fn router(self: Arc<Self>) -> Router {
        Router::new()
            // Health check - only for standalone mode
            .route("/health", get(health_check))
            .route("/metrics", get(metrics_handler))

            // Search endpoints
            .route("/_search", get(search_all).post(search_all))
            .route("/:index/_search", get(search_index).post(search_index))

            // Document endpoints
            .route("/:index/_doc/:id", post(index_document).put(index_document))
            .route("/:index/_doc/:id", get(get_document))
            .route("/:index/_doc/:id", delete(delete_document))

            // Index management
            .route("/:index", put(create_index))
            .route("/:index", delete(delete_index))
            .route("/:index/_mapping", get(get_mapping))
            .route("/_cat/indices", get(cat_indices))

            .layer(CorsLayer::permissive())
            .with_state(self)
    }

    /// Create router for embedding in another API (no /health to avoid conflicts)
    ///
    /// Use this method when the Search API is being merged into a unified API
    /// that already has its own /health endpoint.
    pub fn router_for_embedding(self: Arc<Self>) -> Router {
        Router::new()
            // Note: No /health here - the parent API provides it
            .route("/metrics", get(metrics_handler))

            // Search endpoints
            .route("/_search", get(search_all).post(search_all))
            .route("/:index/_search", get(search_index).post(search_index))

            // Document endpoints
            .route("/:index/_doc/:id", post(index_document).put(index_document))
            .route("/:index/_doc/:id", get(get_document))
            .route("/:index/_doc/:id", delete(delete_document))

            // Index management
            .route("/:index", put(create_index))
            .route("/:index", delete(delete_index))
            .route("/:index/_mapping", get(get_mapping))
            .route("/_cat/indices", get(cat_indices))

            .layer(CorsLayer::permissive())
            .with_state(self)
    }

    /// Create an index with mapping
    pub async fn create_index_with_mapping(
        &self,
        index_name: String,
        mapping: IndexMapping,
    ) -> Result<()> {
        if self.indices.contains_key(&index_name) {
            return Err(Error::InvalidInput(format!("Index {} already exists", index_name)));
        }

        // Build schema from mapping
        let mut schema_builder = Schema::builder();
        let mut fields = HashMap::new();

        for (field_name, field_mapping) in &mapping.properties {
            match field_mapping.field_type.as_str() {
                "text" => {
                    let mut options = TEXT;
                    if field_mapping.store.unwrap_or(false) {
                        options = options | STORED;
                    }
                    let field = schema_builder.add_text_field(field_name, options);
                    fields.insert(field_name.clone(), field);
                },
                "keyword" => {
                    let mut options = STRING;
                    if field_mapping.store.unwrap_or(false) {
                        options = options | STORED;
                    }
                    let field = schema_builder.add_text_field(field_name, options);
                    fields.insert(field_name.clone(), field);
                },
                "long" | "integer" => {
                    let mut options = NumericOptions::default();
                    if field_mapping.index.unwrap_or(true) {
                        options = options.set_indexed();
                    }
                    if field_mapping.store.unwrap_or(false) {
                        options = options.set_stored();
                    }
                    let field = schema_builder.add_i64_field(field_name, options);
                    fields.insert(field_name.clone(), field);
                },
                "geo_point" => {
                    // For geo_point, create two f64 fields: field_lat and field_lon
                    let mut lat_options = NumericOptions::default()
                        .set_indexed()
                        .set_fast();
                    let mut lon_options = NumericOptions::default()
                        .set_indexed()
                        .set_fast();
                    
                    if field_mapping.store.unwrap_or(false) {
                        lat_options = lat_options.set_stored();
                        lon_options = lon_options.set_stored();
                    }
                    
                    let lat_field = schema_builder.add_f64_field(&format!("{}_lat", field_name), lat_options);
                    let lon_field = schema_builder.add_f64_field(&format!("{}_lon", field_name), lon_options);
                    
                    // Store both fields in the fields map
                    fields.insert(format!("{}_lat", field_name), lat_field);
                    fields.insert(format!("{}_lon", field_name), lon_field);
                },
                _ => {
                    return Err(Error::InvalidInput(format!(
                        "Unsupported field type: {}",
                        field_mapping.field_type
                    )));
                }
            }
        }

        let schema = schema_builder.build();
        
        // Create index
        let index = Index::create_in_ram(schema.clone());
        let writer = index.writer(50_000_000)
            .map_err(|e| Error::Internal(format!("Failed to create index writer: {}", e)))?;
        
        let reader = index.reader_builder()
            .reload_policy(tantivy::ReloadPolicy::OnCommitWithDelay)
            .try_into()
            .map_err(|e| Error::Internal(format!("Failed to create index reader: {}", e)))?;

        let state = IndexState {
            index,
            writer: Arc::new(tokio::sync::RwLock::new(writer)),
            reader,
            schema,
            mapping,
        };

        self.indices.insert(index_name, state);
        Ok(())
    }
}