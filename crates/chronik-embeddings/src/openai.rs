//! OpenAI embeddings provider.
//!
//! Uses the OpenAI Embeddings API for generating text embeddings.
//! Includes rate limiting support with exponential backoff for 429 responses.

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};
use tokio::time::sleep;
use tracing::{debug, info, warn};

use crate::provider::{EmbeddingBatch, EmbeddingProvider, EmbeddingResult};

/// Rate limiting configuration for OpenAI API.
#[derive(Debug, Clone)]
pub struct RateLimitConfig {
    /// Maximum number of retries on rate limit (429) errors.
    pub max_retries: u32,
    /// Initial backoff duration.
    pub initial_backoff: Duration,
    /// Maximum backoff duration.
    pub max_backoff: Duration,
    /// Backoff multiplier (exponential factor).
    pub backoff_multiplier: f64,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            max_retries: 5,
            initial_backoff: Duration::from_millis(500),
            max_backoff: Duration::from_secs(60),
            backoff_multiplier: 2.0,
        }
    }
}

/// OpenAI embeddings provider.
pub struct OpenAIProvider {
    client: reqwest::Client,
    api_key: String,
    model: String,
    dimensions: usize,
    endpoint: String,
    rate_limit_config: RateLimitConfig,
}

impl OpenAIProvider {
    /// Create a new OpenAI provider.
    ///
    /// # Arguments
    /// * `api_key` - OpenAI API key (or use OPENAI_API_KEY env var)
    /// * `model` - Model name (e.g., "text-embedding-3-small")
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Result<Self> {
        let api_key = api_key.into();
        let model = model.into();

        if api_key.is_empty() {
            return Err(anyhow!("OpenAI API key is required"));
        }

        let dimensions = match model.as_str() {
            "text-embedding-3-small" => 1536,
            "text-embedding-3-large" => 3072,
            "text-embedding-ada-002" => 1536,
            _ => 1536, // Default
        };

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()?;

        Ok(Self {
            client,
            api_key,
            model,
            dimensions,
            endpoint: "https://api.openai.com/v1/embeddings".to_string(),
            rate_limit_config: RateLimitConfig::default(),
        })
    }

    /// Create from environment variable.
    pub fn from_env(model: impl Into<String>) -> Result<Self> {
        let api_key = std::env::var("OPENAI_API_KEY")
            .map_err(|_| anyhow!("OPENAI_API_KEY environment variable not set"))?;
        Self::new(api_key, model)
    }

    /// Set custom endpoint (for Azure OpenAI or compatible APIs).
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }

    /// Set custom dimensions (for dimension reduction).
    pub fn with_dimensions(mut self, dimensions: usize) -> Self {
        self.dimensions = dimensions;
        self
    }

    /// Set custom rate limit configuration.
    pub fn with_rate_limit_config(mut self, config: RateLimitConfig) -> Self {
        self.rate_limit_config = config;
        self
    }

    /// Calculate backoff duration for a given retry attempt.
    fn calculate_backoff(&self, attempt: u32) -> Duration {
        let backoff = self.rate_limit_config.initial_backoff.as_millis() as f64
            * self.rate_limit_config.backoff_multiplier.powi(attempt as i32);
        let backoff_ms = backoff.min(self.rate_limit_config.max_backoff.as_millis() as f64);
        Duration::from_millis(backoff_ms as u64)
    }

    /// Parse Retry-After header from response.
    fn parse_retry_after(response: &reqwest::Response) -> Option<Duration> {
        response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| {
                // Try parsing as seconds first
                s.parse::<u64>().ok().map(Duration::from_secs)
            })
    }
}

