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
    /// Accept an answer that is missing the nodes which did not reply.
    ///
    /// Off by default, because a merged aggregate cannot tell a node that
    /// returned nothing from a node that never answered: the result comes back
    /// short rather than missing (issue #57). Set it when a node is knowingly
    /// down and a short answer is still worth having — the response then carries
    /// `partial: true` and the nodes it is missing, so the shortfall is on the
    /// answer rather than in a log line nobody reads. `CHRONIK_SQL_ALLOW_PARTIAL`
    /// sets the default for a cluster that runs degraded on purpose.
    #[serde(default)]
    pub allow_partial: Option<bool>,
}

/// Whether this request accepts an answer missing the nodes that did not reply.
///
/// The request decides; `CHRONIK_SQL_ALLOW_PARTIAL=true` sets the default for a
/// cluster that runs a node down on purpose, so its dashboards keep working
/// without every caller being changed. Either way the answer is labelled
/// `partial`, so allowing it is not the same as hiding it.
pub fn allow_partial(request: &SqlRequest) -> bool {
    if let Some(explicit) = request.allow_partial {
        return explicit;
    }
    std::env::var("CHRONIK_SQL_ALLOW_PARTIAL")
        .map(|v| {
            let v = v.trim().to_ascii_lowercase();
            v == "true" || v == "1" || v == "yes"
        })
        .unwrap_or(false)
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

/// Base table names referenced by a query's FROM/JOIN clauses (including one
/// level of derived subqueries and set operations), lowercased with the
/// internal `_hot`/`_cold` suffixes stripped so each maps back to a topic's
/// sanitized name.
///
/// Returns `None` when the SQL cannot be parsed — the caller then keeps the
/// existing behaviour rather than guessing at which topics are involved.
fn referenced_base_tables(sql: &str) -> Option<Vec<String>> {
    use chronik_columnar::datafusion::sql::parser::{DFParser, Statement};
    use chronik_columnar::datafusion::sql::sqlparser::ast::{
        Query, Select, SetExpr, Statement as AstStatement, TableFactor,
    };

    fn walk_query(query: &Query, out: &mut Vec<String>) {
        walk_setexpr(query.body.as_ref(), out);
    }
    fn walk_setexpr(body: &SetExpr, out: &mut Vec<String>) {
        match body {
            SetExpr::Select(select) => walk_select(select, out),
            SetExpr::Query(q) => walk_query(q, out),
            SetExpr::SetOperation { left, right, .. } => {
                walk_setexpr(left, out);
                walk_setexpr(right, out);
            }
            _ => {}
        }
    }
    fn walk_select(select: &Select, out: &mut Vec<String>) {
        for twj in &select.from {
            walk_table_factor(&twj.relation, out);
            for join in &twj.joins {
                walk_table_factor(&join.relation, out);
            }
        }
    }
    fn walk_table_factor(tf: &TableFactor, out: &mut Vec<String>) {
        match tf {
            TableFactor::Table { name, .. } => {
                if let Some(ident) = name.0.last() {
                    out.push(ident.value.clone());
                }
            }
            TableFactor::Derived { subquery, .. } => walk_query(subquery, out),
            _ => {}
        }
    }

    let statements = DFParser::parse_sql(sql).ok()?;
    let statement = statements.front()?;
    let Statement::Statement(ast) = statement else {
        return None;
    };
    let AstStatement::Query(query) = ast.as_ref() else {
        return None;
    };

    let mut raw = Vec::new();
    walk_query(query, &mut raw);

    Some(
        raw.into_iter()
            .map(|t| {
                let lower = t.to_lowercase();
                lower
                    .strip_suffix("_hot")
                    .or_else(|| lower.strip_suffix("_cold"))
                    .unwrap_or(&lower)
                    .to_string()
            })
            .collect(),
    )
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
    /// Whether this answer is missing one or more nodes' data.
    ///
    /// Distinct from `truncated`, which means "there was more and we cut it".
    /// `partial` means "some of the cluster never answered, so a total here is
    /// lower than the truth". Only ever true when the caller asked for a partial
    /// answer with `allow_partial`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub partial: bool,
    /// The nodes missing from a `partial` answer, as `node N: reason`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub incomplete_nodes: Vec<String>,
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
        let data_dir = std::env::var("CHRONIK_DATA_DIR").unwrap_or_else(|_| "./data".to_string());
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
/// the first Parquet segment appears for a topic that started hot-only), or
/// when its cold segments start overlapping (`cold_dedup`), which flips the
/// view from a plain UNION to a de-duplicating one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct ViewComposition {
    has_hot: bool,
    has_cold: bool,
    /// The same `(_partition, _offset)` can appear more than once — either
    /// because cold Parquet segments overlap ([`cold_segments_overlap`]) or
    /// because the hot buffer re-serves offsets already in cold after a restart
    /// ([`hot_cold_overlap`]) — so a plain scan over-counts. When set, the view
    /// keeps one row per `(_partition, _offset)`.
    cold_dedup: bool,
    /// Fingerprint of the JSON columns the cold table exposes beyond the core
    /// envelope ([`cold_json_columns`]). The view names them one by one, so a
    /// topic whose payloads gain a field needs its view rebuilt — otherwise the
    /// new column stays invisible under the topic's own name until a restart.
    /// A fingerprint rather than the names themselves, to keep this `Copy`.
    json_cols: u64,
}

/// A JSON column's type, as both sides of the union have to spell it.
///
/// The inferred schema only ever produces these four: nested objects and arrays
/// are stored as their serialized JSON text
/// (`chronik-columnar/src/json_schema.rs`), so they arrive here as `Text`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JsonColKind {
    Text,
    Int,
    Float,
    Bool,
}

impl JsonColKind {
    fn from_arrow(dt: &datafusion::arrow::datatypes::DataType) -> Option<Self> {
        use datafusion::arrow::datatypes::DataType as D;
        match dt {
            D::Utf8 | D::LargeUtf8 | D::Utf8View => Some(Self::Text),
            D::Int8 | D::Int16 | D::Int32 | D::Int64 => Some(Self::Int),
            D::UInt8 | D::UInt16 | D::UInt32 => Some(Self::Int),
            D::Float16 | D::Float32 | D::Float64 => Some(Self::Float),
            D::Boolean => Some(Self::Bool),
            // Anything else (lists, structs, decimals, timestamps) is not
            // something the JSON inference produces, so it is not a message
            // field and is left out rather than guessed at.
            _ => None,
        }
    }

