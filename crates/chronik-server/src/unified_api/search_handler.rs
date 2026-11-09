//! Search Handler for Unified API
//!
//! Integrates the existing chronik-search API into the unified API.
//! This module provides backward-compatible Elasticsearch-style search endpoints:
//! - POST/GET `/_search` - Search across all indices
//! - POST/GET `/:index/_search` - Search specific index
//! - `/:index/_doc/:id` - Document CRUD operations
//! - `/:index` - Index management
//! - `/_cat/indices` - List indices
//!
//! When a QueryRouter is available (cluster mode), the search endpoints
//! are wrapped with fan-out logic that distributes queries to all peer nodes
//! and merges results. See docs/DISTRIBUTED_QUERY_LAYER.md.

#[cfg(feature = "search")]
use std::sync::Arc;

#[cfg(feature = "search")]
use chronik_search::SearchApi;

#[cfg(feature = "search")]
use chronik_storage::WalIndexer;

/// Create a SearchApi instance for the unified API
///
/// This function creates a SearchApi configured with the WalIndexer
/// for accessing WAL-created Tantivy indexes.
///
/// # Arguments
/// * `wal_indexer` - The WAL indexer for accessing disk-based indexes
/// * `index_base_path` - Full path to tantivy_indexes directory (e.g., "{data_dir}/tantivy_indexes")
///
/// # Returns
/// An Arc-wrapped SearchApi ready for use with Axum
///
/// # Errors
/// Returns an error if the SearchApi cannot be created
#[cfg(feature = "search")]
pub fn create_search_api(
    wal_indexer: Arc<WalIndexer>,
    index_base_path: &str,
) -> Result<Arc<SearchApi>, chronik_common::Error> {
    create_search_api_with_hot(wal_indexer, index_base_path, None)
}

/// HP-1.3: Variant that accepts an optional hot text index for NRT search.
/// When provided, `/_search` and `/:index/_search` merge hot hits with
/// cold results, preferring hot on (_index, _id) dedup.
#[cfg(feature = "search")]
pub fn create_search_api_with_hot(
    wal_indexer: Arc<WalIndexer>,
    index_base_path: &str,
    hot_text_index: Option<Arc<chronik_search::hot_text_index::HotTextIndex>>,
) -> Result<Arc<SearchApi>, chronik_common::Error> {
    let mut api = SearchApi::new_with_wal_indexer(wal_indexer, index_base_path.to_string())?;
    if let Some(hot) = hot_text_index {
        api = api.with_hot_text_index(hot);
    }
    Ok(Arc::new(api))
}

/// Get the search API router for mounting in the unified API
///
/// Returns the search API router without /health endpoint
/// (since the unified API provides its own /health).
#[cfg(feature = "search")]
pub fn search_router(search_api: Arc<SearchApi>) -> axum::Router {
    search_api.router_for_embedding()
}

/// Create a search router with distributed fan-out support.
///
/// When a QueryRouter is available, the `/_search` and `/:index/_search` endpoints
/// are replaced with fan-out-aware wrappers that:
/// 1. Execute the search locally via SearchApi
/// 2. Forward the request to all peer nodes (with X-Chronik-Depth: 1)
/// 3. Merge results by _score, sum shard counts
///
/// All other search endpoints (doc CRUD, index management, _cat/indices) remain unchanged.
#[cfg(feature = "search")]
pub fn search_router_with_fanout(
    search_api: Arc<SearchApi>,
    query_router: Arc<super::query_router::QueryRouter>,
) -> axum::Router {
    use axum::routing::{delete, get, post, put};

    let fanout_state = SearchFanoutState {
        search_api: search_api.clone(),
        query_router,
    };

    // Fan-out-aware search endpoints
    let search_routes = axum::Router::new()
        .route("/_search", get(search_all_fanout).post(search_all_fanout))
        .route("/:index/_search", get(search_index_fanout).post(search_index_fanout))
        .with_state(fanout_state);

    // Original CRUD/management endpoints (no fan-out needed for writes)
    let crud_routes = axum::Router::new()
        .route("/metrics", get(chronik_search::handlers::metrics_handler))
        .route(
            "/:index/_doc/:id",
            post(chronik_search::handlers::index_document)
                .put(chronik_search::handlers::index_document)
                .get(chronik_search::handlers::get_document)
                .delete(chronik_search::handlers::delete_document),
        )
        .route("/:index", put(chronik_search::handlers::create_index))
        .route("/:index", delete(chronik_search::handlers::delete_index))
        .route("/:index/_mapping", get(chronik_search::handlers::get_mapping))
        .route("/_cat/indices", get(chronik_search::handlers::cat_indices))
        .layer(tower_http::cors::CorsLayer::permissive())
        .with_state(search_api);

    search_routes.merge(crud_routes)
}

