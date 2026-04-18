//! Vector Search Handler for Unified API
//!
//! Provides vector/semantic search endpoints:
//! - POST `/_vector/:topic/search` - Search by text query
//! - POST `/_vector/:topic/search_by_vector` - Search by raw vector
//! - GET `/_vector/:topic/stats` - Get index statistics
//! - GET `/_vector/topics` - List topics with vector indexes

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info};

use chronik_columnar::{VectorSearchFilters, VectorSearchResult, VectorSearchService};
use chronik_columnar::hot_vector_index::HotVectorHit;

use super::UnifiedApiState;

/// HP-2.5: merge hot vector hits with cold `VectorSearchResult`s.
///
/// Dedup by `(partition, offset)` — hot wins on conflict because its vector
/// is more recent (the cold path may have a stale vector from an earlier
/// embedding round). Result is sorted by `score` ascending (cold convention:
/// lower = better for distance metrics) and truncated to `top_k`.
fn merge_hot_cold_results(
    hot: Vec<HotVectorHit>,
    cold: Vec<VectorSearchResult>,
    top_k: usize,
) -> Vec<VectorSearchResult> {
    use std::collections::HashSet;
    let mut seen: HashSet<(i32, i64)> = HashSet::with_capacity(hot.len() + cold.len());
    let mut merged: Vec<VectorSearchResult> = Vec::with_capacity(hot.len() + cold.len());
    for h in hot {
        if seen.insert((h.partition, h.offset)) {
            merged.push(VectorSearchResult {
                topic: h.topic,
                partition: h.partition,
                offset: h.offset,
                score: h.distance,
                text_preview: None,
                embedding: None,
            });
        }
    }
    for c in cold {
        if seen.insert((c.partition, c.offset)) {
            merged.push(c);
        }
    }
    merged.sort_by(|a, b| {
        a.score
            .partial_cmp(&b.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    merged.truncate(top_k);
    merged
}

/// Text-based vector search request
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// VO-4: Enable cross-encoder re-ranking (default: false)
    /// When true, overretrieves candidates and re-ranks with cross-encoder
    #[serde(default)]
    pub rerank: bool,
    /// VO-5: Embedding model to search (default: topic's active_model, fallback "default")
    #[serde(default)]
    pub model: Option<String>,
}

fn default_k() -> usize {
    10
}

/// Raw vector search request
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// VO-5: Embedding model to search (default: topic's active_model, fallback "default")
    #[serde(default)]
    pub model: Option<String>,
}

/// Vector search response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorSearchResponse {
    /// Search results
    pub results: Vec<VectorSearchResultItem>,
    /// Number of results returned
    pub count: usize,
    /// Total indexed vectors for topic
    pub total_vectors: usize,
    /// Query execution time in milliseconds
    pub execution_time_ms: u64,
    /// Whether results were re-ranked (VO-4)
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reranked: bool,
    /// VO-5: Model ID that was searched
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// Single search result item
#[derive(Debug, Clone, Serialize, Deserialize)]
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
        ).with_cache(state.embedding_cache.clone());

        service
            .search_by_text(topic, query, k, filters)
            .await
            .map_err(|e| format!("Search failed: {}", e))
    }

    /// Search by raw vector (does not require embedding provider)
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

        let service = VectorSearchService::new_index_only(
            std::sync::Arc::clone(index_manager),
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

/// Default overretrieval factor for re-ranking (fetch k * this many candidates)
const DEFAULT_OVERRETRIEVAL_FACTOR: usize = 5;

/// Check if this request should fan out to peer nodes in cluster mode.
///
/// For vector search, we check whether this node has HNSW indexes for ALL
/// partitions of the topic — not just whether it has partition replicas.
/// With RF=3 (replication factor = node count), all nodes have all partition
/// data, but only the node that ran WalIndexer has HNSW vector indexes.
///
/// Returns true if:
/// - A query router is configured (cluster mode)
/// - The request is NOT a forwarded request (no infinite loops)
/// - This node does NOT have vector indexes for all expected partitions
///
/// Side effect: refreshes the partition map from metadata store.
async fn needs_fan_out(state: &UnifiedApiState, headers: &HeaderMap, topic: &str) -> bool {
    if let Some(ref router) = state.query_router {
        if !super::query_router::is_forwarded_request(headers) {
            router.refresh_partition_map(state.metadata_store.as_ref(), topic).await;

            // Vector-aware check: do we have HNSW indexes for all partitions?
            if let Some(ref index_manager) = state.vector_index_manager {
                let local_vector_partitions = index_manager.partitions_with_vectors(topic).await;
                let partition_count = state.metadata_store
                    .get_topic(topic).await
                    .ok()
                    .flatten()
                    .map(|t| t.config.partition_count as usize)
                    .unwrap_or(0);

                if partition_count > 0 && local_vector_partitions.len() < partition_count {
                    debug!(
                        topic = %topic,
                        local_vector_partitions = local_vector_partitions.len(),
                        expected = partition_count,
                        "Vector fan-out needed: not all partitions have local HNSW indexes"
                    );
                    return true;
                }
            }

            // Fallback to partition-based check
            return !router.all_partitions_local(topic).await;
        }
    }
    false
}

/// Search by text query endpoint
pub async fn search(
    State(state): State<UnifiedApiState>,
    headers: HeaderMap,
    Path(topic): Path<String>,
    Json(request): Json<VectorSearchRequest>,
) -> impl IntoResponse {
    let fan_out_request = request.clone();
    let model_param = request.model.as_deref();
    info!(
        topic = %topic, query = %request.query, k = request.k,
        rerank = request.rerank, model = ?model_param,
        "Vector search by text"
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

    // VO-4: Determine search k — overretrieve when re-ranking is requested
    let rerank_enabled = request.rerank && state.reranker.is_some();
    let search_k = if rerank_enabled {
        request.k * DEFAULT_OVERRETRIEVAL_FACTOR
    } else {
        request.k
    };

    let service = VectorSearchService::new(
        std::sync::Arc::clone(index_manager),
        std::sync::Arc::clone(provider),
    ).with_cache(state.embedding_cache.clone());

    // VO-5: Use model-specific search when model param provided
    let search_result = if let Some(model_id) = model_param {
        service.search_by_text_model(&topic, &request.query, search_k, filters, model_id).await
    } else {
        service.search_by_text(&topic, &request.query, search_k, filters).await
    };

    match search_result {
        Ok(mut results) => {
            // VO-5: Get vector count for the searched model
            let total_vectors = if let Some(model_id) = model_param {
                service.get_vector_count_for_model(&topic, model_id).await
            } else {
                service.get_vector_count(&topic).await
            };
            let mut reranked = false;

            // HP-2.5: merge hot vector hits if the hot path is wired + we have
            // a cached query embedding. The cache is populated by the cold path's
            // search_by_text call immediately above, so this piggybacks on it
            // without a second embedding call.
            if let (Some(hot_idx), Some(cache)) =
                (&state.hot_vector_index, &state.embedding_cache)
            {
                if let Some(qvec) = cache.get(&request.query).await {
                    match hot_idx.search_topic(&topic, &qvec, search_k).await {
                        Ok(hot_hits) if !hot_hits.is_empty() => {
                            results = merge_hot_cold_results(
                                hot_hits,
                                results,
                                search_k,
                            );
                        }
                        Ok(_) => {}
                        Err(e) => {
                            debug!(topic = %topic, "hot vector search failed: {}", e);
                        }
                    }
                }
            }

            // VO-4: Apply cross-encoder re-ranking if enabled
            if rerank_enabled {
                if let Some(ref reranker) = state.reranker {
                    results = apply_reranking(reranker.as_ref(), &request.query, results, request.k).await;
                    reranked = true;
                }
            }

            // Truncate to requested k (in case reranking wasn't applied)
            results.truncate(request.k);

            let execution_time_ms = start.elapsed().as_millis() as u64;

            let response = VectorSearchResponse {
                count: results.len(),
                results: results.into_iter().map(Into::into).collect(),
                total_vectors,
                execution_time_ms,
                reranked,
                model: request.model.clone(),
            };

            // Distributed fan-out: merge results from peer nodes
            // Use all_peers() instead of nodes_for_topic() because with RF=3 and 3 nodes,
            // partition-based routing returns empty (all partitions are local), but vector
            // indexes are per-node and may differ.
            let response = if needs_fan_out(&state, &headers, &topic).await {
                let router = state.query_router.as_ref().unwrap();
                let nodes = router.all_peers();
                let peers: Vec<VectorSearchResponse> = router
                    .fan_out_post(&format!("/_vector/{}/search", topic), &fan_out_request, &nodes)
                    .await;
                debug!(topic = %topic, peer_count = peers.len(), "Merging vector search from peers");
                super::query_router::merge_vector_responses(response, peers, fan_out_request.k)
            } else {
                response
            };

            info!(
                topic = %topic,
                results = response.count,
                time_ms = execution_time_ms,
                reranked = reranked,
                model = ?model_param,
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
    headers: HeaderMap,
    Path(topic): Path<String>,
    Json(request): Json<VectorSearchByVectorRequest>,
) -> impl IntoResponse {
    let fan_out_request = request.clone();
    let model_param = request.model.as_deref();
    info!(
        topic = %topic,
        vector_len = request.vector.len(),
        k = request.k,
        model = ?model_param,
        "Vector search by vector"
    );

    let start = std::time::Instant::now();

    // Check if vector search is available (no embedding provider needed for raw vector search)
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

    let filters = build_filters(request.partitions, request.min_offset, request.max_offset);

    let service = VectorSearchService::new_index_only(
        std::sync::Arc::clone(index_manager),
    );

    // VO-5: Use model-specific search when model param provided
    let search_result = if let Some(model_id) = model_param {
        service.search_by_vector_model(&topic, &request.vector, request.k, filters, model_id).await
    } else {
        service.search_by_vector(&topic, &request.vector, request.k, filters).await
    };

    match search_result {
        Ok(results) => {
            let total_vectors = if let Some(model_id) = model_param {
                service.get_vector_count_for_model(&topic, model_id).await
            } else {
                service.get_vector_count(&topic).await
            };
            let execution_time_ms = start.elapsed().as_millis() as u64;

            let response = VectorSearchResponse {
                count: results.len(),
                results: results.into_iter().map(Into::into).collect(),
                total_vectors,
                execution_time_ms,
                reranked: false,
                model: request.model.clone(),
            };

            // Distributed fan-out: merge results from peer nodes
            // Use all_peers() — vector indexes are per-node, not partition-based
            let response = if needs_fan_out(&state, &headers, &topic).await {
                let router = state.query_router.as_ref().unwrap();
                let nodes = router.all_peers();
                let peers: Vec<VectorSearchResponse> = router
                    .fan_out_post(&format!("/_vector/{}/search_by_vector", topic), &fan_out_request, &nodes)
                    .await;
                debug!(topic = %topic, peer_count = peers.len(), "Merging vector-by-vector search from peers");
                super::query_router::merge_vector_responses(response, peers, fan_out_request.k)
            } else {
                response
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
    /// Whether vector indexes are still loading from disk (warmup phase)
    pub loading: bool,
    pub partitions: Vec<PartitionStats>,
    /// VO-5: Model IDs with indexes for this topic
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
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
        loading: stats.loading,
        partitions: stats
            .partitions
            .into_iter()
            .map(|(partition, ps)| PartitionStats {
                partition,
                vector_count: ps.vector_count,
            })
            .collect(),
        models: stats.models,
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
///
/// Returns topics from two sources:
/// 1. Topics with active vector indexes (already have embeddings)
/// 2. Topics configured with vector.enabled=true in metadata (may still be indexing)
pub async fn list_topics(State(state): State<UnifiedApiState>) -> impl IntoResponse {
    debug!("Listing vector-enabled topics");

    let mut all_topics = std::collections::BTreeSet::new();

    // Source 1: Topics with active vector indexes
    if let Some(ref index_manager) = state.vector_index_manager {
        for topic in index_manager.list_topics().await {
            all_topics.insert(topic);
        }
    }

    // Source 2: Topics configured with vector.enabled=true in metadata
    match state.metadata_store.list_topics().await {
        Ok(topics_meta) => {
            for topic_meta in &topics_meta {
                if topic_meta.config.is_vector_enabled() {
                    all_topics.insert(topic_meta.name.clone());
                }
            }
        }
        Err(e) => {
            debug!("Failed to list topics from metadata store: {}", e);
        }
    }

    let topics: Vec<String> = all_topics.into_iter().collect();
    let count = topics.len();

    let response = ListTopicsResponse { topics, count };
    (StatusCode::OK, Json(response)).into_response()
}

/// VO-4: Apply cross-encoder re-ranking to search results.
///
/// Takes overretrieved candidates, scores (query, document) pairs with the
/// reranker, and returns results reordered by cross-encoder relevance score.
async fn apply_reranking(
    reranker: &dyn chronik_embeddings::reranker::RerankerProvider,
    query: &str,
    results: Vec<VectorSearchResult>,
    top_k: usize,
) -> Vec<VectorSearchResult> {
    if results.is_empty() {
        return results;
    }

    // Extract document texts for the reranker.
    // Use text_preview if available, otherwise use "{topic}:{partition}:{offset}" as a fallback.
    let doc_texts: Vec<String> = results
        .iter()
        .map(|r| {
            r.text_preview
                .clone()
                .unwrap_or_else(|| format!("{}:{}:{}", r.topic, r.partition, r.offset))
        })
        .collect();

    let doc_refs: Vec<&str> = doc_texts.iter().map(|s| s.as_str()).collect();

    match reranker.rerank(query, &doc_refs, top_k).await {
        Ok(rerank_results) => {
            debug!(
                candidates = results.len(),
                reranked = rerank_results.len(),
                "Re-ranking applied successfully"
            );

            // Build reranked results in reranker-determined order
            let mut reranked: Vec<VectorSearchResult> = rerank_results
                .into_iter()
                .filter_map(|rr| {
                    results.get(rr.index).map(|original| {
                        // Replace score with reranker relevance score
                        let mut result = original.clone();
                        result.score = rr.relevance_score;
                        result
                    })
                })
                .collect();

            reranked.truncate(top_k);
            reranked
        }
        Err(e) => {
            tracing::warn!(error = %e, "Re-ranking failed, returning original results");
            // Fallback: return original results truncated to top_k
            let mut fallback = results;
            fallback.truncate(top_k);
            fallback
        }
    }
}

// ============================================================================
// Hybrid Search (Phase 6.4.4) - Combines Vector + Text Search
// ============================================================================

/// Hybrid search request combining vector and text search
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// VO-4: Enable cross-encoder re-ranking after RRF fusion (default: false)
    #[serde(default)]
    pub rerank: bool,
    /// VO-5: Embedding model to search (default: topic's active_model, fallback "default")
    #[serde(default)]
    pub model: Option<String>,
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// Whether results were re-ranked after fusion (VO-4)
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reranked: bool,
    /// VO-5: Model ID that was searched
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// Single hybrid search result item
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    headers: HeaderMap,
    Path(topic): Path<String>,
    Json(request): Json<HybridSearchRequest>,
) -> impl IntoResponse {
    let fan_out_request = request.clone();
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
    ).with_cache(state.embedding_cache.clone());

    // VO-5: Use model-specific search when model param provided
    let mut vector_results = if let Some(ref model_id) = request.model {
        match service.search_by_text_model(&topic, &request.query, search_k, filters.clone(), model_id).await {
            Ok(results) => results,
            Err(e) => {
                error!(topic = %topic, error = %e, "Vector search failed in hybrid");
                Vec::new()
            }
        }
    } else {
        match service.search_by_text(&topic, &request.query, search_k, filters.clone()).await {
            Ok(results) => results,
            Err(e) => {
                error!(topic = %topic, error = %e, "Vector search failed in hybrid");
                Vec::new()
            }
        }
    };

    // HP-2 follow-up C: merge hot vector hits into the hybrid vector side.
    // Piggybacks on the query embedding cache populated by search_by_text above.
    if let (Some(hot_idx), Some(cache)) =
        (&state.hot_vector_index, &state.embedding_cache)
    {
        if let Some(qvec) = cache.get(&request.query).await {
            if let Ok(hot_hits) = hot_idx.search_topic(&topic, &qvec, search_k).await {
                if !hot_hits.is_empty() {
                    vector_results = merge_hot_cold_results(hot_hits, vector_results, search_k);
                }
            }
        }
    }

    let vector_results_count = vector_results.len();

    // Step 2: Text search via Tantivy (BM25)
    // VO-4.8: Wire real Tantivy results into hybrid RRF fusion
    let text_results: Vec<(i32, i64, Option<String>)> = {
        #[cfg(feature = "search")]
        {
            let mut results = Vec::new();
            if let Some(ref search_api) = state.search_api {
                // Search in-memory indices
                if let Some(index_state) = search_api.indices.get(&topic) {
                    match search_tantivy_for_hybrid(&topic, &index_state, &request.query, search_k) {
                        Ok(hits) => results.extend(hits),
                        Err(e) => {
                            debug!(topic = %topic, error = %e, "In-memory text search failed in hybrid");
                        }
                    }
                }
                // Search WAL-created indices on disk
                if let Some(base_path) = search_api.get_index_base_path() {
                    match search_wal_tantivy_for_hybrid(base_path, &topic, &request.query, search_k) {
                        Ok(hits) => results.extend(hits),
                        Err(e) => {
                            debug!(topic = %topic, error = %e, "WAL text search failed in hybrid");
                        }
                    }
                    // Also check real-time indices at sibling directory
                    let realtime_path = base_path.replace("tantivy_indexes", "index");
                    match search_wal_tantivy_for_hybrid(&realtime_path, &topic, &request.query, search_k) {
                        Ok(hits) => results.extend(hits),
                        Err(e) => {
                            debug!(topic = %topic, error = %e, "Real-time text search failed in hybrid");
                        }
                    }
                }
                // HP-2 follow-up C: include hot text index hits so NRT
                // documents participate in RRF fusion alongside the BM25 side.
                if let Some(ref api) = state.search_api {
                    if let Some(ref hot_text) = api.hot_text_index {
                        if let Ok(hot_hits) = hot_text
                            .search_topic(&topic, &request.query, search_k)
                            .await
                        {
                            // Prepend so the earliest ranks go to hot — matches
                            // the "hot wins on conflict" policy from /_search.
                            let mut prefixed: Vec<(i32, i64, Option<String>)> = hot_hits
                                .into_iter()
                                .map(|h| (h.partition, h.offset, Some(h.value)))
                                .collect();
                            prefixed.append(&mut results);
                            // dedup by (partition, offset) preserving first occurrence
                            use std::collections::HashSet;
                            let mut seen: HashSet<(i32, i64)> =
                                HashSet::with_capacity(prefixed.len());
                            prefixed.retain(|(p, o, _)| seen.insert((*p, *o)));
                            results = prefixed;
                        }
                    }
                }
                // Sort by score (carried implicitly by insertion order from TopDocs) and deduplicate
                results.truncate(search_k);
            }
            results
        }
        #[cfg(not(feature = "search"))]
        {
            Vec::new()
        }
    };
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
    for (rank, (partition, offset, preview)) in text_results.iter().enumerate() {
        let key = DocKey {
            partition: *partition,
            offset: *offset,
        };
        if let Some(info) = doc_map.get_mut(&key) {
            info.text_rank = Some(rank + 1);
            // Prefer text preview from BM25 if vector didn't have one
            if info.text_preview.is_none() {
                info.text_preview = preview.clone();
            }
        } else {
            doc_map.insert(
                key,
                DocInfo {
                    vector_rank: None,
                    text_rank: Some(rank + 1),
                    text_preview: preview.clone(),
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

    // VO-4: Apply cross-encoder re-ranking after RRF fusion if requested
    let mut reranked = false;
    if request.rerank {
        if let Some(ref reranker) = state.reranker {
            results = apply_hybrid_reranking(
                reranker.as_ref(),
                &request.query,
                results,
                request.k,
            )
            .await;
            reranked = true;
        }
    }

    // Take top k
    results.truncate(request.k);

    let execution_time_ms = start.elapsed().as_millis() as u64;
    let count = results.len();

    info!(
        topic = %topic,
        results = count,
        vector_results = vector_results_count,
        text_results = text_results_count,
        reranked = reranked,
        time_ms = execution_time_ms,
        "Hybrid search completed"
    );

    let response = HybridSearchResponse {
        results,
        count,
        vector_results_count,
        text_results_count,
        execution_time_ms,
        reranked,
        model: request.model.clone(),
    };

    // Distributed fan-out: merge results from peer nodes
    // Use all_peers() — vector indexes are per-node, not partition-based
    let response = if needs_fan_out(&state, &headers, &topic).await {
        let router = state.query_router.as_ref().unwrap();
        let nodes = router.all_peers();
        let peers: Vec<HybridSearchResponse> = router
            .fan_out_post(&format!("/_vector/{}/hybrid", topic), &fan_out_request, &nodes)
            .await;
        debug!(topic = %topic, peer_count = peers.len(), "Merging hybrid search from peers");
        super::query_router::merge_hybrid_responses(response, peers, fan_out_request.k)
    } else {
        response
    };

    (StatusCode::OK, Json(response)).into_response()
}

/// VO-4: Apply re-ranking to hybrid search results after RRF fusion.
///
/// Takes fused results, sends document texts to the cross-encoder reranker,
/// and reorders by relevance score.
async fn apply_hybrid_reranking(
    reranker: &dyn chronik_embeddings::reranker::RerankerProvider,
    query: &str,
    results: Vec<HybridSearchResultItem>,
    top_k: usize,
) -> Vec<HybridSearchResultItem> {
    if results.is_empty() {
        return results;
    }

    let doc_texts: Vec<String> = results
        .iter()
        .map(|r| {
            r.text_preview
                .clone()
                .unwrap_or_else(|| format!("{}:{}", r.partition, r.offset))
        })
        .collect();

    let doc_refs: Vec<&str> = doc_texts.iter().map(|s| s.as_str()).collect();

    match reranker.rerank(query, &doc_refs, top_k).await {
        Ok(rerank_results) => {
            debug!(
                candidates = results.len(),
                reranked = rerank_results.len(),
                "Hybrid re-ranking applied"
            );

            rerank_results
                .into_iter()
                .filter_map(|rr| {
                    results.get(rr.index).map(|original| {
                        let mut item = original.clone();
                        item.score = rr.relevance_score;
                        item
                    })
                })
                .take(top_k)
                .collect()
        }
        Err(e) => {
            tracing::warn!(error = %e, "Hybrid re-ranking failed, returning RRF results");
            let mut fallback = results;
            fallback.truncate(top_k);
            fallback
        }
    }
}

// ============================================================================
// VO-4.8: Tantivy BM25 helpers for hybrid search
// ============================================================================

/// Search a Tantivy in-memory IndexState and return (partition, offset, text_preview) tuples.
///
/// Used by hybrid search to get BM25 text results for RRF fusion with vector results.
#[cfg(feature = "search")]
fn search_tantivy_for_hybrid(
    topic: &str,
    state: &chronik_search::api::IndexState,
    query_text: &str,
    k: usize,
) -> Result<Vec<(i32, i64, Option<String>)>, String> {
    use tantivy::collector::TopDocs;
    use tantivy::query::QueryParser;
    use tantivy::schema::{FieldType, Value};

    let searcher = state.reader.searcher();

    let text_fields: Vec<_> = state.schema.fields()
        .filter_map(|(field, entry)| {
            match entry.field_type() {
                FieldType::Str(_) | FieldType::JsonObject(_) => Some(field),
                _ => None,
            }
        })
        .collect();

    if text_fields.is_empty() {
        return Ok(vec![]);
    }

    let query_parser = QueryParser::for_index(&state.index, text_fields);
    let parsed_query = query_parser.parse_query(query_text)
        .map_err(|e| format!("Query parse failed: {}", e))?;

    let top_docs = searcher.search(&*parsed_query, &TopDocs::with_limit(k))
        .map_err(|e| format!("Search failed: {}", e))?;

    let mut results = Vec::new();
    for (_score, doc_address) in &top_docs {
        let doc: tantivy::TantivyDocument = match searcher.doc(*doc_address) {
            Ok(d) => d,
            Err(_) => continue,
        };

        let offset_field = state.schema.get_field("offset").ok()
            .or_else(|| state.schema.get_field("_offset").ok());
        let partition_field = state.schema.get_field("partition").ok()
            .or_else(|| state.schema.get_field("_partition").ok());

        let offset = offset_field
            .and_then(|f| doc.get_all(f).next().and_then(|v| v.as_i64()))
            .unwrap_or(results.len() as i64);
        let partition = partition_field
            .and_then(|f| doc.get_all(f).next().and_then(|v| v.as_i64()))
            .unwrap_or(0) as i32;

        let preview = state.schema.get_field("_value").ok()
            .and_then(|f| doc.get_all(f).next().and_then(|v| v.as_str().map(|s| s.to_string())))
            .or_else(|| {
                state.schema.get_field("_json_content").ok()
                    .and_then(|f| doc.get_all(f).next().and_then(|v| v.as_str().map(|s| s.to_string())))
            })
            .or_else(|| {
                state.schema.get_field("_content").ok()
                    .and_then(|f| doc.get_all(f).next().and_then(|v| v.as_str().map(|s| s.to_string())))
            })
            .map(|text| {
                if text.len() > 300 { format!("{}...", &text[..300]) } else { text }
            });

        results.push((partition, offset, preview));
    }

    Ok(results)
}

/// Search WAL-created Tantivy indices on disk for a specific topic.
///
/// Returns (partition, offset, text_preview) tuples for hybrid RRF fusion.
/// WAL indices are stored as `{base_path}/{topic}-{partition}/` directories.
#[cfg(feature = "search")]
fn search_wal_tantivy_for_hybrid(
    base_path: &str,
    topic: &str,
    query_text: &str,
    k: usize,
) -> Result<Vec<(i32, i64, Option<String>)>, String> {
    use std::path::Path;
    use tantivy::collector::TopDocs;
    use tantivy::query::QueryParser;
    use tantivy::schema::{FieldType, Value};
    use tantivy::Index;

    let base = Path::new(base_path);
    if !base.exists() {
        return Ok(vec![]);
    }

    let entries = std::fs::read_dir(base)
        .map_err(|e| format!("Failed to read index directory: {}", e))?;

    let mut all_results = Vec::new();

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let dir_name = path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");

        if !dir_name.starts_with(topic) {
            continue;
        }
        let suffix = &dir_name[topic.len()..];
        if !suffix.is_empty() && !suffix.starts_with('-') {
            continue;
        }

        let index = match Index::open_in_dir(&path) {
            Ok(idx) => idx,
            Err(_) => continue,
        };
        chronik_storage::register_analyzer(&index);

        let reader = match index.reader() {
            Ok(r) => r,
            Err(_) => continue,
        };
        let searcher = reader.searcher();
        let schema = index.schema();

        let text_fields: Vec<_> = schema.fields()
            .filter_map(|(field, entry)| {
                match entry.field_type() {
                    FieldType::Str(_) | FieldType::JsonObject(_) => Some(field),
                    _ => None,
                }
            })
            .collect();

        if text_fields.is_empty() {
            continue;
        }

        let query_parser = QueryParser::for_index(&index, text_fields);
        let parsed_query = match query_parser.parse_query(query_text) {
            Ok(q) => q,
            Err(_) => continue,
        };

        let top_docs = match searcher.search(&*parsed_query, &TopDocs::with_limit(k)) {
            Ok(docs) => docs,
            Err(_) => continue,
        };

        let dir_partition: i32 = suffix.strip_prefix('-')
            .and_then(|p| p.parse().ok())
            .unwrap_or(0);

        for (_score, doc_address) in &top_docs {
            let doc: tantivy::TantivyDocument = match searcher.doc(*doc_address) {
                Ok(d) => d,
                Err(_) => continue,
            };

            let offset_field = schema.get_field("offset").ok()
                .or_else(|| schema.get_field("_offset").ok());

            let offset = offset_field
                .and_then(|f| doc.get_all(f).next().and_then(|v| v.as_i64()))
                .unwrap_or(all_results.len() as i64);

            let preview = schema.get_field("_value").ok()
                .and_then(|f| doc.get_all(f).next().and_then(|v| v.as_str().map(|s| s.to_string())))
                .or_else(|| {
                    schema.get_field("_json_content").ok()
                        .and_then(|f| doc.get_all(f).next().and_then(|v| v.as_str().map(|s| s.to_string())))
                })
                .or_else(|| {
                    schema.get_field("_content").ok()
                        .and_then(|f| doc.get_all(f).next().and_then(|v| v.as_str().map(|s| s.to_string())))
                })
                .map(|text| {
                    if text.len() > 300 { format!("{}...", &text[..300]) } else { text }
                });

            all_results.push((dir_partition, offset, preview));
        }
    }

    Ok(all_results)
}

// ============================================================================
// Vector Embedding Backfill (v2.3.1)
// ============================================================================

/// Request body for vector embedding backfill
#[derive(Debug, Deserialize)]
pub struct BackfillRequest {
    /// Specific partition to backfill (null = all partitions)
    pub partition: Option<i32>,
    /// Model ID to embed with (null = use active_model from topic config)
    #[serde(default)]
    pub model: Option<String>,
}

/// Response for vector embedding backfill
#[derive(Debug, Serialize)]
pub struct BackfillResponse {
    pub topic: String,
    pub segments_processed: usize,
    pub vectors_generated: usize,
    pub skipped_segments: usize,
    pub errors: usize,
    pub duration_secs: u64,
    /// Model ID used for embedding
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// Backfill vector embeddings from existing Tier 2 segments.
/// POST /_vector/:topic/backfill
pub async fn backfill(
    State(state): State<UnifiedApiState>,
    Path(topic): Path<String>,
    Json(request): Json<BackfillRequest>,
) -> impl IntoResponse {
    info!(topic = %topic, partition = ?request.partition, model = ?request.model, "Starting vector embedding backfill");

    let wal_indexer = match &state.wal_indexer {
        Some(w) => w,
        None => {
            let error_response = VectorErrorResponse {
                error: "WalIndexer not available for backfill".to_string(),
                error_type: "ServiceUnavailable".to_string(),
            };
            return (StatusCode::SERVICE_UNAVAILABLE, Json(error_response)).into_response();
        }
    };

    match wal_indexer.backfill_vector_embeddings(&topic, request.partition, request.model.as_deref()).await {
        Ok(stats) => {
            let response = BackfillResponse {
                topic,
                segments_processed: stats.segments_processed,
                vectors_generated: stats.vectors_generated,
                skipped_segments: stats.skipped_segments,
                errors: stats.errors,
                duration_secs: stats.duration_secs,
                model: stats.model_id,
            };
            (StatusCode::OK, Json(response)).into_response()
        }
        Err(e) => {
            error!(topic = %topic, error = %e, "Vector backfill failed");
            let error_response = VectorErrorResponse {
                error: format!("Backfill failed: {}", e),
                error_type: "InternalError".to_string(),
            };
            (StatusCode::INTERNAL_SERVER_ERROR, Json(error_response)).into_response()
        }
    }
}

/// Request body for clearing vector indexes
#[derive(Debug, Deserialize)]
pub struct ClearVectorIndexRequest {
    /// Partition to clear. If None, clears all partitions.
    pub partition: Option<i32>,
}

/// Response for clearing vector indexes
#[derive(Debug, Serialize)]
pub struct ClearVectorIndexResponse {
    pub topic: String,
    pub partition: Option<i32>,
    pub vectors_cleared: usize,
    pub snapshots_deleted: bool,
}

/// DELETE /_vector/:topic/index — Clear vector index and delete snapshots
pub async fn clear_vector_index(
    State(state): State<UnifiedApiState>,
    Path(topic): Path<String>,
    body: Option<Json<ClearVectorIndexRequest>>,
) -> impl IntoResponse {
    let partition = body.and_then(|b| b.partition);

    let index_manager = match state.vector_index_manager {
        Some(ref mgr) => mgr,
        None => {
            return (StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({
                "error": "Vector search not enabled"
            }))).into_response();
        }
    };

    let result = if let Some(p) = partition {
        info!(topic = %topic, partition = p, "Clearing vector index for partition");
        index_manager.clear_partition_with_snapshot(&topic, p).await
    } else {
        info!(topic = %topic, "Clearing all vector indexes for topic");
        index_manager.clear_topic_with_snapshots(&topic).await
    };

    match result {
        Ok(vectors_cleared) => {
            info!(
                topic = %topic,
                partition = ?partition,
                vectors_cleared = vectors_cleared,
                "Vector index cleared"
            );
            Json(ClearVectorIndexResponse {
                topic,
                partition,
                vectors_cleared,
                snapshots_deleted: true,
            }).into_response()
        }
        Err(e) => {
            error!(topic = %topic, error = %e, "Failed to clear vector index");
            (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
                "error": format!("Failed to clear vector index: {}", e)
            }))).into_response()
        }
    }
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

    // ========== VO-4 Reranking Tests ==========

    #[test]
    fn test_search_request_rerank_default_false() {
        let json = r#"{"query": "error logs"}"#;
        let request: VectorSearchRequest = serde_json::from_str(json).unwrap();
        assert!(!request.rerank);
    }

    #[test]
    fn test_search_request_rerank_enabled() {
        let json = r#"{"query": "error logs", "rerank": true}"#;
        let request: VectorSearchRequest = serde_json::from_str(json).unwrap();
        assert!(request.rerank);
    }

    #[test]
    fn test_hybrid_search_request_rerank_default_false() {
        let json = r#"{"query": "error logs"}"#;
        let request: HybridSearchRequest = serde_json::from_str(json).unwrap();
        assert!(!request.rerank);
    }

    #[test]
    fn test_hybrid_search_request_rerank_enabled() {
        let json = r#"{"query": "error logs", "rerank": true, "k": 5}"#;
        let request: HybridSearchRequest = serde_json::from_str(json).unwrap();
        assert!(request.rerank);
        assert_eq!(request.k, 5);
    }

    #[tokio::test]
    async fn test_apply_reranking_empty_results() {
        use chronik_embeddings::reranker::NoOpReranker;
        let reranker = NoOpReranker;
        let results: Vec<VectorSearchResult> = Vec::new();
        let reranked = apply_reranking(&reranker, "query", results, 10).await;
        assert!(reranked.is_empty());
    }

    #[tokio::test]
    async fn test_apply_reranking_with_noop() {
        use chronik_columnar::VectorSearchResult;
        use chronik_embeddings::reranker::NoOpReranker;
        let reranker = NoOpReranker;

        let results = vec![
            VectorSearchResult {
                topic: "logs".to_string(),
                partition: 0,
                offset: 10,
                score: 0.1,
                text_preview: Some("error in payment".to_string()),
                embedding: None,
            },
            VectorSearchResult {
                topic: "logs".to_string(),
                partition: 0,
                offset: 20,
                score: 0.2,
                text_preview: Some("connection timeout".to_string()),
                embedding: None,
            },
            VectorSearchResult {
                topic: "logs".to_string(),
                partition: 0,
                offset: 30,
                score: 0.3,
                text_preview: Some("disk full warning".to_string()),
                embedding: None,
            },
        ];

        let reranked = apply_reranking(&reranker, "payment error", results, 2).await;
        assert_eq!(reranked.len(), 2);
        // NoOp reranker returns in original order, scores decreasing
        assert!(reranked[0].score > reranked[1].score);
    }
}