    /// The canonical SQL type. Both union inputs are cast to it, for the reason
    /// in the `common_cols` comment below: letting DataFusion coerce the two
    /// sides itself breaks every query touching the column.
    fn sql_type(self) -> &'static str {
        match self {
            Self::Text => "VARCHAR",
            Self::Int => "BIGINT",
            Self::Float => "DOUBLE",
            Self::Bool => "BOOLEAN",
        }
    }

    /// How the hot side produces the column. The hot buffer's schema is fixed
    /// and carries only the envelope, so the value is extracted from `_value`
    /// with the same UDFs the hot-buffer docs point callers at. Projecting NULL
    /// instead would be cheaper and wrong: hot holds exactly the rows cold does
    /// not, so every aggregate over recent data would come back quietly short.
    fn extract_from_value(self, path_literal: &str) -> String {
        let f = match self {
            Self::Text | Self::Bool => "json_extract_string",
            Self::Int => "json_extract_int",
            Self::Float => "json_extract_float",
        };
        format!("{f}(_value, '$.{path_literal}')")
    }
}

/// The JSON columns a cold Parquet table carries beyond the core envelope.
///
/// The WalIndexer infers a schema from the message values and writes one typed
/// column per top-level JSON field. Those columns exist only on the cold side,
/// so unless the unified view names them they are unreachable under the topic's
/// own name — `SELECT sev FROM orders` fails while `orders_cold` has `sev`.
///
/// Reserved `_`-prefixed names are skipped: `_headers` belongs to the cold
/// envelope rather than the message body, and the core seven are projected
/// separately. A payload field actually named `_x` would already be
/// indistinguishable from an envelope column, so losing it here changes nothing.
fn cold_json_columns(
    schema: &datafusion::arrow::datatypes::Schema,
) -> Vec<(String, JsonColKind)> {
    schema
        .fields()
        .iter()
        .filter(|f| !f.name().starts_with('_'))
        .filter_map(|f| JsonColKind::from_arrow(f.data_type()).map(|k| (f.name().clone(), k)))
        .collect()
}

/// Order-independent fingerprint of a column set, for [`ViewComposition`].
fn json_cols_fingerprint(cols: &[(String, JsonColKind)]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut total: u64 = 0;
    for (name, kind) in cols {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        name.hash(&mut h);
        (*kind as u8).hash(&mut h);
        // Sum, so the fingerprint does not depend on the field order a given
        // Parquet file happens to use.
        total = total.wrapping_add(h.finish());
    }
    total
}

/// Double a character to escape it, for SQL identifiers (`"`) and string
/// literals (`'`). A JSON key may contain either, and a key that is simply
/// dropped or spliced in raw would be a wrong answer or broken SQL.
fn sql_escape(s: &str, quote: char) -> String {
    s.replace(quote, &format!("{quote}{quote}"))
}

/// Whether a topic's cold Parquet segments overlap in offset range within any
/// single partition.
///
/// The WalIndexer is meant to flush each offset to cold exactly once, but a
/// process that re-indexes WAL segments it has already flushed (e.g. after a
/// restart, when the in-memory "already indexed" guard is empty, or after a
/// follower re-pulls a partition from the start) writes a *new* Parquet segment
/// covering offsets an older segment already holds. Both are then scanned, so
/// every offset in the overlap is counted once per copy — the duplicate-row
/// over-count seen on long-lived, restart-heavy clusters (#41).
///
/// Detecting the overlap lets the view de-duplicate only where it must: a clean
/// topic (non-overlapping, sequential segments) keeps the cheap plain-UNION
/// view and pays nothing. Overlap is judged per partition from the registered
/// segment ranges (`min_offset..=max_offset`), which is all this needs —
/// whether to turn de-duplication on.
async fn cold_segments_overlap(
    metadata_store: &dyn chronik_common::metadata::traits::MetadataStore,
    topic: &str,
) -> bool {
    let segments = match metadata_store.list_parquet_segments(topic, None).await {
        Ok(segments) => segments,
        Err(_) => return false,
    };

    // Group segment offset ranges by partition.
    let mut by_partition: HashMap<i32, Vec<(i64, i64)>> = HashMap::new();
    for seg in segments {
        by_partition
            .entry(seg.partition)
            .or_default()
            .push((seg.min_offset, seg.max_offset));
    }

    // Within a partition, sort by start and look for any range that begins at
    // or before the furthest end seen so far — that is an overlap.
    for mut ranges in by_partition.into_values() {
        ranges.sort_unstable();
        let mut covered_to: i64 = i64::MIN;
        for (min, max) in ranges {
            if min <= covered_to {
                return true;
            }
            covered_to = max;
        }
    }
    false
}

/// Advance the hot buffer's per-partition `flushed_offset` to the cold
/// high-water mark, so it serves only offsets cold does not hold.
///
/// `flushed_offset` is in-memory and resets to 0 on restart; the WalIndexer
/// seeds it when the buffer is wired up, but Parquet-segment metadata may not
/// have finished recovering that early, so some partitions are missed (and a
/// static topic never re-flushes to correct them). This runs at query time,
/// after recovery is complete, as the reliable backstop. It only advances a
/// watermark, so it is safe to run repeatedly and never hides un-flushed rows.
async fn seed_hot_flushed_from_cold(state: &UnifiedApiState, topic: &str) {
    let Some(hot) = state.hot_buffer.as_ref() else {
        return;
    };
    let segments = match state
        .metadata_store
        .list_parquet_segments(topic, None)
        .await
    {
        Ok(segments) => segments,
        Err(_) => return,
    };
    let mut cold_max: HashMap<i32, i64> = HashMap::new();
    for seg in segments {
        let entry = cold_max.entry(seg.partition).or_insert(i64::MIN);
        if seg.max_offset > *entry {
            *entry = seg.max_offset;
        }
    }
    for (partition, max) in cold_max {
        if hot.get_flushed_offset(topic, partition) < max {
            hot.set_flushed_offset(topic, partition, max);
        }
    }
}

