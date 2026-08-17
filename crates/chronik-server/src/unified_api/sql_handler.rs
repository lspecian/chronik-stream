//! SQL Query Handler for Unified API
//!
//! Provides SQL query endpoints via DataFusion:
//! - POST `/_sql` - Execute SQL query
//! - POST `/_sql/explain` - Get query execution plan
//! - GET `/_sql/tables` - List available tables (topics)
//! - GET `/_sql/describe/:table` - Get table schema

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::collections::HashSet;
use tracing::{debug, error, info, warn};

use super::UnifiedApiState;

/// SQL query request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SqlRequest {
    /// SQL query to execute
    pub query: String,
    /// Maximum rows to return. Omit to let the query decide: a query with its
    /// own `LIMIT n` returns up to `n` rows, anything else returns up to
    /// [`DEFAULT_ROW_LIMIT`]. An explicit value always wins.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Query timeout in seconds (default: 30)
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

/// Rows returned when neither the request nor the query says otherwise.
pub const DEFAULT_ROW_LIMIT: usize = 1000;

/// Hard ceiling on rows materialised into a JSON response, mirroring the
/// query engine's own `max_rows`.
pub const MAX_ROW_LIMIT: usize = 100_000;

impl SqlRequest {
    /// Rows this request should return.
    ///
    /// Issue #19: `SELECT ... LIMIT 5000` used to return exactly 1000 rows
    /// because the response cap was applied unconditionally. A query that
    /// states its own limit now gets it (up to [`MAX_ROW_LIMIT`]); callers who
    /// need a different cap still pass `limit` explicitly.
    pub fn effective_limit(&self) -> usize {
        // Clamped either way: the query engine stops at `max_rows` regardless,
        // so a larger cap here would report `truncated: false` on a result the
        // engine had already cut short.
        self.limit
            .or_else(|| sql_statement_limit(&self.query))
            .unwrap_or(DEFAULT_ROW_LIMIT)
            .min(MAX_ROW_LIMIT)
    }
}

/// Extract a top-level `LIMIT <n>` from a SQL statement, if it has one.
///
/// Returns None for absent, non-literal (`LIMIT ?`), or unparseable limits —
/// in which case the default row cap applies.
fn sql_statement_limit(sql: &str) -> Option<usize> {
    use chronik_columnar::datafusion::sql::parser::{DFParser, Statement};
    use chronik_columnar::datafusion::sql::sqlparser::ast::{
        Expr as SqlExpr, SetExpr, Statement as AstStatement, Value,
    };

    let statements = DFParser::parse_sql(sql).ok()?;
    let statement = statements.front()?;
    let Statement::Statement(ast) = statement else {
        return None;
    };
    let AstStatement::Query(query) = ast.as_ref() else {
        return None;
    };

    // A parenthesised query carries its own LIMIT; unwrap one level so
    // `(SELECT ... LIMIT 5000)` is not mistaken for "no limit".
    let limit = match query.limit.as_ref() {
        Some(limit) => Some(limit),
        None => match query.body.as_ref() {
            SetExpr::Query(inner) => inner.limit.as_ref(),
            _ => None,
        },
    }?;

    match limit {
        SqlExpr::Value(Value::Number(n, _)) => n.parse::<usize>().ok(),
        _ => None,
    }
}

fn default_timeout() -> u64 {
    30
}

/// SQL query response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SqlResponse {
    /// Column names
    pub columns: Vec<String>,
    /// Row data (each row is a map of column name to value)
    pub rows: Vec<HashMap<String, serde_json::Value>>,
    /// Number of rows returned
    pub row_count: usize,
    /// Query execution time in milliseconds
    pub execution_time_ms: u64,
    /// Whether results were truncated
    pub truncated: bool,
}

/// Error response
#[derive(Debug, Serialize)]
pub struct SqlErrorResponse {
    pub error: String,
    pub error_type: String,
}

/// Resolves the Parquet segments backing one topic, on demand.
///
/// Used by [`chronik_columnar::LiveParquetTableProvider`] so the `{topic}_cold`
/// table re-reads the segment list at scan time instead of pinning whatever
/// existed when the table was first registered.
struct TopicParquetSource {
    topic: String,
    metadata_store: std::sync::Arc<dyn chronik_common::metadata::traits::MetadataStore>,
    /// Restricts the scan to the partitions this node answers for. `None` in
    /// single-node mode, where there is nobody to double-count with.
    ownership: Option<std::sync::Arc<dyn chronik_columnar::PartitionOwnership>>,
}

/// Answers "which partitions of this topic does this node lead?" for the query
/// layer, from the router's partition map.
///
/// Leadership is the ownership rule because it partitions the work exactly once:
/// every replica holds every partition it replicates, so a replica-based rule
/// either double-counts rows or, when each node serves only what it happens to
/// see locally, drops them (#22).
#[derive(Clone)]
struct LeadPartitions {
    router: std::sync::Arc<super::query_router::QueryRouter>,
    /// Needed to map a topic the router has not seen yet.
    ///
    /// The partition map is populated lazily, and "unknown topic" resolves to
    /// "serve everything" — which on three nodes means every node serving every
    /// partition and the merge counting each row three times. Refreshing on a
    /// miss keeps that answer rare and correct rather than merely safe-looking.
    metadata_store: std::sync::Arc<dyn chronik_common::metadata::traits::MetadataStore>,
}

