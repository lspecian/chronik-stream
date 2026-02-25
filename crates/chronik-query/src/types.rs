//! Request and response types for the `/_query` unified query API.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Unified query request matching the `POST /_query` API spec.
#[derive(Debug, Clone, Deserialize)]
pub struct QueryRequest {
    /// Data sources to query (topics + modes)
    pub sources: Vec<SourceSpec>,
    /// Query parameters (text, semantic, sql)
    pub q: QuerySpec,
    /// Optional filters (timestamp, offset ranges)
    #[serde(default)]
    pub filters: Option<FilterSpec>,
    /// Maximum number of results to return (default: 50)
    #[serde(default = "default_k")]
    pub k: usize,
    /// Ranking configuration
    #[serde(default)]
    pub rank: Option<RankSpec>,
    /// Query timeout in milliseconds (default: 5000)
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: Option<u64>,
    /// Result format: "merged" (single ranked list) or "grouped" (per-topic)
    #[serde(default)]
    pub result_format: ResultFormat,
}

/// How results are organized in the response.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultFormat {
    /// Single ranked list across all topics (default)
    #[default]
    Merged,
    /// Results partitioned by topic, independently ranked
    Grouped,
}

fn default_k() -> usize {
    50
}

fn default_timeout_ms() -> Option<u64> {
    Some(5000)
}

/// Specifies a topic and which query modes to use on it.
#[derive(Debug, Clone, Deserialize)]
pub struct SourceSpec {
    /// Topic name
    pub topic: String,
    /// Query modes to apply: "text", "vector", "sql", "fetch"
    pub modes: Vec<QueryMode>,
}

/// Available query modes
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryMode {
    /// Full-text search via Tantivy
    Text,
    /// Semantic vector search via HNSW + embeddings
    Vector,
    /// SQL query via DataFusion
    Sql,
    /// Direct offset-based fetch from WAL
    Fetch,
}

impl std::fmt::Display for QueryMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QueryMode::Text => write!(f, "text"),
            QueryMode::Vector => write!(f, "vector"),
            QueryMode::Sql => write!(f, "sql"),
            QueryMode::Fetch => write!(f, "fetch"),
        }
    }
}

/// Query parameters
#[derive(Debug, Clone, Deserialize)]
pub struct QuerySpec {
    /// Text query for full-text search (Tantivy)
    #[serde(default)]
    pub text: Option<String>,
    /// Semantic query for vector search (embedded then searched)
    #[serde(default)]
    pub semantic: Option<String>,
    /// SQL query for columnar search (DataFusion)
    #[serde(default)]
    pub sql: Option<String>,
    /// Offset-based fetch parameters
    #[serde(default)]
    pub fetch: Option<FetchSpec>,
}

/// Parameters for offset-based fetch
#[derive(Debug, Clone, Deserialize)]
pub struct FetchSpec {
    /// Starting offset
    pub offset: i64,
    /// Partition to fetch from
    #[serde(default)]
    pub partition: Option<i32>,
    /// Maximum bytes to fetch
    #[serde(default = "default_max_bytes")]
    pub max_bytes: usize,
}

fn default_max_bytes() -> usize {
    1_048_576 // 1MB
}

/// Filters applied to all query backends
#[derive(Debug, Clone, Default, Deserialize)]
pub struct FilterSpec {
    /// Minimum timestamp (inclusive, ISO-8601 or epoch millis)
    #[serde(default)]
    pub timestamp_gte: Option<String>,
    /// Maximum timestamp (inclusive, ISO-8601 or epoch millis)
    #[serde(default)]
    pub timestamp_lte: Option<String>,
    /// Filter to specific partitions
    #[serde(default)]
    pub partitions: Option<Vec<i32>>,
}

/// Ranking configuration
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RankSpec {
    /// Name of the ranking profile to use (default: "default")
    #[serde(default = "default_profile")]
    pub profile: String,
}

fn default_profile() -> String {
    "default".to_string()
}

/// Unified query response
#[derive(Debug, Clone, Serialize)]
pub struct QueryResponse {
    /// Unique query identifier
    pub query_id: String,
    /// Ranked results (populated when result_format is "merged")
    pub results: Vec<RankedResult>,
    /// Results grouped by topic (populated when result_format is "grouped")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grouped_results: Option<HashMap<String, Vec<RankedResult>>>,
    /// Query execution statistics
    pub stats: QueryStats,
}

/// A single ranked result from the unified query
#[derive(Debug, Clone, Serialize)]
pub struct RankedResult {
    /// Source topic
    pub topic: String,
    /// Partition number
    pub partition: i32,
    /// Message offset
    pub offset: i64,
    /// Final computed score after ranking
    pub final_score: f64,
    /// Feature values used in ranking
    pub features: HashMap<String, f64>,
    /// Human-readable explanation of the ranking
    #[serde(skip_serializing_if = "Option::is_none")]
    pub explanation: Option<String>,
    /// Original text preview (if available)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_preview: Option<String>,
}

/// Query execution statistics
#[derive(Debug, Clone, Serialize)]
pub struct QueryStats {
    /// Total candidates collected from all backends
    pub candidates: usize,
    /// Number of results after ranking and truncation
    pub ranked: usize,
    /// Total query latency in milliseconds
    pub latency_ms: u64,
    /// Per-backend statistics
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub backends: Vec<BackendStats>,
}

/// Statistics for a single backend execution
#[derive(Debug, Clone, Serialize)]
pub struct BackendStats {
    /// Backend type (text, vector, sql, fetch)
    pub backend: String,
    /// Topic queried
    pub topic: String,
    /// Number of candidates returned
    pub candidates: usize,
    /// Execution time in milliseconds
    pub latency_ms: u64,
    /// Whether the backend timed out
    pub timed_out: bool,
}