/// Whether the hot buffer would re-serve offsets that are already in cold, so a
/// plain `hot UNION ALL cold` double-counts them.
///
/// The hot buffer only serves offsets above its per-partition `flushed_offset`,
/// which the cold-flush path advances to the highest offset written to Parquet
/// — so in steady state hot and cold are disjoint. But `flushed_offset` lives
/// only in memory: after a restart it resets to 0 and the buffer (rebuilt from
/// the WAL trailing window) re-serves offsets that were already flushed to cold.
/// For a topic still being produced to, the next flush re-advances it and the
/// overlap clears; for a *static* topic nothing re-advances it, so the overlap —
/// and the doubled count — persists. Detecting it lets the view de-duplicate
/// exactly while it lasts, and go back to the cheap UNION once it clears.
async fn hot_cold_overlap(state: &UnifiedApiState, topic: &str) -> bool {
    let Some(hot) = state.hot_buffer.as_ref() else {
        return false;
    };
    let segments = match state
        .metadata_store
        .list_parquet_segments(topic, None)
        .await
    {
        Ok(segments) => segments,
        Err(_) => return false,
    };

    // Highest offset already in cold, per partition.
    let mut cold_max: HashMap<i32, i64> = HashMap::new();
    for seg in segments {
        let entry = cold_max.entry(seg.partition).or_insert(i64::MIN);
        if seg.max_offset > *entry {
            *entry = seg.max_offset;
        }
    }

    // Hot serves from `flushed_offset + 1`. If that floor is at or below cold's
    // max for any partition, the same offset can appear in both tiers.
    for (partition, cmax) in cold_max {
        if hot.get_flushed_offset(topic, partition) < cmax {
            return true;
        }
    }
    false
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
                let paths = resolve_parquet_paths(state.metadata_store.as_ref(), topic).await;

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
                            Some(ownership) => {
                                chronik_columnar::LiveHotTableProvider::with_ownership(
                                    hot_buffer.clone(),
                                    topic.clone(),
                                    ownership,
                                )
                            }
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
            // Restore the "hot serves only what cold does not" invariant before
            // anything reads the view: seed the hot buffer's flushed offset from
            // cold. After this, hot/cold no longer overlap, so the post-restart
            // double-count is gone at the source rather than papered over by the
            // dedup below.
            if has_hot && has_cold {
                seed_hot_flushed_from_cold(state, topic).await;
            }

            // A topic still needs a de-duplicating view when the same
            // (_partition, _offset) can appear more than once. With the seed
            // above, that is now only genuinely overlapping cold Parquet segments
            // — e.g. legacy data written before the indexer became idempotent —
            // and the transient window before the seed takes effect. A clean
            // topic keeps the cheap plain-UNION view.
            let cold_dedup = has_cold
                && (cold_segments_overlap(state.metadata_store.as_ref(), topic).await
                    || (has_hot && hot_cold_overlap(state, topic).await));

            // The typed JSON columns live only on the cold side. Read them off
            // the cold schema so the view can name them; without this the
            // inferred columns are written to Parquet and then dropped by the
            // one table callers actually query.
            let json_cols: Vec<(String, JsonColKind)> = if has_cold {
                match engine.get_table_schema(&cold_table_name).await {
                    Ok(schema) => cold_json_columns(&schema),
                    Err(e) => {
                        debug!(
                            topic = %topic,
                            error = %e,
                            "Could not read cold schema; unified view will carry the envelope only"
                        );
                        Vec::new()
                    }
                }
            } else {
                Vec::new()
            };
            let composition = ViewComposition {
                has_hot,
                has_cold,
                cold_dedup,
                json_cols: json_cols_fingerprint(&json_cols),
            };
            let current = state.sql_tables.topics.get(topic).map(|c| *c);
            let view_exists =
                registered_set.contains(&base_table_name) && current == Some(composition);

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
                let core_cols = "CAST(_topic AS VARCHAR) AS _topic, _partition, _offset, \
                                 _timestamp, _timestamp_type, CAST(_key AS BYTEA) AS _key, \
                                 CAST(_value AS BYTEA) AS _value";

                // The inferred JSON columns, spelled three ways: read off the
                // typed column on the cold side, extracted from `_value` on the
                // hot side, and named plainly on the way out. The casts are
                // identical on both sides for the same reason as above.
                let mut hot_cols = String::from(core_cols);
                let mut cold_cols = String::from(core_cols);
                let mut out_extra = String::new();
                for (name, kind) in &json_cols {
                    let ident = sql_escape(name, '"');
                    let literal = sql_escape(name, '\'');
                    let ty = kind.sql_type();
                    hot_cols.push_str(&format!(
                        ", CAST({} AS {ty}) AS \"{ident}\"",
                        kind.extract_from_value(&literal)
                    ));
                    cold_cols.push_str(&format!(", CAST(\"{ident}\" AS {ty}) AS \"{ident}\""));
                    out_extra.push_str(&format!(", \"{ident}\""));
                }
                let hot_cols = hot_cols.as_str();
                let cold_cols = cold_cols.as_str();

                // The output column list, shared by every view shape below.
                let out_cols = format!(
                    "_topic, _partition, _offset, _timestamp, \
                     _timestamp_type, _key, _value{out_extra}"
                );
                let out_cols = out_cols.as_str();

                let view_sql = if cold_dedup {
                    // #41: cold segments overlap, so the same (_partition, _offset)
                    // can appear more than once. Keep exactly one row per
                    // (_partition, _offset) — an offset is unique within a
                    // partition, so any repeat is a duplicate. Hot is preferred
                    // over cold (src 0 < 1) where both carry the same offset
                    // during the flush hand-off; hot itself is never duplicated.
                    let inner = if has_hot {
                        format!(
                            "SELECT {hot_cols}, 0 AS __src FROM {hot} \
                             UNION ALL \
                             SELECT {cold_cols}, 1 AS __src FROM {cold}",
                            hot_cols = hot_cols,
                            cold_cols = cold_cols,
                            hot = hot_table_name,
                            cold = cold_table_name
                        )
                    } else {
                        format!(
                            "SELECT {cols}, 1 AS __src FROM {cold}",
                            cols = cold_cols,
                            cold = cold_table_name
                        )
                    };
                    format!(
                        "SELECT {out} FROM (\
                           SELECT {out}, ROW_NUMBER() OVER (\
                             PARTITION BY _partition, _offset ORDER BY __src\
                           ) AS __rn FROM ({inner})\
                         ) WHERE __rn = 1",
                        out = out_cols,
                        inner = inner
                    )
                } else if has_hot && has_cold {
                    // Both hot and cold available - union them with explicit columns
                    // Hot data has priority (more recent), cold provides historical
                    format!(
                        "SELECT {} FROM {} UNION ALL SELECT {} FROM {}",
                        hot_cols, hot_table_name, cold_cols, cold_table_name
                    )
                } else if has_hot {
                    // Only hot data. No cold table, so no inferred JSON columns
                    // to carry: `hot_cols` is the bare envelope here.
                    format!("SELECT {} FROM {}", hot_cols, hot_table_name)
                } else {
                    // Only cold data
                    format!("SELECT {} FROM {}", cold_cols, cold_table_name)
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
                    state.sql_tables.topics.insert(topic.clone(), composition);
                    info!(
                        topic = %topic,
                        view_name = %base_table_name,
                        has_hot = has_hot,
                        has_cold = has_cold,
                        cold_dedup = cold_dedup,
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
                    warn!(
                        "Fallback registration of '{}' failed: {}",
                        base_table_name, e
                    );
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
            partial: false,
            incomplete_nodes: Vec::new(),
            truncated,
        })
    }
}

/// Does a topic's SQL table name appear among a query's referenced base tables?
///
/// `referenced` holds base names (already lowercased, `_hot`/`_cold` stripped),
/// so a topic matches when its sanitized, lowercased name is present.
fn topic_matches_referenced(topic: &str, referenced: &HashSet<String>) -> bool {
    referenced.contains(&SqlHandler::sanitize_table_name(topic).to_lowercase())
}