impl std::fmt::Debug for LeadPartitions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LeadPartitions").finish()
    }
}

#[async_trait::async_trait]
impl chronik_columnar::PartitionOwnership for LeadPartitions {
    async fn owned_partitions(&self, topic: &str) -> Option<HashSet<i32>> {
        if let Some(led) = self.router.led_partitions(topic).await {
            return Some(led);
        }

        // Unknown topic: map it and ask again. A topic created after the map was
        // first populated would otherwise stay unknown for the life of the
        // process.
        self.router
            .refresh_partition_map(self.metadata_store.as_ref(), topic)
            .await;
        self.router.led_partitions(topic).await
    }
}

/// Keep only Parquet files belonging to `owned` partitions.
///
/// Columnar output is laid out Hive-style — `.../{topic}/partition=N/...` — so
/// the partition is recoverable from the path without consulting metadata. A
/// path whose partition cannot be parsed is **kept**: dropping it would silently
/// lose data, and an unexpected layout should surface as a duplicate-row bug
/// rather than as missing rows.
fn retain_owned_partitions(paths: Vec<String>, owned: &HashSet<i32>) -> Vec<String> {
    paths
        .into_iter()
        .filter(|path| match partition_of_parquet_path(path) {
            Some(partition) => owned.contains(&partition),
            None => {
                warn!(
                    path = %path,
                    "Parquet path has no partition= component; serving it rather than dropping data"
                );
                true
            }
        })
        .collect()
}

/// Extract `N` from a `partition=N` path component.
fn partition_of_parquet_path(path: &str) -> Option<i32> {
    path.split('/')
        .find_map(|part| part.strip_prefix("partition="))
        .and_then(|n| n.parse().ok())
}

/// The ownership rule for this deployment: leadership in a cluster, none
/// (serve everything) without a router.
fn query_ownership(
    state: &UnifiedApiState,
) -> Option<std::sync::Arc<dyn chronik_columnar::PartitionOwnership>> {
    state.query_router.as_ref().map(|router| {
        std::sync::Arc::new(LeadPartitions {
            router: router.clone(),
            metadata_store: state.metadata_store.clone(),
        }) as std::sync::Arc<dyn chronik_columnar::PartitionOwnership>
    })
}

impl std::fmt::Debug for TopicParquetSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // MetadataStore is not Debug; the topic is the identifying part.
        f.debug_struct("TopicParquetSource")
            .field("topic", &self.topic)
            .finish()
    }
}

#[async_trait::async_trait]
impl chronik_columnar::ParquetPathSource for TopicParquetSource {
    async fn parquet_paths(&self) -> Vec<String> {
        let paths = resolve_parquet_paths(self.metadata_store.as_ref(), &self.topic).await;

        // Resolved per scan, not at registration, so a leadership change takes
        // effect on the next query rather than requiring re-registration.
        match &self.ownership {
            Some(ownership) => match ownership.owned_partitions(&self.topic).await {
                Some(owned) => retain_owned_partitions(paths, &owned),
                None => paths,
            },
            None => paths,
        }
    }
}

/// Current Parquet paths for a topic: metadata store first, filesystem fallback
/// for the window before the WalIndexer has persisted segment metadata.
/// Non-existent paths (stale entries from previous runs) are filtered out.
async fn resolve_parquet_paths(
    metadata_store: &dyn chronik_common::metadata::traits::MetadataStore,
    topic: &str,
) -> Vec<String> {
    let paths = match metadata_store.get_parquet_paths(topic).await {
        Ok(paths) => paths,
        Err(e) => {
            debug!("No Parquet data for topic '{}': {}", topic, e);
            Vec::new()
        }
    };

    // Drop entries pointing at files that no longer exist (stale metadata from a
    // previous run) *before* deciding whether to fall back — otherwise a topic
    // whose recorded paths are all stale looks "non-empty" and skips discovery.
    let mut paths: Vec<String> = paths
        .into_iter()
        .filter(|p| {
            // Object-store URLs are not local paths; only stat real files.
            p.contains("://") || std::path::Path::new(p).exists()
        })
        .collect();

    if paths.is_empty() {
        let data_dir =
            std::env::var("CHRONIK_DATA_DIR").unwrap_or_else(|_| "./data".to_string());
        let columnar_dir = format!("{}/columnar/{}", data_dir, topic);
        let columnar_path = std::path::Path::new(&columnar_dir);
        if columnar_path.exists() {
            if let Ok(mut discovered) = discover_parquet_files(columnar_path) {
                if !discovered.is_empty() {
                    discovered.sort();
                    debug!(
                        topic = %topic,
                        count = discovered.len(),
                        "Discovered Parquet files via filesystem fallback"
                    );
                    paths = discovered;
                }
            }
        }
    }

    paths
}

