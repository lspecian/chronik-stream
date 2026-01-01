//! Vector Search Handler for Unified API
//!
//! Provides vector/semantic search endpoints:
//! - POST `/_vector/:topic/search` - Search by text query
//! - POST `/_vector/:topic/search_by_vector` - Search by raw vector
//! - GET `/_vector/:topic/stats` - Get index statistics
//! - GET `/_vector/topics` - List topics with vector indexes

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info};

use chronik_columnar::{VectorSearchFilters, VectorSearchResult, VectorSearchService};

use super::UnifiedApiState;

/// Text-based vector search request
#[derive(Debug, Deserialize)]
pub struct VectorSearchRequest {
    /// Text query to search for (will be embedded)
    pub query: String,
    /// Number of results to return (default: 10)
    #[serde(default = "default_k")]
    pub k: usize,
    /// Filter to specific partitions
    #[serde(default)]
    pub partitions: Option<Vec<i32>>,
    /// Minimum offset filter
    #[serde(default)]
    pub min_offset: Option<i64>,
    /// Maximum offset filter
    #[serde(default)]
    pub max_offset: Option<i64>,
}

fn default_k() -> usize {
    10
}

/// Raw vector search request
#[derive(Debug, Deserialize)]
pub struct VectorSearchByVectorRequest {
    /// Query embedding vector
    pub vector: Vec<f32>,
    /// Number of results to return (default: 10)
    #[serde(default = "default_k")]
    pub k: usize,
    /// Filter to specific partitions
    #[serde(default)]
    pub partitions: Option<Vec<i32>>,
    /// Minimum offset filter
    #[serde(default)]
    pub min_offset: Option<i64>,
    /// Maximum offset filter
    #[serde(default)]
    pub max_offset: Option<i64>,
}

/// Vector search response
#[derive(Debug, Serialize)]
pub struct VectorSearchResponse {
    /// Search results
    pub results: Vec<VectorSearchResultItem>,
    /// Number of results returned
    pub count: usize,
    /// Total indexed vectors for topic
    pub total_vectors: usize,
    /// Query execution time in milliseconds
    pub execution_time_ms: u64,
}

/// Single search result item
#[derive(Debug, Serialize)]
pub struct VectorSearchResultItem {
    /// Partition number
    pub partition: i32,
    /// Message offset
    pub offset: i64,
    /// Similarity/distance score
    pub score: f32,
    /// Text preview (if available)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_preview: Option<String>,
}

impl From<VectorSearchResult> for VectorSearchResultItem {
    fn from(r: VectorSearchResult) -> Self {
        Self {
            partition: r.partition,
            offset: r.offset,
            score: r.score,
            text_preview: r.text_preview,
        }
    }
}

/// Error response
#[derive(Debug, Serialize)]
pub struct VectorErrorResponse {
    pub error: String,
    pub error_type: String,
}

/// Vector handler for direct usage
pub struct VectorHandler;

impl VectorHandler {
    /// Search by text query
    pub async fn search_by_text(
        state: &UnifiedApiState,
        topic: &str,
        query: &str,
        k: usize,
        filters: Option<VectorSearchFilters>,
    ) -> Result<Vec<VectorSearchResult>, String> {
        let index_manager = state
            .vector_index_manager
            .as_ref()
            .ok_or("Vector index manager not available")?;

        let provider = state
            .embedding_provider
            .as_ref()
            .ok_or("Embedding provider not configured")?;

        let service = VectorSearchService::new(
            std::sync::Arc::clone(index_manager),
            std::sync::Arc::clone(provider),
        );

        service
            .search_by_text(topic, query, k, filters)
            .await
            .map_err(|e| format!("Search failed: {}", e))
    }

    /// Search by raw vector
    pub async fn search_by_vector(
        state: &UnifiedApiState,
        topic: &str,
        vector: &[f32],
        k: usize,
        filters: Option<VectorSearchFilters>,
    ) -> Result<Vec<VectorSearchResult>, String> {
        let index_manager = state
            .vector_index_manager
            .as_ref()
            .ok_or("Vector index manager not available")?;

        let provider = state
            .embedding_provider
            .as_ref()
            .ok_or("Embedding provider not configured")?;

        let service = VectorSearchService::new(
            std::sync::Arc::clone(index_manager),
            std::sync::Arc::clone(provider),
        );

        service
            .search_by_vector(topic, vector, k, filters)
            .await
            .map_err(|e| format!("Search failed: {}", e))
    }
}

