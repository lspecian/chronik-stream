//! HTTP request handlers for the search API.

use crate::api::{
    SearchApi, SearchRequest, SearchResponse, IndexDocumentRequest, IndexDocumentResponse,
    GetDocumentResponse, DeleteDocumentResponse, CatIndexInfo, ErrorResponse, ErrorInfo,
    ShardInfo, HitsInfo, TotalHits, Hit, IndexMapping, FieldMapping, QueryDsl, BoolQuery,
    HighlightConfig,
};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Json},
};
use chronik_common::{Result, Error};
use serde::Serialize;
use std::{
    collections::HashMap,
    sync::Arc,
    time::SystemTime,
};
use tantivy::{
    collector::TopDocs,
    query::{Query as TantivyQuery, QueryParser, TermQuery, RangeQuery, BooleanQuery, Occur, AllQuery},
    schema::{FieldType, Value},
    Term,
};
use tracing::{error, debug};
use uuid::Uuid;

/// Health check response
#[derive(Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub version: String,
}

/// Health check handler
pub async fn health_check() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "green".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    })
}

/// Metrics handler (returns Prometheus metrics)
pub async fn metrics_handler() -> impl IntoResponse {
    use prometheus::Encoder;
    let encoder = prometheus::TextEncoder::new();
    let metric_families = prometheus::gather();
    let mut buffer = Vec::new();
    encoder.encode(&metric_families, &mut buffer).unwrap();
    String::from_utf8(buffer).unwrap()
}

/// HP-1.3: Best-effort extraction of a plain-text query from the simplest
/// ElasticSearch DSLs. Used only for `MatchAll` and flat `Match` — anything
/// structural (Bool, Term, Range, …) must go through the structured hot
/// path (`build_tantivy_query` per partition) so clause semantics are
/// preserved. Issue #2: previously this function concatenated bool.must
/// clauses with spaces, which Tantivy's multi-word parser then OR'd,
/// silently dropping the AND.
fn extract_simple_query_text(dsl: &QueryDsl) -> Option<String> {
    match dsl {
        // match_all is structured (AllQuery): the simple path sanitizes its
        // text to a bag of words, which turned "*" into nothing and made
        // match_all return 0 hits on a hot topic (#65).
        // Only a match on the whole record text may take the simple path: it
        // searches value/key/headers and drops the field name. A match on a
        // named field (`marker`, `user.name`) used to come here too, so it
        // matched the term in ANY field — while the archived index, which
        // honours the field, did not. Field matches go structured.
        QueryDsl::Match(m)
            if m
                .field_value
                .keys()
                .next()
                .is_some_and(|f| matches!(f.as_str(), "value" | "_value" | "_all")) =>
        {
            m.field_value.values().next().and_then(|v| match v {
                serde_json::Value::String(s) => Some(s.clone()),
                serde_json::Value::Object(obj) => obj
                    .get("query")
                    .and_then(|q| q.as_str())
                    .map(|s| s.to_string()),
                _ => None,
            })
        }
        _ => None, // Bool/Term/Range/etc. go through the structured path.
    }
}

/// HP-1.3: Convert a HotHit into the Elasticsearch-compatible Hit shape,
/// matching the field layout produced by `search_tantivy_index`.
fn hot_hit_to_es_hit(hit: crate::hot_text_index::HotHit) -> Hit {
    let mut source = serde_json::Map::new();
    source.insert("topic".to_string(), serde_json::Value::String(hit.topic.clone()));
    source.insert(
        "partition".to_string(),
        serde_json::Value::Number(hit.partition.into()),
    );
    source.insert(
        "offset".to_string(),
        serde_json::Value::Number(hit.offset.into()),
    );
    source.insert(
        "timestamp".to_string(),
        serde_json::Value::Number(hit.timestamp.into()),
    );
    if let Some(k) = hit.key {
        source.insert("key".to_string(), serde_json::Value::String(k));
    }
    source.insert("value".to_string(), serde_json::Value::String(hit.value));

    Hit {
        _index: hit.topic,
        // Partition-qualified, matching search_tantivy_index — offset alone
        // collides across partitions, collapsing the (_index,_id) dedup.
        _id: format!("{}-{}", hit.partition, hit.offset),
        _score: Some(hit.score),
        _source: serde_json::Value::Object(source),
        highlight: None,
    }
}

/// HP-1.3: Query the hot index for a specific topic and return ES-shaped Hits.
/// Empty Vec if hot index absent or topic unknown.
///
/// Routing:
///   - `Match` on the whole record (`value`, `_all`) → flat QueryParser path
///   - Anything else (including `Bool`, Term, Range) → structured path that
///     builds a real Tantivy query per partition, preserving bool semantics
///     (issue #2).
async fn search_hot_topic(
    api: &Arc<SearchApi>,
    topic: &str,
    request: &SearchRequest,
) -> Vec<Hit> {
    let Some(hot_idx) = api.hot_text_index.as_ref() else {
        return Vec::new();
    };
    // No query means match_all, as in Elasticsearch and on the cold path.
    let match_all = QueryDsl::MatchAll(crate::api::MatchAllQuery {});
    let dsl = request.query.as_ref().unwrap_or(&match_all);
    // The whole window: page `from` of the merge may come from here.
    let k = request.from.saturating_add(request.size);

    if let Some(query_text) = extract_simple_query_text(dsl) {
        return match hot_idx.search_topic(topic, &query_text, k).await {
            Ok(hits) => hits.into_iter().map(hot_hit_to_es_hit).collect(),
            Err(e) => {
                debug!("hot text search for topic {} failed: {}", topic, e);
                Vec::new()
            }
        };
    }

    // Structured DSL: build the Tantivy query against each partition's schema.
    let dsl_owned = dsl.clone();
    let result = hot_idx
        .search_topic_structured(
            topic,
            move |schema, index| {
                build_tantivy_query(&dsl_owned, schema, Some(index))
                    .map_err(|e| anyhow::anyhow!("build_tantivy_query: {}", e))
            },
            k,
        )
        .await;
    match result {
        Ok(hits) => hits.into_iter().map(hot_hit_to_es_hit).collect(),
        Err(e) => {
            debug!("hot structured search for topic {} failed: {}", topic, e);
            Vec::new()
        }
    }
}

/// HP-1.3: Query the hot index across every known topic and return ES-shaped Hits.
/// Routes to the same simple-vs-structured split as `search_hot_topic`.
async fn search_hot_all(api: &Arc<SearchApi>, request: &SearchRequest) -> Vec<Hit> {
    let Some(hot_idx) = api.hot_text_index.as_ref() else {
        return Vec::new();
    };
    // No query means match_all, as in Elasticsearch and on the cold path.
    let match_all = QueryDsl::MatchAll(crate::api::MatchAllQuery {});
    let dsl = request.query.as_ref().unwrap_or(&match_all);

    let mut topics: Vec<String> = hot_idx
        .partition_keys()
        .into_iter()
        .map(|(t, _)| t)
        .collect();
    topics.sort();
    topics.dedup();

    // The whole window: page `from` of the merge may come from here.
    let k = request.from.saturating_add(request.size);
    let simple_text = extract_simple_query_text(dsl);
    let mut out = Vec::new();
    for topic in topics {
        if let Some(filter) = &request.index {
            if filter != &topic {
                continue;
            }
        }
        match simple_text.as_ref() {
            Some(q) => match hot_idx.search_topic(&topic, q, k).await {
                Ok(hits) => out.extend(hits.into_iter().map(hot_hit_to_es_hit)),
                Err(e) => debug!("hot text search for topic {} failed: {}", topic, e),
            },
            None => {
                let dsl_owned = dsl.clone();
                let result = hot_idx
                    .search_topic_structured(
                        &topic,
                        move |schema, index| {
                            build_tantivy_query(&dsl_owned, schema, Some(index))
                                .map_err(|e| anyhow::anyhow!("build_tantivy_query: {}", e))
                        },
                        k,
                    )
                    .await;
                match result {
                    Ok(hits) => out.extend(hits.into_iter().map(hot_hit_to_es_hit)),
                    Err(e) => debug!("hot structured search for topic {} failed: {}", topic, e),
                }
            }
        }
    }
    out
}

/// HP-1.3: Merge hot hits with cold hits, preferring hot on (_index, _id) dedup.
/// Both inputs are consumed; returned Vec is sorted by score desc.
fn merge_hot_and_cold_hits(hot: Vec<Hit>, cold: Vec<Hit>) -> Vec<Hit> {
    use std::collections::HashSet;
    let mut seen: HashSet<(String, String)> = HashSet::with_capacity(hot.len() + cold.len());
    let mut merged = Vec::with_capacity(hot.len() + cold.len());

    for h in hot {
        let key = (h._index.clone(), h._id.clone());
        if seen.insert(key) {
            merged.push(h);
        }
    }
    for c in cold {
        let key = (c._index.clone(), c._id.clone());
        if seen.insert(key) {
            merged.push(c);
        }
    }
    merged.sort_by(crate::ranking::hit_order);
    merged
}

/// Search all indices
pub async fn search_all(
    State(api): State<Arc<SearchApi>>,
    Json(request): Json<SearchRequest>,
) -> impl IntoResponse {
    let start = SystemTime::now();
    let deadline = search_deadline(&request, tokio::time::Instant::now());
    if let Err(reason) = crate::ranking::check_result_window(request.from, request.size) {
        return Err(result_window_error(&reason));
    }

    // Generate cache key
    let cache_key = crate::cache::QueryCache::generate_key(None, &request);

    // HP-1.3: skip cache when hot index is active — cached responses would
    // mask newly-visible NRT docs.
    let hot_enabled = api.hot_text_index.is_some();

    // Check cache first
    if !hot_enabled {
        if let Some(cached_response) = api.cache.get(&cache_key) {
            return Ok(Json(cached_response));
        }
    }

    // Search across all indices
    let mut all_hits = Vec::new();
    let indices = api.indices.clone();

    // Search in-memory indices (created via REST API)
    for entry in indices.iter() {
        let index_name = entry.key().clone();
        let state = entry.value();

        match search_in_index(&index_name, &state, &request).await {
            Ok(mut hits) => all_hits.append(&mut hits),
            Err(e) => {
                error!("Error searching in-memory index {}: {}", index_name, e);
            }
        }
    }

    // Also search WAL-created indices from disk
    if let Some(base_path) = api.get_index_base_path() {
        // Search WAL indices (data/tantivy_indexes/)
        match search_wal_indices(base_path, &request).await {
            Ok(mut hits) => {
                debug!("Found {} hits from WAL-created indices", hits.len());
                all_hits.append(&mut hits);
            }
            Err(e) => {
                error!("Error searching WAL-created indices: {}", e);
            }
        }

        // Also search real-time indices (data/index/) - they're in a sibling directory
        let realtime_path = base_path.replace("tantivy_indexes", "index");
        match search_wal_indices(&realtime_path, &request).await {
            Ok(mut hits) => {
                debug!("Found {} hits from real-time indices", hits.len());
                all_hits.append(&mut hits);
            }
            Err(e) => {
                debug!("No real-time indices found (this is normal): {}", e);
            }
        }
    }

    // Object-store cold segments — the WalIndexer's ACTUAL output. Same fix as
    // `search_index`: the on-disk reads above never see the nested `.tar.gz`
    // archives the WalIndexer writes through the object store. `search_index`
    // (topic-scoped `/{topic}/_search`) was wired for this, but memory recall
    // and any client hitting the generic `/_search` came through here and still
    // missed cold data. When the request targets a specific index (recall always
    // does), read that topic's segments; otherwise fan across all topics in the
    // segment index.
    let mut timed_out = false;
    if let Some(wal_indexer) = api.wal_indexer() {
        let index = wal_indexer.segment_index();
        let segments = match request.index {
            Some(ref idx) => index.get_segments_for_topic(idx).await,
            None => index.get_all_segments().await,
        };
        match segments {
            Ok(segments) => {
                let cold = search_cold_segments(&api, segments, &request, deadline).await;
                timed_out = cold.timed_out;
                all_hits.extend(cold.hits);
            }
            Err(e) => debug!("segment lookup for the cold search failed: {}", e),
        }
    }

    // Filter by index/topic name if specified in request body
    if let Some(ref index_filter) = request.index {
        all_hits.retain(|hit| hit._index == *index_filter);
    }

    // HP-1.3: merge with hot index hits (NRT), preferring hot on dedup.
    if hot_enabled {
        let hot_hits = search_hot_all(&api, &request).await;
        if !hot_hits.is_empty() {
            debug!("hot text index contributed {} hits", hot_hits.len());
        }
        all_hits = merge_hot_and_cold_hits(hot_hits, all_hits);
    }
    // Every source returned its top `from + size`; page the merged list once.
    let (all_hits, found) = crate::ranking::page(all_hits, request.from, request.size);

    let took = start.elapsed().unwrap_or_default().as_millis() as u64;

    let total_shards = indices.len() as u32 + if api.get_index_base_path().is_some() { 1 } else { 0 };

    let response = SearchResponse {
        took,
        timed_out,
        _shards: ShardInfo {
            total: total_shards,
            successful: total_shards,
            skipped: 0,
            failed: 0,
        },
        hits: HitsInfo {
            total: TotalHits {
                value: found as u64,
                relation: total_relation(found, request.from + request.size, timed_out).to_string(),
            },
            max_score: all_hits.first().and_then(|h| h._score),
            hits: all_hits,
        },
        aggregations: None,
    };

    // Cache the response — skip when hot path active (would cache stale NRT
    // view), and never cache a partial answer.
    if !hot_enabled && !timed_out {
        api.cache.put(cache_key, response.clone());
    }

    Ok(Json(response))
}

