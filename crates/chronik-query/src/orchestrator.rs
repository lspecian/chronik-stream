//! Query orchestrator for parallel multi-backend execution.
//!
//! The `QueryOrchestrator` takes a `QueryPlan` (list of `ExecutionNode`s),
//! spawns each as a tokio task, collects results with timeout handling,
//! then runs RRF fusion and ranking.
//!
//! ## Flow
//!
//! ```text
//! QueryPlan → spawn tasks → timeout/collect → CandidateSet → RRF → Rank → Response
//! ```
//!
//! The orchestrator is backend-agnostic: it uses trait objects (`BackendAdapter`)
//! to call into the actual search/SQL/vector/fetch backends. This keeps the
//! orchestrator testable without requiring real backend instances.

use crate::candidate::{Candidate, CandidateSet};
use crate::features::FeatureLogger;
use crate::plan::{ExecutionNode, QueryPlan};
use crate::profiles::ProfileStore;
use crate::ranker::{Ranker, RuleRanker};
use crate::rrf::RrfMerger;
use crate::types::*;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

/// Trait for executing individual plan nodes against backends.
///
/// The server implements this trait to connect the orchestrator to the actual
/// SearchApi, ColumnarQueryEngine, VectorSearchService, and WalManager.
/// This decouples the orchestrator from the specific backend implementations.
#[async_trait::async_trait]
pub trait BackendAdapter: Send + Sync {
    /// Execute a text search and return normalized candidates.
    async fn execute_text(
        &self,
        topic: &str,
        query: &str,
        k: usize,
    ) -> Result<Vec<Candidate>, BackendError>;

    /// Execute a vector search and return normalized candidates.
    async fn execute_vector(
        &self,
        topic: &str,
        query: &str,
        k: usize,
    ) -> Result<Vec<Candidate>, BackendError>;

    /// Execute a SQL query and return normalized candidates.
    async fn execute_sql(
        &self,
        topic: &str,
        sql: &str,
        limit: usize,
    ) -> Result<Vec<Candidate>, BackendError>;

    /// Execute a WAL fetch and return normalized candidates.
    async fn execute_fetch(
        &self,
        topic: &str,
        partition: i32,
        offset: i64,
        max_bytes: usize,
    ) -> Result<Vec<Candidate>, BackendError>;
}

/// Errors from backend execution.
#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("Backend not available: {0}")]
    NotAvailable(String),
    #[error("Backend execution failed: {0}")]
    ExecutionFailed(String),
    #[error("Backend timed out after {0}ms")]
    Timeout(u64),
}

/// Errors from the orchestrator.
#[derive(Debug, thiserror::Error)]
pub enum OrchestratorError {
    #[error("Planning error: {0}")]
    PlanError(#[from] crate::plan::PlanError),
    #[error("All backends failed")]
    AllBackendsFailed,
}

/// The query orchestrator: plans, executes, fuses, and ranks.
pub struct QueryOrchestrator {
    adapter: Arc<dyn BackendAdapter>,
    merger: RrfMerger,
    ranker: Box<dyn Ranker>,
    profiles: Arc<ProfileStore>,
    feature_logger: Option<Arc<FeatureLogger>>,
}

impl QueryOrchestrator {
    /// Create a new orchestrator with the given backend adapter.
    pub fn new(adapter: Arc<dyn BackendAdapter>, profiles: Arc<ProfileStore>) -> Self {
        Self {
            adapter,
            merger: RrfMerger::default(),
            ranker: Box::new(RuleRanker),
            profiles,
            feature_logger: None,
        }
    }

    /// Set a custom RRF k constant.
    pub fn with_rrf_k(mut self, k: f64) -> Self {
        self.merger = RrfMerger::new(k);
        self
    }

    /// Set a custom ranker implementation.
    pub fn with_ranker(mut self, ranker: Box<dyn Ranker>) -> Self {
        self.ranker = ranker;
        self
    }

    /// Enable feature logging.
    pub fn with_feature_logger(mut self, logger: Arc<FeatureLogger>) -> Self {
        self.feature_logger = Some(logger);
        self
    }

