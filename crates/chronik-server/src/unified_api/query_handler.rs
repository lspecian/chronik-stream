//! Handler for the `/_query` unified query endpoint.
//!
//! Wires the `chronik-query` orchestrator into the unified API by implementing
//! `BackendAdapter` against the real server backends (SearchApi, ColumnarQueryEngine,
//! VectorSearchService, WalManager).

use super::UnifiedApiState;
use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use chronik_columnar::datafusion::arrow::array::{Int32Array, Int64Array};
use chronik_query::{
    capabilities::TopicCapabilities,
    candidate::Candidate,
    orchestrator::{BackendAdapter, BackendError, QueryOrchestrator},
    plan::QueryPlanner,
    types::*,
};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{debug, warn};

/// Backend adapter that delegates to real server backends.
///
/// Implements `BackendAdapter` by calling into the actual SearchApi,
/// ColumnarQueryEngine, VectorSearchService, and WalManager.
struct ServerBackendAdapter {
    state: UnifiedApiState,
}

#[async_trait::async_trait]
impl BackendAdapter for ServerBackendAdapter {
    async fn execute_text(
        &self,
        topic: &str,
        query: &str,
        k: usize,
    ) -> Result<Vec<Candidate>, BackendError> {
        #[cfg(feature = "search")]
        {
            let search_api = self.state.search_api.as_ref()
                .ok_or_else(|| BackendError::NotAvailable("Text search not enabled".into()))?;

            let mut candidates = Vec::new();

            // 1. Search in-memory indices (created via REST API)
            if let Some(state) = search_api.indices.get(topic) {
                match search_index_for_candidates(topic, &state, query, k) {
                    Ok(hits) => candidates.extend(hits),
                    Err(e) => warn!(topic = %topic, error = %e, "In-memory index search failed"),
                }
            }

            // 2. Search WAL-created indices on disk (topic-specific directories)
            if let Some(base_path) = search_api.get_index_base_path() {
                // WAL indices are stored as {base_path}/{topic}-{partition}/
                match search_wal_topic_for_candidates(base_path, topic, query, k) {
                    Ok(hits) => candidates.extend(hits),
                    Err(e) => debug!(topic = %topic, error = %e, "WAL index search returned no results"),
                }

                // Also check real-time indices at sibling directory
                let realtime_path = base_path.replace("tantivy_indexes", "index");
                match search_wal_topic_for_candidates(&realtime_path, topic, query, k) {
                    Ok(hits) => candidates.extend(hits),
                    Err(e) => debug!(topic = %topic, error = %e, "Real-time index search returned no results"),
                }
            }

            // Sort by score descending and truncate
            candidates.sort_by(|a, b| b.raw_score.partial_cmp(&a.raw_score).unwrap_or(std::cmp::Ordering::Equal));
            candidates.truncate(k);

            // Reassign ranks after sort
            for (i, c) in candidates.iter_mut().enumerate() {
                c.rank = i;
            }

            debug!(topic = %topic, results = candidates.len(), "Text search completed");
            Ok(candidates)
        }

        #[cfg(not(feature = "search"))]
        {
            debug!(topic = %topic, query = %query, k = k, "Text search not available (search feature disabled)");
            Ok(vec![])
        }
    }

    async fn execute_vector(
        &self,
        topic: &str,
        query: &str,
        k: usize,
    ) -> Result<Vec<Candidate>, BackendError> {
        let manager = self.state.vector_index_manager.as_ref()
            .ok_or_else(|| BackendError::NotAvailable("Vector search not enabled".into()))?;

        let provider = self.state.embedding_provider.as_ref()
            .ok_or_else(|| BackendError::NotAvailable("Embedding provider not configured".into()))?;

        // Embed query text
        let embedding_result = provider.embed(query).await
            .map_err(|e| BackendError::ExecutionFailed(format!("Embedding failed: {}", e)))?;

        // Search across all partitions of the topic
        let raw_results = manager.search_topic(topic, &embedding_result.vector, k).await
            .map_err(|e| BackendError::ExecutionFailed(format!("Vector search failed: {}", e)))?;

        // Convert to candidates
        let candidates: Vec<Candidate> = raw_results
            .into_iter()
            .enumerate()
            .map(|(rank, (partition, offset, score))| {
                Candidate::new(
                    topic.to_string(),
                    partition,
                    offset,
                    QueryMode::Vector,
                    score as f64,
                    rank,
                )
            })
            .collect();

        Ok(candidates)
    }

