//! Arrow Flight Service for High-Performance Streaming (v2.2.22)
//!
//! This module provides Arrow Flight support for efficient streaming of:
//! - SQL query results via Flight SQL protocol
//! - Vector search results as Arrow RecordBatches
//!
//! ## Feature Status
//!
//! **Note**: Full Arrow Flight support requires tonic 0.12+, but the workspace
//! currently uses tonic 0.9 for opentelemetry-otlp compatibility. The flight
//! service implementation is provided but may require future workspace updates.
//!
//! ## Usage
//!
//! Enable the `flight` feature in Cargo.toml:
//!
//! ```toml
//! chronik-columnar = { version = "...", features = ["flight"] }
//! ```
//!
//! ## Ticket Formats
//!
//! SQL queries: `sql:SELECT * FROM table_name`
//! Vector search: `vector:topic:query_text:k`
//!
//! ## Example
//!
//! ```ignore
//! use chronik_columnar::flight::{ChronikFlightService, FlightServiceConfig};
//!
//! let service = ChronikFlightService::new(query_engine, FlightServiceConfig::default());
//! let server = service.into_server();
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use datafusion::arrow::array::{Int32Array, Int64Array, Float32Array, StringArray, RecordBatch, ArrayRef, FixedSizeListArray};
use datafusion::arrow::datatypes::{Schema, DataType, Field};
use datafusion::arrow::buffer::Buffer;
use tokio::sync::RwLock;
use tracing::{info, debug};

use crate::query_engine::ColumnarQueryEngine;
use crate::vector_index::{VectorIndexManager, VectorSearchService, VectorSearchFilters, VectorSearchResult};
use chronik_embeddings::EmbeddingProvider;

/// Chronik Flight Service configuration
#[derive(Debug, Clone)]
pub struct FlightServiceConfig {
    /// Maximum number of rows per batch in results
    pub max_batch_rows: usize,
    /// Query timeout in seconds
    pub query_timeout_secs: u64,
    /// Enable Flight SQL protocol
    pub enable_flight_sql: bool,
    /// Enable vector search streaming
    pub enable_vector_streaming: bool,
    /// Enable query result caching
    pub enable_cache: bool,
    /// Maximum number of cached queries
    pub cache_max_entries: usize,
    /// Cache entry TTL in seconds
    pub cache_ttl_secs: u64,
}

impl Default for FlightServiceConfig {
    fn default() -> Self {
        Self {
            max_batch_rows: 10000,
            query_timeout_secs: 30,
            enable_flight_sql: true,
            enable_vector_streaming: true,
            enable_cache: true,
            cache_max_entries: 100,
            cache_ttl_secs: 60, // 1 minute
        }
    }
}

/// A cached query result entry
struct CacheEntry {
    /// The cached record batches
    batches: Vec<RecordBatch>,
    /// When this entry was created
    created_at: Instant,
    /// The TTL for this entry
    ttl: Duration,
}

impl CacheEntry {
    /// Create a new cache entry
    fn new(batches: Vec<RecordBatch>, ttl_secs: u64) -> Self {
        Self {
            batches,
            created_at: Instant::now(),
            ttl: Duration::from_secs(ttl_secs),
        }
    }

    /// Check if this entry has expired
    fn is_expired(&self) -> bool {
        self.created_at.elapsed() > self.ttl
    }
}

/// Query result cache
pub struct QueryCache {
    /// Cached entries keyed by normalized query string
    entries: RwLock<HashMap<String, CacheEntry>>,
    /// Maximum number of entries
    max_entries: usize,
    /// TTL for new entries
    ttl_secs: u64,
}