/// Convert request filters to VectorSearchFilters
fn build_filters(
    partitions: Option<Vec<i32>>,
    min_offset: Option<i64>,
    max_offset: Option<i64>,
) -> Option<VectorSearchFilters> {
    if partitions.is_none() && min_offset.is_none() && max_offset.is_none() {
        return None;
    }

    let mut filters = VectorSearchFilters::new();

    if let Some(parts) = partitions {
        filters = filters.with_partitions(parts);
    }
    if let Some(min) = min_offset {
        filters = filters.with_min_offset(min);
    }
    if let Some(max) = max_offset {
        filters = filters.with_max_offset(max);
    }

    Some(filters)
}

/// Search by text query endpoint
pub async fn search(
    State(state): State<UnifiedApiState>,
    Path(topic): Path<String>,
    Json(request): Json<VectorSearchRequest>,
) -> impl IntoResponse {
    info!(topic = %topic, query = %request.query, k = request.k, "Vector search by text");

    let start = std::time::Instant::now();

    // Check if vector search is available
    let index_manager = match &state.vector_index_manager {
        Some(m) => m,
        None => {
            let error_response = VectorErrorResponse {
                error: "Vector search not available".to_string(),
                error_type: "ServiceUnavailable".to_string(),
            };
            return (StatusCode::SERVICE_UNAVAILABLE, Json(error_response)).into_response();
        }
    };

    let provider = match &state.embedding_provider {
        Some(p) => p,
        None => {
            let error_response = VectorErrorResponse {
                error: "Embedding provider not configured".to_string(),
                error_type: "ServiceUnavailable".to_string(),
            };
            return (StatusCode::SERVICE_UNAVAILABLE, Json(error_response)).into_response();
        }
    };

    let filters = build_filters(request.partitions, request.min_offset, request.max_offset);

    let service = VectorSearchService::new(
        std::sync::Arc::clone(index_manager),
        std::sync::Arc::clone(provider),
    );

    match service.search_by_text(&topic, &request.query, request.k, filters).await {
        Ok(results) => {
            let total_vectors = service.get_vector_count(&topic).await;
            let execution_time_ms = start.elapsed().as_millis() as u64;

            let response = VectorSearchResponse {
                count: results.len(),
                results: results.into_iter().map(Into::into).collect(),
                total_vectors,
                execution_time_ms,
            };

            info!(
                topic = %topic,
                results = response.count,
                time_ms = execution_time_ms,
                "Vector search completed"
            );

            (StatusCode::OK, Json(response)).into_response()
        }
        Err(e) => {
            error!(topic = %topic, error = %e, "Vector search failed");
            let error_response = VectorErrorResponse {
                error: e.to_string(),
                error_type: "SearchError".to_string(),
            };
            (StatusCode::INTERNAL_SERVER_ERROR, Json(error_response)).into_response()
        }
    }
}

/// Search by raw vector endpoint
pub async fn search_by_vector(
    State(state): State<UnifiedApiState>,
    Path(topic): Path<String>,
    Json(request): Json<VectorSearchByVectorRequest>,
) -> impl IntoResponse {
    info!(
        topic = %topic,
        vector_len = request.vector.len(),
        k = request.k,
        "Vector search by vector"
    );

    let start = std::time::Instant::now();

    // Check if vector search is available
    let index_manager = match &state.vector_index_manager {
        Some(m) => m,
        None => {
            let error_response = VectorErrorResponse {
                error: "Vector search not available".to_string(),
                error_type: "ServiceUnavailable".to_string(),
            };
            return (StatusCode::SERVICE_UNAVAILABLE, Json(error_response)).into_response();
        }
    };

    let provider = match &state.embedding_provider {
        Some(p) => p,
        None => {
            let error_response = VectorErrorResponse {
                error: "Embedding provider not configured".to_string(),
                error_type: "ServiceUnavailable".to_string(),
            };
            return (StatusCode::SERVICE_UNAVAILABLE, Json(error_response)).into_response();
        }
    };

    let filters = build_filters(request.partitions, request.min_offset, request.max_offset);

    let service = VectorSearchService::new(
        std::sync::Arc::clone(index_manager),
        std::sync::Arc::clone(provider),
    );

    match service.search_by_vector(&topic, &request.vector, request.k, filters).await {
        Ok(results) => {
            let total_vectors = service.get_vector_count(&topic).await;
            let execution_time_ms = start.elapsed().as_millis() as u64;

            let response = VectorSearchResponse {
                count: results.len(),
                results: results.into_iter().map(Into::into).collect(),
                total_vectors,
                execution_time_ms,
            };

            info!(
                topic = %topic,
                results = response.count,
                time_ms = execution_time_ms,
                "Vector search by vector completed"
            );

            (StatusCode::OK, Json(response)).into_response()
        }
        Err(e) => {
            error!(topic = %topic, error = %e, "Vector search by vector failed");
            let error_response = VectorErrorResponse {
                error: e.to_string(),
                error_type: "SearchError".to_string(),
            };
            (StatusCode::INTERNAL_SERVER_ERROR, Json(error_response)).into_response()
        }
    }
}

