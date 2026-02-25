//! Query plan construction from a `QueryRequest`.
//!
//! The `QueryPlanner` takes a `QueryRequest` and topic capabilities,
//! then builds a `QueryPlan` of `ExecutionNode`s for the orchestrator
//! to execute in parallel.

use crate::capabilities::TopicCapabilities;
use crate::types::{QueryMode, QueryRequest};
use std::collections::HashMap;
use std::time::Duration;

/// A plan describing which backends to query and how.
#[derive(Debug)]
pub struct QueryPlan {
    /// Execution nodes to run in parallel
    pub nodes: Vec<ExecutionNode>,
    /// Overall query timeout
    pub timeout: Duration,
    /// Number of results requested
    pub k: usize,
}

/// A single backend query to execute.
#[derive(Debug, Clone)]
pub enum ExecutionNode {
    /// Full-text search via Tantivy
    Text {
        topic: String,
        query: String,
        k: usize,
    },
    /// Semantic vector search via HNSW
    Vector {
        topic: String,
        query: String,
        k: usize,
    },
    /// SQL query via DataFusion
    Sql {
        topic: String,
        sql: String,
        limit: usize,
    },
    /// Direct offset fetch from WAL
    Fetch {
        topic: String,
        partition: i32,
        offset: i64,
        max_bytes: usize,
    },
}

impl ExecutionNode {
    /// Get the topic this node queries.
    pub fn topic(&self) -> &str {
        match self {
            ExecutionNode::Text { topic, .. } => topic,
            ExecutionNode::Vector { topic, .. } => topic,
            ExecutionNode::Sql { topic, .. } => topic,
            ExecutionNode::Fetch { topic, .. } => topic,
        }
    }

    /// Get the query mode of this node.
    pub fn mode(&self) -> QueryMode {
        match self {
            ExecutionNode::Text { .. } => QueryMode::Text,
            ExecutionNode::Vector { .. } => QueryMode::Vector,
            ExecutionNode::Sql { .. } => QueryMode::Sql,
            ExecutionNode::Fetch { .. } => QueryMode::Fetch,
        }
    }
}

/// Builds a `QueryPlan` from a request and topic capabilities.
pub struct QueryPlanner;

impl QueryPlanner {
    /// Plan a query by matching requested modes against topic capabilities.
    ///
    /// For each source in the request:
    /// 1. Check if the topic exists
    /// 2. For each requested mode, verify the topic supports it
    /// 3. Create an `ExecutionNode` with the appropriate query parameters
    ///
    /// Modes that the topic doesn't support are silently skipped (logged as warnings).
    pub fn plan(
        request: &QueryRequest,
        capabilities: &HashMap<String, TopicCapabilities>,
    ) -> Result<QueryPlan, PlanError> {
        if request.sources.is_empty() {
            return Err(PlanError::NoSources);
        }

        let mut nodes = Vec::new();
        let k = request.k;
        // Request more candidates per-backend than final k to give RRF enough to work with
        let per_backend_k = k * 3;

        for source in &request.sources {
            let topic_caps = capabilities.get(&source.topic);

            if topic_caps.is_none() {
                tracing::warn!(topic = %source.topic, "Topic not found, skipping");
                continue;
            }
            let topic_caps = topic_caps.unwrap();

            for mode in &source.modes {
                if !topic_caps.supports(*mode) {
                    tracing::warn!(
                        topic = %source.topic,
                        mode = %mode,
                        "Topic does not support requested mode, skipping"
                    );
                    continue;
                }

                match mode {
                    QueryMode::Text => {
                        if let Some(ref text) = request.q.text {
                            nodes.push(ExecutionNode::Text {
                                topic: source.topic.clone(),
                                query: text.clone(),
                                k: per_backend_k,
                            });
                        } else {
                            tracing::warn!(
                                topic = %source.topic,
                                "Text mode requested but no text query provided"
                            );
                        }
                    }
                    QueryMode::Vector => {
                        // Use semantic query, falling back to text query for embedding
                        let query = request.q.semantic.as_ref()
                            .or(request.q.text.as_ref());
                        if let Some(q) = query {
                            nodes.push(ExecutionNode::Vector {
                                topic: source.topic.clone(),
                                query: q.clone(),
                                k: per_backend_k,
                            });
                        } else {
                            tracing::warn!(
                                topic = %source.topic,
                                "Vector mode requested but no semantic/text query provided"
                            );
                        }
                    }
                    QueryMode::Sql => {
                        if let Some(ref sql) = request.q.sql {
                            nodes.push(ExecutionNode::Sql {
                                topic: source.topic.clone(),
                                sql: sql.clone(),
                                limit: per_backend_k,
                            });
                        } else {
                            tracing::warn!(
                                topic = %source.topic,
                                "SQL mode requested but no SQL query provided"
                            );
                        }
                    }
                    QueryMode::Fetch => {
                        if let Some(ref fetch) = request.q.fetch {
                            nodes.push(ExecutionNode::Fetch {
                                topic: source.topic.clone(),
                                partition: fetch.partition.unwrap_or(0),
                                offset: fetch.offset,
                                max_bytes: fetch.max_bytes,
                            });
                        } else {
                            tracing::warn!(
                                topic = %source.topic,
                                "Fetch mode requested but no fetch parameters provided"
                            );
                        }
                    }
                }
            }
        }

        if nodes.is_empty() {
            return Err(PlanError::NoExecutableNodes);
        }

        let timeout_ms = request.timeout_ms.unwrap_or(5000).max(100);

        Ok(QueryPlan {
            nodes,
            timeout: Duration::from_millis(timeout_ms),
            k,
        })
    }
}