/// Whether leadership is unknown for any topic this query references.
///
/// When it is, a fan-out that sums per-node results over-counts by the
/// replication factor: each node's providers fall open to "serve everything"
/// (see `LeadPartitions::owned_partitions`), so summing across replicas
/// multiplies every row by RF (#41). The coordinator must answer locally
/// instead — complete under full replication and never a multiplied count.
///
/// Unparseable SQL, a query that references no known topic, or a metadata
/// listing error all return `false`: the caller then keeps its normal fan-out
/// path. On a healthy cluster leadership is always known (the partition map is
/// populated from Raft), so this returns `false` there and the fan-out is
/// unchanged — it only diverges in the degraded, unpopulated-map state.
/// The peers whose answers this query actually needs: those leading at least one
/// partition of a topic it reads.
///
/// A fan-out goes to every peer, but each node's providers restrict it to the
/// partitions it leads, so a peer leading nothing contributes nothing. On a
/// cluster deliberately running a node short, that down node leads nothing —
/// leadership failed over long ago — and treating its silence as data loss would
/// refuse a query whose answer is complete. Returns `None` when the query's
/// topics cannot be resolved, which means "cannot tell": the caller then treats
/// every unanswered peer as load-bearing rather than guessing in the unsafe
/// direction.
async fn peers_this_query_needs(
    state: &UnifiedApiState,
    router: &super::query_router::QueryRouter,
    sql: &str,
) -> Option<HashSet<u64>> {
    let referenced: HashSet<String> = referenced_base_tables(sql)?.into_iter().collect();
    if referenced.is_empty() {
        return None;
    }
    let topics = state.metadata_store.list_topics().await.ok()?;
    let matched: HashSet<String> = topics
        .into_iter()
        .filter(|t| topic_matches_referenced(&t.name, &referenced))
        .map(|t| t.name)
        .collect();
    if matched.is_empty() {
        return None;
    }
    Some(router.peers_leading_any(&matched).await)
}

async fn query_topic_leadership_unknown(
    state: &UnifiedApiState,
    router: &super::query_router::QueryRouter,
    sql: &str,
) -> bool {
    let Some(referenced) = referenced_base_tables(sql) else {
        return false;
    };
    if referenced.is_empty() {
        return false;
    }
    let referenced: HashSet<String> = referenced.into_iter().collect();
    let topics = match state.metadata_store.list_topics().await {
        Ok(topics) => topics,
        Err(_) => return false,
    };
    for topic in &topics {
        if topic_matches_referenced(&topic.name, &referenced)
            && router.led_partitions(&topic.name).await.is_none()
        {
            return true;
        }
    }
    false
}

/// Execute SQL query endpoint
/// Whether this query has to be answered by fanning out to the other nodes.
///
/// Deliberately settled *before* anything is executed. The answer changes what
/// SQL this node should run: when the coordinator is going to merge, clauses
/// like `LIMIT` and `OFFSET` have to come out of the query every node runs —
/// this one included — and be applied once to the merged rows instead. Deciding
/// after the local result already existed is what forced those queries to be
/// refused rather than answered (#55).
async fn fan_out_needed(state: &UnifiedApiState, headers: &HeaderMap, sql: &str) -> bool {
    let Some(router) = state.query_router.as_ref() else {
        return false;
    };
    if super::query_router::is_forwarded_request(headers) {
        return false;
    }

    // Refresh unconditionally, not just when the map is empty.
    //
    // `has_partition_map()` is true as soon as ONE topic is mapped, so the old
    // guard meant a topic created later was never mapped — and an unmapped topic
    // is invisible to the leadership check, which would then happily skip the
    // fan-out and miss that topic's data entirely. These are in-memory metadata
    // reads and this is a query path, not the produce path.
    router
        .refresh_all_partition_maps(state.metadata_store.as_ref())
        .await;

    // Skip the fan-out only when this node LEADS every partition.
    //
    // This used to skip when the node was a *replica* of every partition
    // (`all_topics_local`), which at RF=node_count is always true — so on a
    // full-replication cluster the query never fanned out and returned only what
    // this node happened to see locally. That is #22: partial results, and
    // different ones depending on which node answered.
    //
    // Leadership is the right condition on both sides: if this node leads
    // everything there is nothing to merge, and if it does not, the providers
    // restrict each node to its own led partitions so the union covers every
    // partition exactly once — no gaps, no double-counting.
    if router.leads_all_partitions().await {
        debug!("This node leads every partition, skipping SQL fan-out");
        return false;
    }

    // Leadership could not be resolved for a topic this query touches, even
    // after refreshing the map. Fanning out and summing now would over-count by
    // the replication factor, because every replica's providers fall open to
    // "serve everything" (#41). Answer locally instead.
    if query_topic_leadership_unknown(state, router, sql).await {
        warn!(
            query = %sql,
            "Leadership unknown for a queried topic; serving locally to avoid a fan-out over-count (#41)"
        );
        return false;
    }

    !router.all_peers().is_empty()
}