/// Search specific index
pub async fn search_index(
    Path(index): Path<String>,
    State(api): State<Arc<SearchApi>>,
    Json(request): Json<SearchRequest>,
) -> impl IntoResponse {
    let start = SystemTime::now();
    let deadline = search_deadline(&request, tokio::time::Instant::now());
    if let Err(reason) = crate::ranking::check_result_window(request.from, request.size) {
        return Err(result_window_error(&reason));
    }

    // v2.5.4: Build cold hits from every source available for this topic,
    // in priority order:
    //   1. In-memory REST-registered index (api.indices)     — aggregations
    //   2. WAL Tantivy archives on disk ({data}/tantivy_indexes)
    //   3. Realtime indexer's in-memory-flushed index ({data}/index)
    //
    // Previously `search_index` returned 404 for auto-created Kafka topics
    // unless the hot path was enabled, because it only consulted (1). This
    // matches the multi-source behavior `search_all` already had.
    let mut cold_hits = Vec::<Hit>::new();
    let mut aggregations = None;
    let mut have_any_cold_source = false;

    if let Some(state) = api.indices.get(&index) {
        have_any_cold_source = true;
        let hits = search_in_index(&index, &state, &request)
            .await
            .map_err(|e| search_error(e))?;
        cold_hits.extend(hits);

        if let Some(aggs) = request.aggs.as_ref().or(request.aggregations.as_ref()) {
            let query = match &request.query {
                Some(query_dsl) => build_tantivy_query(query_dsl, &state.schema, Some(&state.index))
                    .map_err(|e| search_error(e))?,
                None => Box::new(AllQuery),
            };
            let agg_executor = crate::aggregations::AggregationExecutor::new(
                state.reader.clone(),
                state.schema.clone(),
            );
            aggregations = Some(
                agg_executor
                    .execute(query, aggs.clone())
                    .map_err(|e| search_error(Error::Internal(format!("Aggregation failed: {}", e))))?,
            );
        }
    }

    if let Some(base_path) = api.get_index_base_path() {
        // WAL-created Tantivy archives under `{data}/tantivy_indexes/{topic}[-part]/`
        match search_wal_indices_for_topic(base_path, &index, &request).await {
            Ok(hits) => {
                if !hits.is_empty() || std::path::Path::new(base_path).exists() {
                    have_any_cold_source = true;
                }
                cold_hits.extend(hits);
            }
            Err(e) => debug!(topic = %index, "WAL disk search returned no results: {}", e),
        }
        // Realtime indexer writes to `{data}/index/{topic}[-part]/`
        let realtime_path = base_path.replace("tantivy_indexes", "index");
        match search_wal_indices_for_topic(&realtime_path, &index, &request).await {
            Ok(hits) => {
                if !hits.is_empty() || std::path::Path::new(&realtime_path).exists() {
                    have_any_cold_source = true;
                }
                cold_hits.extend(hits);
            }
            Err(e) => debug!(topic = %index, "Realtime disk search returned no results: {}", e),
        }
    }

    // Object-store cold segments — the WalIndexer's ACTUAL output. The two
    // on-disk reads above miss these: the WalIndexer writes each segment's
    // Tantivy index as a nested `{topic}/partition-N/segment-*.tar.gz` archive
    // THROUGH the object store (not to the on-disk `index_base_path`), so
    // Kafka-produced searchable topics were served only by the decaying hot
    // index (regression 0e37747). Always consulted: a leftover `{data}/index`
    // directory (RealtimeIndexer, removed in 08b1c58) is frozen at the day that
    // indexer stopped, and skipping the archives whenever it answered served
    // the topic from that stale copy. Records found in both are deduplicated
    // when the page is cut (`crate::ranking::page`).
    let mut timed_out = false;
    if let Some(wal_indexer) = api.wal_indexer() {
        match wal_indexer.segment_index().get_segments_for_topic(&index).await {
            Ok(segments) => {
                let cold = search_cold_segments(&api, segments, &request, deadline).await;
                // Archived segments exist: the topic is known even when none
                // of them matches. Counting it only when it returned hits
                // turned every no-match query on an archived topic into "no
                // such index" once the hot index was off.
                if cold.segments > 0 {
                    have_any_cold_source = true;
                }
                timed_out = cold.timed_out;
                cold_hits.extend(cold.hits);
            }
            Err(e) => debug!(topic = %index, "segment lookup for the cold search failed: {}", e),
        }
    }

    // If no cold source was found AND the hot path is off, preserve the
    // v2.5.x "index not found" error. With the hot path on, absence from
    // cold sources just means the topic is NRT-only for now.
    if !have_any_cold_source && cold_hits.is_empty() && api.hot_text_index.is_none() {
        return Err(index_not_found_error(&index));
    }

    // HP-1.3: merge with hot index hits; hot wins on (_index, _id) dedup.
    let hits = if api.hot_text_index.is_some() {
        let hot_hits = search_hot_topic(&api, &index, &request).await;
        merge_hot_and_cold_hits(hot_hits, cold_hits)
    } else {
        cold_hits
    };
    // Every source returned its top `from + size`; page the merged list once.
    let (hits, found) = crate::ranking::page(hits, request.from, request.size);

    let took = start.elapsed().unwrap_or_default().as_millis() as u64;

    let response = SearchResponse {
        took,
        timed_out,
        _shards: ShardInfo {
            total: 1,
            successful: 1,
            skipped: 0,
            failed: 0,
        },
        hits: HitsInfo {
            total: TotalHits {
                value: found as u64,
                relation: total_relation(found, request.from + request.size, timed_out).to_string(),
            },
            max_score: hits.first().and_then(|h| h._score),
            hits,
        },
        aggregations,
    };
    
    Ok(Json(response))
}

/// Search WAL-created Tantivy indices from disk
/// v2.5.4: Like `search_wal_indices` but filtered to a single topic.
///
/// Used by `search_index` to search auto-created topics' on-disk Tantivy
/// indexes (realtime and WAL) when the REST-registered in-memory index
/// map has no entry. Fixes the "POST /:topic/_search returns 404" gap
/// for Kafka-produced topics when the hot path is disabled or cold.
///
/// WAL/realtime index directories are named `{topic}` or `{topic}-{partition}`.
async fn search_wal_indices_for_topic(
    base_path: &str,
    topic: &str,
    request: &SearchRequest,
) -> Result<Vec<Hit>> {
    use std::fs;
    use std::path::Path;
    use tantivy::Index;

    let mut all_hits = Vec::new();
    let base = Path::new(base_path);
    if !base.exists() {
        return Ok(all_hits);
    }

    let entries = fs::read_dir(base)
        .map_err(|e| Error::Internal(format!("Failed to read index directory: {}", e)))?;

    for entry in entries {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(dir_name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };

        // Match either "topic" or "topic-N". We start with starts_with(topic)
        // then confirm the suffix is empty or begins with '-' to avoid
        // matching "topicfoo" when asked about "topic".
        if !dir_name.starts_with(topic) {
            continue;
        }
        let suffix = &dir_name[topic.len()..];
        if !suffix.is_empty() && !suffix.starts_with('-') {
            continue;
        }

        match Index::open_in_dir(&path) {
            Ok(index) => {
                chronik_storage::register_analyzer(&index);
                match query_searcher(&index)
                    .and_then(|s| search_tantivy_index(topic, &index, &s, request))
                {
                    Ok(mut hits) => {
                        // Normalize _index to the queried topic name (strip "-N" suffix)
                        for h in &mut hits {
                            h._index = topic.to_string();
                        }
                        all_hits.append(&mut hits);
                    }
                    Err(e) => debug!("Search error in {}: {}", dir_name, e),
                }
            }
            Err(e) => debug!("Could not open index at {}: {}", path.display(), e),
        }
    }
    Ok(all_hits)
}

/// Search a topic's cold Tantivy segments via the WalIndexer's object store.
///
/// This is the fix for the 0e37747 regression: the WalIndexer writes each
/// sealed segment's Tantivy index as a nested `{topic}/partition-N/segment-*.tar.gz`
/// archive THROUGH the object store (local dir or S3), registering it in the
/// segment index. The on-disk `search_wal_indices*` readers never see these —
/// they scan `index_base_path` on the filesystem, which is a different physical
/// location and a different (flat, unarchived) layout. So Kafka-produced
/// searchable topics were served only by the in-memory hot-text index, which
/// ages out — making documents silently disappear from search over time.
///
/// The segments are opened through the node's [`ColdSegmentCache`] and
/// searched in parallel on blocking threads, newest first.
///
/// Before v2.17.2 this opened every segment one after another with no bound.
/// A `/_search` naming no topic walked every archive on the node — 6,000 to
/// 13,000 of them on a production cluster, at about five a second the first
/// time — and never answered. Now the search stops at the request's time limit
/// and answers with what it found (`timed_out`), and a query that does not rank
/// (`match_all`, no sort) stops as soon as it has a page of hits.
///
/// [`ColdSegmentCache`]: crate::cold_cache::ColdSegmentCache
async fn search_cold_segments(
    api: &SearchApi,
    mut segments: Vec<chronik_storage::segment_index::SegmentMetadata>,
    request: &SearchRequest,
    deadline: tokio::time::Instant,
) -> ColdSearch {
    use futures::stream::{self, StreamExt};

    let mut out = ColdSearch { segments: segments.len(), ..Default::default() };
    let (Some(wal_indexer), Some(cache)) = (api.wal_indexer(), api.cold_cache()) else {
        return out;
    };
    let window = request.from.saturating_add(request.size);
    if segments.is_empty() || request.size == 0 {
        return out;
    }
    // Newest first, and a fixed order among equals, so the same request
    // searches the same segments in the same order.
    segments.sort_by(|a, b| {
        b.max_timestamp.cmp(&a.max_timestamp).then_with(|| a.object_store_path.cmp(&b.object_store_path))
    });
    let newest: Vec<i64> = segments.iter().map(|s| s.max_timestamp).collect();
    let unranked = is_unranked(request);
    let store = wal_indexer.object_store();
    let request = Arc::new(request.clone());

    // Results come back in segment order (`buffered`, not `buffer_unordered`),
    // so "the segments searched so far" is always a prefix of the newest-first
    // list — which is what makes the early stop below exact.
    let mut searches = stream::iter(segments.into_iter().map(|seg| {
        let (cache, store, request) = (cache.clone(), store.clone(), request.clone());
        async move {
            let reader = cache.reader(store, &seg.object_store_path).await.map_err(|e| {
                Error::Internal(format!("could not open {}: {}", seg.object_store_path, e))
            })?;
            tokio::task::spawn_blocking(move || {
                let mut hits =
                    search_tantivy_index(&seg.topic, reader.index(), &reader.searcher(), &request)?;
                for h in &mut hits {
                    h._index = seg.topic.clone();
                }
                Ok::<_, Error>(hits)
            })
            .await
            .map_err(|e| Error::Internal(format!("segment search task failed: {}", e)))?
        }
    }))
    .buffered(cold_search_concurrency());

    loop {
        match tokio::time::timeout_at(deadline, searches.next()).await {
            Err(_) => {
                out.timed_out = true;
                break;
            }
            Ok(None) => break,
            Ok(Some(result)) => {
                match result {
                    Ok(mut hits) => out.hits.append(&mut hits),
                    Err(e) => debug!("cold segment skipped: {}", e),
                }
                out.searched += 1;
                // Only the top `window` can reach the page; keep a margin for
                // duplicates the merge will drop, and bound memory on a
                // ranked query that reads thousands of segments.
                if out.hits.len() > window.saturating_mul(2) {
                    out.hits.sort_by(crate::ranking::hit_order);
                    out.hits.truncate(window.saturating_mul(2));
                }
                if unranked && can_stop_unranked(&out.hits, window, newest.get(out.searched)) {
                    break;
                }
            }
        }
    }
    if out.timed_out {
        debug!(
            searched = out.searched,
            segments = out.segments,
            "search time limit reached; answering with the segments searched so far"
        );
    }
    out
}