/// Combined state for fan-out-aware search handlers.
/// Holds both the SearchApi (for local execution) and QueryRouter (for fan-out).
#[cfg(feature = "search")]
#[derive(Clone)]
struct SearchFanoutState {
    search_api: Arc<SearchApi>,
    query_router: Arc<super::query_router::QueryRouter>,
}

/// Execute the search_all handler locally and extract the SearchResponse.
///
/// Calls the original handler, converts the `impl IntoResponse` to HTTP,
/// and parses the body back to SearchResponse.
#[cfg(feature = "search")]
async fn execute_local_search_all(
    search_api: Arc<SearchApi>,
    request: chronik_search::api::SearchRequest,
) -> Option<chronik_search::api::SearchResponse> {
    use axum::response::IntoResponse;

    let result = chronik_search::handlers::search_all(
        axum::extract::State(search_api),
        axum::Json(request),
    )
    .await;

    let http_response = result.into_response();
    let (parts, body) = http_response.into_parts();

    if parts.status != axum::http::StatusCode::OK {
        return None;
    }

    let body_bytes = hyper::body::to_bytes(body).await.ok()?;
    serde_json::from_slice(&body_bytes).ok()
}

/// Execute the search_index handler locally and extract the SearchResponse.
#[cfg(feature = "search")]
async fn execute_local_search_index(
    search_api: Arc<SearchApi>,
    index: String,
    request: chronik_search::api::SearchRequest,
) -> Option<chronik_search::api::SearchResponse> {
    use axum::response::IntoResponse;

    let result = chronik_search::handlers::search_index(
        axum::extract::Path(index),
        axum::extract::State(search_api),
        axum::Json(request),
    )
    .await;

    let http_response = result.into_response();
    let (parts, body) = http_response.into_parts();

    if parts.status != axum::http::StatusCode::OK {
        return None;
    }

    let body_bytes = hyper::body::to_bytes(body).await.ok()?;
    serde_json::from_slice(&body_bytes).ok()
}

/// Fan-out-aware search across all indices.
///
/// Executes the search locally, fans out to all peer nodes, and merges results.
#[cfg(feature = "search")]
async fn search_all_fanout(
    axum::extract::State(state): axum::extract::State<SearchFanoutState>,
    headers: axum::http::HeaderMap,
    axum::Json(request): axum::Json<chronik_search::api::SearchRequest>,
) -> axum::response::Response {
    use axum::response::IntoResponse;

    let size = request.size;

    // If this is a forwarded request, just execute locally and return
    if super::query_router::is_forwarded_request(&headers) {
        let result = chronik_search::handlers::search_all(
            axum::extract::State(state.search_api),
            axum::Json(request),
        )
        .await;
        return result.into_response();
    }

    // Execute locally and parse the response
    let local_response = match execute_local_search_all(
        state.search_api.clone(),
        request.clone(),
    )
    .await
    {
        Some(resp) => resp,
        None => {
            // Local search failed — still try peers, return their merged results
            let peers = state.query_router.all_peers();
            if !peers.is_empty() {
                let fan_out = state
                    .query_router
                    .fan_out_post::<_, chronik_search::api::SearchResponse>(
                        "/_search", &request, &peers,
                    )
                    .await;
                let unanswered = fan_out.failures.len();
                if !fan_out.is_complete() {
                    tracing::warn!(
                        failures = %fan_out.failure_summary(),
                        "Serving a partial _search: some nodes did not answer"
                    );
                }
                let peer_responses = fan_out.responses;
                if !peer_responses.is_empty() {
                    let empty = chronik_search::api::SearchResponse {
                        took: 0,
                        timed_out: false,
                        _shards: chronik_search::api::ShardInfo {
                            total: 0,
                            successful: 0,
                            skipped: 0,
                            failed: 0,
                        },
                        hits: chronik_search::api::HitsInfo {
                            total: chronik_search::api::TotalHits {
                                value: 0,
                                relation: "eq".to_string(),
                            },
                            max_score: None,
                            hits: Vec::new(),
                        },
                        aggregations: None,
                    };
                    let mut merged = super::query_router::merge_search_responses(
                        empty,
                        peer_responses,
                        size,
                    );
                    // The local search already failed here, so this answer is
                    // short by the local node too — count it with the peers that
                    // did not reply rather than reporting all-successful.
                    super::query_router::note_unanswered_peers(&mut merged, unanswered + 1);
                    return axum::Json(merged).into_response();
                }
            }
            // No local or peer results — return local error
            let result = chronik_search::handlers::search_all(
                axum::extract::State(state.search_api),
                axum::Json(request),
            )
            .await;
            return result.into_response();
        }
    };

    // Fan out to peers
    let peers = state.query_router.all_peers();
    let response = if !peers.is_empty() {
        let fan_out = state
            .query_router
            .fan_out_post::<_, chronik_search::api::SearchResponse>("/_search", &request, &peers)
            .await;
        let unanswered = fan_out.failures.len();
        if !fan_out.is_complete() {
            tracing::warn!(
                failures = %fan_out.failure_summary(),
                "Serving a partial _search: some nodes did not answer"
            );
        }
        tracing::debug!(
            peer_count = fan_out.responses.len(),
            "Merging ES search results from peers"
        );
        let mut merged =
            super::query_router::merge_search_responses(local_response, fan_out.responses, size);
        // Hits are ranked, so a partial answer is still worth serving — but
        // `_shards.failed` has to say so, or a search that missed a node is
        // indistinguishable from one where that node held no match (issue #57).
        super::query_router::note_unanswered_peers(&mut merged, unanswered);
        merged
    } else {
        local_response
    };

    axum::Json(response).into_response()
}