/// Topic statistics response
#[derive(Debug, Serialize)]
pub struct TopicStatsResponse {
    pub topic: String,
    pub total_vectors: usize,
    pub partition_count: usize,
    pub dimensions: usize,
    pub partitions: Vec<PartitionStats>,
}

/// Per-partition statistics
#[derive(Debug, Serialize)]
pub struct PartitionStats {
    pub partition: i32,
    pub vector_count: usize,
}

/// Get topic vector index statistics
pub async fn get_stats(
    State(state): State<UnifiedApiState>,
    Path(topic): Path<String>,
) -> impl IntoResponse {
    debug!(topic = %topic, "Getting vector index stats");

    let index_manager = match &state.vector_index_manager {
        Some(m) => m,
        None => {
            let error_response = VectorErrorResponse {
                error: "Vector search not available".to_string(),
                error_type: "ServiceUnavailable".to_string(),
            };
            return (StatusCode::SERVICE_UNAVAILABLE, Json(error_response)).into_response();
        }
    };

    let stats = index_manager.get_topic_stats(&topic).await;

    let response = TopicStatsResponse {
        topic: topic.clone(),
        total_vectors: stats.total_vectors,
        partition_count: stats.partition_count,
        dimensions: stats.dimensions,
        partitions: stats
            .partitions
            .into_iter()
            .map(|(partition, ps)| PartitionStats {
                partition,
                vector_count: ps.vector_count,
            })
            .collect(),
    };

    (StatusCode::OK, Json(response)).into_response()
}

/// List topics with vector indexes
#[derive(Debug, Serialize)]
pub struct ListTopicsResponse {
    pub topics: Vec<String>,
    pub count: usize,
}

/// List all topics with vector indexes
pub async fn list_topics(State(state): State<UnifiedApiState>) -> impl IntoResponse {
    debug!("Listing vector-enabled topics");

    let index_manager = match &state.vector_index_manager {
        Some(m) => m,
        None => {
            let error_response = VectorErrorResponse {
                error: "Vector search not available".to_string(),
                error_type: "ServiceUnavailable".to_string(),
            };
            return (StatusCode::SERVICE_UNAVAILABLE, Json(error_response)).into_response();
        }
    };

    let topics = index_manager.list_topics().await;
    let count = topics.len();

    let response = ListTopicsResponse { topics, count };
    (StatusCode::OK, Json(response)).into_response()
}

// ============================================================================
// Hybrid Search (Phase 6.4.4) - Combines Vector + Text Search
// ============================================================================

/// Hybrid search request combining vector and text search
#[derive(Debug, Deserialize)]
pub struct HybridSearchRequest {
    /// Text query for both vector embedding and full-text search
    pub query: String,
    /// Number of results to return (default: 10)
    #[serde(default = "default_k")]
    pub k: usize,
    /// Weight for vector search results (0.0 to 1.0, default: 0.5)
    /// Higher = more weight on semantic similarity
    #[serde(default = "default_vector_weight")]
    pub vector_weight: f32,
    /// Weight for text search results (0.0 to 1.0, default: 0.5)
    /// Higher = more weight on keyword matching
    #[serde(default = "default_text_weight")]
    pub text_weight: f32,
    /// RRF constant k (default: 60)
    /// Higher values give more weight to lower-ranked results
    #[serde(default = "default_rrf_k")]
    pub rrf_k: usize,
    /// Filter to specific partitions
    #[serde(default)]
    pub partitions: Option<Vec<i32>>,
    /// Minimum offset filter
    #[serde(default)]
    pub min_offset: Option<i64>,
    /// Maximum offset filter
    #[serde(default)]
    pub max_offset: Option<i64>,
}