/// For a query whose hits all score alike, whether the segments searched so far
/// already hold the whole top `window`. Hits are ordered newest first
/// ([`crate::ranking::hit_order`]) and segments are searched newest first, so
/// once there are `window` hits and the next segment's newest record is
/// strictly older than the `window`-th hit, nothing unsearched can rank above
/// any hit kept. (Equal timestamps go on searching: the tie-break could still
/// favour the unsearched record.)
fn can_stop_unranked(hits: &[Hit], window: usize, next_newest: Option<&i64>) -> bool {
    if hits.len() < window || window == 0 {
        return false;
    }
    let Some(&next_newest) = next_newest else {
        return true; // nothing left to search
    };
    let mut stamps: Vec<i64> = hits
        .iter()
        .map(|h| crate::ranking::rank_second(h).unwrap_or(i64::MIN))
        .collect();
    // The window-th newest, in the seconds the order compares (rank_second).
    let (_, kth, _) = stamps.select_nth_unstable_by(window - 1, |a, b| b.cmp(a));
    next_newest.div_euclid(1000) < *kth
}

/// What a cold search found.
#[derive(Default)]
struct ColdSearch {
    hits: Vec<Hit>,
    /// Archived segments that could hold a match.
    segments: usize,
    /// Of those, how many were searched.
    searched: usize,
    /// The time limit stopped the search before every segment was searched.
    timed_out: bool,
}

/// "eq" when `found` is every match there is; "gte" when it is a lower bound:
/// the time limit stopped the search, or a source returned a full page and so
/// may have had more.
fn total_relation(found: usize, size: usize, timed_out: bool) -> &'static str {
    if timed_out || found >= size {
        "gte"
    } else {
        "eq"
    }
}

/// A query whose hits all score alike, so any `from + size` of them is a
/// correct page: there is no need to search every segment for the best.
fn is_unranked(request: &SearchRequest) -> bool {
    request.sort.is_none()
        && matches!(request.query, None | Some(QueryDsl::MatchAll(_)))
}

/// Segments searched at once (`CHRONIK_SEARCH_CONCURRENCY`, default: the
/// cores the process may use, at most 8).
fn cold_search_concurrency() -> usize {
    static N: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *N.get_or_init(|| {
        std::env::var("CHRONIK_SEARCH_CONCURRENCY")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|n| *n > 0)
            .unwrap_or_else(|| {
                std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(8)
            })
    })
}