/// Execute SQL query endpoint
pub async fn execute_sql(
    State(state): State<UnifiedApiState>,
    headers: HeaderMap,
    Json(request): Json<SqlRequest>,
) -> impl IntoResponse {
    let row_limit = request.effective_limit();
    info!(query = %request.query, limit = row_limit, "Executing SQL query");

    // Security Phase 4: per-topic authorization.
    //
    // The API key authenticates the caller; this decides which topics that
    // caller may read. Without it a key holder could SELECT from any topic,
    // bypassing the ACLs enforced on the Kafka port.
    if let Some(denied) = authorize_sql_query(&state, &request.query).await {
        return denied;
    }

    // Check if SQL engine is available before executing
    if state.query_engine.is_none() {
        let error_response = SqlErrorResponse {
            error: "SQL query engine not available".to_string(),
            error_type: "ServiceUnavailable".to_string(),
        };
        return (StatusCode::SERVICE_UNAVAILABLE, Json(error_response)).into_response();
    }

    // How the results combine depends on the query, and getting it wrong is
    // silent. Decide before paying for the fan-out — or for the local execution —
    // so an unmergeable query fails fast instead of returning a plausible wrong
    // answer, and a mergeable one can adjust the SQL the nodes are given.
    let distributed = fan_out_needed(&state, &headers, &request.query).await;
    let strategy = if distributed {
        match super::query_router::sql_merge_strategy(&request.query) {
            super::query_router::SqlMerge::Unsupported(why) => {
                warn!(query = %request.query, reason = %why,
                      "Refusing to merge a distributed SQL result");
                let error_response = SqlErrorResponse {
                    error: format!("This query cannot be answered across a cluster: {}", why),
                    error_type: "DistributedQueryUnsupported".to_string(),
                };
                return (StatusCode::BAD_REQUEST, Json(error_response)).into_response();
            }
            strategy => Some(strategy),
        }
    } else {
        None
    };

    // The SQL every node runs, which is the caller's unless a clause had to be
    // lifted out of it for the coordinator to apply once.
    let node_sql = strategy
        .as_ref()
        .and_then(|s| match s {
            super::query_router::SqlMerge::Concatenate(plan)
            | super::query_router::SqlMerge::GroupedAggregate { post: plan, .. }
            | super::query_router::SqlMerge::ScalarAggregate { post: plan, .. } => {
                plan.node_sql.clone()
            }
            _ => None,
        })
        .unwrap_or_else(|| request.query.clone());

    // What each node may contribute, which is not the same as what the caller
    // gets back: a page's candidates can outnumber the response cap, and a cap
    // applied to a node's reply discards them after the query has already
    // picked them out. The response is still held to `row_limit` below.
    let fetch_limit = strategy
        .as_ref()
        .and_then(|s| match s {
            super::query_router::SqlMerge::Concatenate(plan)
            | super::query_router::SqlMerge::GroupedAggregate { post: plan, .. }
            | super::query_router::SqlMerge::ScalarAggregate { post: plan, .. } => {
                Some(plan.node_fetch_rows(row_limit).min(MAX_ROW_LIMIT))
            }
            _ => None,
        })
        .unwrap_or(row_limit);

    match SqlHandler::execute(&state, &node_sql, fetch_limit).await {
        Ok(response) => {
            let response = match (strategy, state.query_router.as_ref()) {
                (Some(strategy), Some(router)) => {
                    let all_peers = router.all_peers();
                    // Peers must run the same SQL this node just ran, and apply
                    // the cap we resolved rather than re-deriving it.
                    let fan_out_request = SqlRequest {
                        query: node_sql,
                        limit: Some(fetch_limit),
                        ..request.clone()
                    };
                    let fan_out = router
                        .fan_out_post::<_, SqlResponse>("/_sql", &fan_out_request, &all_peers)
                        .await;

                    // A node that did not answer must not be merged over.
                    //
                    // Issue #57: the fan-out used to drop a peer that timed out
                    // or errored, and the merge then combined what was left into
                    // a confident 200. On a cluster whose nodes were busy, every
                    // peer dropped and `SELECT COUNT(*)` returned 0 — for a topic
                    // holding 2.55M records, for half an hour, with no indication
                    // anything was missing. Every merge here, aggregate or
                    // row-wise, assumes it was given every node's contribution;
                    // none of them can tell a node that said "nothing" from a
                    // node that said nothing at all.
                    //
                    // So this refuses. A query that cannot be answered completely
                    // gets an error naming the nodes that did not reply, which an
                    // operator can act on, instead of a number they cannot tell
                    // is wrong.
                    // Only a peer that leads part of this query's data can make
                    // the answer short. A peer that leads nothing was asked out
                    // of simplicity and contributes nothing, so its silence is
                    // not a reason to refuse — a cluster running a node down on
                    // purpose would otherwise lose SQL entirely.
                    let needed = peers_this_query_needs(&state, router, &request.query).await;
                    let load_bearing: Vec<&super::query_router::PeerFailure> = fan_out
                        .failures
                        .iter()
                        .filter(|f| needed.as_ref().is_none_or(|n| n.contains(&f.node_id)))
                        .collect();
                    if !fan_out.failures.is_empty() && load_bearing.is_empty() {
                        debug!(
                            query = %request.query,
                            ignorable = %fan_out.failure_summary(),
                            "Peers did not answer but lead none of this query's partitions"
                        );
                    }

                    let mut missing_nodes: Vec<String> = Vec::new();
                    if !load_bearing.is_empty() {
                        let summary = load_bearing
                            .iter()
                            .map(|f| format!("node {}: {}", f.node_id, f.reason))
                            .collect::<Vec<_>>()
                            .join("; ");
                        if !allow_partial(&request) {
                            warn!(
                                query = %request.query,
                                answered = fan_out.responses.len(),
                                asked = all_peers.len(),
                                failures = %summary,
                                "Refusing a distributed SQL answer: not every node replied"
                            );
                            let error_response = SqlErrorResponse {
                                error: format!(
                                    "This query spans the cluster and {} of {} other node(s) did \
                                     not answer, so any result would silently omit their data — a \
                                     COUNT would come back short, not missing. Unanswered: {}. \
                                     Retry once the nodes are reachable, raise \
                                     CHRONIK_QUERY_TIMEOUT_SECS if they are merely slow under \
                                     load, or re-send with \"allow_partial\": true to accept an \
                                     answer labelled as missing them.",
                                    load_bearing.len(),
                                    all_peers.len(),
                                    summary,
                                ),
                                error_type: "IncompleteClusterResult".to_string(),
                            };
                            return (StatusCode::SERVICE_UNAVAILABLE, Json(error_response))
                                .into_response();
                        }
                        warn!(
                            query = %request.query,
                            failures = %summary,
                            "Serving a partial SQL answer on request: totals are short by the \
                             nodes that did not reply"
                        );
                        missing_nodes = load_bearing
                            .iter()
                            .map(|f| format!("node {}: {}", f.node_id, f.reason))
                            .collect();
                    }

                    let peers = fan_out.responses;
                    debug!(peer_count = peers.len(), "Merging SQL results from peers");

                    // A grouped merge needs every node's whole contribution.
                    // Unlike a row-wise result, where a cut-off tail is simply
                    // fewer rows and is reported as such, a group cut off on one
                    // node comes back *present but short* — its total missing
                    // that node's share, with nothing to distinguish it from a
                    // correct one. The nodes cannot be given a LIMIT to make
                    // this safe either: a group that is small on one node and
                    // large overall would be the one dropped.
                    //
                    // A distinct count and a quantile are the acute case, since
                    // they carry a row per value rather than per group, but the
                    // exposure is the same for a plain COUNT.
                    // All three shapes are the same defect, but naming the wrong
                    // one sends the reader looking for the wrong fix: someone who
                    // asked for COUNT(DISTINCT x) wrote no GROUP BY, so being told
                    // about "groups" reads as a message about a different query.
                    let shipped = match &strategy {
                        super::query_router::SqlMerge::GroupedAggregate { post, .. } => {
                            if post.distinct_count.is_some() {
                                Some("one row per distinct value")
                            } else if post.quantile.is_some() {
                                Some("one row per distinct value and its count")
                            } else {
                                Some("one row per group")
                            }
                        }
                        _ => None,
                    };
                    if let Some(shipped) = shipped {
                        if response.truncated || peers.iter().any(|p| p.truncated) {
                            warn!(query = %request.query, limit = fetch_limit, shipped,
                                  "value-collecting merge exceeded the row limit on at least one node");
                            let error_response = SqlErrorResponse {
                                error: format!(
                                    "This query cannot be answered across a cluster: it is merged \
                                     by collecting {shipped} from each node, and at least one node \
                                     had more of those to send than the {fetch_limit}-row limit \
                                     allows. What was cut would come back short rather than \
                                     missing, so there is no honest partial answer. Raise the \
                                     limit, narrow the query with WHERE, or group it more coarsely."
                                ),
                                error_type: "DistributedQueryUnsupported".to_string(),
                            };
                            return (StatusCode::BAD_REQUEST, Json(error_response)).into_response();
                        }
                    }

                    let mut merged_response = match strategy {
                        // The row-shaped merges finish themselves, because a
                        // HAVING or ORDER BY can name an AVG column that only
                        // exists once the totals have been divided.
                        super::query_router::SqlMerge::GroupedAggregate { how, post } => {
                            let merged = super::query_router::merge_grouped_aggregate(
                                response, peers, &how, &post, row_limit,
                            );
                            match post.finish_sql.as_deref() {
                                None => merged,
                                // The nodes returned only what combines; the
                                // shape around it is rebuilt here by the engine,
                                // and only then can the query be filtered and
                                // ordered — those may name a column that does
                                // not exist until it has run.
                                Some(finish_sql) => {
                                    match super::query_router::run_finish_sql(merged, finish_sql)
                                        .await
                                    {
                                        Ok(mut finished) => {
                                            super::query_router::apply_finish(
                                                &mut finished.rows,
                                                &post,
                                            );
                                            finished.truncated |= finished.rows.len() > row_limit;
                                            finished.rows.truncate(row_limit);
                                            finished.row_count = finished.rows.len();
                                            finished
                                        }
                                        Err(why) => {
                                            error!(query = %request.query, reason = %why,
                                                   "Could not finish a distributed query");
                                            let error_response = SqlErrorResponse {
                                                error: why,
                                                error_type: "QueryError".to_string(),
                                            };
                                            return (StatusCode::BAD_REQUEST, Json(error_response))
                                                .into_response();
                                        }
                                    }
                                }
                            }
                        }
                        super::query_router::SqlMerge::Concatenate(post) => {
                            super::query_router::merge_sql_responses(
                                response, peers, row_limit, &post,
                            )
                        }
                        // A single row has nothing to filter or order, so it is
                        // only the division that is left.
                        super::query_router::SqlMerge::ScalarAggregate { how, post } => {
                            let mut merged = super::query_router::merge_scalar_aggregate(
                                response, peers, &how,
                            );
                            super::query_router::finish_derived(&mut merged, &post.derived);
                            // A scalar aggregate can still carry a HAVING —
                            // `SELECT COUNT(*) FROM t HAVING COUNT(*) > 5` is a
                            // row or no row — and it belongs to the combined
                            // total, so it is applied here like any other.
                            super::query_router::apply_finish(&mut merged.rows, &post);
                            merged.row_count = merged.rows.len();
                            merged
                        }
                        super::query_router::SqlMerge::Unsupported(_) => {
                            unreachable!("refusals return above")
                        }
                    };

                    // Label an answer the caller agreed to take incomplete. The
                    // merge functions cannot do this: they only ever see the
                    // responses that arrived.
                    if !missing_nodes.is_empty() {
                        merged_response.partial = true;
                        merged_response.incomplete_nodes = missing_nodes;
                    }
                    merged_response
                }
                (_, _) => response,
            };

            info!(
                rows = response.row_count,
                time_ms = response.execution_time_ms,
                partial = response.partial,
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

            let response = DescribeTableResponse { table, columns };
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
pub(super) fn arrow_value_to_json(
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
        assert_eq!(
            partition_of_parquet_path("/data/columnar/orders/seg.parquet"),
            None
        );
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

/// #41: the coordinator must not fan out and sum when it cannot tell which
/// partitions a node leads, or every row is counted once per replica. These
/// cover the two pure inputs to that decision: which tables a query names, and
/// whether a topic is one of them.
#[cfg(test)]
mod fanout_leadership_guard_tests {
    use super::*;

    fn refs(sql: &str) -> Vec<String> {
        referenced_base_tables(sql).expect("parseable SQL")
    }

    #[test]
    fn count_star_names_its_topic() {
        assert_eq!(refs("SELECT COUNT(*) FROM repro41"), vec!["repro41"]);
    }

    #[test]
    fn hot_and_cold_suffixes_map_back_to_the_base_topic() {
        assert_eq!(refs("SELECT * FROM orders_hot"), vec!["orders"]);
        assert_eq!(refs("SELECT * FROM orders_cold"), vec!["orders"]);
    }

    #[test]
    fn table_names_are_lowercased() {
        assert_eq!(refs("SELECT * FROM Orders"), vec!["orders"]);
    }

    #[test]
    fn a_join_names_every_table() {
        let mut got = refs("SELECT * FROM a JOIN b ON a.id = b.id");
        got.sort();
        assert_eq!(got, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn a_union_names_both_sides() {
        let mut got = refs("SELECT x FROM a UNION ALL SELECT x FROM b");
        got.sort();
        assert_eq!(got, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn a_derived_subquery_is_walked() {
        assert_eq!(
            refs("SELECT n FROM (SELECT COUNT(*) AS n FROM events) t"),
            vec!["events"]
        );
    }

    #[test]
    fn unparseable_sql_yields_none_so_the_caller_keeps_fanning_out() {
        assert!(referenced_base_tables("this is not sql").is_none());
    }

    #[test]
    fn a_topic_matches_the_sanitized_lowercased_form_of_its_name() {
        let referenced: HashSet<String> = refs("SELECT * FROM my_topic_v2").into_iter().collect();
        // Kafka topics use `.` and `-`, which sanitize to `_`.
        assert!(topic_matches_referenced("my.topic-v2", &referenced));
        assert!(!topic_matches_referenced("other", &referenced));
    }
}

/// #41: cold Parquet segments that overlap in offset range make a plain scan
/// count duplicated offsets once per copy. These cover the detection that gates
/// the de-duplicating view on.
#[cfg(test)]
mod cold_overlap_tests {
    use super::*;
    use chronik_common::metadata::{InMemoryMetadataStore, MetadataStore, ParquetSegmentMetadata};

    fn seg(topic: &str, partition: i32, min: i64, max: i64) -> ParquetSegmentMetadata {
        ParquetSegmentMetadata {
            segment_id: format!("{}-{}-{}-{}-parquet", topic, partition, min, max),
            topic: topic.to_string(),
            partition,
            min_offset: min,
            max_offset: max,
            record_count: (max - min + 1).max(0) as usize,
            row_group_count: 1,
            min_timestamp: 0,
            max_timestamp: 0,
            object_store_path: format!(
                "/d/{}/partition={}/{}-{}.parquet",
                topic, partition, min, max
            ),
            size_bytes: 1,
            created_at: 0,
            compression: "zstd".to_string(),
            time_partition_key: None,
            schema_fingerprint: None,
        }
    }

    async fn store_with(segments: &[ParquetSegmentMetadata]) -> InMemoryMetadataStore {
        let store = InMemoryMetadataStore::new();
        for s in segments {
            store.persist_parquet_segment(s.clone()).await.unwrap();
        }
        store
    }

    #[tokio::test]
    async fn sequential_non_overlapping_segments_do_not_need_dedup() {
        let store = store_with(&[
            seg("t", 0, 0, 999),
            seg("t", 0, 1000, 1999),
            seg("t", 1, 0, 500),
        ])
        .await;
        assert!(!cold_segments_overlap(&store, "t").await);
    }

    #[tokio::test]
    async fn nested_reindexed_segments_need_dedup() {
        // The real #41 shape: a partition re-indexed from offset 0 several times.
        let store = store_with(&[
            seg("t", 0, 0, 21539),
            seg("t", 0, 0, 47290),
            seg("t", 0, 0, 105415),
            seg("t", 0, 105416, 109383),
        ])
        .await;
        assert!(cold_segments_overlap(&store, "t").await);
    }

    #[tokio::test]
    async fn overlap_in_any_single_partition_triggers_dedup() {
        let store = store_with(&[
            seg("t", 0, 0, 999),    // p0 clean
            seg("t", 1, 0, 999),    // p1 ...
            seg("t", 1, 500, 1499), // ... overlaps here
        ])
        .await;
        assert!(cold_segments_overlap(&store, "t").await);
    }

    #[tokio::test]
    async fn a_topic_with_no_segments_does_not_need_dedup() {
        let store = InMemoryMetadataStore::new();
        assert!(!cold_segments_overlap(&store, "t").await);
    }
}

/// Authorize the topics a SQL statement reads.
///
/// Returns `Some(response)` when the query must be refused, `None` when it may
/// proceed. Refusal is 403 with the offending topics named, so an operator can
/// see which ACL is missing rather than guessing.
///
/// Unparseable SQL is refused rather than passed through: a statement whose
/// tables cannot be determined cannot be authorized, and running it would mean
/// authorizing nothing.
async fn authorize_sql_query(
    state: &UnifiedApiState,
    query: &str,
) -> Option<axum::response::Response> {
    use axum::response::IntoResponse;
    use chronik_protocol::describe_acls_types::{AclOperation, ResourceType};

    let authorizer = state.authorizer.as_ref()?;
    if !authorizer.is_enabled() {
        return None;
    }

    // The table -> topic map is built forward from the live topic list, because
    // the topic -> table sanitiser is lossy and cannot be inverted safely.
    let topics: Vec<String> = match state.metadata_store.list_topics().await {
        Ok(topics) => topics.into_iter().map(|t| t.name).collect(),
        Err(e) => {
            error!("SQL authorization could not list topics: {:?}", e);
            return Some(
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(SqlErrorResponse {
                        error: "authorization unavailable".to_string(),
                        error_type: "internal_error".to_string(),
                    }),
                )
                    .into_response(),
            );
        }
    };

    let map = super::sql_authz::TableTopicMap::from_topics(topics);
    let required = match super::sql_authz::required_topics(query, &map) {
        Ok(required) => required,
        Err(e) => {
            return Some(
                (
                    StatusCode::BAD_REQUEST,
                    Json(SqlErrorResponse {
                        error: format!("could not determine the tables this query reads: {}", e),
                        error_type: "invalid_query".to_string(),
                    }),
                )
                    .into_response(),
            );
        }
    };

    let mut denied = Vec::new();
    for topic in &required {
        let allowed = authorizer
            .authorize_principal(
                &state.api_principal,
                "*",
                ResourceType::Topic,
                topic,
                AclOperation::Read,
            )
            .await;
        if !allowed {
            denied.push(topic.clone());
        }
    }

    if denied.is_empty() {
        return None;
    }

    warn!(
        "SQL query DENIED for {}: no Read on {:?}",
        state.api_principal, denied
    );
    Some(
        (
            StatusCode::FORBIDDEN,
            Json(SqlErrorResponse {
                error: format!(
                    "{} is not authorized to read: {}",
                    state.api_principal,
                    denied.join(", ")
                ),
                error_type: "authorization_failed".to_string(),
            }),
        )
            .into_response(),
    )
}

#[cfg(test)]
mod cold_json_column_tests {
    use super::*;
    use datafusion::arrow::datatypes::{DataType, Field, Schema};

    fn schema(fields: Vec<Field>) -> Schema {
        Schema::new(fields)
    }

    /// The whole point of the feature: a typed column written to Parquet has to
    /// be reachable under the topic's own name, not only under `{topic}_cold`.
    #[test]
    fn the_inferred_json_columns_are_picked_out_of_a_cold_schema() {
        let s = schema(vec![
            Field::new("_topic", DataType::Utf8, false),
            Field::new("_partition", DataType::Int32, false),
            Field::new("_offset", DataType::Int64, false),
            Field::new("_value", DataType::Binary, false),
            Field::new("sev", DataType::Utf8, true),
            Field::new("n", DataType::Int64, true),
            Field::new("ratio", DataType::Float64, true),
            Field::new("ok", DataType::Boolean, true),
        ]);
        assert_eq!(
            cold_json_columns(&s),
            vec![
                ("sev".to_string(), JsonColKind::Text),
                ("n".to_string(), JsonColKind::Int),
                ("ratio".to_string(), JsonColKind::Float),
                ("ok".to_string(), JsonColKind::Bool),
            ]
        );
    }

    /// `_headers` is part of the cold envelope, not a message field. Including
    /// it would put a column on the view that the hot side cannot produce.
    #[test]
    fn envelope_columns_are_not_mistaken_for_message_fields() {
        let s = schema(vec![
            Field::new("_headers", DataType::Utf8, true),
            Field::new("_timestamp_type", DataType::Int8, false),
            Field::new("id", DataType::Int64, true),
        ]);
        assert_eq!(
            cold_json_columns(&s),
            vec![("id".to_string(), JsonColKind::Int)]
        );
    }

    /// A type the JSON inference never produces is left out rather than
    /// guessed at: there is no UDF that would rebuild it on the hot side, so
    /// naming it in the view would fail every query against the topic.
    #[test]
    fn a_type_with_no_hot_side_equivalent_is_left_out() {
        let s = schema(vec![
            Field::new("when", DataType::Date32, true),
            Field::new(
                "tags",
                DataType::List(std::sync::Arc::new(Field::new("item", DataType::Utf8, true))),
                true,
            ),
            Field::new("name", DataType::Utf8, true),
        ]);
        assert_eq!(
            cold_json_columns(&s),
            vec![("name".to_string(), JsonColKind::Text)]
        );
    }

    /// Cold Parquet hands back the `View` string/binary variants, so these must
    /// map like their plain counterparts or a real topic's columns get dropped.
    #[test]
    fn the_view_string_variants_map_like_plain_utf8() {
        assert_eq!(
            JsonColKind::from_arrow(&DataType::Utf8View),
            Some(JsonColKind::Text)
        );
        assert_eq!(
            JsonColKind::from_arrow(&DataType::LargeUtf8),
            Some(JsonColKind::Text)
        );
        assert_eq!(JsonColKind::from_arrow(&DataType::Binary), None);
    }

    /// The view names its columns one by one, so it has to be rebuilt when the
    /// set changes — and must NOT be rebuilt on every refresh just because a
    /// Parquet file lists the same fields in a different order.
    #[test]
    fn the_fingerprint_tracks_the_column_set_not_its_order() {
        let a = vec![
            ("sev".to_string(), JsonColKind::Text),
            ("n".to_string(), JsonColKind::Int),
        ];
        let reordered = vec![
            ("n".to_string(), JsonColKind::Int),
            ("sev".to_string(), JsonColKind::Text),
        ];
        let gained = vec![
            ("sev".to_string(), JsonColKind::Text),
            ("n".to_string(), JsonColKind::Int),
            ("grp".to_string(), JsonColKind::Text),
        ];
        let retyped = vec![
            ("sev".to_string(), JsonColKind::Text),
            ("n".to_string(), JsonColKind::Float),
        ];
        assert_eq!(json_cols_fingerprint(&a), json_cols_fingerprint(&reordered));
        assert_ne!(json_cols_fingerprint(&a), json_cols_fingerprint(&gained));
        assert_ne!(json_cols_fingerprint(&a), json_cols_fingerprint(&retyped));
        assert_eq!(json_cols_fingerprint(&[]), 0);
    }

    /// A hot row must carry the real value. Projecting NULL would be cheaper
    /// and would quietly understate every aggregate over recent data, because
    /// hot holds exactly the rows cold does not.
    #[test]
    fn the_hot_side_reads_the_field_out_of_the_value() {
        assert_eq!(
            JsonColKind::Text.extract_from_value("sev"),
            "json_extract_string(_value, '$.sev')"
        );
        assert_eq!(
            JsonColKind::Int.extract_from_value("n"),
            "json_extract_int(_value, '$.n')"
        );
        assert_eq!(
            JsonColKind::Float.extract_from_value("ratio"),
            "json_extract_float(_value, '$.ratio')"
        );
        // No boolean UDF exists; the text one plus the cast in the view does it.
        assert_eq!(
            JsonColKind::Bool.extract_from_value("ok"),
            "json_extract_string(_value, '$.ok')"
        );
        assert_eq!(JsonColKind::Bool.sql_type(), "BOOLEAN");
    }

    /// A JSON key may contain a quote. Splicing it in raw would produce SQL
    /// that fails to parse, taking the whole topic's view down with it.
    #[test]
    fn a_quote_in_a_field_name_is_escaped_not_spliced() {
        assert_eq!(sql_escape(r#"we"ird"#, '"'), r#"we""ird"#);
        assert_eq!(sql_escape("it's", '\''), "it''s");
        assert_eq!(sql_escape("plain", '"'), "plain");
    }
}

/// Issue #57: a cluster query whose peers did not answer returned a merged
/// number anyway — `COUNT(*)` came back 0 at HTTP 200 for a topic holding
/// millions of records.
#[cfg(test)]
mod partial_answer_tests {
    use super::*;

    /// `CHRONIK_SQL_ALLOW_PARTIAL` is process-wide and cargo runs tests on
    /// threads, so without this the tests below would set and clear the same
    /// variable under each other and fail at random.
    static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn request() -> SqlRequest {
        SqlRequest {
            query: "SELECT COUNT(*) FROM logs".to_string(),
            limit: None,
            timeout_secs: 30,
            allow_partial: None,
        }
    }

    /// The default has to be refusal. Anything else re-creates the reported
    /// bug for every caller that does not know to ask.
    #[test]
    fn an_incomplete_answer_is_refused_unless_asked_for() {
        let _g = ENV.lock().unwrap();
        std::env::remove_var("CHRONIK_SQL_ALLOW_PARTIAL");
        assert!(!allow_partial(&request()));
    }

    /// An operator who knows a node is down can still get a number, and the
    /// response says what it is missing.
    #[test]
    fn a_caller_may_opt_in_per_request() {
        let _g = ENV.lock().unwrap();
        std::env::remove_var("CHRONIK_SQL_ALLOW_PARTIAL");
        let mut req = request();
        req.allow_partial = Some(true);
        assert!(allow_partial(&req));
    }

    /// A request that explicitly says `false` must keep refusing even where the
    /// cluster default allows it — the narrower statement wins.
    #[test]
    fn an_explicit_refusal_beats_the_cluster_default() {
        let _g = ENV.lock().unwrap();
        std::env::set_var("CHRONIK_SQL_ALLOW_PARTIAL", "true");
        let mut req = request();
        req.allow_partial = Some(false);
        assert!(!allow_partial(&req));
        std::env::remove_var("CHRONIK_SQL_ALLOW_PARTIAL");
    }

    #[test]
    fn the_cluster_default_is_read_from_the_environment() {
        let _g = ENV.lock().unwrap();
        let req = request();
        for yes in ["true", "TRUE", " yes ", "1"] {
            std::env::set_var("CHRONIK_SQL_ALLOW_PARTIAL", yes);
            assert!(allow_partial(&req), "{yes:?} should enable partial answers");
        }
        for no in ["false", "0", "no", "", "nonsense"] {
            std::env::set_var("CHRONIK_SQL_ALLOW_PARTIAL", no);
            assert!(
                !allow_partial(&req),
                "{no:?} must not enable partial answers"
            );
        }
        std::env::remove_var("CHRONIK_SQL_ALLOW_PARTIAL");
    }

    /// `partial` and `truncated` are different claims and must serialize
    /// separately: "we cut the tail off" is not "a node is missing from this
    /// total". Both stay out of the JSON when there is nothing to report.
    #[test]
    fn partial_is_reported_separately_from_truncated() {
        let mut resp = SqlResponse {
            columns: vec!["c".into()],
            rows: Vec::new(),
            row_count: 0,
            execution_time_ms: 1,
            truncated: false,
            partial: false,
            incomplete_nodes: Vec::new(),
        };

        let clean = serde_json::to_string(&resp).unwrap();
        assert!(!clean.contains("partial"), "{clean}");
        assert!(!clean.contains("incomplete_nodes"), "{clean}");

        resp.partial = true;
        resp.incomplete_nodes = vec!["node 3: peer returned HTTP 503".into()];
        let labelled = serde_json::to_string(&resp).unwrap();
        assert!(labelled.contains("\"partial\":true"), "{labelled}");
        assert!(labelled.contains("node 3"), "{labelled}");
        assert!(
            labelled.contains("\"truncated\":false"),
            "truncated must stay its own claim: {labelled}"
        );
    }
}