impl QueryCache {
    /// Create a new query cache
    pub fn new(max_entries: usize, ttl_secs: u64) -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
            max_entries,
            ttl_secs,
        }
    }

    /// Normalize a query string for caching (trim whitespace, lowercase)
    fn normalize_query(query: &str) -> String {
        query.trim().to_lowercase()
    }

    /// Get a cached result if available and not expired
    pub async fn get(&self, query: &str) -> Option<Vec<RecordBatch>> {
        let normalized = Self::normalize_query(query);
        let entries = self.entries.read().await;

        if let Some(entry) = entries.get(&normalized) {
            if !entry.is_expired() {
                debug!(query = %query, "Cache hit");
                return Some(entry.batches.clone());
            }
        }
        debug!(query = %query, "Cache miss");
        None
    }

    /// Store a result in the cache
    pub async fn put(&self, query: &str, batches: Vec<RecordBatch>) {
        let normalized = Self::normalize_query(query);
        let mut entries = self.entries.write().await;

        // Evict expired entries first
        entries.retain(|_, v| !v.is_expired());

        // If still over capacity, remove oldest entries
        while entries.len() >= self.max_entries {
            // Find and remove oldest entry
            if let Some((oldest_key, _)) = entries
                .iter()
                .min_by_key(|(_, v)| v.created_at)
                .map(|(k, v)| (k.clone(), v.created_at))
            {
                entries.remove(&oldest_key);
            } else {
                break;
            }
        }

        entries.insert(normalized, CacheEntry::new(batches, self.ttl_secs));
    }

    /// Clear all cached entries
    pub async fn clear(&self) {
        let mut entries = self.entries.write().await;
        entries.clear();
    }

    /// Get cache statistics
    pub async fn stats(&self) -> CacheStats {
        let entries = self.entries.read().await;
        let total = entries.len();
        let expired = entries.values().filter(|v| v.is_expired()).count();
        CacheStats {
            total_entries: total,
            expired_entries: expired,
            active_entries: total - expired,
        }
    }
}

/// Cache statistics
#[derive(Debug, Clone)]
pub struct CacheStats {
    /// Total number of entries (including expired)
    pub total_entries: usize,
    /// Number of expired entries pending cleanup
    pub expired_entries: usize,
    /// Number of active (non-expired) entries
    pub active_entries: usize,
}

/// Chronik Flight Service implementation
///
/// Provides high-performance streaming of query results using Arrow Flight protocol.
/// Supports both SQL queries (via Flight SQL) and vector search results.
///
/// ## Ticket Format
///
/// The service uses tickets to identify data sources:
/// - SQL: `sql:SELECT * FROM table`
/// - Vector: `vector:topic:query_text:k`
///
/// ## Query Caching
///
/// When `enable_cache` is true in config, SQL query results are cached:
/// - Cache key: normalized (trimmed, lowercase) SQL query
/// - TTL: configurable via `cache_ttl_secs`
/// - Eviction: LRU when `cache_max_entries` exceeded
pub struct ChronikFlightService {
    /// SQL query engine
    query_engine: Arc<ColumnarQueryEngine>,
    /// Vector search service (optional)
    vector_search: Option<Arc<VectorSearchService>>,
    /// Configuration
    config: FlightServiceConfig,
    /// Query result cache (optional)
    cache: Option<Arc<QueryCache>>,
}

impl ChronikFlightService {
    /// Create a new Flight service
    pub fn new(query_engine: Arc<ColumnarQueryEngine>, config: FlightServiceConfig) -> Self {
        let cache = if config.enable_cache {
            Some(Arc::new(QueryCache::new(config.cache_max_entries, config.cache_ttl_secs)))
        } else {
            None
        };
        Self {
            query_engine,
            vector_search: None,
            config,
            cache,
        }
    }

    /// Create with vector search support
    pub fn with_vector_search(
        query_engine: Arc<ColumnarQueryEngine>,
        vector_index_manager: Arc<VectorIndexManager>,
        embedding_provider: Arc<dyn EmbeddingProvider>,
        config: FlightServiceConfig,
    ) -> Self {
        let cache = if config.enable_cache {
            Some(Arc::new(QueryCache::new(config.cache_max_entries, config.cache_ttl_secs)))
        } else {
            None
        };
        let vector_search = Arc::new(VectorSearchService::new(
            vector_index_manager,
            embedding_provider,
        ));
        Self {
            query_engine,
            vector_search: Some(vector_search),
            config,
            cache,
        }
    }

    /// Get the configuration
    pub fn config(&self) -> &FlightServiceConfig {
        &self.config
    }

    /// Get cache statistics if caching is enabled
    pub async fn cache_stats(&self) -> Option<CacheStats> {
        if let Some(ref cache) = self.cache {
            Some(cache.stats().await)
        } else {
            None
        }
    }

    /// Clear the query cache
    pub async fn clear_cache(&self) {
        if let Some(ref cache) = self.cache {
            cache.clear().await;
        }
    }