    async fn execute_sql(
        &self,
        topic: &str,
        sql: &str,
        limit: usize,
    ) -> Result<Vec<Candidate>, BackendError> {
        let engine = self.state.query_engine.as_ref()
            .ok_or_else(|| BackendError::NotAvailable("SQL query engine not enabled".into()))?;

        // SQL table names use sanitized topic names (- and . replaced with _)
        let sanitized = super::sql_handler::SqlHandler::sanitize_table_name(topic);

        // Check if table exists (try base name, then cold, then hot)
        let table_available = engine.table_exists(&sanitized).await
            || engine.table_exists(&format!("{}_cold", sanitized)).await
            || engine.table_exists(&format!("{}_hot", sanitized)).await;

        if !table_available {
            return Err(BackendError::NotAvailable(format!(
                "Table '{}' (sanitized: '{}') not registered", topic, sanitized
            )));
        }

        // Execute SQL query
        let batches = engine.execute_sql(sql).await
            .map_err(|e| BackendError::ExecutionFailed(format!("SQL query failed: {}", e)))?;

        // Convert RecordBatch rows to candidates
        let mut candidates = Vec::new();
        let mut rank = 0;

        for batch in &batches {
            let schema = batch.schema();

            // Try to find offset and partition columns
            let offset_col_idx = schema.index_of("offset").or_else(|_| schema.index_of("kafka_offset")).ok();
            let partition_col_idx = schema.index_of("partition").or_else(|_| schema.index_of("kafka_partition")).ok();
            let timestamp_col_idx = schema.index_of("timestamp").or_else(|_| schema.index_of("kafka_timestamp")).ok();

            for row_idx in 0..batch.num_rows() {
                if rank >= limit {
                    break;
                }

                let offset = offset_col_idx
                    .and_then(|idx| {
                        let col = batch.column(idx);
                        col.as_any()
                            .downcast_ref::<Int64Array>()
                            .map(|arr| arr.value(row_idx))
                    })
                    .unwrap_or(rank as i64);

                let partition = partition_col_idx
                    .and_then(|idx| {
                        let col = batch.column(idx);
                        col.as_any()
                            .downcast_ref::<Int32Array>()
                            .map(|arr| arr.value(row_idx))
                    })
                    .unwrap_or(0);

                let mut candidate = Candidate::new(
                    topic.to_string(),
                    partition,
                    offset,
                    QueryMode::Sql,
                    1.0, // SQL results are ordered by the query itself
                    rank,
                );

                if let Some(ts_idx) = timestamp_col_idx {
                    let col = batch.column(ts_idx);
                    if let Some(arr) = col.as_any().downcast_ref::<Int64Array>() {
                        candidate = candidate.with_timestamp(arr.value(row_idx));
                    }
                }

                candidates.push(candidate);
                rank += 1;
            }
        }

        Ok(candidates)
    }

    async fn execute_fetch(
        &self,
        topic: &str,
        partition: i32,
        offset: i64,
        max_bytes: usize,
    ) -> Result<Vec<Candidate>, BackendError> {
        let wal = self.state.wal_manager.as_ref()
            .ok_or_else(|| BackendError::NotAvailable("WAL manager not available".into()))?;

        // Calculate max records from max_bytes (estimate ~512 bytes per record)
        let max_records = (max_bytes / 512).max(1);

        let records = wal.read_from(topic, partition, offset, max_records).await
            .map_err(|e| BackendError::ExecutionFailed(format!("WAL fetch failed: {}", e)))?;

        let candidates: Vec<Candidate> = records
            .into_iter()
            .enumerate()
            .map(|(rank, record)| {
                // Extract offset from WalRecord enum variants
                let record_offset = match &record {
                    chronik_wal::WalRecord::V1 { offset, .. } => *offset,
                    chronik_wal::WalRecord::V2 { base_offset, .. } => *base_offset,
                };

                let candidate = Candidate::new(
                    topic.to_string(),
                    partition,
                    record_offset,
                    QueryMode::Fetch,
                    1.0,
                    rank,
                );

                candidate
            })
            .collect();

        Ok(candidates)
    }
}