    /// Execute a query: plan → parallel execute → fuse → rank → respond.
    pub async fn execute(
        &self,
        request: &QueryRequest,
        plan: QueryPlan,
    ) -> Result<QueryResponse, OrchestratorError> {
        let start = Instant::now();
        let query_id = uuid::Uuid::new_v4().to_string();

        // Execute all plan nodes in parallel with timeout
        let (candidates, backend_stats) = self.execute_plan(&plan).await;

        let total_candidates = candidates.len();

        if total_candidates == 0 && backend_stats.iter().all(|s| s.timed_out) {
            return Err(OrchestratorError::AllBackendsFailed);
        }

        // Build candidate set for deduplication
        let mut candidate_set = CandidateSet::new();
        candidate_set.add_candidates(candidates);

        // RRF merge
        let mut entries = self.merger.merge(candidate_set, plan.k * 2);

        // Get ranking profile
        let profile_name = request
            .rank
            .as_ref()
            .map(|r| r.profile.as_str())
            .unwrap_or("default");
        let profile = self.profiles.get(profile_name).await;

        // Apply ranking
        self.ranker.rank(&mut entries, &profile);

        // Truncate to requested k
        entries.truncate(plan.k);

        // Convert to response
        let results: Vec<RankedResult> = entries
            .into_iter()
            .map(|e| e.primary.into_ranked_result())
            .collect();

        let latency_ms = start.elapsed().as_millis() as u64;

        // Build response based on result_format
        let (final_results, grouped_results) = match request.result_format {
            ResultFormat::Grouped => {
                let mut groups: HashMap<String, Vec<RankedResult>> = HashMap::new();
                for r in results {
                    groups.entry(r.topic.clone()).or_default().push(r);
                }
                (Vec::new(), Some(groups))
            }
            ResultFormat::Merged => (results, None),
        };

        let ranked_count = grouped_results
            .as_ref()
            .map(|g| g.values().map(|v| v.len()).sum())
            .unwrap_or(final_results.len());

        let response = QueryResponse {
            query_id,
            stats: QueryStats {
                candidates: total_candidates,
                ranked: ranked_count,
                latency_ms,
                backends: backend_stats,
            },
            results: final_results,
            grouped_results,
        };

        // Log features (non-blocking)
        if let Some(ref logger) = self.feature_logger {
            logger.log(request, &response, profile_name);
        }

        Ok(response)
    }