/// What a topic's SQL view is currently built from.
///
/// The unified view captures its base providers at CREATE time, so it must be
/// rebuilt when a topic gains a source it did not have before (typically when
/// the first Parquet segment appears for a topic that started hot-only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct ViewComposition {
    has_hot: bool,
    has_cold: bool,
}

/// Per-topic SQL registration bookkeeping.
///
/// The providers themselves are live (they re-resolve their data on every
/// scan), so this only tracks what has been registered and throttles the
/// "has cold data appeared yet?" probe, which costs a metadata scan.
#[derive(Debug, Default)]
pub struct SqlTableRegistry {
    topics: dashmap::DashMap<String, ViewComposition>,
    last_cold_probe_ms: dashmap::DashMap<String, u64>,
}

impl SqlTableRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    /// Whether to re-probe for newly appeared cold data for `topic`.
    /// Once the cold table exists there is nothing left to probe — the live
    /// provider refreshes itself.
    fn should_probe_cold(&self, topic: &str, interval_ms: u64) -> bool {
        let now = Self::now_ms();
        // Read the guard in its own statement: holding a DashMap `Ref` across
        // an `insert` on the same shard deadlocks.
        let last = self.last_cold_probe_ms.get(topic).map(|v| *v);
        if last.is_some_and(|last| now.saturating_sub(last) < interval_ms) {
            return false;
        }
        self.last_cold_probe_ms.insert(topic.to_string(), now);
        true
    }
}