/// Search a Tantivy IndexState and return Candidates.
///
/// Builds a query from the text using QueryParser, executes it, and extracts
/// offset/partition/timestamp from stored fields to build Candidates.
#[cfg(feature = "search")]
fn search_index_for_candidates(
    topic: &str,
    state: &chronik_search::api::IndexState,
    query_text: &str,
    k: usize,
) -> Result<Vec<Candidate>, BackendError> {
    use tantivy::collector::TopDocs;
    use tantivy::query::QueryParser;
    use tantivy::schema::{FieldType, Value};

    let searcher = state.reader.searcher();

    // Find all searchable fields: text (Str) AND JSON object fields
    // JSON object fields (like _json_content) store structured data that is tokenized
    // and searchable across all paths within the JSON
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
        .map_err(|e| BackendError::ExecutionFailed(format!("Query parse failed: {}", e)))?;

    let top_docs = searcher.search(&*parsed_query, &TopDocs::with_limit(k))
        .map_err(|e| BackendError::ExecutionFailed(format!("Search failed: {}", e)))?;

    let mut candidates = Vec::new();
    for (rank, (score, doc_address)) in top_docs.iter().enumerate() {
        let doc: tantivy::TantivyDocument = searcher.doc(*doc_address)
            .map_err(|e| BackendError::ExecutionFailed(format!("Doc retrieval failed: {}", e)))?;

        // Extract offset and partition from stored fields
        // Try both with and without underscore prefix (different index schemas)
        let offset_field = state.schema.get_field("offset").ok()
            .or_else(|| state.schema.get_field("_offset").ok());
        let partition_field = state.schema.get_field("partition").ok()
            .or_else(|| state.schema.get_field("_partition").ok());
        let timestamp_field = state.schema.get_field("timestamp").ok()
            .or_else(|| state.schema.get_field("_timestamp").ok());

        let offset = offset_field
            .and_then(|f| doc.get_all(f).next().and_then(|v| v.as_i64()))
            .unwrap_or(rank as i64);

        let partition = partition_field
            .and_then(|f| doc.get_all(f).next().and_then(|v| v.as_i64()))
            .unwrap_or(0) as i32;

        let mut candidate = Candidate::new(
            topic.to_string(),
            partition,
            offset,
            QueryMode::Text,
            *score as f64,
            rank,
        );

        if let Some(ts) = timestamp_field.and_then(|f| doc.get_all(f).next().and_then(|v| v.as_i64())) {
            candidate = candidate.with_timestamp(ts);
        }

        // Extract text preview from stored content fields
        let preview = state.schema.get_field("_value").ok()
            .and_then(|f| doc.get_all(f).next().and_then(|v| v.as_str().map(|s| s.to_string())))
            .or_else(|| {
                state.schema.get_field("_json_content").ok()
                    .and_then(|f| doc.get_all(f).next().and_then(|v| v.as_str().map(|s| s.to_string())))
            })
            .or_else(|| {
                state.schema.get_field("_content").ok()
                    .and_then(|f| doc.get_all(f).next().and_then(|v| v.as_str().map(|s| s.to_string())))
            });
        if let Some(text) = preview {
            // Strip system fields from JSON content for cleaner preview
            let cleaned = strip_system_fields(&text);
            let truncated = if cleaned.len() > 300 { format!("{}...", &cleaned[..300]) } else { cleaned };
            candidate = candidate.with_text_preview(truncated);
        }

        candidates.push(candidate);
    }

    Ok(candidates)
}

/// Search WAL-created Tantivy indices on disk for a specific topic.
///
/// WAL indices are stored as `{base_path}/{topic}-{partition}/` directories.
/// This searches all partition indices for the given topic.
#[cfg(feature = "search")]
fn search_wal_topic_for_candidates(
    base_path: &str,
    topic: &str,
    query_text: &str,
    k: usize,
) -> Result<Vec<Candidate>, BackendError> {
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
        .map_err(|e| BackendError::ExecutionFailed(format!("Failed to read index directory: {}", e)))?;

    let mut all_candidates = Vec::new();

    for entry in entries {
        let entry = entry
            .map_err(|e| BackendError::ExecutionFailed(format!("Failed to read entry: {}", e)))?;
        let path = entry.path();

        if !path.is_dir() {
            continue;
        }

        // Check if this directory matches the topic (format: "topic-partition" or just "topic")
        let dir_name = path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");

        if !dir_name.starts_with(topic) {
            continue;
        }
        // Verify exact match: dir_name should be "topic" or "topic-N"
        let suffix = &dir_name[topic.len()..];
        if !suffix.is_empty() && !suffix.starts_with('-') {
            continue;
        }

        // Try to open and search this index
        let index = match Index::open_in_dir(&path) {
            Ok(idx) => idx,
            Err(_) => continue,
        };

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

        // Extract partition from directory name
        let dir_partition: i32 = suffix.strip_prefix('-')
            .and_then(|p| p.parse().ok())
            .unwrap_or(0);

        for (rank, (score, doc_address)) in top_docs.iter().enumerate() {
            let doc: tantivy::TantivyDocument = match searcher.doc(*doc_address) {
                Ok(d) => d,
                Err(_) => continue,
            };

            let offset_field = schema.get_field("offset").ok()
                .or_else(|| schema.get_field("_offset").ok());
            let timestamp_field = schema.get_field("timestamp").ok()
                .or_else(|| schema.get_field("_timestamp").ok());

            let offset = offset_field
                .and_then(|f| doc.get_all(f).next().and_then(|v| v.as_i64()))
                .unwrap_or(rank as i64);

            let mut candidate = Candidate::new(
                topic.to_string(),
                dir_partition,
                offset,
                QueryMode::Text,
                *score as f64,
                all_candidates.len() + rank,
            );

            if let Some(ts) = timestamp_field.and_then(|f| doc.get_all(f).next().and_then(|v| v.as_i64())) {
                candidate = candidate.with_timestamp(ts);
            }

            // Extract text preview from stored content fields
            let preview = schema.get_field("_value").ok()
                .and_then(|f| doc.get_all(f).next().and_then(|v| v.as_str().map(|s| s.to_string())))
                .or_else(|| {
                    schema.get_field("_json_content").ok()
                        .and_then(|f| doc.get_all(f).next().and_then(|v| v.as_str().map(|s| s.to_string())))
                })
                .or_else(|| {
                    schema.get_field("_content").ok()
                        .and_then(|f| doc.get_all(f).next().and_then(|v| v.as_str().map(|s| s.to_string())))
                });
            if let Some(text) = preview {
                let cleaned = strip_system_fields(&text);
                let truncated = if cleaned.len() > 300 { format!("{}...", &cleaned[..300]) } else { cleaned };
                candidate = candidate.with_text_preview(truncated);
            }

            all_candidates.push(candidate);
        }
    }

    Ok(all_candidates)
}

