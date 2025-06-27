//! REST API for search operations.

use crate::indexer::{TantivyIndexer, SearchResult};
use chronik_common::{Result, Error};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::{delete, get, post, put},
    Router,
};
use dashmap::DashMap;
use prometheus::{register_counter_vec, register_histogram_vec, CounterVec, HistogramVec};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tantivy::{
    collector::TopDocs,
    query::{BooleanQuery, Occur, Query as TantivyQuery, TermQuery, RangeQuery},
    schema::{Schema, Field, TEXT, STORED, STRING, NumericOptions},
    Index, IndexReader, IndexWriter,
    Term,
};
use tower_http::cors::CorsLayer;
use tracing::{debug, error, info};

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
#[derive(Debug, Deserialize)]
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
}

fn default_size() -> usize {
    10
}

/// Query DSL (Elasticsearch compatible)
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryDsl {
    Match(MatchQuery),
    Term(TermQueryDsl),
    Range(RangeQueryDsl),
    Bool(BoolQuery),
    MatchAll(MatchAllQuery),
}

#[derive(Debug, Deserialize)]
pub struct MatchQuery {
    #[serde(flatten)]
    pub field_value: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct TermQueryDsl {
    #[serde(flatten)]
    pub field_value: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct RangeQueryDsl {
    #[serde(flatten)]
    pub field_range: HashMap<String, RangeClause>,
}

#[derive(Debug, Deserialize)]
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

#[derive(Debug, Deserialize)]
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

#[derive(Debug, Deserialize)]
pub struct MatchAllQuery {}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum SortClause {
    Field(String),
    Object(HashMap<String, SortOrder>),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SortOrder {
    Asc,
    Desc,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum SourceFilter {
    Bool(bool),
    Fields(Vec<String>),
}

/// Search response (Elasticsearch compatible)
#[derive(Debug, Serialize)]
pub struct SearchResponse {
    pub took: u64,
    pub timed_out: bool,
    pub _shards: ShardInfo,
    pub hits: HitsInfo,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aggregations: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub struct ShardInfo {
    pub total: u32,
    pub successful: u32,
    pub skipped: u32,
    pub failed: u32,
}

#[derive(Debug, Serialize)]
pub struct HitsInfo {
    pub total: TotalHits,
    pub max_score: Option<f32>,
    pub hits: Vec<Hit>,
}

#[derive(Debug, Serialize)]
pub struct TotalHits {
    pub value: u64,
    pub relation: String,
}

#[derive(Debug, Serialize)]
pub struct Hit {
    pub _index: String,
    pub _id: String,
    pub _score: Option<f32>,
    pub _source: serde_json::Value,
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
    /// Create a new search API.
    pub fn new() -> Result<Self> {
        Ok(Self {
            indices: Arc::new(DashMap::new()),
            metrics: ApiMetrics::new()?,
        })
    }

    /// Create router for the API
    pub fn router(self: Arc<Self>) -> Router {
        Router::new()
            // Health check
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
            let field = match field_mapping.field_type.as_str() {
                "text" => {
                    let mut options = TEXT;
                    if field_mapping.store.unwrap_or(false) {
                        options = options | STORED;
                    }
                    schema_builder.add_text_field(field_name, options)
                },
                "keyword" => {
                    let mut options = STRING;
                    if field_mapping.store.unwrap_or(false) {
                        options = options | STORED;
                    }
                    schema_builder.add_text_field(field_name, options)
                },
                "long" | "integer" => {
                    let mut options = NumericOptions::default();
                    if field_mapping.index.unwrap_or(true) {
                        options = options.set_indexed();
                    }
                    if field_mapping.store.unwrap_or(false) {
                        options = options.set_stored();
                    }
                    schema_builder.add_i64_field(field_name, options)
                },
                _ => {
                    return Err(Error::InvalidInput(format!(
                        "Unsupported field type: {}",
                        field_mapping.field_type
                    )));
                }
            };
            fields.insert(field_name.clone(), field);
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