/// When a search has to answer: the request's `timeout`, or
/// `CHRONIK_SEARCH_TIMEOUT_MS` (default 10 s). Comfortably inside the 30 s a
/// node waits for its peers, so a slow node still answers with partial hits
/// instead of being dropped from the merge.
fn search_deadline(request: &SearchRequest, start: tokio::time::Instant) -> tokio::time::Instant {
    static DEFAULT_MS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    let default_ms = *DEFAULT_MS.get_or_init(|| {
        std::env::var("CHRONIK_SEARCH_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(10_000)
    });
    let limit = request
        .timeout
        .as_deref()
        .and_then(parse_time_value)
        .unwrap_or(std::time::Duration::from_millis(default_ms));
    start + limit
}

/// An Elasticsearch time value: "250ms", "10s", "2m"; a bare number is ms.
fn parse_time_value(raw: &str) -> Option<std::time::Duration> {
    use std::time::Duration;
    let v = raw.trim();
    let (num, unit) = match v.find(|c: char| !c.is_ascii_digit()) {
        Some(i) => v.split_at(i),
        None => (v, "ms"),
    };
    let n: u64 = num.parse().ok()?;
    match unit.trim() {
        "ms" => Some(Duration::from_millis(n)),
        "s" => Some(Duration::from_secs(n)),
        "m" => Some(Duration::from_secs(n * 60)),
        "h" => Some(Duration::from_secs(n * 3600)),
        _ => None,
    }
}

/// A searcher for an on-disk index opened for one query. The default reload
/// policy would start a thread to watch the directory for the reader's
/// lifetime; a query does not live long enough to want new commits.
fn query_searcher(index: &tantivy::Index) -> Result<tantivy::Searcher> {
    index
        .reader_builder()
        .reload_policy(tantivy::ReloadPolicy::Manual)
        .try_into()
        .map(|r: tantivy::IndexReader| r.searcher())
        .map_err(|e| Error::Internal(format!("Failed to create index reader: {}", e)))
}

async fn search_wal_indices(base_path: &str, request: &SearchRequest) -> Result<Vec<Hit>> {
    use std::fs;
    use std::path::Path;
    use tantivy::Index;

    let mut all_hits = Vec::new();
    let base = Path::new(base_path);

    if !base.exists() {
        debug!("WAL index base path does not exist: {}", base_path);
        return Ok(all_hits);
    }

    // Discover all index directories (format: topic-partition)
    let entries = fs::read_dir(base)
        .map_err(|e| Error::Internal(format!("Failed to read index directory: {}", e)))?;

    for entry in entries {
        let entry = entry
            .map_err(|e| Error::Internal(format!("Failed to read directory entry: {}", e)))?;
        let path = entry.path();

        if path.is_dir() {
            let index_name = path.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown")
                .to_string();

            debug!("Attempting to open WAL-created index at: {}", path.display());

            // Try to open the Tantivy index
            match Index::open_in_dir(&path) {
                Ok(index) => {
                    chronik_storage::register_analyzer(&index);
                    debug!("Successfully opened index: {}", index_name);

                    // Search this index
                    match query_searcher(&index)
                        .and_then(|s| search_tantivy_index(&index_name, &index, &s, request))
                    {
                        Ok(mut hits) => {
                            debug!("Found {} hits in index {}", hits.len(), index_name);
                            all_hits.append(&mut hits);
                        }
                        Err(e) => {
                            error!("Error searching WAL index {}: {}", index_name, e);
                        }
                    }
                }
                Err(e) => {
                    debug!("Could not open index at {}: {}", path.display(), e);
                }
            }
        }
    }

    debug!("Total hits from WAL indices: {}", all_hits.len());
    Ok(all_hits)
}

/// Search a Tantivy index directly (for WAL-created indices)
///
/// Synchronous and CPU-bound: the cold path runs it on a blocking thread.
fn search_tantivy_index(
    index_name: &str,
    index: &tantivy::Index,
    searcher: &tantivy::Searcher,
    request: &SearchRequest,
) -> Result<Vec<Hit>> {
    // Handle size=0: return empty hits (caller just wants count)
    // Tantivy's TopDocs collector panics if limit is 0
    if request.size == 0 {
        return Ok(Vec::new());
    }

    // Build query - for WAL indices, we'll do a match_all by default
    // since we don't have the schema mapping readily available.
    //
    // v2.5.4: for an EXPLICIT QueryDsl (not "no query" → match_all), do NOT
    // silently upgrade build failures into `AllQuery`. That broke
    // `bool.must` against any disk index whose schema used different field
    // names (e.g., the realtime indexer uses `_value`/`_key` while users
    // query `value`/`key`) — the mismatch produced AllQuery which matched
    // every doc, making bool.must behave like bool.should (issue #2 regression
    // when cold sources were added in v2.5.4). Return empty from this source
    // instead; other sources (hot, in-memory REST) still contribute.
    let schema = index.schema();
    let query: Box<dyn TantivyQuery> = if let Some(query_dsl) = &request.query {
        match build_tantivy_query(query_dsl, &schema, Some(index)) {
            Ok(q) => q,
            Err(e) => {
                debug!(
                    "Skipping disk index {} for this query (schema mismatch: {})",
                    index_name, e
                );
                return Ok(Vec::new());
            }
        }
    } else {
        Box::new(AllQuery)
    };

    // Execute search
    // The source's top `from + size`, unskipped: the merge pages once
    // (crate::ranking). Skipping `from` here skipped it once per segment.
    let top_docs = crate::ranking::top_docs(searcher, &*query, request.from.saturating_add(request.size))
        .map_err(|e| Error::Internal(format!("Search failed: {}", e)))?;

    // Collect results
    let mut hits = Vec::new();
    for (score, doc_address) in top_docs {
        let doc: tantivy::TantivyDocument = searcher.doc(doc_address)
            .map_err(|e| Error::Internal(format!("Failed to retrieve document: {}", e)))?;

        // An archived segment's document holds Chronik's internal columns
        // (raw `_value` bytes, producer ids, sequences...). Dumping them gave
        // a record a different `_source` once it was archived — `_value` as a
        // list of byte numbers, no `offset` or `timestamp`. Present it the way
        // the hot index does.
        if let Some(source) = archived_record_source(index_name, &schema, &doc)
            .or_else(|| legacy_record_source(index_name, &schema, &doc))
        {
            let id = format!(
                "{}-{}",
                source.get("partition").and_then(|v| v.as_i64()).unwrap_or(0),
                source.get("offset").and_then(|v| v.as_i64()).unwrap_or(0)
            );
            let topic = source
                .get("topic")
                .and_then(|v| v.as_str())
                .unwrap_or(index_name)
                .to_string();
            hits.push(Hit {
                _index: topic,
                _id: id,
                _score: Some(score),
                _source: serde_json::Value::Object(source),
                highlight: None,
            });
            continue;
        }

        // Convert Tantivy document to JSON
        let mut source = serde_json::Map::new();
        for field in schema.fields() {
            let field_name = schema.get_field_name(field.0);
            {
                let field_values: Vec<_> = doc.get_all(field.0).collect();
                if !field_values.is_empty() {
                    let json_values: Vec<serde_json::Value> = field_values.into_iter().filter_map(|v| {
                        // Convert Tantivy values to JSON
                        v.as_str().map(|s| serde_json::Value::String(s.to_string()))
                            .or_else(|| v.as_u64().map(|n| serde_json::Value::Number(n.into())))
                            .or_else(|| v.as_i64().map(|n| serde_json::Value::Number(n.into())))
                            .or_else(|| v.as_f64().and_then(|f| serde_json::Number::from_f64(f).map(serde_json::Value::Number)))
                            .or_else(|| v.as_bytes().map(|b| serde_json::Value::String(format!("{:?}", b))))
                    }).collect();

                    if json_values.len() == 1 {
                        source.insert(field_name.to_string(), json_values.into_iter().next().unwrap());
                    } else if !json_values.is_empty() {
                        source.insert(field_name.to_string(), serde_json::Value::Array(json_values));
                    }
                }
            }
        }

        // Generate document ID. Try both naming conventions: the cold
        // TantivyIndexer uses `offset`, the realtime indexer uses `_offset`.
        // v2.5.4: also fall back to `_id` for REST-created indexes. Stable
        // IDs are required for hot↔cold dedup in `search_index` to work.
        //
        // Multi-partition fix: qualify the id by partition when present. Offset
        // alone collides across partitions (every partition has offset 0,1,2…),
        // so the (_index,_id) dedup in `merge_hot_and_cold_hits` would collapse
        // distinct docs from different partitions — dropping ~2/3 of a
        // 3-partition topic's hits.
        let offset_part = source.get("offset").and_then(|v| v.as_i64())
            .or_else(|| source.get("_offset").and_then(|v| v.as_i64()));
        let partition_part = source.get("partition").and_then(|v| v.as_i64())
            .or_else(|| source.get("_partition").and_then(|v| v.as_i64()));
        let doc_id = match (partition_part, offset_part) {
            (Some(p), Some(o)) => format!("{}-{}", p, o),
            (None, Some(o)) => o.to_string(),
            _ => source.get("_id").and_then(|v| v.as_str()).map(|s| s.to_string())
                .unwrap_or_else(|| Uuid::new_v4().to_string()),
        };

        hits.push(Hit {
            _index: index_name.to_string(),
            _id: doc_id,
            _score: Some(score),
            _source: serde_json::Value::Object(source),
            highlight: None,
        });
    }

    Ok(hits)
}

/// `_source` for a document of an archived segment, in the shape a hot hit
/// has (`hot_hit_to_es_hit`): topic, partition, offset, timestamp, key, value.
/// `None` when the schema is not an archive segment's.
fn archived_record_source(
    topic: &str,
    schema: &tantivy::schema::Schema,
    doc: &tantivy::TantivyDocument,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    let offset_field = schema.get_field("_offset").ok()?;
    let ts_field = schema.get_field("_ts").ok()?;
    let raw_value_field = schema.get_field("_value").ok()?;
    if !matches!(schema.get_field_entry(raw_value_field).field_type(), FieldType::Bytes(_)) {
        return None;
    }
    let offset = doc.get_first(offset_field).and_then(|v| v.as_i64())?;

    let mut source = serde_json::Map::new();
    source.insert("topic".to_string(), serde_json::Value::String(topic.to_string()));
    let partition = schema
        .get_field("partition")
        .ok()
        .and_then(|f| doc.get_first(f).and_then(|v| v.as_i64()))
        .unwrap_or(0);
    source.insert("partition".to_string(), partition.into());
    source.insert("offset".to_string(), offset.into());
    if let Some(ts) = doc.get_first(ts_field).and_then(|v| v.as_datetime()) {
        source.insert("timestamp".to_string(), ts.into_timestamp_millis().into());
    }
    if let Some(key) = schema
        .get_field("_key")
        .ok()
        .and_then(|f| doc.get_first(f).and_then(|v| v.as_bytes()))
    {
        source.insert(
            "key".to_string(),
            serde_json::Value::String(String::from_utf8_lossy(key).into_owned()),
        );
    }
    if let Some(value) = doc.get_first(raw_value_field).and_then(|v| v.as_bytes()) {
        source.insert(
            "value".to_string(),
            serde_json::Value::String(String::from_utf8_lossy(value).into_owned()),
        );
    }
    Some(source)
}

/// `_source` for a document of a leftover `{data_dir}/index/{topic}` index,
/// written by the RealtimeIndexer removed in 08b1c58 (`_topic`, `_partition`,
/// `_offset`, `_timestamp`, `_key`/`_value` as text), in the same shape as a
/// hot or archived hit. Those indexes are still read, and their raw documents
/// carried no `partition`/`offset`/`timestamp`, so they could neither be
/// ordered nor deduplicated against the same record from an archive — paging
/// a topic with one of them repeated and dropped records. `None` for any other
/// schema.
fn legacy_record_source(
    index_name: &str,
    schema: &tantivy::schema::Schema,
    doc: &tantivy::TantivyDocument,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    let partition_field = schema.get_field("_partition").ok()?;
    let offset_field = schema.get_field("_offset").ok()?;
    let ts_field = schema.get_field("_timestamp").ok()?;
    let offset = doc.get_first(offset_field).and_then(|v| v.as_i64())?;
    let partition = doc.get_first(partition_field).and_then(|v| v.as_i64())?;

    let text = |name: &str| {
        schema
            .get_field(name)
            .ok()
            .and_then(|f| doc.get_first(f).and_then(|v| v.as_str().map(str::to_string)))
    };
    let mut source = serde_json::Map::new();
    let topic = text("_topic").unwrap_or_else(|| index_name.to_string());
    source.insert("topic".to_string(), serde_json::Value::String(topic));
    source.insert("partition".to_string(), partition.into());
    source.insert("offset".to_string(), offset.into());
    if let Some(ts) = doc.get_first(ts_field).and_then(|v| v.as_datetime()) {
        source.insert("timestamp".to_string(), ts.into_timestamp_millis().into());
    }
    if let Some(key) = text("_key") {
        source.insert("key".to_string(), serde_json::Value::String(key));
    }
    // The body went to `_value`, or — for JSON records — only to the
    // `_json_content` object (what thunderbird's leftover indexes hold).
    let value = text("_value").or_else(|| {
        let field = schema.get_field("_json_content").ok()?;
        let owned: tantivy::schema::OwnedValue = doc.get_first(field)?.into();
        match owned {
            // Stored as the record's JSON text: already the value.
            tantivy::schema::OwnedValue::Str(s) => Some(s),
            other => serde_json::to_string(&other).ok(),
        }
    });
    if let Some(value) = value {
        source.insert("value".to_string(), serde_json::Value::String(value));
    }
    Some(source)
}

/// Helper function to search within a single index
async fn search_in_index(
    index_name: &str,
    state: &crate::api::IndexState,
    request: &SearchRequest,
) -> Result<Vec<Hit>> {
    // Handle size=0: return empty hits (caller just wants count)
    // Tantivy's TopDocs collector panics if limit is 0
    if request.size == 0 {
        return Ok(Vec::new());
    }

    let searcher = state.reader.searcher();

    // Build Tantivy query from Elasticsearch query DSL
    let query = match &request.query {
        Some(query_dsl) => build_tantivy_query(query_dsl, &state.schema, Some(&state.index))?,
        None => Box::new(AllQuery),
    };

    // Execute search
    let top_docs = searcher.search(&*query, &TopDocs::with_limit(request.from.saturating_add(request.size)))
        .map_err(|e| Error::Internal(format!("Search failed: {}", e)))?;
    
    // Collect results
    let mut hits = Vec::new();
    for (score, doc_address) in top_docs {
        let doc: tantivy::TantivyDocument = searcher.doc(doc_address)
            .map_err(|e| Error::Internal(format!("Failed to retrieve document: {}", e)))?;
        
        // Convert Tantivy document to JSON
        let mut source = serde_json::Map::new();
        for field in state.schema.fields() {
            let field_name = state.schema.get_field_name(field.0);
            {
                let field_values: Vec<_> = doc.get_all(field.0).collect();
                if !field_values.is_empty() {
                let json_values: Vec<serde_json::Value> = field_values.into_iter().filter_map(|v| {
                    // Convert Tantivy values to JSON
                    v.as_str().map(|s| serde_json::Value::String(s.to_string()))
                        .or_else(|| v.as_u64().map(|n| serde_json::Value::Number(n.into())))
                        .or_else(|| v.as_i64().map(|n| serde_json::Value::Number(n.into())))
                        .or_else(|| v.as_f64().and_then(|f| serde_json::Number::from_f64(f).map(serde_json::Value::Number)))
                }).collect();
                
                if json_values.len() == 1 {
                    source.insert(field_name.to_string(), json_values.into_iter().next().unwrap());
                } else if !json_values.is_empty() {
                    source.insert(field_name.to_string(), serde_json::Value::Array(json_values));
                }
                }
            }
        }
        
        // Generate document ID if not present
        let doc_id = source.get("_id")
            .and_then(|v| v.as_str())
            .unwrap_or(&Uuid::new_v4().to_string())
            .to_string();
        
        // Generate highlights if requested
        let highlight = if request.highlight.is_some() {
            generate_highlights(&doc, &query, &state.schema, request.highlight.as_ref().unwrap())?
        } else {
            None
        };
        
        hits.push(Hit {
            _index: index_name.to_string(),
            _id: doc_id,
            _score: Some(score),
            _source: serde_json::Value::Object(source),
            highlight,
        });
    }
    
    Ok(hits)
}

/// v2.5.5: Look up a field by name, tolerating the two Chronik schema
/// conventions — plain (`value`, `key`, `offset`, …) used by the hot text
/// index and the WAL-archived TantivyIndexer, and underscore-prefixed
/// (`_value`, `_key`, `_offset`, …) used by the realtime indexer. A user
/// writing `{"match": {"value": "foo"}}` should get hits from every
/// source they have, not just the one whose schema happens to match
/// their spelling.
///
/// Resolution order:
///   1. exact name the user wrote
///   2. same name with a leading `_` added (when user wrote `value`, try `_value`)
///   3. same name with a leading `_` stripped (when user wrote `_value`, try `value`)
///
/// Returns `Err` only when none of the three variants exist in the schema.
fn resolve_field(
    schema: &tantivy::schema::Schema,
    field_name: &str,
) -> std::result::Result<tantivy::schema::Field, ()> {
    if let Ok(f) = schema.get_field(field_name) {
        return Ok(f);
    }
    if !field_name.starts_with('_') {
        let prefixed = format!("_{}", field_name);
        if let Ok(f) = schema.get_field(&prefixed) {
            return Ok(f);
        }
    } else if let Some(stripped) = field_name.strip_prefix('_') {
        if let Ok(f) = schema.get_field(stripped) {
            return Ok(f);
        }
    }
    Err(())
}

/// What a field name in a query refers to in one particular index.
#[derive(Debug, Clone)]
enum FieldTarget {
    /// A field of the schema itself (`value`, `key`, `offset`, ...).
    Schema(tantivy::schema::Field),
    /// A field inside the record's JSON value, e.g. `marker` or `user.name`,
    /// reached through the schema's JSON field.
    JsonPath(tantivy::schema::Field, String),
    /// An index written before JSON values were indexed by field (archives
    /// from v2.17.0 and earlier). The field cannot be told apart there, so the
    /// query runs as full text over the whole value — the same documents the
    /// hot index used to return for any field name.
    LegacyFullText(tantivy::schema::Field),
}

/// Resolve a query's field name against one index's schema.
///
/// Order: an exact (or underscore-aliased) schema field wins, so `value`,
/// `key`, `offset`, `partition` keep their meaning; any other name is a path
/// into the JSON value when the schema indexes one; failing that, legacy
/// indexes fall back to full text on the value.
fn resolve_target(
    schema: &tantivy::schema::Schema,
    field_name: &str,
) -> Result<FieldTarget> {
    if let Ok(field) = resolve_field(schema, field_name) {
        return Ok(FieldTarget::Schema(field));
    }
    if let Ok(json) = schema.get_field(chronik_storage::json_index::JSON_FIELD) {
        return Ok(FieldTarget::JsonPath(json, field_name.to_string()));
    }
    if let Ok(value) = resolve_field(schema, "value") {
        if matches!(schema.get_field_entry(value).field_type(), FieldType::Str(_)) {
            return Ok(FieldTarget::LegacyFullText(value));
        }
    }
    Err(Error::InvalidInput(format!("Field {} not found", field_name)))
}

/// Analyze `text` with the tokenizer `field` is indexed with.
fn analyzed_tokens(
    index: &tantivy::Index,
    field: tantivy::schema::Field,
    text: &str,
) -> Result<Vec<String>> {
    let mut analyzer = index
        .tokenizer_for_field(field)
        .map_err(|e| Error::Internal(format!("tokenizer for field: {}", e)))?;
    let mut stream = analyzer.token_stream(text);
    let mut tokens = Vec::new();
    while let Some(token) = stream.next() {
        tokens.push(token.text.clone());
    }
    Ok(tokens)
}

/// One query term per analyzed token of `text` at `path` in a JSON field.
fn json_text_terms(
    index: &tantivy::Index,
    json_field: tantivy::schema::Field,
    path: &str,
    text: &str,
) -> Result<Vec<Term>> {
    Ok(analyzed_tokens(index, json_field, text)?
        .into_iter()
        .map(|token| {
            let mut term = Term::from_field_json_path(json_field, path, true);
            term.append_type_and_str(&token);
            term
        })
        .collect())
}

/// A typed numeric term at `path` in a JSON field. Tantivy indexes JSON
/// integers as i64 (u64 only above i64::MAX) and other numbers as f64.
fn json_number_term(
    json_field: tantivy::schema::Field,
    path: &str,
    number: &serde_json::Number,
) -> Result<Term> {
    let mut term = Term::from_field_json_path(json_field, path, true);
    if let Some(i) = number.as_i64() {
        term.append_type_and_fast_value(i);
    } else if let Some(u) = number.as_u64() {
        term.append_type_and_fast_value(u);
    } else if let Some(f) = number.as_f64() {
        term.append_type_and_fast_value(f);
    } else {
        return Err(Error::InvalidInput(format!("Invalid number {}", number)));
    }
    Ok(term)
}

/// Combine terms into one query: a single term stands alone, several are
/// joined with `occur` (Should for match's OR semantics, Must for all-of).
fn terms_query(terms: Vec<Term>, occur: Occur) -> Box<dyn TantivyQuery> {
    let mut queries: Vec<(Occur, Box<dyn TantivyQuery>)> = terms
        .into_iter()
        .map(|t| {
            (
                occur,
                Box::new(TermQuery::new(t, tantivy::schema::IndexRecordOption::WithFreqs))
                    as Box<dyn TantivyQuery>,
            )
        })
        .collect();
    if queries.len() == 1 {
        queries.pop().map(|(_, q)| q).expect("one query")
    } else {
        Box::new(BooleanQuery::new(queries))
    }
}

/// Lower and upper bounds of a range clause, built by `make` from each bound
/// value. `gt`/`lt` are exclusive, `gte`/`lte` inclusive; a missing side is
/// unbounded.
fn range_bounds(
    clause: &crate::api::RangeClause,
    mut make: impl FnMut(&serde_json::Value) -> Result<Term>,
) -> Result<(std::ops::Bound<Term>, std::ops::Bound<Term>)> {
    use std::ops::Bound;
    let lower = match (&clause.gte, &clause.gt) {
        (Some(v), _) => Bound::Included(make(v)?),
        (None, Some(v)) => Bound::Excluded(make(v)?),
        (None, None) => Bound::Unbounded,
    };
    let upper = match (&clause.lte, &clause.lt) {
        (Some(v), _) => Bound::Included(make(v)?),
        (None, Some(v)) => Bound::Excluded(make(v)?),
        (None, None) => Bound::Unbounded,
    };
    if matches!((&lower, &upper), (Bound::Unbounded, Bound::Unbounded)) {
        return Err(Error::InvalidInput("Range query must specify a bound".to_string()));
    }
    Ok((lower, upper))
}

/// Run `f` with the index to analyze against: the real one when the caller
/// has it, else a throwaway in-RAM index of the same schema with the Chronik
/// analyzer registered (tokenizers are not persisted with a schema).
fn with_query_index<T>(
    schema: &tantivy::schema::Schema,
    index: Option<&tantivy::Index>,
    f: impl FnOnce(&tantivy::Index) -> Result<T>,
) -> Result<T> {
    match index {
        Some(idx) => f(idx),
        None => {
            let tmp = tantivy::Index::create_in_ram(schema.clone());
            chronik_storage::register_analyzer(&tmp);
            f(&tmp)
        }
    }
}

/// A typed boolean term at `path` in a JSON field.
fn json_bool_term(json_field: tantivy::schema::Field, path: &str, b: bool) -> Term {
    let mut term = Term::from_field_json_path(json_field, path, true);
    term.append_type_and_fast_value(b);
    term
}

/// `match` on a field of the JSON value: any analyzed token of the query at
/// that path (Elasticsearch's default OR operator). A number or boolean
/// matches the typed value, and a number also matches the same digits held
/// as a string.
fn json_match_query(
    schema: &tantivy::schema::Schema,
    index: Option<&tantivy::Index>,
    json: tantivy::schema::Field,
    path: &str,
    value: &serde_json::Value,
) -> Result<Box<dyn TantivyQuery>> {
    with_query_index(schema, index, |idx| {
        let mut terms = Vec::new();
        let text = match value {
            serde_json::Value::Number(n) => {
                terms.push(json_number_term(json, path, n)?);
                n.to_string()
            }
            serde_json::Value::Bool(b) => {
                terms.push(json_bool_term(json, path, *b));
                b.to_string()
            }
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let text_terms = json_text_terms(idx, json, path, &text)?;
        if text_terms.is_empty() && terms.is_empty() {
            // Stop-word-only query: same fallback as a match on a schema field.
            debug!("Query '{}' on {} has only stop words, matching all", text, path);
            return Ok(Box::new(AllQuery) as Box<dyn TantivyQuery>);
        }
        terms.extend(text_terms);
        Ok(terms_query(terms, Occur::Should))
    })
}

/// `term` on a field of the JSON value. JSON strings are indexed as analyzed
/// text, so an exact string term is all of its analyzed tokens at that path;
/// numbers and booleans match the typed value exactly.
fn json_term_query(
    schema: &tantivy::schema::Schema,
    index: Option<&tantivy::Index>,
    json: tantivy::schema::Field,
    path: &str,
    value: &serde_json::Value,
) -> Result<Box<dyn TantivyQuery>> {
    match value {
        serde_json::Value::Number(n) => Ok(Box::new(TermQuery::new(
            json_number_term(json, path, n)?,
            tantivy::schema::IndexRecordOption::Basic,
        ))),
        serde_json::Value::Bool(b) => Ok(Box::new(TermQuery::new(
            json_bool_term(json, path, *b),
            tantivy::schema::IndexRecordOption::Basic,
        ))),
        serde_json::Value::String(s) => with_query_index(schema, index, |idx| {
            let terms = json_text_terms(idx, json, path, s)?;
            if terms.is_empty() {
                return Ok(Box::new(tantivy::query::EmptyQuery) as Box<dyn TantivyQuery>);
            }
            Ok(terms_query(terms, Occur::Must))
        }),
        _ => Err(Error::InvalidInput("Term query value must be string, number or boolean".to_string())),
    }
}

/// `term` on a field name an index from before field-level JSON indexing
/// cannot resolve: every analyzed token of the value somewhere in the
/// record's text.
fn legacy_term_query(
    schema: &tantivy::schema::Schema,
    index: Option<&tantivy::Index>,
    value_field: tantivy::schema::Field,
    value: &serde_json::Value,
) -> Result<Box<dyn TantivyQuery>> {
    let text = match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(_) | serde_json::Value::Bool(_) => value.to_string(),
        _ => return Err(Error::InvalidInput("Term query value must be string, number or boolean".to_string())),
    };
    with_query_index(schema, index, |idx| {
        let terms: Vec<Term> = analyzed_tokens(idx, value_field, &text)?
            .into_iter()
            .map(|t| Term::from_field_text(value_field, &t))
            .collect();
        if terms.is_empty() {
            return Ok(Box::new(tantivy::query::EmptyQuery) as Box<dyn TantivyQuery>);
        }
        Ok(terms_query(terms, Occur::Must))
    })
}

/// Build Tantivy query from Elasticsearch query DSL.
///
/// Exposed crate-wide so the hot text index can build structured queries
/// against its own per-partition schemas (fixes GitHub issue #2 where
/// bool.must was flattened into an OR in the hot path).
pub(crate) fn build_tantivy_query(
    query_dsl: &QueryDsl,
    schema: &tantivy::schema::Schema,
    index: Option<&tantivy::Index>,
) -> Result<Box<dyn TantivyQuery>> {
    match query_dsl {
        QueryDsl::MatchAll(_) => Ok(Box::new(AllQuery)),

        QueryDsl::Match(match_query) => {
            // Get the field and value from the match query
            let (field_name, value) = match_query.field_value.iter().next()
                .ok_or_else(|| Error::InvalidInput("Match query must specify a field".to_string()))?;
            // Long form `{"field": {"query": "text", ...}}` means the same as
            // `{"field": "text"}`; without this the object's JSON text was
            // searched for literally.
            let value = match value {
                serde_json::Value::Object(o) => o.get("query").unwrap_or(value),
                _ => value,
            };

            // Handle _all meta-field: search across all TEXT and JSON fields
            let fields = if field_name == "_all" {
                schema.fields()
                    .filter_map(|(field, entry)| {
                        match entry.field_type() {
                            FieldType::Str(_) | FieldType::JsonObject(_) => Some(field),
                            _ => None,
                        }
                    })
                    .collect::<Vec<_>>()
            } else {
                // v2.5.5: resolve `value` against `_value` too (and vice versa)
                // so the same query works against hot, WAL-archived, and realtime
                // schemas without the caller knowing which is live. Other names
                // are fields of the record's JSON value.
                match resolve_target(schema, field_name)? {
                    FieldTarget::Schema(field) | FieldTarget::LegacyFullText(field) => vec![field],
                    FieldTarget::JsonPath(json, path) => {
                        return json_match_query(schema, index, json, &path, value);
                    }
                }
            };

            if fields.is_empty() {
                return Ok(Box::new(AllQuery));
            }

            // Use the real index for QueryParser when available (proper tokenizers),
            // fall back to temporary in-memory index
            let tmp_index;
            let idx = match index {
                Some(i) => i,
                None => {
                    tmp_index = tantivy::Index::create_in_ram(schema.clone());
                    chronik_storage::register_analyzer(&tmp_index);
                    &tmp_index
                }
            };
            // Extract raw query text (avoid JSON quoting from value.to_string())
            let query_text = match value.as_str() {
                Some(s) => s.to_string(),
                None => value.to_string(),
            };

            // For multi-word queries, split into individual terms and combine with OR.
            // Tantivy's QueryParser doesn't handle multi-word queries well for JSON fields
            // — terms in different JSON paths (e.g., color="turquoise", class="pillows")
            // won't match when parsed as a single query string.
            let words: Vec<&str> = query_text.split_whitespace().collect();
            // If the analyzer removes all tokens (stop-word-only query like "the"),
            // fall back to match-all so the user still gets results.
            if !chronik_storage::text_analysis::has_tokens(&query_text) {
                debug!("Query '{}' contains only stop words, falling back to match-all", query_text);
                return Ok(Box::new(AllQuery));
            }

            let query_parser = QueryParser::for_index(idx, fields.clone());
            let query: Box<dyn TantivyQuery> = if words.len() <= 1 {
                query_parser.parse_query(&query_text)
                    .map_err(|e| Error::InvalidInput(format!("Invalid query: {}", e)))?
            } else {
                // Create per-word sub-queries and OR them together,
                // skipping words that are stop words
                let mut sub_queries: Vec<(Occur, Box<dyn TantivyQuery>)> = Vec::new();
                for word in &words {
                    if chronik_storage::text_analysis::has_tokens(word) {
                        if let Ok(sub_q) = query_parser.parse_query(word) {
                            sub_queries.push((Occur::Should, sub_q));
                        }
                    }
                }
                if sub_queries.is_empty() {
                    Box::new(AllQuery)
                } else {
                    Box::new(BooleanQuery::new(sub_queries))
                }
            };

            Ok(query)
        },
        
        QueryDsl::Term(term_query) => {
            let (field_name, value) = term_query.field_value.iter().next()
                .ok_or_else(|| Error::InvalidInput("Term query must specify a field".to_string()))?;
            
            let field = match resolve_target(schema, field_name)? {
                FieldTarget::Schema(field) => field,
                // A JSON string field is analyzed text, so an exact term is
                // every analyzed token of the value present at that path.
                FieldTarget::JsonPath(json, path) => {
                    return json_term_query(schema, index, json, &path, value);
                }
                FieldTarget::LegacyFullText(field) => {
                    return legacy_term_query(schema, index, field, value);
                }
            };

            let term = match value {
                serde_json::Value::String(s) => Term::from_field_text(field, s),
                serde_json::Value::Number(n) => {
                    if let Some(i) = n.as_i64() {
                        Term::from_field_i64(field, i)
                    } else if let Some(u) = n.as_u64() {
                        Term::from_field_u64(field, u)
                    } else {
                        return Err(Error::InvalidInput("Invalid numeric value".to_string()));
                    }
                },
                _ => return Err(Error::InvalidInput("Term query value must be string or number".to_string())),
            };
            
            Ok(Box::new(TermQuery::new(term, Default::default())))
        },
        
        QueryDsl::Range(range_query) => {
            let (field_name, range_clause) = range_query.field_range.iter().next()
                .ok_or_else(|| Error::InvalidInput("Range query must specify a field".to_string()))?;

            let field = match resolve_target(schema, field_name)? {
                FieldTarget::Schema(field) => field,
                // Tantivy 0.24 runs a range over a JSON path only when the JSON
                // field is a fast (columnar) field, which would add a column
                // per JSON path per document to every hot and archived index.
                // Refused here rather than failing inside the search.
                FieldTarget::JsonPath(_, path) => {
                    return Err(Error::InvalidInput(format!(
                        "Range queries on JSON fields ({}) are not supported",
                        path
                    )));
                }
                FieldTarget::LegacyFullText(_) => {
                    return Err(Error::InvalidInput(format!(
                        "Field {} is not indexed by field in this index",
                        field_name
                    )));
                }
            };

            let field_type = schema.get_field_entry(field).field_type();
            match field_type {
                FieldType::I64(_) => {
                    let (lower, upper) = range_bounds(range_clause, |v| {
                        v.as_i64()
                            .map(|i| Term::from_field_i64(field, i))
                            .ok_or_else(|| Error::InvalidInput("Range bound must be an integer".to_string()))
                    })?;
                    Ok(Box::new(RangeQuery::new(lower, upper)))
                }
                FieldType::U64(_) => {
                    let (lower, upper) = range_bounds(range_clause, |v| {
                        v.as_u64()
                            .map(|u| Term::from_field_u64(field, u))
                            .ok_or_else(|| Error::InvalidInput("Range bound must be a non-negative integer".to_string()))
                    })?;
                    Ok(Box::new(RangeQuery::new(lower, upper)))
                }
                _ => Err(Error::InvalidInput("Range queries only supported for numeric fields".to_string())),
            }
        },
        
        QueryDsl::Bool(bool_query) => {
            let mut clauses = Vec::new();

            // Add must clauses
            for clause in &bool_query.must {
                let sub_query = build_tantivy_query(clause, schema, index)?;
                clauses.push((Occur::Must, sub_query));
            }

            // Add should clauses
            for clause in &bool_query.should {
                let sub_query = build_tantivy_query(clause, schema, index)?;
                clauses.push((Occur::Should, sub_query));
            }

            // Add must_not clauses
            for clause in &bool_query.must_not {
                let sub_query = build_tantivy_query(clause, schema, index)?;
                clauses.push((Occur::MustNot, sub_query));
            }

            // Add filter clauses (similar to must but don't affect scoring)
            for clause in &bool_query.filter {
                let sub_query = build_tantivy_query(clause, schema, index)?;
                clauses.push((Occur::Must, sub_query));
            }

            Ok(Box::new(BooleanQuery::new(clauses)))
        },
        
        QueryDsl::GeoDistance(geo_query) => {
            use crate::geo::{GeoQuery, GeoPoint, DistanceType, parse_geo_point, parse_distance_string};
            
            let center = parse_geo_point(&geo_query.center)?;
            let distance = parse_distance_string(&geo_query.distance)?;
            let distance_type = match geo_query.distance_type.as_deref() {
                Some("plane") => DistanceType::Plane,
                _ => DistanceType::Arc,
            };
            
            let query = GeoQuery::Distance {
                field: geo_query.field.clone(),
                center,
                distance,
                distance_type,
            };
            
            query.to_tantivy_query(schema)
        },
        
        QueryDsl::GeoBoundingBox(geo_query) => {
            use crate::geo::{GeoQuery, parse_geo_point};
            
            let top_left = parse_geo_point(&geo_query.top_left)?;
            let bottom_right = parse_geo_point(&geo_query.bottom_right)?;
            
            let query = GeoQuery::BoundingBox {
                field: geo_query.field.clone(),
                top_left,
                bottom_right,
            };
            
            query.to_tantivy_query(schema)
        },
        
        QueryDsl::GeoPolygon(geo_query) => {
            use crate::geo::{GeoQuery, parse_geo_point};
            
            let points = geo_query.points.iter()
                .map(|p| parse_geo_point(p))
                .collect::<Result<Vec<_>>>()?;
            
            let query = GeoQuery::Polygon {
                field: geo_query.field.clone(),
                points,
            };
            
            query.to_tantivy_query(schema)
        },
    }
}

/// Index or update a document
pub async fn index_document(
    Path((index, id)): Path<(String, String)>,
    State(api): State<Arc<SearchApi>>,
    Json(request): Json<IndexDocumentRequest>,
) -> impl IntoResponse {
    let state = match api.indices.get(&index) {
        Some(state) => state,
        None => return Err(index_not_found_error(&index)),
    };
    
    // Add _id field to document
    let mut document = request.document;
    if let serde_json::Value::Object(ref mut map) = document {
        map.insert("_id".to_string(), serde_json::Value::String(id.clone()));
    }
    
    // Convert JSON to Tantivy document
    let tantivy_doc = json_to_tantivy_doc(&document, &state.schema, &state.mapping)
        .map_err(|e| indexing_error(e))?;
    
    // Add document to index
    let mut writer = state.writer.write().await;
    writer.add_document(tantivy_doc)
        .map_err(|e| Error::Internal(format!("Failed to index document: {}", e)))
        .map_err(|e| indexing_error(e))?;
    
    // Commit immediately for consistency
    writer.commit()
        .map_err(|e| Error::Internal(format!("Failed to commit: {}", e)))
        .map_err(|e| indexing_error(e))?;

    // The reader uses ReloadPolicy::OnCommitWithDelay, so a commit alone does not
    // make the write visible to the next searcher() until the delayed reload fires.
    // Force a reload here so an index is immediately followed by a consistent
    // get/search (Elasticsearch realtime-GET semantics), honoring the commit above.
    state.reader.reload()
        .map_err(|e| Error::Internal(format!("Failed to reload reader: {}", e)))
        .map_err(|e| indexing_error(e))?;

    let response = IndexDocumentResponse {
        _index: index,
        _id: id,
        _version: 1,
        result: "created".to_string(),
        _shards: ShardInfo {
            total: 1,
            successful: 1,
            skipped: 0,
            failed: 0,
        },
        _seq_no: 0,
        _primary_term: 1,
    };
    
    Ok((StatusCode::CREATED, Json(response)))
}

/// Get a document by ID
pub async fn get_document(
    Path((index, id)): Path<(String, String)>,
    State(api): State<Arc<SearchApi>>,
) -> impl IntoResponse {
    let state = match api.indices.get(&index) {
        Some(state) => state,
        None => return Err(index_not_found_error(&index)),
    };
    
    // Search for document with matching _id
    let searcher = state.reader.searcher();
    let id_field = state.schema.get_field("_id")
        .map_err(|_| Error::Internal("_id field not found".to_string()))
        .map_err(|e| search_error(e))?;
    
    let term = Term::from_field_text(id_field, &id);
    let query = TermQuery::new(term, Default::default());
    
    let top_docs = searcher.search(&query, &TopDocs::with_limit(1))
        .map_err(|e| Error::Internal(format!("Search failed: {}", e)))
        .map_err(|e| search_error(e))?;
    
    if let Some((_score, doc_address)) = top_docs.first() {
        let doc: tantivy::TantivyDocument = searcher.doc(*doc_address)
            .map_err(|e| Error::Internal(format!("Failed to retrieve document: {}", e)))
            .map_err(|e| search_error(e))?;
        
        // Convert to JSON
        let mut source = serde_json::Map::new();
        for field in state.schema.fields() {
            let field_name = state.schema.get_field_name(field.0);
            
            if field_name == "_id" {
                continue; // Skip internal _id field
            }
            
            {
                let field_values: Vec<_> = doc.get_all(field.0).collect();
                if !field_values.is_empty() {
                let json_values: Vec<serde_json::Value> = field_values.into_iter().filter_map(|v| {
                    // Convert Tantivy values to JSON
                    v.as_str().map(|s| serde_json::Value::String(s.to_string()))
                        .or_else(|| v.as_u64().map(|n| serde_json::Value::Number(n.into())))
                        .or_else(|| v.as_i64().map(|n| serde_json::Value::Number(n.into())))
                        .or_else(|| v.as_f64().and_then(|f| serde_json::Number::from_f64(f).map(serde_json::Value::Number)))
                }).collect();
                
                if json_values.len() == 1 {
                    source.insert(field_name.to_string(), json_values.into_iter().next().unwrap());
                } else if !json_values.is_empty() {
                    source.insert(field_name.to_string(), serde_json::Value::Array(json_values));
                }
                }
            }
        }
        
        let response = GetDocumentResponse {
            _index: index,
            _id: id,
            _version: 1,
            _seq_no: 0,
            _primary_term: 1,
            found: true,
            _source: Some(serde_json::Value::Object(source)),
        };
        
        Ok(Json(response))
    } else {
        let response = GetDocumentResponse {
            _index: index,
            _id: id,
            _version: 1,
            _seq_no: 0,
            _primary_term: 1,
            found: false,
            _source: None,
        };
        
        Ok(Json(response))
    }
}

/// Delete a document
pub async fn delete_document(
    Path((index, id)): Path<(String, String)>,
    State(api): State<Arc<SearchApi>>,
) -> impl IntoResponse {
    let state = match api.indices.get(&index) {
        Some(state) => state,
        None => return Err(index_not_found_error(&index)),
    };
    
    // Delete document by _id
    let id_field = state.schema.get_field("_id")
        .map_err(|_| Error::Internal("_id field not found".to_string()))
        .map_err(|e| deletion_error(e))?;
    
    let term = Term::from_field_text(id_field, &id);
    
    let mut writer = state.writer.write().await;
    writer.delete_term(term);
    writer.commit()
        .map_err(|e| Error::Internal(format!("Failed to commit: {}", e)))
        .map_err(|e| deletion_error(e))?;

    // Force a reader reload so the deletion is immediately visible to the next
    // get/search (see the note in index_document — OnCommitWithDelay otherwise lags).
    state.reader.reload()
        .map_err(|e| Error::Internal(format!("Failed to reload reader: {}", e)))
        .map_err(|e| deletion_error(e))?;

    let response = DeleteDocumentResponse {
        _index: index,
        _id: id,
        _version: 1,
        result: "deleted".to_string(),
        _shards: ShardInfo {
            total: 1,
            successful: 1,
            skipped: 0,
            failed: 0,
        },
        _seq_no: 0,
        _primary_term: 1,
    };
    
    Ok(Json(response))
}

/// Create an index
pub async fn create_index(
    Path(index): Path<String>,
    State(api): State<Arc<SearchApi>>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    // Extract mappings from request body
    let mapping = if let Some(mappings) = body.get("mappings") {
        match serde_json::from_value::<IndexMapping>(mappings.clone()) {
            Ok(m) => m,
            Err(e) => return Err(mapping_error(Error::InvalidInput(format!("Invalid mapping: {}", e)))),
        }
    } else {
        // Default mapping with _id field
        let mut properties = HashMap::new();
        properties.insert("_id".to_string(), FieldMapping {
            field_type: "keyword".to_string(),
            analyzer: None,
            store: Some(true),
            index: Some(true),
        });
        IndexMapping { properties }
    };
    
    match api.create_index_with_mapping(index.clone(), mapping).await {
        Ok(_) => {},
        Err(e) => return Err(index_creation_error(e)),
    }
    
    Ok((
        StatusCode::OK,
        Json(serde_json::json!({
            "acknowledged": true,
            "shards_acknowledged": true,
            "index": index
        }))
    ))
}

/// Delete an index
pub async fn delete_index(
    Path(index): Path<String>,
    State(api): State<Arc<SearchApi>>,
) -> impl IntoResponse {
    if api.indices.remove(&index).is_none() {
        return Err(index_not_found_error(&index));
    }
    
    Ok(Json(serde_json::json!({
        "acknowledged": true
    })))
}

/// Get index mapping
pub async fn get_mapping(
    Path(index): Path<String>,
    State(api): State<Arc<SearchApi>>,
) -> impl IntoResponse {
    let state = match api.indices.get(&index) {
        Some(state) => state,
        None => return Err(index_not_found_error(&index)),
    };
    
    Ok(Json(serde_json::json!({
        index: {
            "mappings": state.mapping.clone()
        }
    })))
}

/// List all indices
pub async fn cat_indices(
    State(api): State<Arc<SearchApi>>,
) -> impl IntoResponse {
    let mut indices = Vec::new();
    
    for entry in api.indices.iter() {
        let index_name = entry.key();
        let state = entry.value();
        
        // Get document count
        let searcher = state.reader.searcher();
        let doc_count = searcher.num_docs();
        
        indices.push(CatIndexInfo {
            health: "green".to_string(),
            status: "open".to_string(),
            index: index_name.clone(),
            uuid: Uuid::new_v4().to_string(),
            pri: 1,
            rep: 0,
            docs_count: doc_count,
            docs_deleted: 0,
            store_size: format!("{}kb", doc_count / 10), // Rough estimate
            pri_store_size: format!("{}kb", doc_count / 10),
        });
    }
    
    Json(indices)
}

// Helper functions

fn json_to_tantivy_doc(
    json: &serde_json::Value,
    schema: &tantivy::schema::Schema,
    mapping: &IndexMapping,
) -> Result<tantivy::TantivyDocument> {
    let mut doc = tantivy::TantivyDocument::new();
    
    if let serde_json::Value::Object(map) = json {
        for (field_name, value) in map {
            if let Some(field_mapping) = mapping.properties.get(field_name) {
                if let Ok(field) = schema.get_field(field_name) {
                    match field_mapping.field_type.as_str() {
                        "text" | "keyword" => {
                            if let serde_json::Value::String(s) = value {
                                doc.add_text(field, s);
                            }
                        },
                        "long" | "integer" => {
                            if let Some(n) = value.as_i64() {
                                doc.add_i64(field, n);
                            }
                        },
                        "geo_point" => {
                            use crate::geo::parse_geo_point;
                            
                            if let Ok(point) = parse_geo_point(value) {
                                // Add lat and lon as separate fields
                                if let Ok(lat_field) = schema.get_field(&format!("{}_lat", field_name)) {
                                    doc.add_f64(lat_field, point.lat);
                                }
                                if let Ok(lon_field) = schema.get_field(&format!("{}_lon", field_name)) {
                                    doc.add_f64(lon_field, point.lon);
                                }
                            }
                        },
                        _ => {},
                    }
                }
            }
        }
    }
    
    Ok(doc)
}


// Error response helpers

fn error_response(error_type: &str, reason: &str, status: StatusCode) -> (StatusCode, Json<ErrorResponse>) {
    (
        status,
        Json(ErrorResponse {
            error: ErrorInfo {
                error_type: error_type.to_string(),
                reason: reason.to_string(),
                caused_by: None,
            },
            status: status.as_u16(),
        })
    )
}

fn index_not_found_error(index: &str) -> (StatusCode, Json<ErrorResponse>) {
    error_response(
        "index_not_found_exception",
        &format!("no such index [{}]", index),
        StatusCode::NOT_FOUND
    )
}

fn result_window_error(reason: &str) -> (StatusCode, Json<ErrorResponse>) {
    error_response("illegal_argument_exception", reason, StatusCode::BAD_REQUEST)
}

fn search_error(error: Error) -> (StatusCode, Json<ErrorResponse>) {
    error_response(
        "search_phase_execution_exception",
        &error.to_string(),
        StatusCode::BAD_REQUEST
    )
}

fn indexing_error(error: Error) -> (StatusCode, Json<ErrorResponse>) {
    error_response(
        "mapper_parsing_exception",
        &error.to_string(),
        StatusCode::BAD_REQUEST
    )
}

fn deletion_error(error: Error) -> (StatusCode, Json<ErrorResponse>) {
    error_response(
        "version_conflict_engine_exception",
        &error.to_string(),
        StatusCode::CONFLICT
    )
}

fn mapping_error(error: Error) -> (StatusCode, Json<ErrorResponse>) {
    error_response(
        "mapper_parsing_exception",
        &error.to_string(),
        StatusCode::BAD_REQUEST
    )
}

fn index_creation_error(error: Error) -> (StatusCode, Json<ErrorResponse>) {
    error_response(
        "resource_already_exists_exception",
        &error.to_string(),
        StatusCode::BAD_REQUEST
    )
}

/// Generate highlights for matching fields
fn generate_highlights(
    _doc: &tantivy::TantivyDocument,
    _query: &Box<dyn TantivyQuery>,
    _schema: &tantivy::schema::Schema,
    _config: &HighlightConfig,
) -> Result<Option<HashMap<String, Vec<String>>>> {
    // TODO: Implement highlighting with updated Tantivy API
    // The snippet API has changed in recent versions of Tantivy
    // For now, return None to allow compilation
    Ok(None)
}

#[cfg(test)]
mod hot_merge_tests {
    use super::*;
    use crate::api::MatchQuery;

    fn h(idx: &str, id: &str, score: f32) -> Hit {
        Hit {
            _index: idx.to_string(),
            _id: id.to_string(),
            _score: Some(score),
            _source: serde_json::json!({}),
            highlight: None,
        }
    }

    #[test]
    fn extract_match_all_takes_the_structured_path() {
        // The simple path sanitized "*" to nothing: match_all found 0 hot docs (#65).
        let q = QueryDsl::MatchAll(crate::api::MatchAllQuery {});
        assert_eq!(extract_simple_query_text(&q), None);
    }

    #[test]
    fn extract_match_flat_string() {
        let mut fv = HashMap::new();
        fv.insert(
            "value".to_string(),
            serde_json::Value::String("leather sofa".into()),
        );
        let q = QueryDsl::Match(MatchQuery { field_value: fv });
        assert_eq!(
            extract_simple_query_text(&q),
            Some("leather sofa".to_string())
        );
    }

    #[test]
    fn extract_match_object_form_with_query_key() {
        let mut fv = HashMap::new();
        fv.insert(
            "value".to_string(),
            serde_json::json!({ "query": "running shoes" }),
        );
        let q = QueryDsl::Match(MatchQuery { field_value: fv });
        assert_eq!(
            extract_simple_query_text(&q),
            Some("running shoes".to_string())
        );
    }

    /// v2.5.5 regression: `resolve_field` must tolerate the two Chronik
    /// schema conventions (`value` vs `_value`, etc.) so a single user
    /// query works across hot / WAL-archive / realtime disk indexes
    /// without the caller knowing which naming scheme is in play.
    #[test]
    fn resolve_field_maps_across_underscore_convention() {
        use tantivy::schema::{Schema, TEXT, STRING, STORED};

        // Realtime-style schema (underscore-prefixed).
        let rt = {
            let mut b = Schema::builder();
            b.add_text_field("_value", TEXT | STORED);
            b.add_text_field("_key", STRING | STORED);
            b.build()
        };
        assert!(resolve_field(&rt, "value").is_ok(), "value → _value");
        assert!(resolve_field(&rt, "key").is_ok(), "key → _key");
        assert!(resolve_field(&rt, "_value").is_ok(), "_value exact");
        assert!(resolve_field(&rt, "bogus").is_err(), "unknown field is Err");

        // Plain-style schema (hot / WAL TantivyIndexer).
        let hot = {
            let mut b = Schema::builder();
            b.add_text_field("value", TEXT | STORED);
            b.add_text_field("key", TEXT | STORED);
            b.build()
        };
        assert!(resolve_field(&hot, "_value").is_ok(), "_value → value");
        assert!(resolve_field(&hot, "_key").is_ok(), "_key → key");
        assert!(resolve_field(&hot, "value").is_ok(), "value exact");
    }

    /// Issue #2 regression: extract_simple_query_text must NOT flatten bool.
    /// Bool queries go through the structured hot-path instead (search_hot_topic
    /// → search_topic_structured → build_tantivy_query per partition), which
    /// preserves AND/OR semantics. Flattening to a space-joined string caused
    /// Tantivy's multi-word handling to OR the clauses, silently dropping
    /// `must` constraints.
    #[test]
    fn extract_bool_returns_none_so_structured_path_fires() {
        let mut fv1 = HashMap::new();
        fv1.insert("value".to_string(), serde_json::Value::String("alpha".into()));
        let mut fv2 = HashMap::new();
        fv2.insert("key".to_string(), serde_json::Value::String("beta".into()));
        let q = QueryDsl::Bool(crate::api::BoolQuery {
            must: vec![
                QueryDsl::Match(MatchQuery { field_value: fv1 }),
                QueryDsl::Match(MatchQuery { field_value: fv2 }),
            ],
            should: vec![],
            must_not: vec![],
            filter: vec![],
        });
        assert_eq!(
            extract_simple_query_text(&q),
            None,
            "Bool must route through structured path, not be flattened"
        );
    }

    #[test]
    fn merge_prefers_hot_on_conflict() {
        // Same (index, id) present in both — hot wins regardless of score.
        let hot = vec![h("topic", "42", 1.0)];
        let cold = vec![h("topic", "42", 99.0), h("topic", "7", 5.0)];
        let merged = merge_hot_and_cold_hits(hot.clone(), cold);

        // Total length = 2 (hot 42 replaces cold 42) + cold 7
        assert_eq!(merged.len(), 2);

        // The hot 42 (score 1.0) should be in the merged set — verified
        // by checking its score matches the hot version, not the cold.
        let hit_42 = merged.iter().find(|h| h._id == "42").unwrap();
        assert_eq!(hit_42._score, Some(1.0));
    }

    #[test]
    fn merge_sorts_by_score_desc() {
        let hot = vec![h("t", "1", 0.5)];
        let cold = vec![h("t", "2", 10.0), h("t", "3", 2.0)];
        let merged = merge_hot_and_cold_hits(hot, cold);
        assert_eq!(merged[0]._id, "2");
        assert_eq!(merged[1]._id, "3");
        assert_eq!(merged[2]._id, "1");
    }

    #[test]
    fn hot_hit_conversion_preserves_fields() {
        let hh = crate::hot_text_index::HotHit {
            topic: "orders".into(),
            partition: 3,
            offset: 1234,
            timestamp: 1_700_000_000,
            score: 0.75,
            key: Some("k1".into()),
            value: "payload text".into(),
        };
        let es = hot_hit_to_es_hit(hh);
        assert_eq!(es._index, "orders");
        // Partition-qualified: offset alone collides across partitions.
        assert_eq!(es._id, "3-1234");
        assert_eq!(es._score, Some(0.75));
        let src = es._source.as_object().unwrap();
        assert_eq!(src.get("partition").and_then(|v| v.as_i64()), Some(3));
        assert_eq!(src.get("offset").and_then(|v| v.as_i64()), Some(1234));
        assert_eq!(
            src.get("value").and_then(|v| v.as_str()),
            Some("payload text")
        );
    }
}
/// Field queries on JSON record values: the hot index and archived segments
/// both index a JSON-object value into `_json`, and a query on a field the
/// schema does not define is a path into it.
#[cfg(test)]
mod json_field_tests {
    use super::*;
    use tantivy::schema::{Schema, INDEXED, STORED, TEXT};

    const DOCS: &[&str] = &[
        r#"{"marker":"quixoticbellwether","n":3,"user":{"name":"Ada Lovelace"},"ok":true}"#,
        r#"{"marker":"ordinary","note":"the quixoticbellwether was mentioned","n":10,"ok":false}"#,
        "quixoticbellwether in plain text",
        r#"{"user.name":"Grace Hopper","n":2.5}"#,
    ];

    /// An index shaped like the hot/archive schemas: whole value as text,
    /// offset, and (unless `legacy`) the JSON field.
    fn index(legacy: bool) -> tantivy::Index {
        let mut b = Schema::builder();
        let value = b.add_text_field("value", TEXT | STORED);
        let offset = b.add_i64_field("offset", INDEXED | STORED);
        let json = (!legacy).then(|| {
            b.add_json_field(
                chronik_storage::json_index::JSON_FIELD,
                chronik_storage::json_index::json_field_options(),
            )
        });
        let index = tantivy::Index::create_in_ram(b.build());
        chronik_storage::register_analyzer(&index);
        let mut writer: tantivy::IndexWriter = index.writer(15_000_000).unwrap();
        for (i, raw) in DOCS.iter().enumerate() {
            let mut doc = tantivy::TantivyDocument::default();
            doc.add_text(value, raw);
            doc.add_i64(offset, i as i64);
            if let (Some(json), Some(obj)) =
                (json, chronik_storage::json_index::json_object(str::as_bytes(raw)))
            {
                doc.add_object(json, obj.into_iter().map(|(k, v)| (k, v.into())).collect());
            }
            writer.add_document(doc).unwrap();
        }
        writer.commit().unwrap();
        index
    }

    /// Offsets matched by an Elasticsearch query DSL, sorted.
    fn hits(index: &tantivy::Index, dsl: serde_json::Value) -> Vec<i64> {
        let dsl: QueryDsl = serde_json::from_value(dsl).unwrap();
        let schema = index.schema();
        let query = build_tantivy_query(&dsl, &schema, Some(index)).unwrap();
        let searcher = index.reader().unwrap().searcher();
        let offset = schema.get_field("offset").unwrap();
        let mut out: Vec<i64> = searcher
            .search(&*query, &TopDocs::with_limit(100))
            .unwrap()
            .into_iter()
            .map(|(_, addr)| {
                let doc: tantivy::TantivyDocument = searcher.doc(addr).unwrap();
                doc.get_first(offset).and_then(|v| v.as_i64()).unwrap()
            })
            .collect();
        out.sort();
        out
    }

    #[test]
    fn match_on_a_json_field_honours_the_field() {
        let idx = index(false);
        // The term is in doc 0's `marker`, in doc 1's `note`, in doc 2's text.
        assert_eq!(hits(&idx, serde_json::json!({"match": {"marker": "quixoticbellwether"}})), vec![0]);
        assert_eq!(hits(&idx, serde_json::json!({"match": {"note": "quixoticbellwether"}})), vec![1]);
        assert_eq!(hits(&idx, serde_json::json!({"match": {"value": "quixoticbellwether"}})), vec![0, 1, 2]);
        assert_eq!(hits(&idx, serde_json::json!({"match": {"nosuchfield": "quixoticbellwether"}})), Vec::<i64>::new());
        // Long form.
        assert_eq!(hits(&idx, serde_json::json!({"match": {"marker": {"query": "ordinary"}}})), vec![1]);
    }

    #[test]
    fn nested_and_dotted_keys_share_one_path() {
        let idx = index(false);
        assert_eq!(hits(&idx, serde_json::json!({"match": {"user.name": "lovelace"}})), vec![0]);
        assert_eq!(hits(&idx, serde_json::json!({"match": {"user.name": "hopper"}})), vec![3]);
    }

    #[test]
    fn numbers_and_booleans_match_typed_values() {
        let idx = index(false);
        assert_eq!(hits(&idx, serde_json::json!({"term": {"n": 3}})), vec![0]);
        assert_eq!(hits(&idx, serde_json::json!({"match": {"n": 10}})), vec![1]);
        assert_eq!(hits(&idx, serde_json::json!({"term": {"n": 2.5}})), vec![3]);
        assert_eq!(hits(&idx, serde_json::json!({"term": {"ok": true}})), vec![0]);
        assert_eq!(hits(&idx, serde_json::json!({"term": {"ok": false}})), vec![1]);
    }

    /// Tantivy 0.24 cannot range over a JSON path without a fast field; the
    /// query is refused up front instead of failing inside the search.
    #[test]
    fn ranges_on_json_fields_are_refused() {
        let idx = index(false);
        let dsl: QueryDsl =
            serde_json::from_value(serde_json::json!({"range": {"n": {"gte": 3}}})).unwrap();
        assert!(build_tantivy_query(&dsl, &idx.schema(), Some(&idx)).is_err());
    }

    #[test]
    fn schema_field_ranges_respect_exclusive_bounds() {
        let idx = index(false);
        // `gt`/`lt` used to be treated as inclusive, and one-sided ranges failed.
        assert_eq!(hits(&idx, serde_json::json!({"range": {"offset": {"gt": 0, "lt": 3}}})), vec![1, 2]);
        assert_eq!(hits(&idx, serde_json::json!({"range": {"offset": {"gte": 2}}})), vec![2, 3]);
    }

    #[test]
    fn bool_must_combines_json_fields() {
        let idx = index(false);
        let q = serde_json::json!({"bool": {"must": [
            {"match": {"marker": "ordinary"}},
            {"match": {"note": "mentioned"}}
        ]}});
        assert_eq!(hits(&idx, q), vec![1]);
        let q = serde_json::json!({"bool": {"must": [
            {"match": {"marker": "quixoticbellwether"}},
            {"match": {"note": "mentioned"}}
        ]}});
        assert_eq!(hits(&idx, q), Vec::<i64>::new());
    }

    /// Archives written before field-level indexing have no `_json`: a field
    /// query there searches the whole value instead of matching nothing.
    #[test]
    fn indexes_without_a_json_field_fall_back_to_full_text() {
        let idx = index(true);
        assert_eq!(hits(&idx, serde_json::json!({"match": {"marker": "quixoticbellwether"}})), vec![0, 1, 2]);
        assert_eq!(hits(&idx, serde_json::json!({"term": {"marker": "ordinary"}})), vec![1]);
        let dsl: QueryDsl = serde_json::from_value(serde_json::json!({"range": {"n": {"gte": 1}}})).unwrap();
        assert!(build_tantivy_query(&dsl, &idx.schema(), Some(&idx)).is_err());
    }

    #[test]
    fn only_whole_record_matches_take_the_hot_simple_path() {
        let q: QueryDsl = serde_json::from_value(serde_json::json!({"match": {"marker": "x"}})).unwrap();
        assert_eq!(extract_simple_query_text(&q), None);
        let q: QueryDsl = serde_json::from_value(serde_json::json!({"match": {"value": "x"}})).unwrap();
        assert_eq!(extract_simple_query_text(&q), Some("x".to_string()));
    }
}

/// A record found in an archived segment must look the same as when the hot
/// index found it, and field queries must reach it.
#[cfg(test)]
mod archived_hit_tests {
    use super::*;
    use chronik_storage::canonical_record::{
        CanonicalRecord, CanonicalRecordEntry, CompressionType, TimestampType,
    };
    use chronik_storage::tantivy_segment::TantivySegmentWriter;

    fn archive() -> tantivy::Index {
        let records = (0..10)
            .map(|i| CanonicalRecordEntry {
                offset: 100 + i,
                timestamp: 1_700_000_000_000 + i,
                key: Some(format!("k{}", i).into_bytes()),
                value: Some(
                    format!(
                        r#"{{"marker":"{}"}}"#,
                        if i == 4 { "quixoticbellwether" } else { "ordinary" }
                    )
                    .into_bytes(),
                ),
                headers: vec![],
                attributes: 0,
            })
            .collect();
        let canonical = CanonicalRecord {
            base_offset: 100,
            partition_leader_epoch: 0,
            producer_id: -1,
            producer_epoch: -1,
            base_sequence: -1,
            is_transactional: false,
            is_control: false,
            compression: CompressionType::None,
            timestamp_type: TimestampType::CreateTime,
            base_timestamp: 1_700_000_000_000,
            max_timestamp: 1_700_000_000_009,
            records,
            compressed_records_wire_bytes: None,
            original_v1_wire_format: None,
            original_v2_wire_format: None,
        };
        let mut writer = TantivySegmentWriter::new("orders".to_string(), 2, 100).unwrap();
        writer.write_batch(&canonical).unwrap();
        writer.commit().unwrap().0
    }

    #[tokio::test]
    async fn archived_hits_have_the_hot_shape() {
        let index = archive();
        let request: SearchRequest = serde_json::from_value(serde_json::json!({
            "query": {"match": {"marker": "quixoticbellwether"}}
        }))
        .unwrap();
        let hits = search_tantivy_index("orders", &index, &query_searcher(&index).unwrap(), &request).unwrap();
        assert_eq!(hits.len(), 1, "the field query reaches the archive");
        let hit = &hits[0];
        assert_eq!(hit._id, "2-104", "same id format as hot hits, for dedup");
        assert_eq!(
            hit._source,
            serde_json::json!({
                "topic": "orders",
                "partition": 2,
                "offset": 104,
                "timestamp": 1_700_000_000_004i64,
                "key": "k4",
                "value": r#"{"marker":"quixoticbellwether"}"#,
            })
        );
    }
}

#[cfg(test)]
mod search_limit_tests {
    use super::*;
    use std::time::Duration;

    fn request(body: serde_json::Value) -> SearchRequest {
        serde_json::from_value(body).unwrap()
    }

    #[test]
    fn time_values_parse_as_elasticsearch_writes_them() {
        assert_eq!(parse_time_value("250ms"), Some(Duration::from_millis(250)));
        assert_eq!(parse_time_value("10s"), Some(Duration::from_secs(10)));
        assert_eq!(parse_time_value("2m"), Some(Duration::from_secs(120)));
        assert_eq!(parse_time_value("1500"), Some(Duration::from_millis(1500)));
        assert_eq!(parse_time_value("soon"), None);
        assert_eq!(parse_time_value("1.5s"), None);
    }

    #[test]
    fn the_request_timeout_sets_the_deadline() {
        let start = tokio::time::Instant::now();
        let r = request(serde_json::json!({"query": {"match_all": {}}, "timeout": "300ms"}));
        assert_eq!(search_deadline(&r, start), start + Duration::from_millis(300));
        // Unparseable: the server default, not "no limit".
        let r = request(serde_json::json!({"timeout": "whenever"}));
        assert!(search_deadline(&r, start) > start);
    }

    #[test]
    fn only_unranked_queries_stop_at_one_page() {
        assert!(is_unranked(&request(serde_json::json!({}))));
        assert!(is_unranked(&request(serde_json::json!({"query": {"match_all": {}}}))));
        assert!(!is_unranked(&request(serde_json::json!({"query": {"match": {"value": "x"}}}))));
        assert!(!is_unranked(&request(
            serde_json::json!({"query": {"match_all": {}}, "sort": [{"offset": "desc"}]})
        )));
    }

    #[test]
    fn a_timed_out_or_full_page_count_is_a_lower_bound() {
        assert_eq!(total_relation(3, 10, false), "eq");
        assert_eq!(total_relation(10, 10, false), "gte");
        assert_eq!(total_relation(3, 10, true), "gte");
    }
}

#[cfg(test)]
mod unranked_stop_tests {
    use super::*;

    fn at(ts: i64) -> Hit {
        Hit {
            _index: "t".into(),
            _id: format!("0-{ts}"),
            _score: Some(1.0),
            _source: serde_json::json!({"timestamp": ts * 1000, "partition": 0, "offset": ts}),
            highlight: None,
        }
    }

    #[test]
    fn stops_only_when_nothing_unsearched_can_rank_higher() {
        let hits: Vec<Hit> = [100, 90, 80, 70].into_iter().map(at).collect();
        // Not a full window yet.
        assert!(!can_stop_unranked(&hits[..2], 3, Some(&10_000)));
        // The 3rd newest is 80; the next segment's newest is older: done.
        assert!(can_stop_unranked(&hits, 3, Some(&79_999)));
        // A tie at 80 could still win the tie-break: keep going.
        assert!(!can_stop_unranked(&hits, 3, Some(&80_000)));
        // A newer record could be in the next segment.
        assert!(!can_stop_unranked(&hits, 3, Some(&95_000)));
        // Nothing left.
        assert!(can_stop_unranked(&hits, 3, None));
    }
}

#[cfg(test)]
mod legacy_index_tests {
    use super::*;
    use chronik_storage::canonical_record::{
        CanonicalRecord, CanonicalRecordEntry, CompressionType, TimestampType,
    };
    use chronik_storage::tantivy_segment::TantivySegmentWriter;
    use tantivy::schema::{DateOptions, NumericOptions, Schema, STORED, STRING, TEXT};

    const PER_PARTITION: i64 = 40;

    /// Same timestamp for every record of a batch, as producers send them —
    /// so ties are broken by partition and offset.
    fn ts(offset: i64) -> i64 {
        1_700_000_000_000 + offset / 10
    }

    /// The index the removed RealtimeIndexer wrote: one per topic, every
    /// partition in it (schema copied from 08b1c58^).
    fn legacy_index() -> tantivy::Index {
        let mut b = Schema::builder();
        let id = b.add_text_field("_id", STRING | STORED);
        let topic = b.add_text_field("_topic", STRING | STORED | tantivy::schema::FAST);
        let fast_i64 = || NumericOptions::default().set_indexed().set_stored().set_fast();
        let partition = b.add_i64_field("_partition", fast_i64());
        let offset = b.add_i64_field("_offset", fast_i64());
        let timestamp = b.add_date_field("_timestamp", DateOptions::default().set_indexed().set_stored().set_fast());
        let key = b.add_text_field("_key", STRING | STORED);
        let value = b.add_text_field("_value", TEXT | STORED);
        let index = tantivy::Index::create_in_ram(b.build());
        let mut w: tantivy::IndexWriter = index.writer(15_000_000).unwrap();
        for p in 0..3i64 {
            for o in 0..PER_PARTITION {
                let mut doc = tantivy::TantivyDocument::default();
                doc.add_text(id, format!("orders-{p}-{o}"));
                doc.add_text(topic, "orders");
                doc.add_i64(partition, p);
                doc.add_i64(offset, o);
                doc.add_date(timestamp, tantivy::DateTime::from_timestamp_millis(ts(o)));
                doc.add_text(key, format!("k{o}"));
                doc.add_text(value, format!("{{\"n\":{o}}}"));
                w.add_document(doc).unwrap();
            }
        }
        w.commit().unwrap();
        index
    }

    /// The same records, archived by the WalIndexer: one index per partition.
    fn archives() -> Vec<tantivy::Index> {
        (0..3i32)
            .map(|p| {
                let records = (0..PER_PARTITION)
                    .map(|o| CanonicalRecordEntry {
                        offset: o,
                        timestamp: ts(o),
                        key: Some(format!("k{o}").into_bytes()),
                        value: Some(format!("{{\"n\":{o}}}").into_bytes()),
                        headers: vec![],
                        attributes: 0,
                    })
                    .collect();
                let batch = CanonicalRecord {
                    base_offset: 0,
                    partition_leader_epoch: 0,
                    producer_id: -1,
                    producer_epoch: -1,
                    base_sequence: -1,
                    is_transactional: false,
                    is_control: false,
                    compression: CompressionType::None,
                    timestamp_type: TimestampType::CreateTime,
                    base_timestamp: ts(0),
                    max_timestamp: ts(PER_PARTITION - 1),
                    records,
                    compressed_records_wire_bytes: None,
                    original_v1_wire_format: None,
                    original_v2_wire_format: None,
                };
                let mut w = TantivySegmentWriter::new("orders".to_string(), p, 0).unwrap();
                w.write_batch(&batch).unwrap();
                w.commit().unwrap().0
            })
            .collect()
    }

    /// One page across `sources`, the way search_index assembles it: each
    /// source's top `from + size`, then one cut.
    fn page_of(sources: &[tantivy::Index], from: usize, size: usize) -> Vec<String> {
        let window: SearchRequest =
            serde_json::from_value(serde_json::json!({"from": 0, "size": from + size})).unwrap();
        let mut hits = Vec::new();
        for idx in sources {
            hits.extend(search_tantivy_index("orders", idx, &query_searcher(idx).unwrap(), &window).unwrap());
        }
        crate::ranking::page(hits, from, size).0.into_iter().map(|h| h._id).collect()
    }

    #[test]
    fn legacy_hits_have_the_common_shape() {
        let idx = legacy_index();
        let r: SearchRequest = serde_json::from_value(serde_json::json!({"size": 1})).unwrap();
        let hit = &search_tantivy_index("orders-dir", &idx, &query_searcher(&idx).unwrap(), &r).unwrap()[0];
        assert_eq!(hit._index, "orders", "the topic, not the directory name");
        let s = &hit._source;
        for k in ["topic", "partition", "offset", "timestamp", "key", "value"] {
            assert!(s.get(k).is_some(), "{k} missing from {s}");
        }
        assert_eq!(
            hit._id,
            format!("{}-{}", s["partition"].as_i64().unwrap(), s["offset"].as_i64().unwrap())
        );
    }

    #[test]
    fn pages_over_a_legacy_index_and_archives_tile_every_record_once() {
        let mut sources = vec![legacy_index()];
        sources.extend(archives());
        let total = (3 * PER_PARTITION) as usize;
        let all = page_of(&sources, 0, total + 10);
        assert_eq!(all.len(), total, "each record once, though two sources hold it");
        let mut paged = Vec::new();
        for from in (0..total + 10).step_by(17) {
            paged.extend(page_of(&sources, from, 17));
        }
        assert_eq!(paged, all, "pages in order equal one search");
        // The legacy index alone pages the same way (nodes without archives).
        let legacy = [legacy_index()];
        let alone = page_of(&legacy, 0, total);
        let mut paged = Vec::new();
        for from in (0..total).step_by(13) {
            paged.extend(page_of(&legacy, from, 13));
        }
        assert_eq!(paged, alone);
        assert_eq!(alone, all, "and in the same order as with the archives");
    }
}
