//! End-to-end integration tests for the query orchestrator pipeline.
//!
//! Tests the full flow: plan → parallel execute → RRF fuse → rank → response,
//! including multi-topic queries, result formats, profile switching,
//! timeout handling, and feature logging.

use chronik_query::{
    candidate::Candidate,
    capabilities::TopicCapabilities,
    features::FeatureLogger,
    orchestrator::{BackendAdapter, BackendError, QueryOrchestrator},
    plan::QueryPlanner,
    profiles::ProfileStore,
    types::*,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Mock backend: returns deterministic results for testing
// ---------------------------------------------------------------------------

struct IntegrationMockAdapter;

#[async_trait::async_trait]
impl BackendAdapter for IntegrationMockAdapter {
    async fn execute_text(
        &self,
        topic: &str,
        _query: &str,
        k: usize,
    ) -> Result<Vec<Candidate>, BackendError> {
        let mut results = Vec::new();
        for i in 0..k.min(5) {
            let mut c = Candidate::new(
                topic.to_string(),
                0,
                (i * 10) as i64,
                QueryMode::Text,
                1.0 - (i as f64 * 0.1),
                i,
            );
            c = c.with_timestamp(1700000000 + (i as i64 * 1000));
            c = c.with_text_preview(format!("text result {} for {}", i, topic));
            results.push(c);
        }
        Ok(results)
    }

    async fn execute_vector(
        &self,
        topic: &str,
        _query: &str,
        k: usize,
    ) -> Result<Vec<Candidate>, BackendError> {
        let mut results = Vec::new();
        for i in 0..k.min(4) {
            // Overlap: offsets 0 and 10 match text results (for RRF testing)
            let offset = (i * 10) as i64;
            let c = Candidate::new(
                topic.to_string(),
                0,
                offset,
                QueryMode::Vector,
                0.95 - (i as f64 * 0.15),
                i,
            )
            .with_timestamp(1700000000 + (i as i64 * 1000));
            results.push(c);
        }
        Ok(results)
    }

    async fn execute_sql(
        &self,
        topic: &str,
        _sql: &str,
        limit: usize,
    ) -> Result<Vec<Candidate>, BackendError> {
        let mut results = Vec::new();
        for i in 0..limit.min(3) {
            let c = Candidate::new(
                topic.to_string(),
                0,
                (100 + i) as i64,
                QueryMode::Sql,
                1.0,
                i,
            )
            .with_timestamp(1700000000 + (i as i64 * 500));
            results.push(c);
        }
        Ok(results)
    }

    async fn execute_fetch(
        &self,
        topic: &str,
        partition: i32,
        offset: i64,
        _max_bytes: usize,
    ) -> Result<Vec<Candidate>, BackendError> {
        let mut results = Vec::new();
        for i in 0..3 {
            let c = Candidate::new(
                topic.to_string(),
                partition,
                offset + i,
                QueryMode::Fetch,
                1.0,
                i as usize,
            );
            results.push(c);
        }
        Ok(results)
    }
}

fn make_capabilities() -> HashMap<String, TopicCapabilities> {
    let mut caps = HashMap::new();
    caps.insert(
        "logs".into(),
        TopicCapabilities {
            topic: "logs".into(),
            text_search: true,
            vector_search: true,
            sql_query: false,
            fetch: true,
        },
    );
    caps.insert(
        "orders".into(),
        TopicCapabilities {
            topic: "orders".into(),
            text_search: false,
            vector_search: false,
            sql_query: true,
            fetch: true,
        },
    );
    caps.insert(
        "events".into(),
        TopicCapabilities {
            topic: "events".into(),
            text_search: true,
            vector_search: false,
            sql_query: false,
            fetch: true,
        },
    );
    caps
}

fn make_orchestrator() -> QueryOrchestrator {
    let adapter = Arc::new(IntegrationMockAdapter);
    let profiles = Arc::new(ProfileStore::new());
    QueryOrchestrator::new(adapter, profiles)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_single_topic_text_only() {
    let request = QueryRequest {
        sources: vec![SourceSpec {
            topic: "logs".into(),
            modes: vec![QueryMode::Text],
        }],
        q: QuerySpec {
            text: Some("error occurred".into()),
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

    let caps = make_capabilities();
    let plan = QueryPlanner::plan(&request, &caps).unwrap();
    assert_eq!(plan.nodes.len(), 1);

    let orchestrator = make_orchestrator();
    let response = orchestrator.execute(&request, plan).await.unwrap();

    assert_eq!(response.results.len(), 5);
    assert!(response.stats.latency_ms < 5000);
    assert_eq!(response.stats.backends.len(), 1);
    assert_eq!(response.stats.backends[0].backend, "text");
    assert!(!response.stats.backends[0].timed_out);

    // Results should be ranked by score (highest first)
    for i in 1..response.results.len() {
        assert!(
            response.results[i - 1].final_score >= response.results[i].final_score,
            "Results should be sorted by final_score descending"
        );
    }

    // Each result should have features
    for r in &response.results {
        assert!(r.features.contains_key("rrf_score"), "Missing rrf_score feature");
    }
}

#[tokio::test]
async fn test_hybrid_text_vector_rrf_boost() {
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

    let caps = make_capabilities();
    let plan = QueryPlanner::plan(&request, &caps).unwrap();
    assert_eq!(plan.nodes.len(), 2, "Should have text + vector nodes");

    let orchestrator = make_orchestrator();
    let response = orchestrator.execute(&request, plan).await.unwrap();

    assert_eq!(response.stats.backends.len(), 2);

    // Offsets 0 and 10 appear in both text and vector results
    // These should have source_count > 1 and higher RRF scores
    let offset_0 = response.results.iter().find(|r| r.offset == 0);
    assert!(offset_0.is_some(), "Offset 0 should appear in results (both backends)");

    let offset_0 = offset_0.unwrap();
    let source_count = offset_0.features.get("source_count").copied().unwrap_or(0.0);
    assert!(
        source_count >= 2.0,
        "Offset 0 found by both backends, source_count should be >= 2 but was {}",
        source_count
    );

    // Multi-source candidates should rank higher than single-source
    let single_source = response
        .results
        .iter()
        .find(|r| r.features.get("source_count").copied().unwrap_or(0.0) < 2.0);
    if let Some(single) = single_source {
        assert!(
            offset_0.final_score >= single.final_score,
            "Multi-source result (score={}) should rank >= single-source (score={})",
            offset_0.final_score,
            single.final_score
        );
    }
}

#[tokio::test]
async fn test_multi_topic_query() {
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
            SourceSpec {
                topic: "events".into(),
                modes: vec![QueryMode::Text],
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

    let caps = make_capabilities();
    let plan = QueryPlanner::plan(&request, &caps).unwrap();
    assert_eq!(plan.nodes.len(), 3, "Should have 3 execution nodes");

    let orchestrator = make_orchestrator();
    let response = orchestrator.execute(&request, plan).await.unwrap();

    assert_eq!(response.stats.backends.len(), 3);

    // Verify we have results from multiple topics
    let topics: Vec<&str> = response.results.iter().map(|r| r.topic.as_str()).collect();
    assert!(topics.contains(&"logs"), "Should have logs results");
    assert!(topics.contains(&"orders"), "Should have orders results");
    assert!(topics.contains(&"events"), "Should have events results");
}

#[tokio::test]
async fn test_grouped_result_format() {
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
            text: Some("test".into()),
            semantic: None,
            sql: Some("SELECT * FROM orders".into()),
            fetch: None,
        },
        filters: None,
        k: 20,
        rank: None,
        timeout_ms: None,
        result_format: ResultFormat::Grouped,
    };

    let caps = make_capabilities();
    let plan = QueryPlanner::plan(&request, &caps).unwrap();
    let orchestrator = make_orchestrator();
    let response = orchestrator.execute(&request, plan).await.unwrap();

    // Merged results should be empty for grouped format
    assert!(
        response.results.is_empty(),
        "Merged results should be empty for grouped format"
    );

    // Grouped results should be populated
    let groups = response
        .grouped_results
        .as_ref()
        .expect("grouped_results should be Some for grouped format");

    assert!(groups.contains_key("logs"), "Should have logs group");
    assert!(groups.contains_key("orders"), "Should have orders group");
    assert!(!groups["logs"].is_empty(), "Logs group should have results");
    assert!(!groups["orders"].is_empty(), "Orders group should have results");

    // Stats should reflect total ranked count across groups
    let total: usize = groups.values().map(|v| v.len()).sum();
    assert_eq!(response.stats.ranked, total);
}

#[tokio::test]
async fn test_merged_result_format() {
    let request = QueryRequest {
        sources: vec![SourceSpec {
            topic: "logs".into(),
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

    let caps = make_capabilities();
    let plan = QueryPlanner::plan(&request, &caps).unwrap();
    let orchestrator = make_orchestrator();
    let response = orchestrator.execute(&request, plan).await.unwrap();

    assert!(!response.results.is_empty(), "Merged results should be populated");
    assert!(
        response.grouped_results.is_none(),
        "grouped_results should be None for merged format"
    );
}

#[tokio::test]
async fn test_profile_switching_affects_ranking() {
    let adapter = Arc::new(IntegrationMockAdapter);
    let profiles = Arc::new(ProfileStore::new());
    let orchestrator = QueryOrchestrator::new(adapter, profiles);

    let make_req = |profile: &str| QueryRequest {
        sources: vec![SourceSpec {
            topic: "logs".into(),
            modes: vec![QueryMode::Text, QueryMode::Vector],
        }],
        q: QuerySpec {
            text: Some("test".into()),
            semantic: Some("test".into()),
            sql: None,
            fetch: None,
        },
        filters: None,
        k: 10,
        rank: Some(RankSpec {
            profile: profile.to_string(),
        }),
        timeout_ms: None,
        result_format: ResultFormat::Merged,
    };

    let caps = make_capabilities();

    // Run with default profile
    let plan_default = QueryPlanner::plan(&make_req("default"), &caps).unwrap();
    let resp_default = orchestrator
        .execute(&make_req("default"), plan_default)
        .await
        .unwrap();

    // Run with freshness profile
    let plan_fresh = QueryPlanner::plan(&make_req("freshness"), &caps).unwrap();
    let resp_fresh = orchestrator
        .execute(&make_req("freshness"), plan_fresh)
        .await
        .unwrap();

    // Run with relevance profile
    let plan_rel = QueryPlanner::plan(&make_req("relevance"), &caps).unwrap();
    let resp_rel = orchestrator
        .execute(&make_req("relevance"), plan_rel)
        .await
        .unwrap();

    // All should return the same number of results
    assert_eq!(resp_default.results.len(), resp_fresh.results.len());
    assert_eq!(resp_default.results.len(), resp_rel.results.len());

    // But final_scores may differ due to different weight distributions
    // (This is a soft assertion — the mock data may not always produce different orderings,
    // but the scores should at least differ)
    let default_scores: Vec<f64> = resp_default.results.iter().map(|r| r.final_score).collect();
    let fresh_scores: Vec<f64> = resp_fresh.results.iter().map(|r| r.final_score).collect();
    let rel_scores: Vec<f64> = resp_rel.results.iter().map(|r| r.final_score).collect();

    // At least one pair of profiles should produce different scores
    assert!(
        default_scores != fresh_scores || default_scores != rel_scores,
        "Different profiles should produce different scores"
    );
}

#[tokio::test]
async fn test_timeout_returns_partial_results() {
    struct SlowTextAdapter;

    #[async_trait::async_trait]
    impl BackendAdapter for SlowTextAdapter {
        async fn execute_text(
            &self,
            _topic: &str,
            _query: &str,
            _k: usize,
        ) -> Result<Vec<Candidate>, BackendError> {
            // Simulate a very slow text search
            tokio::time::sleep(Duration::from_secs(10)).await;
            Ok(vec![])
        }

        async fn execute_vector(
            &self,
            topic: &str,
            _query: &str,
            _k: usize,
        ) -> Result<Vec<Candidate>, BackendError> {
            // Fast vector search
            Ok(vec![
                Candidate::new(topic.into(), 0, 42, QueryMode::Vector, 0.95, 0),
                Candidate::new(topic.into(), 0, 43, QueryMode::Vector, 0.90, 1),
            ])
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

    let adapter = Arc::new(SlowTextAdapter);
    let profiles = Arc::new(ProfileStore::new());
    let orchestrator = QueryOrchestrator::new(adapter, profiles);

    let request = QueryRequest {
        sources: vec![SourceSpec {
            topic: "logs".into(),
            modes: vec![QueryMode::Text, QueryMode::Vector],
        }],
        q: QuerySpec {
            text: Some("test".into()),
            semantic: Some("test".into()),
            sql: None,
            fetch: None,
        },
        filters: None,
        k: 10,
        rank: None,
        timeout_ms: Some(200), // Short timeout
        result_format: ResultFormat::Merged,
    };

    let caps = make_capabilities();
    let plan = QueryPlanner::plan(&request, &caps).unwrap();

    // Override plan timeout to be short
    let plan = chronik_query::plan::QueryPlan {
        timeout: Duration::from_millis(200),
        ..plan
    };

    let response = orchestrator.execute(&request, plan).await.unwrap();

    // Text should timeout, vector should succeed
    let text_stat = response
        .stats
        .backends
        .iter()
        .find(|s| s.backend == "text");
    let vector_stat = response
        .stats
        .backends
        .iter()
        .find(|s| s.backend == "vector");

    assert!(text_stat.unwrap().timed_out, "Text backend should timeout");
    assert!(
        !vector_stat.unwrap().timed_out,
        "Vector backend should not timeout"
    );

    // Should still have vector results
    assert_eq!(
        response.results.len(),
        2,
        "Should have 2 results from vector (text timed out)"
    );
}

#[tokio::test]
async fn test_missing_capability_skipped() {
    let request = QueryRequest {
        sources: vec![SourceSpec {
            topic: "orders".into(),
            modes: vec![QueryMode::Text, QueryMode::Sql],
        }],
        q: QuerySpec {
            text: Some("test".into()),
            semantic: None,
            sql: Some("SELECT * FROM orders".into()),
            fetch: None,
        },
        filters: None,
        k: 10,
        rank: None,
        timeout_ms: None,
        result_format: ResultFormat::Merged,
    };

    let caps = make_capabilities();
    // orders has text_search: false, so text mode should be skipped
    let plan = QueryPlanner::plan(&request, &caps).unwrap();
    assert_eq!(
        plan.nodes.len(),
        1,
        "Only SQL should be planned (text not supported on orders)"
    );

    let orchestrator = make_orchestrator();
    let response = orchestrator.execute(&request, plan).await.unwrap();

    assert_eq!(response.stats.backends.len(), 1);
    assert_eq!(response.stats.backends[0].backend, "sql");
    assert!(!response.results.is_empty());
}

#[tokio::test]
async fn test_empty_results() {
    struct EmptyAdapter;

    #[async_trait::async_trait]
    impl BackendAdapter for EmptyAdapter {
        async fn execute_text(
            &self,
            _: &str,
            _: &str,
            _: usize,
        ) -> Result<Vec<Candidate>, BackendError> {
            Ok(vec![])
        }
        async fn execute_vector(
            &self,
            _: &str,
            _: &str,
            _: usize,
        ) -> Result<Vec<Candidate>, BackendError> {
            Ok(vec![])
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

    let adapter = Arc::new(EmptyAdapter);
    let profiles = Arc::new(ProfileStore::new());
    let orchestrator = QueryOrchestrator::new(adapter, profiles);

    let request = QueryRequest {
        sources: vec![SourceSpec {
            topic: "logs".into(),
            modes: vec![QueryMode::Text],
        }],
        q: QuerySpec {
            text: Some("nonexistent".into()),
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

    let caps = make_capabilities();
    let plan = QueryPlanner::plan(&request, &caps).unwrap();
    let response = orchestrator.execute(&request, plan).await.unwrap();

    assert!(response.results.is_empty());
    assert_eq!(response.stats.candidates, 0);
    assert_eq!(response.stats.ranked, 0);
}

#[tokio::test]
async fn test_feature_logging_connected() {
    let adapter = Arc::new(IntegrationMockAdapter);
    let profiles = Arc::new(ProfileStore::new());
    let logger = Arc::new(FeatureLogger::new());

    let orchestrator = QueryOrchestrator::new(adapter, profiles)
        .with_feature_logger(logger.clone());

    let request = QueryRequest {
        sources: vec![SourceSpec {
            topic: "logs".into(),
            modes: vec![QueryMode::Text],
        }],
        q: QuerySpec {
            text: Some("test".into()),
            semantic: None,
            sql: None,
            fetch: None,
        },
        filters: None,
        k: 5,
        rank: None,
        timeout_ms: None,
        result_format: ResultFormat::Merged,
    };

    let caps = make_capabilities();
    let plan = QueryPlanner::plan(&request, &caps).unwrap();
    let response = orchestrator.execute(&request, plan).await.unwrap();

    // If feature logging works, results should have features
    assert!(!response.results.is_empty());
    for r in &response.results {
        assert!(
            !r.features.is_empty(),
            "Each result should have features for logging"
        );
    }

    // The logger runs asynchronously — give it a moment to process
    tokio::time::sleep(Duration::from_millis(50)).await;
    // No panic or error means logging worked
}

#[tokio::test]
async fn test_query_response_has_query_id() {
    let request = QueryRequest {
        sources: vec![SourceSpec {
            topic: "logs".into(),
            modes: vec![QueryMode::Text],
        }],
        q: QuerySpec {
            text: Some("test".into()),
            semantic: None,
            sql: None,
            fetch: None,
        },
        filters: None,
        k: 5,
        rank: None,
        timeout_ms: None,
        result_format: ResultFormat::Merged,
    };

    let caps = make_capabilities();
    let plan = QueryPlanner::plan(&request, &caps).unwrap();
    let orchestrator = make_orchestrator();
    let response = orchestrator.execute(&request, plan).await.unwrap();

    // query_id should be a valid UUID
    assert!(
        !response.query_id.is_empty(),
        "query_id should be populated"
    );
    assert!(
        response.query_id.len() == 36,
        "query_id should be a UUID (36 chars), got: {}",
        response.query_id
    );
}

#[tokio::test]
async fn test_response_serializable_to_json() {
    let request = QueryRequest {
        sources: vec![
            SourceSpec {
                topic: "logs".into(),
                modes: vec![QueryMode::Text, QueryMode::Vector],
            },
            SourceSpec {
                topic: "orders".into(),
                modes: vec![QueryMode::Sql],
            },
        ],
        q: QuerySpec {
            text: Some("payment".into()),
            semantic: Some("card declined".into()),
            sql: Some("SELECT * FROM orders".into()),
            fetch: None,
        },
        filters: None,
        k: 10,
        rank: Some(RankSpec {
            profile: "relevance".into(),
        }),
        timeout_ms: None,
        result_format: ResultFormat::Merged,
    };

    let caps = make_capabilities();
    let plan = QueryPlanner::plan(&request, &caps).unwrap();
    let orchestrator = make_orchestrator();
    let response = orchestrator.execute(&request, plan).await.unwrap();

    // Serialize to JSON and verify structure
    let json = serde_json::to_value(&response).unwrap();

    assert!(json.get("query_id").is_some());
    assert!(json.get("results").is_some());
    assert!(json.get("stats").is_some());

    let stats = json.get("stats").unwrap();
    assert!(stats.get("candidates").is_some());
    assert!(stats.get("ranked").is_some());
    assert!(stats.get("latency_ms").is_some());
    assert!(stats.get("backends").is_some());

    let results = json.get("results").unwrap().as_array().unwrap();
    assert!(!results.is_empty());

    let first = &results[0];
    assert!(first.get("topic").is_some());
    assert!(first.get("partition").is_some());
    assert!(first.get("offset").is_some());
    assert!(first.get("final_score").is_some());
    assert!(first.get("features").is_some());
}

#[tokio::test]
async fn test_k_truncation() {
    let request = QueryRequest {
        sources: vec![SourceSpec {
            topic: "logs".into(),
            modes: vec![QueryMode::Text, QueryMode::Vector],
        }],
        q: QuerySpec {
            text: Some("test".into()),
            semantic: Some("test".into()),
            sql: None,
            fetch: None,
        },
        filters: None,
        k: 3, // Only want top 3
        rank: None,
        timeout_ms: None,
        result_format: ResultFormat::Merged,
    };

    let caps = make_capabilities();
    let plan = QueryPlanner::plan(&request, &caps).unwrap();
    let orchestrator = make_orchestrator();
    let response = orchestrator.execute(&request, plan).await.unwrap();

    assert!(
        response.results.len() <= 3,
        "Should return at most k={} results, got {}",
        3,
        response.results.len()
    );
    assert!(
        response.stats.candidates > response.results.len(),
        "Total candidates ({}) should be more than returned results ({})",
        response.stats.candidates,
        response.results.len()
    );
}