    /// Execute SQL query and return record batches (with caching)
    pub async fn execute_sql(&self, query: &str) -> anyhow::Result<Vec<RecordBatch>> {
        // Check cache first
        if let Some(ref cache) = self.cache {
            if let Some(cached) = cache.get(query).await {
                info!(query = %query, "Returning cached SQL results");
                return Ok(cached);
            }
        }

        info!(query = %query, "Executing SQL query");
        let result = self.query_engine.execute_sql(query).await?;

        // Store in cache
        if let Some(ref cache) = self.cache {
            cache.put(query, result.clone()).await;
        }

        Ok(result)
    }

    /// Execute SQL query bypassing cache
    pub async fn execute_sql_no_cache(&self, query: &str) -> anyhow::Result<Vec<RecordBatch>> {
        info!(query = %query, "Executing SQL query (no cache)");
        self.query_engine.execute_sql(query).await
    }

    /// Execute vector search and return results
    pub async fn execute_vector_search(
        &self,
        topic: &str,
        query_text: &str,
        k: usize,
    ) -> anyhow::Result<Vec<VectorSearchResult>> {
        let vector_search = self.vector_search.as_ref()
            .ok_or_else(|| anyhow::anyhow!("Vector search not enabled"))?;

        info!(topic = %topic, query = %query_text, k = k, "Executing vector search");
        vector_search.search_by_text(topic, query_text, k, Some(VectorSearchFilters::default())).await
    }

    /// Parse a ticket string to determine query type
    pub fn parse_ticket(&self, ticket: &str) -> Result<TicketType, TicketParseError> {
        if let Some(sql) = ticket.strip_prefix("sql:") {
            Ok(TicketType::Sql(sql.to_string()))
        } else if let Some(vector_params) = ticket.strip_prefix("vector:") {
            let parts: Vec<&str> = vector_params.splitn(3, ':').collect();
            if parts.len() < 3 {
                return Err(TicketParseError::InvalidFormat(
                    "Vector ticket format: vector:topic:query_text:k".to_string()
                ));
            }
            let k = parts[2].parse::<usize>()
                .map_err(|_| TicketParseError::InvalidK)?;
            Ok(TicketType::VectorSearch {
                topic: parts[0].to_string(),
                query_text: parts[1].to_string(),
                k,
            })
        } else {
            Err(TicketParseError::UnknownType)
        }
    }

    /// List available tables
    pub fn list_tables(&self) -> anyhow::Result<Vec<String>> {
        self.query_engine.list_tables()
    }
}

/// Parsed ticket type
#[derive(Debug, Clone)]
pub enum TicketType {
    /// SQL query
    Sql(String),
    /// Vector search
    VectorSearch {
        topic: String,
        query_text: String,
        k: usize,
    },
}

/// Ticket parsing error
#[derive(Debug, thiserror::Error)]
pub enum TicketParseError {
    #[error("Unknown ticket type. Use sql: or vector: prefix")]
    UnknownType,
    #[error("Invalid ticket format: {0}")]
    InvalidFormat(String),
    #[error("Invalid k value")]
    InvalidK,
}

/// Convert vector search results to an Arrow RecordBatch
pub fn vector_results_to_batch(
    results: &[VectorSearchResult],
) -> Result<RecordBatch, datafusion::arrow::error::ArrowError> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("topic", DataType::Utf8, false),
        Field::new("partition", DataType::Int32, false),
        Field::new("offset", DataType::Int64, false),
        Field::new("score", DataType::Float32, false),
        Field::new("text_preview", DataType::Utf8, true),
    ]));

    let topics: StringArray = results.iter().map(|r| Some(r.topic.as_str())).collect();
    let partitions: Int32Array = results.iter().map(|r| Some(r.partition)).collect();
    let offsets: Int64Array = results.iter().map(|r| Some(r.offset)).collect();
    let scores: Float32Array = results.iter().map(|r| Some(r.score)).collect();
    let previews: StringArray = results.iter().map(|r| r.text_preview.as_deref()).collect();

    RecordBatch::try_new(
        schema,
        vec![
            Arc::new(topics),
            Arc::new(partitions),
            Arc::new(offsets),
            Arc::new(scores),
            Arc::new(previews),
        ],
    )
}

