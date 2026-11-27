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
    metadata_store: Arc<dyn chronik_common::metadata::MetadataStore>,
) -> axum::Router {
    use axum::routing::{delete, get, post, put};

    let fanout_state = SearchFanoutState {
        search_api: search_api.clone(),
        query_router,
        metadata_store,
    };

    // Fan-out-aware search endpoints
    let search_routes = axum::Router::new()
        .route("/_search", get(search_all_fanout).post(search_all_fanout))
        .route("/{index}/_search", get(search_index_fanout).post(search_index_fanout))
        .with_state(fanout_state);

    // Original CRUD/management endpoints (no fan-out needed for writes)
    let crud_routes = axum::Router::new()
        .route("/metrics", get(chronik_search::handlers::metrics_handler))
        .route(
            "/{index}/_doc/{id}",
            post(chronik_search::handlers::index_document)
                .put(chronik_search::handlers::index_document)
                .get(chronik_search::handlers::get_document)
                .delete(chronik_search::handlers::delete_document),
        )
        .route("/{index}", put(chronik_search::handlers::create_index))
        .route("/{index}", delete(chronik_search::handlers::delete_index))
        .route("/{index}/_mapping", get(chronik_search::handlers::get_mapping))
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
    /// For the partition map: which partitions this node leads, read fresh
    /// for counts and aggregations.
    metadata_store: Arc<dyn chronik_common::metadata::MetadataStore>,
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
        _segments: None,
        _chronik_partial: None,
    }
}

/// What each node (this one included) is asked for: its top `from + size`
/// hits, unpaged. Each node paging on its own skipped `from` hits per node, so
/// page 2 of a cluster search lost the records that belonged on it; the
/// coordinator pages the merged list once instead.
#[cfg(feature = "search")]
fn shard_request(request: &chronik_search::api::SearchRequest) -> chronik_search::api::SearchRequest {
    let mut shard = request.clone();
    shard.size = request.from.saturating_add(request.size);
    shard.from = 0;
    // Each node counts the partitions it leads itself.
    shard._chronik_partitions = None;
    shard
}

/// Turn the merged partial (each node's count and aggregations over the
/// partitions it leads) into the exact total and the final aggregations.
#[cfg(feature = "search")]
fn finish_counts(
    request: &chronik_search::api::SearchRequest,
    merged: &mut chronik_search::api::SearchResponse,
    unanswered: usize,
) {
    let Some(partial) = merged._chronik_partial.take() else { return };
    let exact = unanswered == 0 && !merged.timed_out;
    merged.hits.total = chronik_search::api::TotalHits {
        value: partial.count,
        relation: if exact { "eq" } else { "gte" }.to_string(),
    };
    if let Some(aggs) = request.aggs.as_ref().or(request.aggregations.as_ref()) {
        match chronik_search::topic_aggs::TopicAggs::prepare(aggs).and_then(|t| t.finalize(partial.aggs)) {
            Ok(v) => merged.aggregations = Some(v),
            Err(e) => tracing::warn!("finalising aggregations failed: {}", e),
        }
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
    F: std::future::Future<Output = chronik_search::handlers::SearchResult>,
{
    use axum::response::IntoResponse;

    let (from, size) = (request.from, request.size);
    let sort = chronik_search::sort::SortSpec::of(request.sort.as_deref());
    let shard = shard_request(&request);
    let peers = state.query_router.all_peers();
    let (local, fan_out) = tokio::join!(local_search_response_of(local), async {
        if peers.is_empty() {
            None
        } else {
            Some(
                state
                    .query_router
                    .fan_out_post::<_, chronik_search::api::SearchResponse>(&path, &shard, &peers)
                    .await,
            )
        }
    });

    let Some(fan_out) = fan_out else {
        // The local search answered the whole window: page it here too.
        return match local {
            Ok(resp) => {
                let mut merged = super::query_router::merge_search_responses(resp, Vec::new(), from, size, sort.as_ref());
                finish_counts(&request, &mut merged, 0);
                axum::Json(merged).into_response()
            }
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
                super::query_router::merge_search_responses(local_resp, fan_out.responses, from, size, sort.as_ref());
            // Hits are ranked, so a partial answer is still worth serving — but
            // `_shards.failed` has to say so, or a search that missed a node is
            // indistinguishable from one where that node held no match (issue #57).
            super::query_router::note_unanswered_peers(&mut merged, unanswered);
            finish_counts(&request, &mut merged, unanswered);
            axum::Json(merged).into_response()
        }
        Err(error_response) if fan_out.responses.is_empty() => error_response,
        Err(_) => {
            let mut merged = super::query_router::merge_search_responses(
                empty_search_response(),
                fan_out.responses,
                from,
                size,
                sort.as_ref(),
            );
            // The local search failed, so this answer is short by this node
            // too — count it with the peers that did not reply.
            super::query_router::note_unanswered_peers(&mut merged, unanswered + 1);
            finish_counts(&request, &mut merged, unanswered + 1);
            axum::Json(merged).into_response()
        }
    }
}

#[cfg(feature = "search")]
#[allow(clippy::result_large_err)] // axum error response, built once per failed request
async fn local_search_response_of<F>(
    local: F,
) -> Result<chronik_search::api::SearchResponse, axum::response::Response>
where
    F: std::future::Future<Output = chronik_search::handlers::SearchResult>,
{
    use axum::response::IntoResponse;
    local.await.map(|axum::Json(resp)| resp).map_err(IntoResponse::into_response)
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
    // A forwarded request is a peer's share of a fan-out and already asks for
    // the whole window; otherwise this node is one shard of its own fan-out.
    let forwarded = super::query_router::is_forwarded_request(&headers);
    let local_request = if forwarded { request.clone() } else { shard_request(&request) };
    let local = async move {
        chronik_search::handlers::search_all(axum::extract::State(api), axum::Json(local_request)).await
    };
    // A forwarded request is a peer's share of a fan-out: answer locally only.
    if forwarded {
        return local.await.into_response();
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
    // A forwarded request is a peer's share of a fan-out and already asks for
    // the whole window; otherwise this node is one shard of its own fan-out.
    let forwarded = super::query_router::is_forwarded_request(&headers);
    let mut request = request;
    // An exact count or aggregations: every node (this one too) answers a
    // partial over the partitions it leads; the merge adds them up.
    if !forwarded
        && chronik_search::topic_aggs::wants_exact_count(
            request.size,
            request.track_total_hits.as_ref(),
            request.aggs.is_some() || request.aggregations.is_some(),
        )
    {
        request._chronik_partial = true;
    }
    let mut local_request = if forwarded { request.clone() } else { shard_request(&request) };
    // Counting: which partitions this node leads, read fresh — a topic created
    // after the map was filled is otherwise unknown ("count everything", so
    // every replica counted), and leadership moves.
    if local_request._chronik_partial {
        state.query_router.refresh_partition_map(state.metadata_store.as_ref(), &index).await;
    }
    local_request._chronik_partitions = state.query_router.led_partitions(&index).await.map(|led| {
        let mut led: Vec<i32> = led.into_iter().collect();
        led.sort_unstable();
        led
    });
    let local_index = index.clone();
    let local = async move {
        chronik_search::handlers::search_index(
            axum::extract::Path(local_index),
            axum::extract::State(api),
            axum::Json(local_request),
        )
        .await
    };
    if forwarded {
        return local.await.into_response();
    }
    search_everywhere(state, format!("/{}/_search", index), request, local, "index _search").await
}