/// How often to check whether a hot-only topic has gained Parquet segments.
fn cold_probe_interval_ms() -> u64 {
    std::env::var("CHRONIK_SQL_COLD_PROBE_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1_000)
}

/// SQL Handler (for direct usage without HTTP)
pub struct SqlHandler;

impl SqlHandler {
    /// v2.2.23: Ensure topics are registered as SQL tables with hot/cold union
    ///
    /// This function lists all topics from the metadata store and registers:
    /// - `{topic}_cold`: Parquet files (historical data)
    /// - `{topic}_hot`: Hot buffer from WAL (recent data, sub-second latency)
    /// - `{topic}`: Union view of hot + cold for seamless queries
    ///
    /// Table names are sanitized (replacing - and . with _) for SQL compatibility.
    ///
    /// v2.10.7 (issue #19): every registration is **live**. `{topic}_hot` reads
    /// the WAL at scan time and `{topic}_cold` re-lists its Parquet segments at
    /// scan time, so a table registered once keeps returning the whole topic as
    /// it grows. Before this, both were point-in-time snapshots pinned by the
    /// first query after startup, which silently froze `COUNT(*)` (and every
    /// other scan) at whatever the topic held at that instant.
    pub async fn ensure_topics_registered(
        state: &UnifiedApiState,
        engine: &chronik_columnar::ColumnarQueryEngine,
    ) {
        // Get list of already registered tables
        let registered = engine.list_tables().unwrap_or_default();
        let registered_set: HashSet<_> = registered.into_iter().collect();

        // List all topics from metadata store
        let topics = match state.metadata_store.list_topics().await {
            Ok(topics) => topics,
            Err(e) => {
                warn!("Failed to list topics for SQL registration: {}", e);
                return;
            }
        };

        let probe_interval = cold_probe_interval_ms();

        // For each topic, register hot and cold tables
        for topic_meta in topics {
            let topic = &topic_meta.name;
            let base_table_name = Self::sanitize_table_name(topic);
            let cold_table_name = format!("{}_cold", base_table_name);
            let hot_table_name = format!("{}_hot", base_table_name);

            // ============================================================
            // Register COLD table (live Parquet listing)
            // ============================================================
            let mut has_cold = registered_set.contains(&cold_table_name);

            // Only probe while the topic has no cold table yet: afterwards the
            // provider keeps itself current and this scan would be wasted work.
            if !has_cold && state.sql_tables.should_probe_cold(topic, probe_interval) {
                let paths =
                    resolve_parquet_paths(state.metadata_store.as_ref(), topic).await;

                if let Some(sample) = paths.first() {
                    let source = std::sync::Arc::new(TopicParquetSource {
                        topic: topic.clone(),
                        metadata_store: state.metadata_store.clone(),
                        ownership: query_ownership(state),
                    });

                    match engine
                        .register_live_parquet_table(
                            &cold_table_name,
                            source,
                            sample,
                            chronik_columnar::DEFAULT_PARQUET_REFRESH_MS,
                        )
                        .await
                    {
                        Ok(()) => {
                            info!(
                                topic = %topic,
                                table_name = %cold_table_name,
                                num_files = paths.len(),
                                "Registered cold (Parquet) table"
                            );
                            has_cold = true;
                        }
                        Err(e) => {
                            warn!("Failed to register cold table '{}': {}", cold_table_name, e)
                        }
                    }
                }
            }

            // ============================================================
            // Register HOT table (live view of the WAL hot buffer)
            // ============================================================
            let mut has_hot = registered_set.contains(&hot_table_name);

            if !has_hot {
                if let Some(hot_buffer) = &state.hot_buffer {
                    if hot_buffer.is_enabled() {
                        // The hot table needs the same ownership rule as the cold
                        // one. Without it, every replica would serve every
                        // partition's un-flushed records and the fan-out would
                        // count them once per replica.
                        let provider = std::sync::Arc::new(match query_ownership(state) {
                            Some(ownership) => chronik_columnar::LiveHotTableProvider::with_ownership(
                                hot_buffer.clone(),
                                topic.clone(),
                                ownership,
                            ),
                            None => chronik_columnar::LiveHotTableProvider::new(
                                hot_buffer.clone(),
                                topic.clone(),
                            ),
                        });
                        match engine.register_table_provider(&hot_table_name, provider) {
                            Ok(()) => {
                                info!(
                                    topic = %topic,
                                    table_name = %hot_table_name,
                                    "Registered hot (WAL) table"
                                );
                                has_hot = true;
                            }
                            Err(e) => {
                                debug!("Failed to register hot table '{}': {}", hot_table_name, e)
                            }
                        }
                    }
                }
            }

            // ============================================================
            // Create unified VIEW (hot UNION ALL cold)
            // ============================================================
            let composition = ViewComposition { has_hot, has_cold };
            let current = state.sql_tables.topics.get(topic).map(|c| *c);
            let view_exists = registered_set.contains(&base_table_name)
                && current == Some(composition);

            if !view_exists && (has_hot || has_cold) {
                // v2.2.23: Use explicit columns for UNION to handle schema differences
                // Hot buffer (MemTable) and cold (Parquet) may have different schemas:
                // - Hot: Utf8, Binary types without _headers
                // - Cold: Utf8View, BinaryView types with _headers
                // We select only the common core columns to ensure UNION compatibility
                //
                // Issue #19: the string/binary columns are additionally CAST to a
                // single canonical type on *both* sides. Letting UNION coerce
                // Binary vs BinaryView itself trips DataFusion 44's
                // `optimize_projections` rule ("No field named {table}._value"),
                // which fails every query that touches `_value` or `_key` on the
                // unified view — including `SELECT _value FROM {topic}` and any
                // json_extract_*() aggregation. Identical casts on both sides make
                // the union inputs schema-identical, so no coercion is inserted.
                let common_cols = "CAST(_topic AS VARCHAR) AS _topic, _partition, _offset, \
                                   _timestamp, _timestamp_type, CAST(_key AS BYTEA) AS _key, \
                                   CAST(_value AS BYTEA) AS _value";

                let view_sql = if has_hot && has_cold {
                    // Both hot and cold available - union them with explicit columns
                    // Hot data has priority (more recent), cold provides historical
                    format!(
                        "SELECT {} FROM {} UNION ALL SELECT {} FROM {}",
                        common_cols, hot_table_name, common_cols, cold_table_name
                    )
                } else if has_hot {
                    // Only hot data
                    format!("SELECT {} FROM {}", common_cols, hot_table_name)
                } else {
                    // Only cold data
                    format!("SELECT {} FROM {}", common_cols, cold_table_name)
                };

                if let Err(e) = engine.register_view(&base_table_name, &view_sql).await {
                    warn!(
                        "Failed to register unified view '{}': {} (falling back to a single source)",
                        base_table_name, e
                    );
                    // Backward compatibility: the base name must still resolve
                    // to something queryable. Bind it to whichever single
                    // source exists (hot wins — it is always registrable).
                    Self::register_single_source_fallback(
                        state,
                        engine,
                        topic,
                        &base_table_name,
                        &cold_table_name,
                        has_hot,
                        has_cold,
                    )
                    .await;
                } else {
                    state
                        .sql_tables
                        .topics
                        .insert(topic.clone(), composition);
                    info!(
                        topic = %topic,
                        view_name = %base_table_name,
                        has_hot = has_hot,
                        has_cold = has_cold,
                        "Registered unified hot/cold view"
                    );
                }
            }
        }
    }

    /// Bind `base_table_name` directly to a single live source.
    ///
    /// Only reached when the hot ∪ cold view cannot be created (e.g. the two
    /// schemas refuse to unify). Registering a second live provider under the
    /// base name keeps `SELECT ... FROM {topic}` working — and keeps it live —
    /// at the cost of covering one tier instead of both.
    async fn register_single_source_fallback(
        state: &UnifiedApiState,
        engine: &chronik_columnar::ColumnarQueryEngine,
        topic: &str,
        base_table_name: &str,
        cold_table_name: &str,
        has_hot: bool,
        has_cold: bool,
    ) {
        if has_hot {
            if let Some(hot_buffer) = &state.hot_buffer {
                let provider = std::sync::Arc::new(chronik_columnar::LiveHotTableProvider::new(
                    hot_buffer.clone(),
                    topic.to_string(),
                ));
                if let Err(e) = engine.register_table_provider(base_table_name, provider) {
                    warn!("Fallback registration of '{}' failed: {}", base_table_name, e);
                }
            }
            return;
        }

        if has_cold {
            let paths = resolve_parquet_paths(state.metadata_store.as_ref(), topic).await;
            if let Some(sample) = paths.first() {
                let source = std::sync::Arc::new(TopicParquetSource {
                    topic: topic.to_string(),
                    metadata_store: state.metadata_store.clone(),
                    ownership: query_ownership(state),
                });
                if let Err(e) = engine
                    .register_live_parquet_table(
                        base_table_name,
                        source,
                        sample,
                        chronik_columnar::DEFAULT_PARQUET_REFRESH_MS,
                    )
                    .await
                {
                    warn!(
                        "Fallback registration of '{}' from '{}' failed: {}",
                        base_table_name, cold_table_name, e
                    );
                }
            }
        }
    }

    /// Sanitize topic name to be a valid SQL table name
    pub fn sanitize_table_name(topic: &str) -> String {
        topic
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    }

    /// Execute a SQL query directly
    pub async fn execute(
        state: &UnifiedApiState,
        query: &str,
        limit: usize,
    ) -> Result<SqlResponse, String> {
        let start = std::time::Instant::now();

        let engine = state
            .query_engine
            .as_ref()
            .ok_or("SQL query engine not available")?;

        // v2.2.22: Dynamically register topics with Parquet data before query execution
        // This ensures all columnar-enabled topics are available as SQL tables
        Self::ensure_topics_registered(state, engine).await;

        // Execute query
        let batches = engine
            .execute_sql(query)
            .await
            .map_err(|e| format!("Query execution failed: {}", e))?;

        // Convert to response format
        let mut columns: Vec<String> = Vec::new();
        let mut rows: Vec<HashMap<String, serde_json::Value>> = Vec::new();
        let mut truncated = false;

        for batch in batches {
            // Get column names from first batch
            if columns.is_empty() {
                columns = batch
                    .schema()
                    .fields()
                    .iter()
                    .map(|f| f.name().clone())
                    .collect();
            }

            // Convert rows
            let num_rows = batch.num_rows();
            for row_idx in 0..num_rows {
                if rows.len() >= limit {
                    truncated = true;
                    break;
                }

                let mut row: HashMap<String, serde_json::Value> = HashMap::new();
                for (col_idx, col_name) in columns.iter().enumerate() {
                    let column = batch.column(col_idx);
                    let value = arrow_value_to_json(column, row_idx);
                    row.insert(col_name.clone(), value);
                }
                rows.push(row);
            }

            if truncated {
                break;
            }
        }

        let row_count = rows.len();
        let execution_time_ms = start.elapsed().as_millis() as u64;

        Ok(SqlResponse {
            columns,
            rows,
            row_count,
            execution_time_ms,
            truncated,
        })
    }
}

/// Execute SQL query endpoint
pub async fn execute_sql(
    State(state): State<UnifiedApiState>,
    headers: HeaderMap,
    Json(request): Json<SqlRequest>,
) -> impl IntoResponse {
    let row_limit = request.effective_limit();
    // Peers must apply the same cap we resolved, not re-derive it.
    let fan_out_request = SqlRequest {
        limit: Some(row_limit),
        ..request.clone()
    };
    info!(query = %request.query, limit = row_limit, "Executing SQL query");

    // Check if SQL engine is available before executing
    if state.query_engine.is_none() {
        let error_response = SqlErrorResponse {
            error: "SQL query engine not available".to_string(),
            error_type: "ServiceUnavailable".to_string(),
        };
        return (StatusCode::SERVICE_UNAVAILABLE, Json(error_response)).into_response();
    }

    match SqlHandler::execute(&state, &request.query, row_limit).await {
        Ok(response) => {
            // Distributed fan-out: merge SQL results from all peer nodes
            let response = if let Some(ref router) = state.query_router {
                if !super::query_router::is_forwarded_request(&headers) {
                    // Skip the fan-out only when this node LEADS every partition.
                    //
                    // This used to skip when the node was a *replica* of every
                    // partition (`all_topics_local`), which at RF=node_count is
                    // always true — so on a full-replication cluster the query
                    // never fanned out and returned only what this node happened
                    // to see locally. That is #22: partial results, and different
                    // ones depending on which node answered, because the cold
                    // table is built from segment metadata registered by each
                    // partition's leader and filtered by local path existence.
                    //
                    // Leadership is the right condition on both sides: if this
                    // node leads everything there is nothing to merge, and if it
                    // does not, the providers restrict each node to its own led
                    // partitions so the union covers every partition exactly once
                    // — no gaps, no double-counting.
                    // Refresh unconditionally, not just when the map is empty.
                    //
                    // `has_partition_map()` is true as soon as ONE topic is
                    // mapped, so the old guard meant a topic created later was
                    // never mapped — and an unmapped topic is invisible to the
                    // leadership check, which would then happily skip the fan-out
                    // and miss that topic's data entirely. These are in-memory
                    // metadata reads and this is a query path, not the produce
                    // path.
                    router.refresh_all_partition_maps(state.metadata_store.as_ref()).await;
                    if router.leads_all_partitions().await {
                        debug!("This node leads every partition, skipping SQL fan-out");
                        response
                    } else {
                        let all_peers = router.all_peers();
                        if !all_peers.is_empty() {
                            // How the results combine depends on the query, and
                            // getting it wrong is silent. Decide before paying
                            // for the fan-out so an unmergeable query fails fast
                            // instead of returning a plausible wrong answer.
                            match super::query_router::sql_merge_strategy(&request.query) {
                                super::query_router::SqlMerge::Unsupported(why) => {
                                    warn!(query = %request.query, reason = why,
                                          "Refusing to merge a distributed SQL result");
                                    let error_response = SqlErrorResponse {
                                        error: format!(
                                            "This query cannot be answered across a cluster: {}",
                                            why
                                        ),
                                        error_type: "DistributedQueryUnsupported".to_string(),
                                    };
                                    return (StatusCode::BAD_REQUEST, Json(error_response))
                                        .into_response();
                                }
                                strategy => {
                                    let peers: Vec<SqlResponse> = router
                                        .fan_out_post("/_sql", &fan_out_request, &all_peers)
                                        .await;
                                    debug!(peer_count = peers.len(), "Merging SQL results from peers");
                                    match strategy {
                                        super::query_router::SqlMerge::ScalarAggregate(how) => {
                                            super::query_router::merge_scalar_aggregate(
                                                response, peers, &how,
                                            )
                                        }
                                        _ => super::query_router::merge_sql_responses(
                                            response, peers, row_limit,
                                        ),
                                    }
                                }
                            }
                        } else {
                            response
                        }
                    }
                } else {
                    response
                }
            } else {
                response
            };

            info!(
                rows = response.row_count,
                time_ms = response.execution_time_ms,
                "SQL query completed"
            );
            (StatusCode::OK, Json(response)).into_response()
        }
        Err(e) => {
            error!(error = %e, "SQL query failed");
            let error_response = SqlErrorResponse {
                error: e,
                error_type: "QueryError".to_string(),
            };
            (StatusCode::BAD_REQUEST, Json(error_response)).into_response()
        }
    }
}

/// Explain query request
#[derive(Debug, Deserialize)]
pub struct ExplainRequest {
    /// SQL query to explain
    pub query: String,
    /// Include physical plan
    #[serde(default)]
    pub physical: bool,
}

/// Explain query response
#[derive(Debug, Serialize)]
pub struct ExplainResponse {
    /// Logical query plan
    pub logical_plan: String,
    /// Physical query plan (if requested)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub physical_plan: Option<String>,
}

/// Explain SQL query endpoint
pub async fn explain_sql(
    State(state): State<UnifiedApiState>,
    Json(request): Json<ExplainRequest>,
) -> impl IntoResponse {
    info!(query = %request.query, "Explaining SQL query");

    let engine = match &state.query_engine {
        Some(e) => e,
        None => {
            let error_response = SqlErrorResponse {
                error: "SQL query engine not available".to_string(),
                error_type: "ServiceUnavailable".to_string(),
            };
            return (StatusCode::SERVICE_UNAVAILABLE, Json(error_response)).into_response();
        }
    };

    match engine.explain(&request.query).await {
        Ok(plan) => {
            let response = ExplainResponse {
                logical_plan: plan,
                physical_plan: None, // TODO: Add physical plan support
            };
            (StatusCode::OK, Json(response)).into_response()
        }
        Err(e) => {
            error!(error = %e, "Explain failed");
            let error_response = SqlErrorResponse {
                error: e.to_string(),
                error_type: "ExplainError".to_string(),
            };
            (StatusCode::BAD_REQUEST, Json(error_response)).into_response()
        }
    }
}

/// List tables response
#[derive(Debug, Serialize)]
pub struct ListTablesResponse {
    pub tables: Vec<TableInfo>,
}

/// Table information
#[derive(Debug, Serialize)]
pub struct TableInfo {
    pub name: String,
    pub table_type: String,
}

/// List available tables (topics)
pub async fn list_tables(State(state): State<UnifiedApiState>) -> impl IntoResponse {
    debug!("Listing SQL tables");

    let engine = match &state.query_engine {
        Some(e) => e,
        None => {
            let error_response = SqlErrorResponse {
                error: "SQL query engine not available".to_string(),
                error_type: "ServiceUnavailable".to_string(),
            };
            return (StatusCode::SERVICE_UNAVAILABLE, Json(error_response)).into_response();
        }
    };

    let tables = engine.list_registered_topics().await;

    let response = ListTablesResponse {
        tables: tables
            .into_iter()
            .map(|name| TableInfo {
                name,
                table_type: "TOPIC".to_string(),
            })
            .collect(),
    };

    (StatusCode::OK, Json(response)).into_response()
}

/// Describe table response
#[derive(Debug, Serialize)]
pub struct DescribeTableResponse {
    pub table: String,
    pub columns: Vec<ColumnInfo>,
}

/// Column information
#[derive(Debug, Serialize)]
pub struct ColumnInfo {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
}

/// Describe a table's schema
pub async fn describe_table(
    State(state): State<UnifiedApiState>,
    Path(table): Path<String>,
) -> impl IntoResponse {
    debug!(table = %table, "Describing table");

    let engine = match &state.query_engine {
        Some(e) => e,
        None => {
            let error_response = SqlErrorResponse {
                error: "SQL query engine not available".to_string(),
                error_type: "ServiceUnavailable".to_string(),
            };
            return (StatusCode::SERVICE_UNAVAILABLE, Json(error_response)).into_response();
        }
    };

    match engine.get_table_schema(&table).await {
        Ok(schema) => {
            let columns: Vec<ColumnInfo> = schema
                .fields()
                .iter()
                .map(|f| ColumnInfo {
                    name: f.name().clone(),
                    data_type: format!("{:?}", f.data_type()),
                    nullable: f.is_nullable(),
                })
                .collect();

            let response = DescribeTableResponse {
                table,
                columns,
            };
            (StatusCode::OK, Json(response)).into_response()
        }
        Err(e) => {
            error!(table = %table, error = %e, "Describe table failed");
            let error_response = SqlErrorResponse {
                error: e.to_string(),
                error_type: "TableNotFound".to_string(),
            };
            (StatusCode::NOT_FOUND, Json(error_response)).into_response()
        }
    }
}

/// Recursively discover all `.parquet` files under a directory.
///
/// Used as a fallback when the metadata store hasn't recorded parquet paths yet
/// (e.g., during early indexing before WalIndexer persists metadata).
fn discover_parquet_files(dir: &std::path::Path) -> std::io::Result<Vec<String>> {
    let mut files = Vec::new();
    if dir.is_dir() {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                files.extend(discover_parquet_files(&path)?);
            } else if path.extension().map(|e| e == "parquet").unwrap_or(false) {
                files.push(path.to_string_lossy().to_string());
            }
        }
    }
    Ok(files)
}

