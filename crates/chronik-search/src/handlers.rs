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
        QueryDsl::MatchAll(_) => Some("*".to_string()),
        QueryDsl::Match(m) => m
            .field_value
            .values()
            .next()
            .and_then(|v| match v {
                serde_json::Value::String(s) => Some(s.clone()),
                serde_json::Value::Object(obj) => obj
                    .get("query")
                    .and_then(|q| q.as_str())
                    .map(|s| s.to_string()),
                _ => None,
            }),
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
        _id: hit.offset.to_string(),
        _score: Some(hit.score),
        _source: serde_json::Value::Object(source),
        highlight: None,
    }
}

/// HP-1.3: Query the hot index for a specific topic and return ES-shaped Hits.
/// Empty Vec if hot index absent or topic unknown.
///
/// Routing:
///   - `MatchAll` / flat `Match` → flat QueryParser path (fastest, common case)
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
    let Some(dsl) = request.query.as_ref() else {
        return Vec::new();
    };
    let k = request.size.max(10);

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
    let Some(dsl) = request.query.as_ref() else {
        return Vec::new();
    };

    let mut topics: Vec<String> = hot_idx
        .partition_keys()
        .into_iter()
        .map(|(t, _)| t)
        .collect();
    topics.sort();
    topics.dedup();

    let k = request.size.max(10);
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
    merged.sort_by(|a, b| {
        b._score
            .unwrap_or(0.0)
            .partial_cmp(&a._score.unwrap_or(0.0))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    merged
}

/// Search all indices
pub async fn search_all(
    State(api): State<Arc<SearchApi>>,
    Json(request): Json<SearchRequest>,
) -> impl IntoResponse {
    let start = SystemTime::now();

    // Generate cache key
    let cache_key = crate::cache::QueryCache::generate_key(None, &request);

    // HP-1.3: skip cache when hot index is active — cached responses would
    // mask newly-visible NRT docs.
    let hot_enabled = api.hot_text_index.is_some();

    // Check cache first
    if !hot_enabled {
        if let Some(cached_response) = api.cache.get(&cache_key) {
            return Json(cached_response);
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
    } else {
        // preserve existing behavior (cold-only sort)
        all_hits.sort_by(|a, b| b._score.partial_cmp(&a._score).unwrap());
    }
    all_hits.truncate(request.size);

    let took = start.elapsed().unwrap_or_default().as_millis() as u64;

    let total_shards = indices.len() as u32 + if api.get_index_base_path().is_some() { 1 } else { 0 };

    let response = SearchResponse {
        took,
        timed_out: false,
        _shards: ShardInfo {
            total: total_shards,
            successful: total_shards,
            skipped: 0,
            failed: 0,
        },
        hits: HitsInfo {
            total: TotalHits {
                value: all_hits.len() as u64,
                relation: "eq".to_string(),
            },
            max_score: all_hits.first().and_then(|h| h._score),
            hits: all_hits,
        },
        aggregations: None,
    };

    // Cache the response — skip when hot path active (would cache stale NRT view)
    if !hot_enabled {
        api.cache.put(cache_key, response.clone());
    }

    Json(response)
}

/// Search specific index
pub async fn search_index(
    Path(index): Path<String>,
    State(api): State<Arc<SearchApi>>,
    Json(request): Json<SearchRequest>,
) -> impl IntoResponse {
    let start = SystemTime::now();

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

    let took = start.elapsed().unwrap_or_default().as_millis() as u64;
    
    let response = SearchResponse {
        took,
        timed_out: false,
        _shards: ShardInfo {
            total: 1,
            successful: 1,
            skipped: 0,
            failed: 0,
        },
        hits: HitsInfo {
            total: TotalHits {
                value: hits.len() as u64,
                relation: "eq".to_string(),
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
                match search_tantivy_index(topic, &index, request).await {
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
                    match search_tantivy_index(&index_name, &index, request).await {
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
async fn search_tantivy_index(
    index_name: &str,
    index: &tantivy::Index,
    request: &SearchRequest,
) -> Result<Vec<Hit>> {
    let reader = index.reader()
        .map_err(|e| Error::Internal(format!("Failed to create index reader: {}", e)))?;
    let searcher = reader.searcher();

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
    let top_docs = searcher.search(&*query, &TopDocs::with_limit(request.size).and_offset(request.from))
        .map_err(|e| Error::Internal(format!("Search failed: {}", e)))?;

    // Collect results
    let mut hits = Vec::new();
    for (score, doc_address) in top_docs {
        let doc: tantivy::TantivyDocument = searcher.doc(doc_address)
            .map_err(|e| Error::Internal(format!("Failed to retrieve document: {}", e)))?;

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
        let doc_id = source.get("offset")
            .and_then(|v| v.as_i64())
            .map(|o| o.to_string())
            .or_else(|| source.get("_offset").and_then(|v| v.as_i64()).map(|o| o.to_string()))
            .or_else(|| source.get("_id").and_then(|v| v.as_str()).map(|s| s.to_string()))
            .unwrap_or_else(|| Uuid::new_v4().to_string());

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
    let top_docs = searcher.search(&*query, &TopDocs::with_limit(request.size).and_offset(request.from))
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
                let field = schema.get_field(field_name)
                    .map_err(|_| Error::InvalidInput(format!("Field {} not found", field_name)))?;
                vec![field]
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
            
            let field = schema.get_field(field_name)
                .map_err(|_| Error::InvalidInput(format!("Field {} not found", field_name)))?;
            
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
            
            let field = schema.get_field(field_name)
                .map_err(|_| Error::InvalidInput(format!("Field {} not found", field_name)))?;
            
            // For now, we'll create a simple numeric range query
            // In a full implementation, this would handle dates and other types
            let field_type = schema.get_field_entry(field).field_type();
            
            match field_type {
                FieldType::I64(_) | FieldType::U64(_) => {
                    // Extract bounds
                    let lower_bound = range_clause.gte.as_ref()
                        .or(range_clause.gt.as_ref())
                        .and_then(|v| v.as_i64());
                    
                    let upper_bound = range_clause.lte.as_ref()
                        .or(range_clause.lt.as_ref())
                        .and_then(|v| v.as_i64());
                    
                    if let (Some(lower), Some(upper)) = (lower_bound, upper_bound) {
                        let lower_term = Term::from_field_i64(field, lower);
                        let upper_term = Term::from_field_i64(field, upper);
                        Ok(Box::new(RangeQuery::new(
                            std::ops::Bound::Included(lower_term),
                            std::ops::Bound::Included(upper_term)
                        )))
                    } else {
                        Err(Error::InvalidInput("Range query must specify bounds".to_string()))
                    }
                },
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
    fn extract_match_all_returns_star() {
        let q = QueryDsl::MatchAll(crate::api::MatchAllQuery {});
        assert_eq!(extract_simple_query_text(&q), Some("*".to_string()));
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
        assert_eq!(es._id, "1234");
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