/// Check if a specific topic has a Tantivy text search index available.
#[cfg(feature = "search")]
fn topic_has_text_index(search_api: &chronik_search::SearchApi, topic: &str) -> bool {
    // Check in-memory indices
    if search_api.indices.contains_key(topic) {
        return true;
    }
    // Check WAL-created indices on disk
    if let Some(base_path) = search_api.get_index_base_path() {
        let base = std::path::Path::new(base_path);
        if base.exists() {
            if let Ok(entries) = std::fs::read_dir(base) {
                for entry in entries.flatten() {
                    let dir_name = entry.file_name();
                    let dir_name = dir_name.to_string_lossy();
                    if dir_name == topic || dir_name.starts_with(&format!("{}-", topic)) {
                        return true;
                    }
                }
            }
        }
        // Also check real-time index directory
        let realtime = base_path.replace("tantivy_indexes", "index");
        let rt_base = std::path::Path::new(&realtime);
        if rt_base.exists() {
            if let Ok(entries) = std::fs::read_dir(rt_base) {
                for entry in entries.flatten() {
                    let dir_name = entry.file_name();
                    let dir_name = dir_name.to_string_lossy();
                    if dir_name == topic || dir_name.starts_with(&format!("{}-", topic)) {
                        return true;
                    }
                }
            }
        }
    }
    false
}

/// Detect per-topic capabilities from the actual server state.
fn detect_topic_capabilities(
    state: &UnifiedApiState,
    topic: &str,
) -> TopicCapabilities {
    let text_search = {
        #[cfg(feature = "search")]
        {
            state.search_api.as_ref()
                .map(|api| topic_has_text_index(api, topic))
                .unwrap_or(false)
        }
        #[cfg(not(feature = "search"))]
        { false }
    };

    let vector_search = state.vector_index_manager.is_some();

    let sql_query = if let Some(engine) = &state.query_engine {
        // SQL tables use sanitized names (e.g., "e2e-logs" → "e2e_logs")
        let sanitized = super::sql_handler::SqlHandler::sanitize_table_name(topic);
        // Check synchronously — table_exists is cheap (DataFusion catalog lookup)
        // We use a blocking check here since we need the result for planning
        tokio::task::block_in_place(|| {
            let handle = tokio::runtime::Handle::current();
            handle.block_on(engine.table_exists(&sanitized))
                || handle.block_on(engine.table_exists(&format!("{}_cold", sanitized)))
                || handle.block_on(engine.table_exists(&format!("{}_hot", sanitized)))
        })
    } else {
        false
    };

    TopicCapabilities {
        topic: topic.to_string(),
        text_search,
        vector_search,
        sql_query,
        fetch: state.wal_manager.is_some(),
    }
}