/// Fan-out-aware search for a specific index.
///
/// Executes the search locally, fans out to all peer nodes, and merges results.
#[cfg(feature = "search")]
async fn search_index_fanout(
    axum::extract::Path(index): axum::extract::Path<String>,
    axum::extract::State(state): axum::extract::State<SearchFanoutState>,
    headers: axum::http::HeaderMap,
    axum::Json(request): axum::Json<chronik_search::api::SearchRequest>,
) -> axum::response::Response {
    use axum::response::IntoResponse;

    let size = request.size;

    // If this is a forwarded request, just execute locally and return
    if super::query_router::is_forwarded_request(&headers) {
        let result = chronik_search::handlers::search_index(
            axum::extract::Path(index),
            axum::extract::State(state.search_api),
            axum::Json(request),
        )
        .await;
        return result.into_response();
    }

    // Execute locally and parse the response
    let local_response = execute_local_search_index(
        state.search_api.clone(),
        index.clone(),
        request.clone(),
    )
    .await;

    // Fan out to peers
    let peers = state.query_router.all_peers();
    let path = format!("/{}/_search", index);

    if let Some(local_resp) = local_response {
        // Local succeeded — merge with peers
        let response = if !peers.is_empty() {
            let fan_out = state
                .query_router
                .fan_out_post::<_, chronik_search::api::SearchResponse>(&path, &request, &peers)
                .await;
            let unanswered = fan_out.failures.len();
            if !fan_out.is_complete() {
                tracing::warn!(
                    index = %index,
                    failures = %fan_out.failure_summary(),
                    "Serving a partial index _search: some nodes did not answer"
                );
            }
            tracing::debug!(
                index = %index,
                peer_count = fan_out.responses.len(),
                "Merging ES index search results from peers"
            );
            let mut merged =
                super::query_router::merge_search_responses(local_resp, fan_out.responses, size);
            super::query_router::note_unanswered_peers(&mut merged, unanswered);
            merged
        } else {
            local_resp
        };
        axum::Json(response).into_response()
    } else if !peers.is_empty() {
        // Local failed but peers might have the index
        let fan_out = state
            .query_router
            .fan_out_post::<_, chronik_search::api::SearchResponse>(&path, &request, &peers)
            .await;
        let unanswered = fan_out.failures.len();
        if !fan_out.is_complete() {
            tracing::warn!(
                index = %index,
                failures = %fan_out.failure_summary(),
                "Serving a partial index _search: some nodes did not answer"
            );
        }
        let peer_responses = fan_out.responses;
        if !peer_responses.is_empty() {
            let empty = chronik_search::api::SearchResponse {
                took: 0,
                timed_out: false,
                _shards: chronik_search::api::ShardInfo {
                    total: 0,
                    successful: 0,
                    skipped: 0,
                    failed: 0,
                },
                hits: chronik_search::api::HitsInfo {
                    total: chronik_search::api::TotalHits {
                        value: 0,
                        relation: "eq".to_string(),
                    },
                    max_score: None,
                    hits: Vec::new(),
                },
                aggregations: None,
            };
            let mut merged =
                super::query_router::merge_search_responses(empty, peer_responses, size);
            // The local index search failed, so count this node among the
            // shards that did not contribute.
            super::query_router::note_unanswered_peers(&mut merged, unanswered + 1);
            axum::Json(merged).into_response()
        } else {
            // No results from anywhere — return original local error
            let result = chronik_search::handlers::search_index(
                axum::extract::Path(index),
                axum::extract::State(state.search_api),
                axum::Json(request),
            )
            .await;
            result.into_response()
        }
    } else {
        // No peers and local failed — return local error
        let result = chronik_search::handlers::search_index(
            axum::extract::Path(index),
            axum::extract::State(state.search_api),
            axum::Json(request),
        )
        .await;
        result.into_response()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_module_compiles() {
        // Basic compile test
        assert!(true);
    }
}