#[async_trait]
impl EmbeddingProvider for OpenAIProvider {
    async fn embed(&self, text: &str) -> Result<EmbeddingResult> {
        let batch = self.embed_batch(&[text]).await?;
        batch
            .embeddings
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("Empty response from OpenAI"))
    }

    async fn embed_batch(&self, texts: &[&str]) -> Result<EmbeddingBatch> {
        if texts.is_empty() {
            return Err(anyhow!("Cannot embed empty batch"));
        }

        let start = Instant::now();

        let request = OpenAIRequest {
            input: texts.iter().map(|s| s.to_string()).collect(),
            model: self.model.clone(),
            dimensions: if self.model.starts_with("text-embedding-3") {
                Some(self.dimensions)
            } else {
                None
            },
            encoding_format: Some("float".to_string()),
        };

        debug!(
            "OpenAI embedding request: {} texts, model={}",
            texts.len(),
            self.model
        );

        // Retry loop with exponential backoff for rate limiting
        let mut attempt = 0u32;
        let response = loop {
            let response = self
                .client
                .post(&self.endpoint)
                .header("Authorization", format!("Bearer {}", self.api_key))
                .header("Content-Type", "application/json")
                .json(&request)
                .send()
                .await?;

            let status = response.status();

            // Handle rate limiting (429 Too Many Requests)
            if status.as_u16() == 429 {
                if attempt >= self.rate_limit_config.max_retries {
                    let error_text = response.text().await.unwrap_or_default();
                    warn!(
                        "OpenAI rate limit exceeded after {} retries: {}",
                        attempt, error_text
                    );
                    return Err(anyhow!(
                        "OpenAI rate limit exceeded after {} retries: {}",
                        attempt,
                        error_text
                    ));
                }

                // Use Retry-After header if available, otherwise use exponential backoff
                let backoff = Self::parse_retry_after(&response)
                    .unwrap_or_else(|| self.calculate_backoff(attempt));

                info!(
                    "OpenAI rate limit hit (attempt {}/{}), backing off for {:?}",
                    attempt + 1,
                    self.rate_limit_config.max_retries,
                    backoff
                );

                sleep(backoff).await;
                attempt += 1;
                continue;
            }

            // Handle other errors
            if !status.is_success() {
                let error_text = response.text().await.unwrap_or_default();
                warn!("OpenAI API error: {} - {}", status, error_text);
                return Err(anyhow!("OpenAI API error {}: {}", status, error_text));
            }

            break response;
        };

        let response: OpenAIResponse = response.json().await?;

        let embeddings: Vec<EmbeddingResult> = response
            .data
            .into_iter()
            .zip(texts.iter())
            .map(|(data, &text)| EmbeddingResult {
                vector: data.embedding,
                text: text.to_string(),
                token_count: None, // OpenAI doesn't return per-text token counts
            })
            .collect();

        let duration = start.elapsed();
        debug!(
            "OpenAI embedding complete: {} embeddings in {:?}, {} tokens{}",
            embeddings.len(),
            duration,
            response.usage.total_tokens,
            if attempt > 0 {
                format!(" (after {} retries)", attempt)
            } else {
                String::new()
            }
        );

        let mut batch = EmbeddingBatch::new(embeddings, duration);
        batch.total_tokens = Some(response.usage.total_tokens);

        Ok(batch)
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }

    fn name(&self) -> &str {
        "openai"
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn max_batch_size(&self) -> usize {
        2048 // OpenAI limit
    }

    fn max_input_tokens(&self) -> usize {
        8191 // OpenAI limit for embedding models
    }
}

#[derive(Debug, Serialize)]
struct OpenAIRequest {
    input: Vec<String>,
    model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    dimensions: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    encoding_format: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenAIResponse {
    data: Vec<OpenAIEmbedding>,
    usage: OpenAIUsage,
}

#[derive(Debug, Deserialize)]
struct OpenAIEmbedding {
    embedding: Vec<f32>,
    #[allow(dead_code)]
    index: usize,
}

#[derive(Debug, Deserialize)]
struct OpenAIUsage {
    #[allow(dead_code)]
    prompt_tokens: usize,
    total_tokens: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_provider() {
        let provider = OpenAIProvider::new("sk-test", "text-embedding-3-small").unwrap();
        assert_eq!(provider.dimensions(), 1536);
        assert_eq!(provider.name(), "openai");
        assert_eq!(provider.model(), "text-embedding-3-small");
    }

    #[test]
    fn test_empty_api_key_fails() {
        let result = OpenAIProvider::new("", "text-embedding-3-small");
        assert!(result.is_err());
    }

    #[test]
    fn test_dimensions_by_model() {
        let small = OpenAIProvider::new("sk-test", "text-embedding-3-small").unwrap();
        assert_eq!(small.dimensions(), 1536);

        let large = OpenAIProvider::new("sk-test", "text-embedding-3-large").unwrap();
        assert_eq!(large.dimensions(), 3072);

        let ada = OpenAIProvider::new("sk-test", "text-embedding-ada-002").unwrap();
        assert_eq!(ada.dimensions(), 1536);
    }

    #[test]
    fn test_custom_dimensions() {
        let provider = OpenAIProvider::new("sk-test", "text-embedding-3-small")
            .unwrap()
            .with_dimensions(512);
        assert_eq!(provider.dimensions(), 512);
    }

