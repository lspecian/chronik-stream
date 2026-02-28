//! Re-ranking providers for vector search quality improvement (VO-4).
//!
//! Provides a `RerankerProvider` trait for cross-encoder re-ranking after
//! initial HNSW/RRF retrieval. Typical NDCG@10 improvement: +10-20%.
//!
//! ## Usage
//!
//! ```text
//! HNSW search (k×5 candidates) → Reranker (score all) → Top-k results
//! ```
//!
//! ## Supported Providers
//!
//! - **External**: HTTP endpoint (Cohere Rerank, Jina Reranker, custom model)
//! - **NoOp**: Pass-through for testing (returns original order)

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

/// Result of re-ranking a single document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RerankResult {
    /// Index into the original documents array.
    pub index: usize,
    /// Relevance score from the cross-encoder (higher = more relevant).
    pub relevance_score: f32,
}

/// Trait for re-ranking providers.
///
/// Implementations score (query, document) pairs using a cross-encoder model
/// and return results sorted by relevance.
#[async_trait]
pub trait RerankerProvider: Send + Sync {
    /// Re-rank documents against a query.
    ///
    /// # Arguments
    /// * `query` - The search query text
    /// * `documents` - Documents to re-rank (text content)
    /// * `top_k` - Number of top results to return
    ///
    /// # Returns
    /// Results sorted by descending relevance score, truncated to top_k.
    async fn rerank(
        &self,
        query: &str,
        documents: &[&str],
        top_k: usize,
    ) -> Result<Vec<RerankResult>>;

    /// Provider name for logging/metrics.
    fn name(&self) -> &str;

    /// Maximum documents per rerank call.
    fn max_documents(&self) -> usize {
        1000
    }
}

/// External HTTP re-ranker (Cohere, Jina, or custom endpoint).
///
/// Sends POST requests to an HTTP endpoint with query + documents,
/// expects JSON response with scored results.
pub struct ExternalReranker {
    /// HTTP client
    client: reqwest::Client,
    /// Endpoint URL
    endpoint: String,
    /// Model name (sent in request body)
    model: String,
    /// API key (sent as Bearer token)
    api_key: Option<String>,
}

impl ExternalReranker {
    /// Create a new external reranker.
    pub fn new(endpoint: String, model: String, api_key: Option<String>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap_or_default();

        Self {
            client,
            endpoint,
            model,
            api_key,
        }
    }
}

/// Request body for external reranker API (Cohere-compatible format).
#[derive(Serialize)]
struct RerankRequest<'a> {
    model: &'a str,
    query: &'a str,
    documents: &'a [&'a str],
    top_n: usize,
}

/// Response from external reranker API.
#[derive(Deserialize)]
struct RerankResponse {
    results: Vec<RerankResponseItem>,
}

/// Single result item from reranker response.
#[derive(Deserialize)]
struct RerankResponseItem {
    index: usize,
    relevance_score: f32,
}

#[async_trait]
impl RerankerProvider for ExternalReranker {
    async fn rerank(
        &self,
        query: &str,
        documents: &[&str],
        top_k: usize,
    ) -> Result<Vec<RerankResult>> {
        if documents.is_empty() {
            return Ok(Vec::new());
        }

        let request = RerankRequest {
            model: &self.model,
            query,
            documents,
            top_n: top_k,
        };

        let mut req = self.client.post(&self.endpoint).json(&request);
        if let Some(ref key) = self.api_key {
            req = req.bearer_auth(key);
        }

        let response = req.send().await
            .map_err(|e| anyhow!("Reranker request failed: {}", e))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!("Reranker returned {}: {}", status, body));
        }

        let rerank_response: RerankResponse = response.json().await
            .map_err(|e| anyhow!("Failed to parse reranker response: {}", e))?;

        let results: Vec<RerankResult> = rerank_response.results
            .into_iter()
            .map(|item| RerankResult {
                index: item.index,
                relevance_score: item.relevance_score,
            })
            .collect();

        debug!(
            "Reranked {} documents, returning top {} results",
            documents.len(), results.len()
        );

        Ok(results)
    }

    fn name(&self) -> &str {
        "external"
    }
}

/// No-op reranker that returns documents in original order.
///
/// Used for testing or when re-ranking is disabled.
pub struct NoOpReranker;

#[async_trait]
impl RerankerProvider for NoOpReranker {
    async fn rerank(
        &self,
        _query: &str,
        documents: &[&str],
        top_k: usize,
    ) -> Result<Vec<RerankResult>> {
        let results: Vec<RerankResult> = (0..documents.len().min(top_k))
            .map(|i| RerankResult {
                index: i,
                relevance_score: 1.0 - (i as f32 / documents.len().max(1) as f32),
            })
            .collect();
        Ok(results)
    }

    fn name(&self) -> &str {
        "noop"
    }
}

/// Configuration for the reranker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RerankerConfig {
    /// Whether re-ranking is enabled.
    pub enabled: bool,
    /// Provider type.
    pub provider: RerankerProviderType,
    /// Model name (e.g., "rerank-v3.5" for Cohere).
    pub model: String,
    /// Endpoint URL for external provider.
    pub endpoint: Option<String>,
    /// API key (or read from CHRONIK_RERANKER_API_KEY env).
    pub api_key: Option<String>,
    /// Overretrieval factor: HNSW fetches k * overretrieval_factor candidates.
    pub overretrieval_factor: usize,
}