fn default_vector_weight() -> f32 {
    0.5
}

fn default_text_weight() -> f32 {
    0.5
}

fn default_rrf_k() -> usize {
    60
}

/// Hybrid search response
#[derive(Debug, Serialize)]
pub struct HybridSearchResponse {
    /// Fused search results
    pub results: Vec<HybridSearchResultItem>,
    /// Number of results returned
    pub count: usize,
    /// Number of vector search results before fusion
    pub vector_results_count: usize,
    /// Number of text search results before fusion
    pub text_results_count: usize,
    /// Query execution time in milliseconds
    pub execution_time_ms: u64,
}

/// Single hybrid search result item
#[derive(Debug, Serialize)]
pub struct HybridSearchResultItem {
    /// Partition number
    pub partition: i32,
    /// Message offset
    pub offset: i64,
    /// Fused score (RRF-based, higher = more relevant)
    pub score: f32,
    /// Vector search rank (None if not in vector results)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vector_rank: Option<usize>,
    /// Text search rank (None if not in text results)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_rank: Option<usize>,
    /// Text preview (if available)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_preview: Option<String>,
}

/// Reciprocal Rank Fusion (RRF) score calculation
///
/// RRF combines rankings from multiple search systems by computing:
/// score(d) = Σ 1 / (k + rank_i(d))
///
/// Where:
/// - k is a constant (typically 60) to prevent high-ranked items from dominating
/// - rank_i(d) is the rank of document d in result set i
///
/// This approach is robust to score scale differences between systems.
fn calculate_rrf_score(
    vector_rank: Option<usize>,
    text_rank: Option<usize>,
    vector_weight: f32,
    text_weight: f32,
    rrf_k: usize,
) -> f32 {
    let mut score = 0.0;

    if let Some(rank) = vector_rank {
        score += vector_weight / (rrf_k as f32 + rank as f32);
    }

    if let Some(rank) = text_rank {
        score += text_weight / (rrf_k as f32 + rank as f32);
    }

    score
}