    #[test]
    fn test_custom_endpoint() {
        let provider = OpenAIProvider::new("sk-test", "text-embedding-3-small")
            .unwrap()
            .with_endpoint("https://custom.api.com/v1/embeddings");
        assert_eq!(provider.endpoint, "https://custom.api.com/v1/embeddings");
    }

    #[test]
    fn test_rate_limit_config_default() {
        let config = RateLimitConfig::default();
        assert_eq!(config.max_retries, 5);
        assert_eq!(config.initial_backoff, Duration::from_millis(500));
        assert_eq!(config.max_backoff, Duration::from_secs(60));
        assert!((config.backoff_multiplier - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_custom_rate_limit_config() {
        let config = RateLimitConfig {
            max_retries: 3,
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(10),
            backoff_multiplier: 1.5,
        };

        let provider = OpenAIProvider::new("sk-test", "text-embedding-3-small")
            .unwrap()
            .with_rate_limit_config(config.clone());

        assert_eq!(provider.rate_limit_config.max_retries, 3);
        assert_eq!(provider.rate_limit_config.initial_backoff, Duration::from_millis(100));
    }

    #[test]
    fn test_calculate_backoff() {
        let provider = OpenAIProvider::new("sk-test", "text-embedding-3-small").unwrap();

        // Default config: initial=500ms, multiplier=2.0
        // Attempt 0: 500ms * 2^0 = 500ms
        assert_eq!(provider.calculate_backoff(0), Duration::from_millis(500));
        // Attempt 1: 500ms * 2^1 = 1000ms
        assert_eq!(provider.calculate_backoff(1), Duration::from_millis(1000));
        // Attempt 2: 500ms * 2^2 = 2000ms
        assert_eq!(provider.calculate_backoff(2), Duration::from_millis(2000));
        // Attempt 3: 500ms * 2^3 = 4000ms
        assert_eq!(provider.calculate_backoff(3), Duration::from_millis(4000));
    }

    #[test]
    fn test_calculate_backoff_respects_max() {
        let config = RateLimitConfig {
            max_retries: 10,
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(5),
            backoff_multiplier: 10.0,
        };

        let provider = OpenAIProvider::new("sk-test", "text-embedding-3-small")
            .unwrap()
            .with_rate_limit_config(config);

        // Attempt 0: 1s * 10^0 = 1s
        assert_eq!(provider.calculate_backoff(0), Duration::from_secs(1));
        // Attempt 1: 1s * 10^1 = 10s, capped to 5s
        assert_eq!(provider.calculate_backoff(1), Duration::from_secs(5));
        // Attempt 2: 1s * 10^2 = 100s, capped to 5s
        assert_eq!(provider.calculate_backoff(2), Duration::from_secs(5));
    }
}

/// Integration tests with mock server
#[cfg(test)]
mod mock_tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn sample_openai_response() -> serde_json::Value {
        serde_json::json!({
            "data": [
                {
                    "embedding": vec![0.1f32; 1536],
                    "index": 0
                }
            ],
            "usage": {
                "prompt_tokens": 5,
                "total_tokens": 5
            }
        })
    }

    fn sample_batch_response(count: usize) -> serde_json::Value {
        let data: Vec<serde_json::Value> = (0..count)
            .map(|i| {
                serde_json::json!({
                    "embedding": vec![0.1f32; 1536],
                    "index": i
                })
            })
            .collect();

        serde_json::json!({
            "data": data,
            "usage": {
                "prompt_tokens": count * 5,
                "total_tokens": count * 5
            }
        })
    }

    #[tokio::test]
    async fn test_embed_single_text() {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .and(header("Authorization", "Bearer sk-mock-key"))
            .and(header("Content-Type", "application/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(sample_openai_response()))
            .expect(1)
            .mount(&mock_server)
            .await;

        let provider = OpenAIProvider::new("sk-mock-key", "text-embedding-3-small")
            .unwrap()
            .with_endpoint(format!("{}/v1/embeddings", mock_server.uri()));

        let result = provider.embed("Hello, world!").await;
        assert!(result.is_ok());

        let embedding = result.unwrap();
        assert_eq!(embedding.vector.len(), 1536);
        assert_eq!(embedding.text, "Hello, world!");
    }

    #[tokio::test]
    async fn test_embed_batch() {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(sample_batch_response(3)))
            .expect(1)
            .mount(&mock_server)
            .await;

        let provider = OpenAIProvider::new("sk-mock-key", "text-embedding-3-small")
            .unwrap()
            .with_endpoint(format!("{}/v1/embeddings", mock_server.uri()));

        let texts = vec!["Hello", "World", "Test"];
        let result = provider.embed_batch(&texts).await;
        assert!(result.is_ok());

        let batch = result.unwrap();
        assert_eq!(batch.embeddings.len(), 3);
        assert_eq!(batch.total_tokens, Some(15)); // 3 * 5
    }

    #[tokio::test]
    async fn test_rate_limit_retry() {
        let mock_server = MockServer::start().await;

        // First request returns 429, second returns success
        // Use up_to_n_times(1) so 429 only fires once, then success matches
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(429).set_body_string("Rate limit exceeded"))
            .up_to_n_times(1)
            .mount(&mock_server)
            .await;

        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(sample_openai_response()))
            .mount(&mock_server)
            .await;

        // Use very short backoff for testing
        let config = RateLimitConfig {
            max_retries: 3,
            initial_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(10),
            backoff_multiplier: 2.0,
        };

        let provider = OpenAIProvider::new("sk-mock-key", "text-embedding-3-small")
            .unwrap()
            .with_endpoint(format!("{}/v1/embeddings", mock_server.uri()))
            .with_rate_limit_config(config);

        let result = provider.embed("test").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_rate_limit_retry_with_retry_after_header() {
        let mock_server = MockServer::start().await;

        // First request returns 429 with Retry-After header
        // Use up_to_n_times(1) so 429 only fires once
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("Retry-After", "1")
                    .set_body_string("Rate limit exceeded"),
            )
            .up_to_n_times(1)
            .mount(&mock_server)
            .await;

        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(sample_openai_response()))
            .mount(&mock_server)
            .await;

        let config = RateLimitConfig {
            max_retries: 3,
            initial_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_secs(5),
            backoff_multiplier: 2.0,
        };

        let provider = OpenAIProvider::new("sk-mock-key", "text-embedding-3-small")
            .unwrap()
            .with_endpoint(format!("{}/v1/embeddings", mock_server.uri()))
            .with_rate_limit_config(config);

        let result = provider.embed("test").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_rate_limit_exhausted() {
        let mock_server = MockServer::start().await;

        // Always return 429
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(429).set_body_string("Rate limit exceeded"))
            .mount(&mock_server)
            .await;

        let config = RateLimitConfig {
            max_retries: 2,
            initial_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(10),
            backoff_multiplier: 2.0,
        };

        let provider = OpenAIProvider::new("sk-mock-key", "text-embedding-3-small")
            .unwrap()
            .with_endpoint(format!("{}/v1/embeddings", mock_server.uri()))
            .with_rate_limit_config(config);

        let result = provider.embed("test").await;
        assert!(result.is_err());
        let err = result.err().unwrap();
        assert!(err.to_string().contains("rate limit"));
    }