/// Errors that can occur during query planning.
#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    #[error("No sources specified in query request")]
    NoSources,
    #[error("No executable nodes could be created (all modes unsupported or missing query parameters)")]
    NoExecutableNodes,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{SourceSpec, QuerySpec, RankSpec, ResultFormat};

    fn make_caps() -> HashMap<String, TopicCapabilities> {
        let mut caps = HashMap::new();
        caps.insert("logs".into(), TopicCapabilities {
            topic: "logs".into(),
            text_search: true,
            vector_search: true,
            sql_query: false,
            fetch: true,
        });
        caps.insert("orders".into(), TopicCapabilities {
            topic: "orders".into(),
            text_search: false,
            vector_search: false,
            sql_query: true,
            fetch: true,
        });
        caps
    }

    #[test]
    fn test_plan_text_and_vector() {
        let request = QueryRequest {
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
        };

        let plan = QueryPlanner::plan(&request, &make_caps()).unwrap();
        assert_eq!(plan.nodes.len(), 2);
        assert!(matches!(plan.nodes[0], ExecutionNode::Text { .. }));
        assert!(matches!(plan.nodes[1], ExecutionNode::Vector { .. }));
    }

    #[test]
    fn test_plan_skips_unsupported_mode() {
        let request = QueryRequest {
            sources: vec![SourceSpec {
                topic: "orders".into(),
                modes: vec![QueryMode::Text, QueryMode::Sql],
            }],
            q: QuerySpec {
                text: Some("test".into()),
                semantic: None,
                sql: Some("SELECT * FROM orders LIMIT 10".into()),
                fetch: None,
            },
            filters: None,
            k: 10,
            rank: None,
            timeout_ms: None,
            result_format: ResultFormat::Merged,
        };

        let plan = QueryPlanner::plan(&request, &make_caps()).unwrap();
        // Only SQL should be created (text not supported on orders)
        assert_eq!(plan.nodes.len(), 1);
        assert!(matches!(plan.nodes[0], ExecutionNode::Sql { .. }));
    }

    #[test]
    fn test_plan_multi_topic() {
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
                sql: Some("SELECT * FROM orders WHERE status = 'failed'".into()),
                fetch: None,
            },
            filters: None,
            k: 20,
            rank: None,
            timeout_ms: None,
            result_format: ResultFormat::Merged,
        };

        let plan = QueryPlanner::plan(&request, &make_caps()).unwrap();
        assert_eq!(plan.nodes.len(), 2);
    }

    #[test]
    fn test_plan_no_sources_error() {
        let request = QueryRequest {
            sources: vec![],
            q: QuerySpec {
                text: None,
                semantic: None,
                sql: None,
                fetch: None,
            },
            filters: None,
            k: 10,
            rank: None,
            timeout_ms: None,
            result_format: ResultFormat::Merged,
        };

        let result = QueryPlanner::plan(&request, &make_caps());
        assert!(matches!(result, Err(PlanError::NoSources)));
    }

    #[test]
    fn test_plan_unknown_topic() {
        let request = QueryRequest {
            sources: vec![SourceSpec {
                topic: "nonexistent".into(),
                modes: vec![QueryMode::Text],
            }],
            q: QuerySpec {
                text: Some("test".into()),
                semantic: None,
                sql: None,
                fetch: None,
            },
            filters: None,
            k: 10,
            rank: None,
            timeout_ms: None,
            result_format: ResultFormat::Merged,
        };

        let result = QueryPlanner::plan(&request, &make_caps());
        assert!(matches!(result, Err(PlanError::NoExecutableNodes)));
    }
}