/// Hybrid search endpoint - combines vector semantic search with full-text search
///
/// Uses Reciprocal Rank Fusion (RRF) to merge results from both search types,
/// providing the best of both worlds: semantic understanding and keyword matching.
pub async fn hybrid_search(
    State(state): State<UnifiedApiState>,
    Path(topic): Path<String>,
    Json(request): Json<HybridSearchRequest>,
) -> impl IntoResponse {
    info!(
        topic = %topic,
        query = %request.query,
        k = request.k,
        vector_weight = request.vector_weight,
        text_weight = request.text_weight,
        "Hybrid search"
    );

    let start = std::time::Instant::now();

    // Check if vector search is available
    let index_manager = match &state.vector_index_manager {
        Some(m) => m,
        None => {
            let error_response = VectorErrorResponse {
                error: "Vector search not available".to_string(),
                error_type: "ServiceUnavailable".to_string(),
            };
            return (StatusCode::SERVICE_UNAVAILABLE, Json(error_response)).into_response();
        }
    };

    let provider = match &state.embedding_provider {
        Some(p) => p,
        None => {
            let error_response = VectorErrorResponse {
                error: "Embedding provider not configured".to_string(),
                error_type: "ServiceUnavailable".to_string(),
            };
            return (StatusCode::SERVICE_UNAVAILABLE, Json(error_response)).into_response();
        }
    };

    let filters = build_filters(request.partitions, request.min_offset, request.max_offset);

    // Request more results than k for better fusion
    let search_k = request.k * 3;

    // Step 1: Vector semantic search
    let service = VectorSearchService::new(
        std::sync::Arc::clone(index_manager),
        std::sync::Arc::clone(provider),
    );

    let vector_results = match service
        .search_by_text(&topic, &request.query, search_k, filters.clone())
        .await
    {
        Ok(results) => results,
        Err(e) => {
            error!(topic = %topic, error = %e, "Vector search failed in hybrid");
            Vec::new() // Continue with text search only
        }
    };

    let vector_results_count = vector_results.len();

    // Step 2: Text search (using Tantivy via search API if available)
    // For now, we'll simulate text search results. In production, this would
    // call into the chronik-search crate's SearchApi.
    // TODO: Integrate with actual full-text search when search endpoints are migrated
    let text_results: Vec<(i32, i64)> = Vec::new(); // (partition, offset) pairs
    let text_results_count = text_results.len();

    // Step 3: Build a map of all unique (partition, offset) pairs
    use std::collections::HashMap;
    #[derive(Hash, Eq, PartialEq, Clone)]
    struct DocKey {
        partition: i32,
        offset: i64,
    }

    struct DocInfo {
        vector_rank: Option<usize>,
        text_rank: Option<usize>,
        text_preview: Option<String>,
    }

    let mut doc_map: HashMap<DocKey, DocInfo> = HashMap::new();

    // Add vector results
    for (rank, result) in vector_results.iter().enumerate() {
        let key = DocKey {
            partition: result.partition,
            offset: result.offset,
        };
        doc_map.insert(
            key,
            DocInfo {
                vector_rank: Some(rank + 1), // Ranks start at 1
                text_rank: None,
                text_preview: result.text_preview.clone(),
            },
        );
    }

    // Add/update text results
    for (rank, (partition, offset)) in text_results.iter().enumerate() {
        let key = DocKey {
            partition: *partition,
            offset: *offset,
        };
        if let Some(info) = doc_map.get_mut(&key) {
            info.text_rank = Some(rank + 1);
        } else {
            doc_map.insert(
                key,
                DocInfo {
                    vector_rank: None,
                    text_rank: Some(rank + 1),
                    text_preview: None,
                },
            );
        }
    }

    // Step 4: Calculate RRF scores and sort
    let mut results: Vec<HybridSearchResultItem> = doc_map
        .into_iter()
        .map(|(key, info)| {
            let score = calculate_rrf_score(
                info.vector_rank,
                info.text_rank,
                request.vector_weight,
                request.text_weight,
                request.rrf_k,
            );

            HybridSearchResultItem {
                partition: key.partition,
                offset: key.offset,
                score,
                vector_rank: info.vector_rank,
                text_rank: info.text_rank,
                text_preview: info.text_preview,
            }
        })
        .collect();

    // Sort by score descending (higher = more relevant)
    results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));

    // Take top k
    results.truncate(request.k);

    let execution_time_ms = start.elapsed().as_millis() as u64;
    let count = results.len();

    info!(
        topic = %topic,
        results = count,
        vector_results = vector_results_count,
        text_results = text_results_count,
        time_ms = execution_time_ms,
        "Hybrid search completed"
    );

    let response = HybridSearchResponse {
        results,
        count,
        vector_results_count,
        text_results_count,
        execution_time_ms,
    };

    (StatusCode::OK, Json(response)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_search_request_defaults() {
        let json = r#"{"query": "error logs"}"#;
        let request: VectorSearchRequest = serde_json::from_str(json).unwrap();
        assert_eq!(request.query, "error logs");
        assert_eq!(request.k, 10);
        assert!(request.partitions.is_none());
    }

    #[test]
    fn test_search_request_with_filters() {
        let json = r#"{
            "query": "error logs",
            "k": 5,
            "partitions": [0, 1, 2],
            "min_offset": 100
        }"#;
        let request: VectorSearchRequest = serde_json::from_str(json).unwrap();
        assert_eq!(request.k, 5);
        assert_eq!(request.partitions, Some(vec![0, 1, 2]));
        assert_eq!(request.min_offset, Some(100));
    }

    #[test]
    fn test_build_filters_none() {
        let filters = build_filters(None, None, None);
        assert!(filters.is_none());
    }

    #[test]
    fn test_build_filters_with_partitions() {
        let filters = build_filters(Some(vec![1, 2]), None, None).unwrap();
        assert!(filters.matches_partition(1));
        assert!(filters.matches_partition(2));
        assert!(!filters.matches_partition(3));
    }

    // ========== Hybrid Search Tests ==========

    #[test]
    fn test_hybrid_search_request_defaults() {
        let json = r#"{"query": "error logs"}"#;
        let request: HybridSearchRequest = serde_json::from_str(json).unwrap();
        assert_eq!(request.query, "error logs");
        assert_eq!(request.k, 10);
        assert!((request.vector_weight - 0.5).abs() < f32::EPSILON);
        assert!((request.text_weight - 0.5).abs() < f32::EPSILON);
        assert_eq!(request.rrf_k, 60);
    }

    #[test]
    fn test_hybrid_search_request_custom_weights() {
        let json = r#"{
            "query": "error logs",
            "k": 20,
            "vector_weight": 0.7,
            "text_weight": 0.3,
            "rrf_k": 30
        }"#;
        let request: HybridSearchRequest = serde_json::from_str(json).unwrap();
        assert_eq!(request.k, 20);
        assert!((request.vector_weight - 0.7).abs() < f32::EPSILON);
        assert!((request.text_weight - 0.3).abs() < f32::EPSILON);
        assert_eq!(request.rrf_k, 30);
    }

    #[test]
    fn test_rrf_score_vector_only() {
        // Document ranked #1 in vector results, not in text results
        let score = calculate_rrf_score(Some(1), None, 0.5, 0.5, 60);
        // Expected: 0.5 / (60 + 1) = 0.5 / 61 ≈ 0.00819672
        assert!((score - 0.5 / 61.0).abs() < 0.0001);
    }

    #[test]
    fn test_rrf_score_text_only() {
        // Document ranked #1 in text results, not in vector results
        let score = calculate_rrf_score(None, Some(1), 0.5, 0.5, 60);
        // Expected: 0.5 / (60 + 1) = 0.5 / 61 ≈ 0.00819672
        assert!((score - 0.5 / 61.0).abs() < 0.0001);
    }

    #[test]
    fn test_rrf_score_both_sources() {
        // Document ranked #1 in both results
        let score = calculate_rrf_score(Some(1), Some(1), 0.5, 0.5, 60);
        // Expected: 0.5 / (60 + 1) + 0.5 / (60 + 1) = 1.0 / 61 ≈ 0.01639344
        assert!((score - 1.0 / 61.0).abs() < 0.0001);
    }

    #[test]
    fn test_rrf_score_different_ranks() {
        // Document ranked #1 in vector, #10 in text
        let score = calculate_rrf_score(Some(1), Some(10), 0.5, 0.5, 60);
        // Expected: 0.5 / (60 + 1) + 0.5 / (60 + 10) = 0.5/61 + 0.5/70
        let expected = 0.5 / 61.0 + 0.5 / 70.0;
        assert!((score - expected).abs() < 0.0001);
    }

    #[test]
    fn test_rrf_score_higher_weight_vector() {
        // Vector-heavy weighting
        let score = calculate_rrf_score(Some(1), Some(1), 0.8, 0.2, 60);
        // Expected: 0.8 / (60 + 1) + 0.2 / (60 + 1) = 1.0 / 61
        assert!((score - 1.0 / 61.0).abs() < 0.0001);
    }

    #[test]
    fn test_rrf_score_lower_k_increases_high_rank_weight() {
        // Lower k value (30) gives higher scores to top-ranked results
        let score_k30 = calculate_rrf_score(Some(1), None, 0.5, 0.5, 30);
        let score_k60 = calculate_rrf_score(Some(1), None, 0.5, 0.5, 60);
        // 0.5 / 31 > 0.5 / 61
        assert!(score_k30 > score_k60);
    }

    #[test]
    fn test_rrf_score_ordering() {
        // Higher ranks should produce higher scores
        let score_rank1 = calculate_rrf_score(Some(1), None, 0.5, 0.5, 60);
        let score_rank5 = calculate_rrf_score(Some(5), None, 0.5, 0.5, 60);
        let score_rank10 = calculate_rrf_score(Some(10), None, 0.5, 0.5, 60);

        assert!(score_rank1 > score_rank5);
        assert!(score_rank5 > score_rank10);
    }

    #[test]
    fn test_rrf_score_zero_when_no_results() {
        let score = calculate_rrf_score(None, None, 0.5, 0.5, 60);
        assert!((score - 0.0).abs() < f32::EPSILON);
    }
}