    #[tokio::test]
    async fn test_api_error() {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(
                ResponseTemplate::new(401).set_body_string("Invalid API key"),
            )
            .expect(1)
            .mount(&mock_server)
            .await;

        let provider = OpenAIProvider::new("sk-invalid-key", "text-embedding-3-small")
            .unwrap()
            .with_endpoint(format!("{}/v1/embeddings", mock_server.uri()));

        let result = provider.embed("test").await;
        assert!(result.is_err());
        let err = result.err().unwrap();
        assert!(err.to_string().contains("401"));
    }

    #[tokio::test]
    async fn test_server_error() {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(
                ResponseTemplate::new(500).set_body_string("Internal server error"),
            )
            .expect(1)
            .mount(&mock_server)
            .await;

        let provider = OpenAIProvider::new("sk-mock-key", "text-embedding-3-small")
            .unwrap()
            .with_endpoint(format!("{}/v1/embeddings", mock_server.uri()));

        let result = provider.embed("test").await;
        assert!(result.is_err());
        let err = result.err().unwrap();
        assert!(err.to_string().contains("500"));
    }

    #[tokio::test]
    async fn test_empty_batch_fails() {
        let provider = OpenAIProvider::new("sk-mock-key", "text-embedding-3-small").unwrap();

        let empty: Vec<&str> = vec![];
        let result = provider.embed_batch(&empty).await;
        assert!(result.is_err());
        let err = result.err().unwrap();
        assert!(err.to_string().contains("empty"));
    }
}