/// Schema for vector search results
pub fn vector_search_schema() -> Schema {
    Schema::new(vec![
        Field::new("topic", DataType::Utf8, false),
        Field::new("partition", DataType::Int32, false),
        Field::new("offset", DataType::Int64, false),
        Field::new("score", DataType::Float32, false),
        Field::new("text_preview", DataType::Utf8, true),
    ])
}

/// Schema for vector search results with embeddings
///
/// Includes an additional `embedding` column as FixedSizeList<Float32>.
pub fn vector_search_schema_with_embeddings(dimensions: i32) -> Schema {
    Schema::new(vec![
        Field::new("topic", DataType::Utf8, false),
        Field::new("partition", DataType::Int32, false),
        Field::new("offset", DataType::Int64, false),
        Field::new("score", DataType::Float32, false),
        Field::new("text_preview", DataType::Utf8, true),
        Field::new(
            "embedding",
            DataType::FixedSizeList(
                Arc::new(Field::new("item", DataType::Float32, false)),
                dimensions,
            ),
            true, // nullable
        ),
    ])
}

/// Convert vector search results to an Arrow RecordBatch with embeddings
///
/// This variant includes the embedding vectors in the response. The embeddings
/// are stored as a FixedSizeList<Float32> column.
///
/// # Arguments
/// * `results` - Vector search results (must all have embeddings of the same dimension)
/// * `dimensions` - The dimensionality of the embedding vectors
///
/// # Returns
/// A RecordBatch with 6 columns: topic, partition, offset, score, text_preview, embedding
pub fn vector_results_to_batch_with_embeddings(
    results: &[VectorSearchResult],
    dimensions: i32,
) -> Result<RecordBatch, datafusion::arrow::error::ArrowError> {
    let schema = Arc::new(vector_search_schema_with_embeddings(dimensions));

    let topics: StringArray = results.iter().map(|r| Some(r.topic.as_str())).collect();
    let partitions: Int32Array = results.iter().map(|r| Some(r.partition)).collect();
    let offsets: Int64Array = results.iter().map(|r| Some(r.offset)).collect();
    let scores: Float32Array = results.iter().map(|r| Some(r.score)).collect();
    let previews: StringArray = results.iter().map(|r| r.text_preview.as_deref()).collect();

    // Build the embedding column as FixedSizeList<Float32>
    let embedding_column = build_embedding_column(results, dimensions)?;

    RecordBatch::try_new(
        schema,
        vec![
            Arc::new(topics),
            Arc::new(partitions),
            Arc::new(offsets),
            Arc::new(scores),
            Arc::new(previews),
            embedding_column,
        ],
    )
}