impl Default for RerankerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: RerankerProviderType::External,
            model: "rerank-v3.5".to_string(),
            endpoint: None,
            api_key: None,
            overretrieval_factor: 5,
        }
    }
}

impl RerankerConfig {
    /// Parse reranker configuration from topic config HashMap.
    pub fn from_topic_config(config: &std::collections::HashMap<String, String>) -> Result<Self> {
        let mut result = Self::default();

        if let Some(v) = config.get("vector.reranker.enabled") {
            result.enabled = v.eq_ignore_ascii_case("true");
        }

        if let Some(v) = config.get("vector.reranker.provider") {
            result.provider = v.parse()?;
        }

        if let Some(v) = config.get("vector.reranker.model") {
            result.model = v.clone();
        }

        if let Some(v) = config.get("vector.reranker.endpoint") {
            result.endpoint = Some(v.clone());
        }

        if let Some(v) = config.get("vector.reranker.api_key") {
            result.api_key = Some(v.clone());
        }

        // Also check env var
        if result.api_key.is_none() {
            if let Ok(key) = std::env::var("CHRONIK_RERANKER_API_KEY") {
                result.api_key = Some(key);
            }
        }

        if let Some(v) = config.get("vector.reranker.overretrieval_factor") {
            result.overretrieval_factor = v.parse()
                .map_err(|_| anyhow!("Invalid overretrieval_factor: {}", v))?;
        }

        Ok(result)
    }
}

/// Reranker provider type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RerankerProviderType {
    /// External HTTP endpoint (Cohere, Jina, custom).
    #[default]
    External,
    /// No-op (pass-through, for testing).
    NoOp,
}

impl std::str::FromStr for RerankerProviderType {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            "external" | "cohere" | "jina" => Ok(Self::External),
            "noop" | "none" => Ok(Self::NoOp),
            _ => Err(anyhow!("Invalid reranker provider: {}. Valid: external, noop", s)),
        }
    }
}

/// Create a reranker provider from config.
pub fn create_reranker(config: &RerankerConfig) -> Result<Box<dyn RerankerProvider>> {
    match config.provider {
        RerankerProviderType::External => {
            let endpoint = config.endpoint.as_ref()
                .ok_or_else(|| anyhow!("External reranker requires vector.reranker.endpoint"))?;
            Ok(Box::new(ExternalReranker::new(
                endpoint.clone(),
                config.model.clone(),
                config.api_key.clone(),
            )))
        }
        RerankerProviderType::NoOp => {
            Ok(Box::new(NoOpReranker))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reranker_config_default() {
        let config = RerankerConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.provider, RerankerProviderType::External);
        assert_eq!(config.overretrieval_factor, 5);
    }

    #[test]
    fn test_reranker_config_parsing() {
        let mut topic_config = std::collections::HashMap::new();
        topic_config.insert("vector.reranker.enabled".to_string(), "true".to_string());
        topic_config.insert("vector.reranker.provider".to_string(), "cohere".to_string());
        topic_config.insert("vector.reranker.model".to_string(), "rerank-v3.5".to_string());
        topic_config.insert("vector.reranker.endpoint".to_string(), "https://api.cohere.com/v2/rerank".to_string());
        topic_config.insert("vector.reranker.overretrieval_factor".to_string(), "10".to_string());

        let config = RerankerConfig::from_topic_config(&topic_config).unwrap();
        assert!(config.enabled);
        assert_eq!(config.provider, RerankerProviderType::External);
        assert_eq!(config.model, "rerank-v3.5");
        assert_eq!(config.endpoint, Some("https://api.cohere.com/v2/rerank".to_string()));
        assert_eq!(config.overretrieval_factor, 10);
    }

    #[test]
    fn test_reranker_provider_type_parsing() {
        assert_eq!("external".parse::<RerankerProviderType>().unwrap(), RerankerProviderType::External);
        assert_eq!("cohere".parse::<RerankerProviderType>().unwrap(), RerankerProviderType::External);
        assert_eq!("jina".parse::<RerankerProviderType>().unwrap(), RerankerProviderType::External);
        assert_eq!("noop".parse::<RerankerProviderType>().unwrap(), RerankerProviderType::NoOp);
        assert!("invalid".parse::<RerankerProviderType>().is_err());
    }

    #[tokio::test]
    async fn test_noop_reranker() {
        let reranker = NoOpReranker;
        let docs = vec!["doc1", "doc2", "doc3"];
        let results = reranker.rerank("query", &docs, 2).await.unwrap();

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].index, 0);
        assert_eq!(results[1].index, 1);
        assert!(results[0].relevance_score > results[1].relevance_score);
    }

    #[test]
    fn test_create_noop_reranker() {
        let config = RerankerConfig {
            enabled: true,
            provider: RerankerProviderType::NoOp,
            ..Default::default()
        };
        let reranker = create_reranker(&config).unwrap();
        assert_eq!(reranker.name(), "noop");
    }

    #[test]
    fn test_create_external_reranker_requires_endpoint() {
        let config = RerankerConfig {
            enabled: true,
            provider: RerankerProviderType::External,
            endpoint: None,
            ..Default::default()
        };
        assert!(create_reranker(&config).is_err());
    }

    #[test]
    fn test_create_external_reranker_with_endpoint() {
        let config = RerankerConfig {
            enabled: true,
            provider: RerankerProviderType::External,
            endpoint: Some("https://api.cohere.com/v2/rerank".to_string()),
            ..Default::default()
        };
        let reranker = create_reranker(&config).unwrap();
        assert_eq!(reranker.name(), "external");
    }
}
