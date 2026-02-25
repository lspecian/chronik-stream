//! Feature logging infrastructure for training data collection.
//!
//! Every query produces a feature log entry containing the query, features,
//! scores, and results. These logs can be used to bootstrap ML ranking models
//! and measure retrieval quality over time.
//!
//! Feature logs are written to an internal Chronik topic (`_chronik_query_logs`)
//! so they're durable, searchable, and exportable via Kafka consumer protocol.

use crate::types::{QueryRequest, QueryResponse};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing;

/// A feature log entry recording one query's execution details.
#[derive(Debug, Clone, Serialize)]
pub struct FeatureLogEntry {
    /// ISO-8601 timestamp of the query
    pub timestamp: String,
    /// Unique query identifier
    pub query_id: String,
    /// Profile used for ranking
    pub profile: String,
    /// Number of sources queried
    pub source_count: usize,
    /// Topics queried
    pub topics: Vec<String>,
    /// Modes used across all sources
    pub modes: Vec<String>,
    /// Text query (if any)
    pub text_query: Option<String>,
    /// Semantic query (if any)
    pub semantic_query: Option<String>,
    /// SQL query (if any)
    pub sql_query: Option<String>,
    /// Number of candidates before ranking
    pub total_candidates: usize,
    /// Number of results after ranking
    pub ranked_results: usize,
    /// Total query latency in milliseconds
    pub latency_ms: u64,
    /// Per-result feature vectors (top-N only to limit log size)
    pub result_features: Vec<ResultFeatures>,
}

/// Features for a single result in the feature log.
#[derive(Debug, Clone, Serialize)]
pub struct ResultFeatures {
    /// Result position (0-indexed)
    pub position: usize,
    /// Topic
    pub topic: String,
    /// Partition
    pub partition: i32,
    /// Offset
    pub offset: i64,
    /// Final score
    pub final_score: f64,
    /// All feature values
    pub features: HashMap<String, f64>,
}

/// Asynchronous feature logger.
///
/// Receives feature log entries via a channel and writes them to the
/// configured sink (log, topic, or file).
pub struct FeatureLogger {
    sender: mpsc::UnboundedSender<FeatureLogEntry>,
}

impl FeatureLogger {
    /// Create a new feature logger that logs to tracing.
    ///
    /// In production, this would write to `_chronik_query_logs` topic.
    /// For now, it logs via the tracing framework for observability.
    pub fn new() -> Self {
        let (sender, mut receiver) = mpsc::unbounded_channel::<FeatureLogEntry>();

        tokio::spawn(async move {
            while let Some(entry) = receiver.recv().await {
                match serde_json::to_string(&entry) {
                    Ok(json) => {
                        tracing::info!(
                            target: "chronik_query_features",
                            query_id = %entry.query_id,
                            profile = %entry.profile,
                            candidates = entry.total_candidates,
                            results = entry.ranked_results,
                            latency_ms = entry.latency_ms,
                            "feature_log: {}",
                            json
                        );
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "Failed to serialize feature log entry");
                    }
                }
            }
        });

        Self { sender }
    }

    /// Create a feature logger backed by a shared sender.
    pub fn from_sender(sender: mpsc::UnboundedSender<FeatureLogEntry>) -> Self {
        Self { sender }
    }

    /// Log a query's features and results.
    ///
    /// This is non-blocking — the entry is sent to a background task.
    pub fn log(&self, request: &QueryRequest, response: &QueryResponse, profile_name: &str) {
        let entry = Self::build_entry(request, response, profile_name);
        if let Err(e) = self.sender.send(entry) {
            tracing::warn!(error = %e, "Feature log channel closed");
        }
    }

    /// Build a feature log entry from request and response.
    fn build_entry(
        request: &QueryRequest,
        response: &QueryResponse,
        profile_name: &str,
    ) -> FeatureLogEntry {
        let topics: Vec<String> = request
            .sources
            .iter()
            .map(|s| s.topic.clone())
            .collect();

        let modes: Vec<String> = request
            .sources
            .iter()
            .flat_map(|s| s.modes.iter().map(|m| m.to_string()))
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();

        // Log features for top-20 results to keep log size reasonable
        let max_logged = 20;
        let result_features: Vec<ResultFeatures> = response
            .results
            .iter()
            .take(max_logged)
            .enumerate()
            .map(|(i, r)| ResultFeatures {
                position: i,
                topic: r.topic.clone(),
                partition: r.partition,
                offset: r.offset,
                final_score: r.final_score,
                features: r.features.clone(),
            })
            .collect();

        FeatureLogEntry {
            timestamp: chrono::Utc::now().to_rfc3339(),
            query_id: response.query_id.clone(),
            profile: profile_name.to_string(),
            source_count: request.sources.len(),
            topics,
            modes,
            text_query: request.q.text.clone(),
            semantic_query: request.q.semantic.clone(),
            sql_query: request.q.sql.clone(),
            total_candidates: response.stats.candidates,
            ranked_results: response.stats.ranked,
            latency_ms: response.stats.latency_ms,
            result_features,
        }
    }
}

impl Default for FeatureLogger {
    fn default() -> Self {
        Self::new()
    }
}

/// Create a shared feature logger wrapped in Arc for use across handlers.
pub fn create_feature_logger() -> Arc<FeatureLogger> {
    Arc::new(FeatureLogger::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::*;

    fn sample_request() -> QueryRequest {
        QueryRequest {
            sources: vec![SourceSpec {
                topic: "logs".into(),
                modes: vec![QueryMode::Text, QueryMode::Vector],
            }],
            q: QuerySpec {
                text: Some("payment failed".into()),
                semantic: Some("card declined".into()),
                sql: None,
                fetch: None,
            },
            filters: None,
            k: 10,
            rank: None,
            timeout_ms: None,
            result_format: ResultFormat::Merged,
        }
    }

    fn sample_response() -> QueryResponse {
        QueryResponse {
            query_id: "test-query-123".into(),
            results: vec![RankedResult {
                topic: "logs".into(),
                partition: 0,
                offset: 42,
                final_score: 0.95,
                features: {
                    let mut f = HashMap::new();
                    f.insert("rrf_score".into(), 0.033);
                    f.insert("freshness".into(), 0.98);
                    f
                },
                explanation: Some("Top match".into()),
                text_preview: None,
            }],
            grouped_results: None,
            stats: QueryStats {
                candidates: 50,
                ranked: 1,
                latency_ms: 15,
                backends: vec![],
            },
        }
    }

    #[test]
    fn test_build_entry() {
        let request = sample_request();
        let response = sample_response();

        let entry = FeatureLogger::build_entry(&request, &response, "default");

        assert_eq!(entry.query_id, "test-query-123");
        assert_eq!(entry.profile, "default");
        assert_eq!(entry.source_count, 1);
        assert_eq!(entry.topics, vec!["logs"]);
        assert_eq!(entry.total_candidates, 50);
        assert_eq!(entry.ranked_results, 1);
        assert_eq!(entry.result_features.len(), 1);
        assert_eq!(entry.result_features[0].offset, 42);
        assert!(entry.result_features[0].features.contains_key("rrf_score"));
    }

    #[test]
    fn test_entry_serialization() {
        let request = sample_request();
        let response = sample_response();
        let entry = FeatureLogger::build_entry(&request, &response, "relevance");

        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("test-query-123"));
        assert!(json.contains("relevance"));
        assert!(json.contains("payment failed"));
    }
}