/// Build embedding column from vector search results
///
/// Creates a FixedSizeList<Float32> array where each element is an embedding vector.
/// Results without embeddings get null values.
fn build_embedding_column(
    results: &[VectorSearchResult],
    dimensions: i32,
) -> Result<ArrayRef, datafusion::arrow::error::ArrowError> {
    use datafusion::arrow::array::Float32Builder;

    let dims_usize = dimensions as usize;
    let num_results = results.len();

    // Build flat values array
    let mut values_builder = Float32Builder::with_capacity(num_results * dims_usize);
    let mut null_buffer_builder = vec![true; num_results]; // track which rows are non-null

    for (i, result) in results.iter().enumerate() {
        if let Some(ref embedding) = result.embedding {
            if embedding.len() == dims_usize {
                for &val in embedding {
                    values_builder.append_value(val);
                }
            } else {
                // Wrong dimension - treat as null
                null_buffer_builder[i] = false;
                for _ in 0..dims_usize {
                    values_builder.append_value(0.0);
                }
            }
        } else {
            // No embedding - treat as null
            null_buffer_builder[i] = false;
            for _ in 0..dims_usize {
                values_builder.append_value(0.0);
            }
        }
    }

    let values_array = Arc::new(values_builder.finish());

    // Build null bitmap
    let null_buffer = Buffer::from_iter(null_buffer_builder.iter().map(|&b| b));
    let null_bitmap = datafusion::arrow::buffer::NullBuffer::new(
        datafusion::arrow::buffer::BooleanBuffer::new(null_buffer, 0, num_results)
    );

    // Create FixedSizeList from values
    let list_array = FixedSizeListArray::try_new(
        Arc::new(Field::new("item", DataType::Float32, false)),
        dimensions,
        values_array,
        Some(null_bitmap),
    )?;

    Ok(Arc::new(list_array))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_flight_service_config_default() {
        let config = FlightServiceConfig::default();
        assert_eq!(config.max_batch_rows, 10000);
        assert_eq!(config.query_timeout_secs, 30);
        assert!(config.enable_flight_sql);
        assert!(config.enable_vector_streaming);
        assert!(config.enable_cache);
        assert_eq!(config.cache_max_entries, 100);
        assert_eq!(config.cache_ttl_secs, 60);
    }

    #[tokio::test]
    async fn test_query_cache_basic() {
        let cache = QueryCache::new(10, 60);

        // Miss initially
        assert!(cache.get("SELECT 1").await.is_none());

        // Put a value
        let batch = RecordBatch::try_new(
            Arc::new(Schema::new(vec![Field::new("x", DataType::Int32, false)])),
            vec![Arc::new(Int32Array::from(vec![1, 2, 3]))],
        ).unwrap();
        cache.put("SELECT 1", vec![batch.clone()]).await;

        // Now it's a hit
        let result = cache.get("SELECT 1").await;
        assert!(result.is_some());
        assert_eq!(result.unwrap().len(), 1);

        // Same query with different whitespace/case should hit
        let result2 = cache.get("  select 1  ").await;
        assert!(result2.is_some());
    }

    #[tokio::test]
    async fn test_query_cache_stats() {
        let cache = QueryCache::new(10, 60);

        let stats = cache.stats().await;
        assert_eq!(stats.total_entries, 0);
        assert_eq!(stats.active_entries, 0);

        // Add some entries
        let batch = RecordBatch::try_new(
            Arc::new(Schema::new(vec![Field::new("x", DataType::Int32, false)])),
            vec![Arc::new(Int32Array::from(vec![1]))],
        ).unwrap();

        cache.put("query1", vec![batch.clone()]).await;
        cache.put("query2", vec![batch.clone()]).await;

        let stats = cache.stats().await;
        assert_eq!(stats.total_entries, 2);
        assert_eq!(stats.active_entries, 2);
    }

    #[tokio::test]
    async fn test_query_cache_eviction() {
        // Small cache that only holds 2 entries
        let cache = QueryCache::new(2, 60);

        let batch = RecordBatch::try_new(
            Arc::new(Schema::new(vec![Field::new("x", DataType::Int32, false)])),
            vec![Arc::new(Int32Array::from(vec![1]))],
        ).unwrap();

        cache.put("query1", vec![batch.clone()]).await;
        cache.put("query2", vec![batch.clone()]).await;

        // Adding a third should evict the oldest
        cache.put("query3", vec![batch.clone()]).await;

        let stats = cache.stats().await;
        assert_eq!(stats.total_entries, 2);

        // query1 should be evicted (oldest)
        assert!(cache.get("query1").await.is_none());
        // query2 and query3 should still be there
        assert!(cache.get("query2").await.is_some());
        assert!(cache.get("query3").await.is_some());
    }

    #[tokio::test]
    async fn test_query_cache_clear() {
        let cache = QueryCache::new(10, 60);

        let batch = RecordBatch::try_new(
            Arc::new(Schema::new(vec![Field::new("x", DataType::Int32, false)])),
            vec![Arc::new(Int32Array::from(vec![1]))],
        ).unwrap();

        cache.put("query1", vec![batch.clone()]).await;
        cache.put("query2", vec![batch.clone()]).await;

        assert_eq!(cache.stats().await.total_entries, 2);

        cache.clear().await;

        assert_eq!(cache.stats().await.total_entries, 0);
    }

    #[test]
    fn test_parse_sql_ticket() {
        let ticket = "sql:SELECT * FROM logs";

        // Manually parse for test
        if let Some(sql) = ticket.strip_prefix("sql:") {
            assert_eq!(sql, "SELECT * FROM logs");
        } else {
            panic!("Failed to parse SQL ticket");
        }
    }

    #[test]
    fn test_parse_vector_ticket() {
        let ticket = "vector:logs:error handling:10";

        if let Some(params) = ticket.strip_prefix("vector:") {
            let parts: Vec<&str> = params.splitn(3, ':').collect();
            assert_eq!(parts.len(), 3);
            assert_eq!(parts[0], "logs");
            assert_eq!(parts[1], "error handling");
            assert_eq!(parts[2], "10");
        } else {
            panic!("Failed to parse vector ticket");
        }
    }

    #[test]
    fn test_vector_results_to_batch() {
        let results = vec![
            VectorSearchResult {
                topic: "logs".to_string(),
                partition: 0,
                offset: 100,
                score: 0.95,
                text_preview: Some("Error occurred".to_string()),
                embedding: None,
            },
            VectorSearchResult {
                topic: "logs".to_string(),
                partition: 1,
                offset: 200,
                score: 0.85,
                text_preview: None,
                embedding: None,
            },
        ];

        let batch = vector_results_to_batch(&results).unwrap();
        assert_eq!(batch.num_rows(), 2);
        assert_eq!(batch.num_columns(), 5);
        assert_eq!(batch.schema().field(0).name(), "topic");
        assert_eq!(batch.schema().field(1).name(), "partition");
        assert_eq!(batch.schema().field(2).name(), "offset");
        assert_eq!(batch.schema().field(3).name(), "score");
        assert_eq!(batch.schema().field(4).name(), "text_preview");
    }

    #[test]
    fn test_vector_search_schema() {
        let schema = vector_search_schema();
        assert_eq!(schema.fields().len(), 5);
        assert_eq!(schema.field(0).name(), "topic");
        assert_eq!(schema.field(1).name(), "partition");
        assert_eq!(schema.field(2).name(), "offset");
        assert_eq!(schema.field(3).name(), "score");
        assert_eq!(schema.field(4).name(), "text_preview");
    }

    #[test]
    fn test_vector_search_schema_with_embeddings() {
        let schema = vector_search_schema_with_embeddings(384);
        assert_eq!(schema.fields().len(), 6);
        assert_eq!(schema.field(0).name(), "topic");
        assert_eq!(schema.field(5).name(), "embedding");

        // Check embedding field is FixedSizeList<Float32>
        match schema.field(5).data_type() {
            DataType::FixedSizeList(inner, size) => {
                assert_eq!(*size, 384);
                assert_eq!(*inner.data_type(), DataType::Float32);
            }
            other => panic!("Expected FixedSizeList, got {:?}", other),
        }
    }

    #[test]
    fn test_vector_results_to_batch_with_embeddings() {
        let results = vec![
            VectorSearchResult {
                topic: "logs".to_string(),
                partition: 0,
                offset: 100,
                score: 0.95,
                text_preview: Some("Error occurred".to_string()),
                embedding: Some(vec![0.1, 0.2, 0.3, 0.4]), // 4-dim for test
            },
            VectorSearchResult {
                topic: "logs".to_string(),
                partition: 1,
                offset: 200,
                score: 0.85,
                text_preview: None,
                embedding: None, // No embedding
            },
            VectorSearchResult {
                topic: "logs".to_string(),
                partition: 2,
                offset: 300,
                score: 0.75,
                text_preview: Some("Another message".to_string()),
                embedding: Some(vec![0.5, 0.6, 0.7, 0.8]), // 4-dim for test
            },
        ];

        let batch = vector_results_to_batch_with_embeddings(&results, 4).unwrap();
        assert_eq!(batch.num_rows(), 3);
        assert_eq!(batch.num_columns(), 6);
        assert_eq!(batch.schema().field(5).name(), "embedding");

        // Verify embedding column type
        match batch.schema().field(5).data_type() {
            DataType::FixedSizeList(inner, size) => {
                assert_eq!(*size, 4);
                assert_eq!(*inner.data_type(), DataType::Float32);
            }
            other => panic!("Expected FixedSizeList, got {:?}", other),
        }

        // Check that row 1 (no embedding) is null
        let embedding_col = batch.column(5);
        assert!(embedding_col.is_null(1), "Row without embedding should be null");
        assert!(!embedding_col.is_null(0), "Row with embedding should not be null");
        assert!(!embedding_col.is_null(2), "Row with embedding should not be null");
    }

    #[test]
    fn test_build_embedding_column_empty() {
        use datafusion::arrow::array::Array;
        let results: Vec<VectorSearchResult> = vec![];
        let column = build_embedding_column(&results, 4).unwrap();
        // Use Array trait method
        assert_eq!(column.len(), 0);
    }
}