    /// Execute all plan nodes in parallel, collecting candidates and stats.
    async fn execute_plan(
        &self,
        plan: &QueryPlan,
    ) -> (Vec<Candidate>, Vec<BackendStats>) {
        let mut handles = Vec::new();

        for node in &plan.nodes {
            let adapter = self.adapter.clone();
            let node = node.clone();
            let timeout = plan.timeout;

            let handle = tokio::spawn(async move {
                let start = Instant::now();
                let topic = node.topic().to_string();
                let mode = node.mode();

                let result = tokio::time::timeout(timeout, async {
                    match &node {
                        ExecutionNode::Text { topic, query, k } => {
                            adapter.execute_text(topic, query, *k).await
                        }
                        ExecutionNode::Vector { topic, query, k } => {
                            adapter.execute_vector(topic, query, *k).await
                        }
                        ExecutionNode::Sql { topic, sql, limit } => {
                            adapter.execute_sql(topic, sql, *limit).await
                        }
                        ExecutionNode::Fetch {
                            topic,
                            partition,
                            offset,
                            max_bytes,
                        } => {
                            adapter.execute_fetch(topic, *partition, *offset, *max_bytes).await
                        }
                    }
                })
                .await;

                let elapsed_ms = start.elapsed().as_millis() as u64;

                match result {
                    Ok(Ok(candidates)) => {
                        let count = candidates.len();
                        (
                            candidates,
                            BackendStats {
                                backend: mode.to_string(),
                                topic,
                                candidates: count,
                                latency_ms: elapsed_ms,
                                timed_out: false,
                            },
                        )
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(
                            backend = %mode,
                            topic = %topic,
                            error = %e,
                            "Backend execution failed"
                        );
                        (
                            Vec::new(),
                            BackendStats {
                                backend: mode.to_string(),
                                topic,
                                candidates: 0,
                                latency_ms: elapsed_ms,
                                timed_out: false,
                            },
                        )
                    }
                    Err(_) => {
                        tracing::warn!(
                            backend = %mode,
                            topic = %topic,
                            timeout_ms = %elapsed_ms,
                            "Backend timed out"
                        );
                        (
                            Vec::new(),
                            BackendStats {
                                backend: mode.to_string(),
                                topic,
                                candidates: 0,
                                latency_ms: elapsed_ms,
                                timed_out: true,
                            },
                        )
                    }
                }
            });

            handles.push(handle);
        }

        // Collect all results
        let mut all_candidates = Vec::new();
        let mut all_stats = Vec::new();

        for handle in handles {
            match handle.await {
                Ok((candidates, stats)) => {
                    all_candidates.extend(candidates);
                    all_stats.push(stats);
                }
                Err(e) => {
                    tracing::error!(error = %e, "Task join error");
                    all_stats.push(BackendStats {
                        backend: "unknown".to_string(),
                        topic: "unknown".to_string(),
                        candidates: 0,
                        latency_ms: 0,
                        timed_out: false,
                    });
                }
            }
        }

        (all_candidates, all_stats)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::time::Duration;

    /// Mock backend that returns predetermined candidates.
    struct MockAdapter {
        text_results: Vec<Candidate>,
        vector_results: Vec<Candidate>,
        sql_results: Vec<Candidate>,
    }

    impl MockAdapter {
        fn new() -> Self {
            Self {
                text_results: vec![
                    Candidate::new("logs".into(), 0, 10, QueryMode::Text, 0.95, 0)
                        .with_text_preview("payment failed error".into()),
                    Candidate::new("logs".into(), 0, 20, QueryMode::Text, 0.80, 1)
                        .with_text_preview("connection timeout".into()),
                    Candidate::new("logs".into(), 0, 30, QueryMode::Text, 0.70, 2)
                        .with_text_preview("disk full warning".into()),
                ],
                vector_results: vec![
                    Candidate::new("logs".into(), 0, 10, QueryMode::Vector, 0.88, 0)
                        .with_text_preview("payment failed error".into()),
                    Candidate::new("logs".into(), 0, 40, QueryMode::Vector, 0.75, 1)
                        .with_text_preview("credit card declined".into()),
                ],
                sql_results: vec![
                    Candidate::new("orders".into(), 0, 100, QueryMode::Sql, 1.0, 0),
                    Candidate::new("orders".into(), 0, 101, QueryMode::Sql, 1.0, 1),
                ],
            }
        }
    }

    #[async_trait::async_trait]
    impl BackendAdapter for MockAdapter {
        async fn execute_text(
            &self,
            _topic: &str,
            _query: &str,
            _k: usize,
        ) -> Result<Vec<Candidate>, BackendError> {
            Ok(self.text_results.clone())
        }

        async fn execute_vector(
            &self,
            _topic: &str,
            _query: &str,
            _k: usize,
        ) -> Result<Vec<Candidate>, BackendError> {
            Ok(self.vector_results.clone())
        }

        async fn execute_sql(
            &self,
            _topic: &str,
            _sql: &str,
            _limit: usize,
        ) -> Result<Vec<Candidate>, BackendError> {
            Ok(self.sql_results.clone())
        }

        async fn execute_fetch(
            &self,
            topic: &str,
            partition: i32,
            offset: i64,
            _max_bytes: usize,
        ) -> Result<Vec<Candidate>, BackendError> {
            Ok(vec![Candidate::new(
                topic.into(),
                partition,
                offset,
                QueryMode::Fetch,
                1.0,
                0,
            )])
        }
    }

    fn make_request(modes: Vec<QueryMode>) -> QueryRequest {
        QueryRequest {
            sources: vec![SourceSpec {
                topic: "logs".into(),
                modes,
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

    fn make_plan(modes: Vec<QueryMode>) -> QueryPlan {
        let mut nodes = Vec::new();
        for mode in modes {
            match mode {
                QueryMode::Text => nodes.push(ExecutionNode::Text {
                    topic: "logs".into(),
                    query: "payment failed".into(),
                    k: 30,
                }),
                QueryMode::Vector => nodes.push(ExecutionNode::Vector {
                    topic: "logs".into(),
                    query: "card declined".into(),
                    k: 30,
                }),
                QueryMode::Sql => nodes.push(ExecutionNode::Sql {
                    topic: "orders".into(),
                    sql: "SELECT * FROM orders".into(),
                    limit: 30,
                }),
                QueryMode::Fetch => nodes.push(ExecutionNode::Fetch {
                    topic: "logs".into(),
                    partition: 0,
                    offset: 0,
                    max_bytes: 1_048_576,
                }),
            }
        }

        QueryPlan {
            nodes,
            timeout: Duration::from_secs(5),
            k: 10,
        }
    }

    #[tokio::test]
    async fn test_single_backend_query() {
        let adapter = Arc::new(MockAdapter::new());
        let profiles = Arc::new(ProfileStore::new());
        let orchestrator = QueryOrchestrator::new(adapter, profiles);

        let request = make_request(vec![QueryMode::Text]);
        let plan = make_plan(vec![QueryMode::Text]);

        let response = orchestrator.execute(&request, plan).await.unwrap();

        assert_eq!(response.results.len(), 3);
        assert!(response.stats.latency_ms < 1000);
        assert_eq!(response.stats.backends.len(), 1);
        assert_eq!(response.stats.backends[0].backend, "text");
    }

    #[tokio::test]
    async fn test_hybrid_query_with_rrf() {
        let adapter = Arc::new(MockAdapter::new());
        let profiles = Arc::new(ProfileStore::new());
        let orchestrator = QueryOrchestrator::new(adapter, profiles);

        let request = make_request(vec![QueryMode::Text, QueryMode::Vector]);
        let plan = make_plan(vec![QueryMode::Text, QueryMode::Vector]);

        let response = orchestrator.execute(&request, plan).await.unwrap();

        // Offset 10 was found by both text and vector, should rank high
        assert!(!response.results.is_empty());
        assert_eq!(response.stats.backends.len(), 2);

        // The result at offset 10 should have high RRF score (multi-backend)
        let offset_10 = response.results.iter().find(|r| r.offset == 10);
        assert!(offset_10.is_some());
    }

    #[tokio::test]
    async fn test_multi_topic_query() {
        let adapter = Arc::new(MockAdapter::new());
        let profiles = Arc::new(ProfileStore::new());
        let orchestrator = QueryOrchestrator::new(adapter, profiles);

        let request = QueryRequest {
            sources: vec![
                SourceSpec {
                    topic: "logs".into(),
                    modes: vec![QueryMode::Text],
                },
                SourceSpec {
                    topic: "orders".into(),
                    modes: vec![QueryMode::Sql],
                },
            ],
            q: QuerySpec {
                text: Some("payment".into()),
                semantic: None,
                sql: Some("SELECT * FROM orders".into()),
                fetch: None,
            },
            filters: None,
            k: 20,
            rank: None,
            timeout_ms: None,
            result_format: ResultFormat::Merged,
        };

        let plan = make_plan(vec![QueryMode::Text, QueryMode::Sql]);
        let response = orchestrator.execute(&request, plan).await.unwrap();

        // Should have results from both topics
        assert!(!response.results.is_empty());
        let topics: Vec<&str> = response.results.iter().map(|r| r.topic.as_str()).collect();
        assert!(topics.contains(&"logs") || topics.contains(&"orders"));
    }

    #[tokio::test]
    async fn test_timeout_handling() {
        struct SlowAdapter;

        #[async_trait::async_trait]
        impl BackendAdapter for SlowAdapter {
            async fn execute_text(
                &self,
                _topic: &str,
                _query: &str,
                _k: usize,
            ) -> Result<Vec<Candidate>, BackendError> {
                tokio::time::sleep(Duration::from_secs(10)).await;
                Ok(vec![])
            }

            async fn execute_vector(
                &self,
                _topic: &str,
                _query: &str,
                _k: usize,
            ) -> Result<Vec<Candidate>, BackendError> {
                Ok(vec![Candidate::new(
                    "logs".into(),
                    0,
                    1,
                    QueryMode::Vector,
                    0.9,
                    0,
                )])
            }

            async fn execute_sql(
                &self,
                _: &str,
                _: &str,
                _: usize,
            ) -> Result<Vec<Candidate>, BackendError> {
                Ok(vec![])
            }

            async fn execute_fetch(
                &self,
                _: &str,
                _: i32,
                _: i64,
                _: usize,
            ) -> Result<Vec<Candidate>, BackendError> {
                Ok(vec![])
            }
        }

        let adapter = Arc::new(SlowAdapter);
        let profiles = Arc::new(ProfileStore::new());
        let orchestrator = QueryOrchestrator::new(adapter, profiles);

        let request = make_request(vec![QueryMode::Text, QueryMode::Vector]);
        let plan = QueryPlan {
            nodes: vec![
                ExecutionNode::Text {
                    topic: "logs".into(),
                    query: "test".into(),
                    k: 10,
                },
                ExecutionNode::Vector {
                    topic: "logs".into(),
                    query: "test".into(),
                    k: 10,
                },
            ],
            timeout: Duration::from_millis(100), // Very short timeout
            k: 10,
        };

        let response = orchestrator.execute(&request, plan).await.unwrap();

        // Vector should succeed, text should timeout
        let text_stats = response
            .stats
            .backends
            .iter()
            .find(|s| s.backend == "text")
            .unwrap();
        assert!(text_stats.timed_out);

        let vector_stats = response
            .stats
            .backends
            .iter()
            .find(|s| s.backend == "vector")
            .unwrap();
        assert!(!vector_stats.timed_out);

        // Should still have the vector result
        assert_eq!(response.results.len(), 1);
    }

    #[tokio::test]
    async fn test_empty_results() {
        struct EmptyAdapter;

        #[async_trait::async_trait]
        impl BackendAdapter for EmptyAdapter {
            async fn execute_text(&self, _: &str, _: &str, _: usize) -> Result<Vec<Candidate>, BackendError> {
                Ok(vec![])
            }
            async fn execute_vector(&self, _: &str, _: &str, _: usize) -> Result<Vec<Candidate>, BackendError> {
                Ok(vec![])
            }
            async fn execute_sql(&self, _: &str, _: &str, _: usize) -> Result<Vec<Candidate>, BackendError> {
                Ok(vec![])
            }
            async fn execute_fetch(&self, _: &str, _: i32, _: i64, _: usize) -> Result<Vec<Candidate>, BackendError> {
                Ok(vec![])
            }
        }

        let adapter = Arc::new(EmptyAdapter);
        let profiles = Arc::new(ProfileStore::new());
        let orchestrator = QueryOrchestrator::new(adapter, profiles);

        let request = make_request(vec![QueryMode::Text]);
        let plan = make_plan(vec![QueryMode::Text]);

        let response = orchestrator.execute(&request, plan).await.unwrap();
        assert!(response.results.is_empty());
        assert_eq!(response.stats.candidates, 0);
    }

    #[tokio::test]
    async fn test_profile_switching() {
        let adapter = Arc::new(MockAdapter::new());
        let profiles = Arc::new(ProfileStore::new());
        let orchestrator = QueryOrchestrator::new(adapter, profiles);

        // Default profile
        let mut request = make_request(vec![QueryMode::Text]);
        let plan = make_plan(vec![QueryMode::Text]);
        let response_default = orchestrator.execute(&request, plan).await.unwrap();

        // Relevance profile
        request.rank = Some(RankSpec {
            profile: "relevance".into(),
        });
        let plan2 = make_plan(vec![QueryMode::Text]);
        let response_relevance = orchestrator.execute(&request, plan2).await.unwrap();

        // Both should return results, but scores may differ due to different weights
        assert_eq!(response_default.results.len(), response_relevance.results.len());
    }
}