/// Handle `POST /_query` — unified query endpoint.
pub async fn handle_query(
    State(state): State<UnifiedApiState>,
    Json(request): Json<QueryRequest>,
) -> impl IntoResponse {
    // Record metrics
    chronik_monitoring::MetricsRecorder::record_query_request();

    // Ensure SQL tables are registered (same as /_sql handler does)
    // This registers Parquet files + hot buffer as DataFusion tables
    if let Some(engine) = &state.query_engine {
        super::sql_handler::SqlHandler::ensure_topics_registered(&state, engine).await;
    }

    // Per-topic capability detection
    let mut capabilities = HashMap::new();
    for source in &request.sources {
        let cap = detect_topic_capabilities(&state, &source.topic);
        capabilities.insert(source.topic.clone(), cap);
    }

    // Plan the query
    let plan = match QueryPlanner::plan(&request, &capabilities) {
        Ok(plan) => plan,
        Err(e) => {
            chronik_monitoring::MetricsRecorder::record_query_error("planning");
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": e.to_string(),
                    "type": "planning_error"
                })),
            )
                .into_response();
        }
    };

    debug!(
        nodes = plan.nodes.len(),
        k = plan.k,
        "Query plan created"
    );

    // Create orchestrator with shared state
    let adapter = Arc::new(ServerBackendAdapter { state: state.clone() });
    let orchestrator = QueryOrchestrator::new(adapter, state.profile_store.clone())
        .with_feature_logger(state.feature_logger.clone());

    // Execute
    match orchestrator.execute(&request, plan).await {
        Ok(response) => {
            chronik_monitoring::MetricsRecorder::record_query_latency(
                std::time::Duration::from_millis(response.stats.latency_ms)
            );
            chronik_monitoring::MetricsRecorder::record_query_candidates(
                response.stats.candidates as u64
            );
            debug!(
                query_id = %response.query_id,
                candidates = response.stats.candidates,
                results = response.stats.ranked,
                latency_ms = response.stats.latency_ms,
                "Query executed"
            );
            match serde_json::to_value(&response) {
                Ok(json) => (StatusCode::OK, Json(json)).into_response(),
                Err(e) => {
                    tracing::error!(error = %e, "Failed to serialize query response");
                    (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
                        "error": "Response serialization failed",
                        "type": "internal_error"
                    }))).into_response()
                }
            }
        }
        Err(e) => {
            chronik_monitoring::MetricsRecorder::record_query_error("execution");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": e.to_string(),
                    "type": "execution_error"
                })),
            )
                .into_response()
        }
    }
}

/// Handle `GET /_query/capabilities` — returns per-topic capabilities.
pub async fn handle_capabilities(
    State(state): State<UnifiedApiState>,
) -> impl IntoResponse {
    // Ensure SQL tables are registered so capabilities detection works
    if let Some(engine) = &state.query_engine {
        super::sql_handler::SqlHandler::ensure_topics_registered(&state, engine).await;
    }

    // List topics from metadata store
    let topics = match state.metadata_store.list_topics().await {
        Ok(topics) => topics,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": format!("Failed to list topics: {}", e)
                })),
            )
                .into_response();
        }
    };

    let mut capabilities = Vec::new();
    for topic_meta in &topics {
        let cap = detect_topic_capabilities(&state, &topic_meta.name);
        capabilities.push(cap);
    }

    match serde_json::to_value(&capabilities) {
        Ok(json) => (StatusCode::OK, Json(json)).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "Failed to serialize capabilities");
            (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
                "error": "Response serialization failed",
                "type": "internal_error"
            }))).into_response()
        }
    }
}

/// Strip internal system fields (prefixed with _) from a JSON string for cleaner previews.
fn strip_system_fields(text: &str) -> String {
    if let Ok(mut json) = serde_json::from_str::<serde_json::Value>(text) {
        if let Some(obj) = json.as_object_mut() {
            obj.retain(|k, _| !k.starts_with('_'));
            return serde_json::to_string(obj).unwrap_or_else(|_| text.to_string());
        }
    }
    text.to_string()
}

/// Handle `GET /_query/profiles` — returns available ranking profiles.
pub async fn handle_profiles(
    State(state): State<UnifiedApiState>,
) -> impl IntoResponse {
    let names = state.profile_store.list().await;

    #[derive(Serialize)]
    struct ProfileInfo {
        name: String,
        weights: HashMap<String, f64>,
    }

    let mut profiles = Vec::new();
    for name in names {
        let profile = state.profile_store.get(&name).await;
        profiles.push(ProfileInfo {
            name: profile.name,
            weights: profile.weights,
        });
    }

    match serde_json::to_value(&profiles) {
        Ok(json) => (StatusCode::OK, Json(json)).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "Failed to serialize profiles");
            (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
                "error": "Response serialization failed",
                "type": "internal_error"
            }))).into_response()
        }
    }
}