/// Convert an Arrow array value at a given index to JSON
fn arrow_value_to_json(
    column: &chronik_columnar::datafusion::arrow::array::ArrayRef,
    row_idx: usize,
) -> serde_json::Value {
    use chronik_columnar::datafusion::arrow::array::*;

    if column.is_null(row_idx) {
        return serde_json::Value::Null;
    }

    // Handle different array types
    if let Some(arr) = column.as_any().downcast_ref::<Int64Array>() {
        return serde_json::Value::Number(arr.value(row_idx).into());
    }
    if let Some(arr) = column.as_any().downcast_ref::<Int32Array>() {
        return serde_json::Value::Number(arr.value(row_idx).into());
    }
    if let Some(arr) = column.as_any().downcast_ref::<Float64Array>() {
        return serde_json::json!(arr.value(row_idx));
    }
    if let Some(arr) = column.as_any().downcast_ref::<Float32Array>() {
        return serde_json::json!(arr.value(row_idx) as f64);
    }
    if let Some(arr) = column.as_any().downcast_ref::<StringArray>() {
        return serde_json::Value::String(arr.value(row_idx).to_string());
    }
    if let Some(arr) = column.as_any().downcast_ref::<BooleanArray>() {
        return serde_json::Value::Bool(arr.value(row_idx));
    }
    if let Some(arr) = column.as_any().downcast_ref::<BinaryArray>() {
        // Return binary as base64
        return serde_json::Value::String(base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            arr.value(row_idx),
        ));
    }
    if let Some(arr) = column.as_any().downcast_ref::<BinaryViewArray>() {
        return serde_json::Value::String(base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            arr.value(row_idx),
        ));
    }
    if let Some(arr) = column.as_any().downcast_ref::<LargeBinaryArray>() {
        return serde_json::Value::String(base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            arr.value(row_idx),
        ));
    }
    if let Some(arr) = column.as_any().downcast_ref::<StringViewArray>() {
        return serde_json::Value::String(arr.value(row_idx).to_string());
    }
    if let Some(arr) = column.as_any().downcast_ref::<LargeStringArray>() {
        return serde_json::Value::String(arr.value(row_idx).to_string());
    }
    if let Some(arr) = column.as_any().downcast_ref::<Int8Array>() {
        return serde_json::Value::Number(arr.value(row_idx).into());
    }
    if let Some(arr) = column.as_any().downcast_ref::<Int16Array>() {
        return serde_json::Value::Number(arr.value(row_idx).into());
    }
    if let Some(arr) = column.as_any().downcast_ref::<UInt32Array>() {
        return serde_json::Value::Number(arr.value(row_idx).into());
    }
    if let Some(arr) = column.as_any().downcast_ref::<UInt64Array>() {
        return serde_json::Value::Number(arr.value(row_idx).into());
    }
    if let Some(arr) = column.as_any().downcast_ref::<TimestampMillisecondArray>() {
        return serde_json::Value::Number(arr.value(row_idx).into());
    }

    // Fallback: try to use Arrow's display format for the specific row
    use chronik_columnar::datafusion::arrow::util::display::ArrayFormatter;
    if let Ok(formatter) = ArrayFormatter::try_new(column.as_ref(), &Default::default()) {
        return serde_json::Value::String(formatter.value(row_idx).to_string());
    }
    serde_json::Value::String(format!("unsupported_type:{}", column.data_type()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_request(json: &str) -> SqlRequest {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn test_sql_request_defaults() {
        let request = parse_request(r#"{"query": "SELECT * FROM test"}"#);
        assert_eq!(request.query, "SELECT * FROM test");
        assert_eq!(request.limit, None);
        assert_eq!(request.effective_limit(), DEFAULT_ROW_LIMIT);
        assert_eq!(request.timeout_secs, 30);
    }

    #[test]
    fn test_sql_request_custom_limit() {
        let request = parse_request(r#"{"query": "SELECT * FROM test", "limit": 100}"#);
        assert_eq!(request.limit, Some(100));
        assert_eq!(request.effective_limit(), 100);
    }

    /// Issue #19: `LIMIT 5000` returned exactly 1000 rows.
    #[test]
    fn test_query_limit_is_honoured_when_request_has_none() {
        let request = parse_request(r#"{"query": "SELECT * FROM test LIMIT 5000"}"#);
        assert_eq!(request.effective_limit(), 5000);

        let request = parse_request(r#"{"query": "SELECT * FROM test limit 999"}"#);
        assert_eq!(request.effective_limit(), 999);
    }

    #[test]
    fn test_explicit_request_limit_beats_query_limit() {
        let request = parse_request(r#"{"query": "SELECT * FROM t LIMIT 5000", "limit": 10}"#);
        assert_eq!(request.effective_limit(), 10);
    }

    #[test]
    fn test_query_limit_is_capped_and_falls_back() {
        // Beyond the engine's own ceiling.
        let request = parse_request(r#"{"query": "SELECT * FROM t LIMIT 999999999"}"#);
        assert_eq!(request.effective_limit(), MAX_ROW_LIMIT);

        // An explicit request limit is capped too — the engine stops at
        // max_rows anyway, and a higher cap would mis-report `truncated`.
        let request = parse_request(r#"{"query": "SELECT * FROM t", "limit": 500000}"#);
        assert_eq!(request.effective_limit(), MAX_ROW_LIMIT);

        // Aggregates carry no LIMIT — default applies, and it never truncates a
        // one-row result anyway.
        let request = parse_request(r#"{"query": "SELECT COUNT(*) FROM t"}"#);
        assert_eq!(request.effective_limit(), DEFAULT_ROW_LIMIT);

        // Unparseable input must not panic or change behaviour.
        let request = parse_request(r#"{"query": "NOT SQL AT ALL"}"#);
        assert_eq!(request.effective_limit(), DEFAULT_ROW_LIMIT);
    }

    #[test]
    fn test_cold_probe_is_throttled() {
        let registry = SqlTableRegistry::new();
        assert!(registry.should_probe_cold("t", 60_000), "first probe runs");
        assert!(
            !registry.should_probe_cold("t", 60_000),
            "second probe inside the interval is skipped"
        );
        assert!(
            registry.should_probe_cold("t", 0),
            "a zero interval always re-probes"
        );
        assert!(
            registry.should_probe_cold("other", 60_000),
            "throttling is per-topic"
        );
    }
}

#[cfg(test)]
mod partition_ownership_tests {
    use super::*;

    fn owned(ids: &[i32]) -> HashSet<i32> {
        ids.iter().copied().collect()
    }

    #[test]
    fn partition_is_parsed_from_the_hive_style_path() {
        assert_eq!(
            partition_of_parquet_path("/data/columnar/orders/partition=3/seg-0.parquet"),
            Some(3)
        );
        assert_eq!(
            partition_of_parquet_path("s3://bucket/columnar/orders/partition=11/seg.parquet"),
            Some(11)
        );
        assert_eq!(partition_of_parquet_path("/data/columnar/orders/seg.parquet"), None);
        assert_eq!(
            partition_of_parquet_path("/data/columnar/orders/partition=abc/seg.parquet"),
            None
        );
    }

    /// #22: a node must scan only the partitions it leads, or the fan-out counts
    /// every row once per replica.
    #[test]
    fn only_led_partitions_are_scanned() {
        let paths = vec![
            "/d/columnar/t/partition=0/a.parquet".to_string(),
            "/d/columnar/t/partition=1/b.parquet".to_string(),
            "/d/columnar/t/partition=2/c.parquet".to_string(),
        ];

        let kept = retain_owned_partitions(paths, &owned(&[0, 2]));
        assert_eq!(
            kept,
            vec![
                "/d/columnar/t/partition=0/a.parquet".to_string(),
                "/d/columnar/t/partition=2/c.parquet".to_string(),
            ]
        );
    }

    /// Leading nothing means serving nothing — the peers that lead those
    /// partitions answer for them. Returning rows here would double-count.
    #[test]
    fn leading_no_partitions_serves_nothing() {
        let paths = vec!["/d/columnar/t/partition=0/a.parquet".to_string()];
        assert!(retain_owned_partitions(paths, &owned(&[])).is_empty());
    }

    /// An unparseable path is kept, deliberately.
    ///
    /// Dropping it would silently lose data if the layout ever changes; keeping
    /// it surfaces as duplicate rows, which is loud. Given a choice between two
    /// wrong behaviours, prefer the one an operator will notice.
    #[test]
    fn a_path_without_a_partition_component_is_kept_not_dropped() {
        let paths = vec![
            "/d/columnar/t/legacy.parquet".to_string(),
            "/d/columnar/t/partition=5/x.parquet".to_string(),
        ];
        let kept = retain_owned_partitions(paths, &owned(&[0]));
        assert_eq!(kept, vec!["/d/columnar/t/legacy.parquet".to_string()]);
    }
}
