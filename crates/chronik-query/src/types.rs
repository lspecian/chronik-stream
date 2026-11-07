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
    /// VO-4: Enable cross-encoder re-ranking after RRF fusion (default: false)
    #[serde(default)]
    pub rerank: bool,
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

/// Filters applied to all query backends.
///
/// `deny_unknown_fields`, so a filter this does not support is an error
/// rather than silence. A reporter lost time on `filter.key`: it is not a
/// field here, serde dropped it, and filtering on a key that matched
/// everything and one that matched nothing returned identical results.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
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
    /// Which backends found this hit, sorted — `["text"]`, `["vector"]`, or
    /// both when they agreed.
    ///
    /// RRF flattens every backend into one list, so without this a caller
    /// cannot tell where a hit came from, and a hit carrying no
    /// `text_preview` is indistinguishable from one whose content failed to
    /// parse. With both fields the two cases separate: a hit with a backend
    /// and no preview is a hit whose content could not be read, not a miss.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub backends: Vec<String>,
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

#[cfg(test)]
mod filter_spec_tests {
    use super::*;

    /// Issue #56's closing note: filtering on a key present in the topic and on
    /// a key matching nothing returned identical results. `key` is not a field
    /// here, so serde dropped it and the filter never existed.
    #[test]
    fn an_unsupported_filter_field_is_an_error_not_silence() {
        let err = serde_json::from_str::<FilterSpec>(r#"{"key": "user-42"}"#)
            .expect_err("an unknown filter must not be silently accepted");
        assert!(
            err.to_string().contains("key"),
            "the error should name the offending field: {err}"
        );
    }

    #[test]
    fn the_supported_filters_still_parse() {
        let f: FilterSpec =
            serde_json::from_str(r#"{"partitions": [0, 2], "timestamp_gte": "2026-01-01"}"#)
                .expect("documented fields must parse");
        assert_eq!(f.partitions, Some(vec![0, 2]));
        assert_eq!(f.timestamp_gte.as_deref(), Some("2026-01-01"));
    }

    /// A whole-request parse must fail the same way, since that is where a
    /// caller's filter actually arrives.
    #[test]
    fn a_request_carrying_an_unsupported_filter_is_rejected() {
        let body = r#"{
            "sources": [{"topic": "logs", "modes": ["vector"]}],
            "q": {"text": "anything"},
            "filters": {"key": "user-42"}
        }"#;
        assert!(serde_json::from_str::<QueryRequest>(body).is_err());
    }
}
