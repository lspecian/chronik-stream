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

/// Run a local search handler and read its `SearchResponse` back. A non-200
/// answer (e.g. "no such index") is returned as it was, for the caller to
/// serve when no peer has anything either — re-running the search to rebuild
/// it cost another full search, time limit included.
#[cfg(feature = "search")]
async fn local_search_response(
    response: axum::response::Response,
) -> Result<chronik_search::api::SearchResponse, axum::response::Response> {
    use axum::response::IntoResponse;

    let (parts, body) = response.into_parts();
    let status = parts.status;
    let body_bytes = match hyper::body::to_bytes(body).await {
        Ok(b) => b,
        Err(e) => {
            return Err((
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("could not read the local search response: {e}"),
            )
                .into_response())
        }
    };
    if status == axum::http::StatusCode::OK {
        if let Ok(parsed) = serde_json::from_slice(&body_bytes) {
            return Ok(parsed);
        }
    }
    Err(axum::response::Response::from_parts(parts, axum::body::boxed(axum::body::Full::from(body_bytes))))
}

#[cfg(feature = "search")]
fn empty_search_response() -> chronik_search::api::SearchResponse {
    chronik_search::api::SearchResponse {
        took: 0,
        timed_out: false,
        _shards: chronik_search::api::ShardInfo { total: 0, successful: 0, skipped: 0, failed: 0 },
        hits: chronik_search::api::HitsInfo {
            total: chronik_search::api::TotalHits { value: 0, relation: "eq".to_string() },
            max_score: None,
            hits: Vec::new(),
        },
        aggregations: None,
    }
}

/// Search this node and every peer at the same time, and merge.
///
/// The local search and the fan-out used to run one after the other, so a
/// node that spent its whole time limit on its own archives made the client
/// wait that long again for the peers.
#[cfg(feature = "search")]
async fn search_everywhere<F>(
    state: SearchFanoutState,
    path: String,
    request: chronik_search::api::SearchRequest,
    local: F,
    what: &'static str,
) -> axum::response::Response
where
    F: std::future::Future<Output = axum::response::Response>,
{
    use axum::response::IntoResponse;

    let size = request.size;
    let peers = state.query_router.all_peers();
    let (local, fan_out) = tokio::join!(local_search_response_of(local), async {
        if peers.is_empty() {
            None
        } else {
            Some(
                state
                    .query_router
                    .fan_out_post::<_, chronik_search::api::SearchResponse>(&path, &request, &peers)
                    .await,
            )
        }
    });

    let Some(fan_out) = fan_out else {
        return match local {
            Ok(resp) => axum::Json(resp).into_response(),
            Err(error_response) => error_response,
        };
    };
    let unanswered = fan_out.failures.len();
    if !fan_out.is_complete() {
        tracing::warn!(
            path = %path,
            failures = %fan_out.failure_summary(),
            "Serving a partial {}: some nodes did not answer", what
        );
    }
    tracing::debug!(path = %path, peer_count = fan_out.responses.len(), "Merging {} results from peers", what);

    match local {
        Ok(local_resp) => {
            let mut merged =
                super::query_router::merge_search_responses(local_resp, fan_out.responses, size);
            // Hits are ranked, so a partial answer is still worth serving — but
            // `_shards.failed` has to say so, or a search that missed a node is
            // indistinguishable from one where that node held no match (issue #57).
            super::query_router::note_unanswered_peers(&mut merged, unanswered);
            axum::Json(merged).into_response()
        }
        Err(error_response) if fan_out.responses.is_empty() => error_response,
        Err(_) => {
            let mut merged = super::query_router::merge_search_responses(
                empty_search_response(),
                fan_out.responses,
                size,
            );
            // The local search failed, so this answer is short by this node
            // too — count it with the peers that did not reply.
            super::query_router::note_unanswered_peers(&mut merged, unanswered + 1);
            axum::Json(merged).into_response()
        }
    }
}

#[cfg(feature = "search")]
async fn local_search_response_of<F>(
    local: F,
) -> Result<chronik_search::api::SearchResponse, axum::response::Response>
where
    F: std::future::Future<Output = axum::response::Response>,
{
    local_search_response(local.await).await
}

/// Fan-out-aware search across all indices.
#[cfg(feature = "search")]
async fn search_all_fanout(
    axum::extract::State(state): axum::extract::State<SearchFanoutState>,
    headers: axum::http::HeaderMap,
    axum::Json(request): axum::Json<chronik_search::api::SearchRequest>,
) -> axum::response::Response {
    use axum::response::IntoResponse;

    let api = state.search_api.clone();
    let local_request = request.clone();
    let local = async move {
        chronik_search::handlers::search_all(axum::extract::State(api), axum::Json(local_request))
            .await
            .into_response()
    };
    // A forwarded request is a peer's share of a fan-out: answer locally only.
    if super::query_router::is_forwarded_request(&headers) {
        return local.await;
    }
    search_everywhere(state, "/_search".to_string(), request, local, "_search").await
}

/// Fan-out-aware search for a specific index.
#[cfg(feature = "search")]
async fn search_index_fanout(
    axum::extract::Path(index): axum::extract::Path<String>,
    axum::extract::State(state): axum::extract::State<SearchFanoutState>,
    headers: axum::http::HeaderMap,
    axum::Json(request): axum::Json<chronik_search::api::SearchRequest>,
) -> axum::response::Response {
    use axum::response::IntoResponse;

    let api = state.search_api.clone();
    let local_request = request.clone();
    let local_index = index.clone();
    let local = async move {
        chronik_search::handlers::search_index(
            axum::extract::Path(local_index),
            axum::extract::State(api),
            axum::Json(local_request),
        )
        .await
        .into_response()
    };
    if super::query_router::is_forwarded_request(&headers) {
        return local.await;
    }
    search_everywhere(state, format!("/{}/_search", index), request, local, "index _search").await
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_module_compiles() {
        // Basic compile test
        assert!(true);
    }
}
