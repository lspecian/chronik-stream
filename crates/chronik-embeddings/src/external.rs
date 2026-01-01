//! External embeddings provider.
//!
//! Uses a custom HTTP endpoint for generating text embeddings.
//! Supports any API that follows a simple JSON request/response format.

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tracing::{debug, warn};

use crate::provider::{EmbeddingBatch, EmbeddingProvider, EmbeddingResult};

/// External embeddings provider for custom HTTP endpoints.
pub struct ExternalProvider {
    client: reqwest::Client,
    endpoint: String,
    model: String,
    dimensions: usize,
    headers: HashMap<String, String>,
    batch_size: usize,
}

impl ExternalProvider {
    /// Create a new external provider.
    ///
    /// # Arguments
    /// * `endpoint` - The HTTP endpoint URL
    /// * `dimensions` - Vector dimensions returned by the endpoint
    pub fn new(endpoint: impl Into<String>, dimensions: usize) -> Result<Self> {
        let endpoint = endpoint.into();

        if endpoint.is_empty() {
            return Err(anyhow!("Endpoint URL is required"));
        }

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()?;

        Ok(Self {
            client,
            endpoint,
            model: "external".to_string(),
            dimensions,
            headers: HashMap::new(),
            batch_size: 32,
        })
    }

    /// Set the model name (for logging).
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    /// Add a custom header.
    pub fn with_header(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.insert(key.into(), value.into());
        self
    }

    /// Set authorization header (Bearer token).
    pub fn with_bearer_token(self, token: impl Into<String>) -> Self {
        self.with_header("Authorization", format!("Bearer {}", token.into()))
    }

    /// Set API key header.
    pub fn with_api_key(self, key: impl Into<String>) -> Self {
        self.with_header("X-API-Key", key)
    }

    /// Set maximum batch size.
    pub fn with_batch_size(mut self, size: usize) -> Self {
        self.batch_size = size;
        self
    }
}

#[async_trait]
impl EmbeddingProvider for ExternalProvider {
    async fn embed(&self, text: &str) -> Result<EmbeddingResult> {
        let batch = self.embed_batch(&[text]).await?;
        batch
            .embeddings
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("Empty response from external endpoint"))
    }

    async fn embed_batch(&self, texts: &[&str]) -> Result<EmbeddingBatch> {
        if texts.is_empty() {
            return Err(anyhow!("Cannot embed empty batch"));
        }

        let start = Instant::now();

        let request = ExternalRequest {
            texts: texts.iter().map(|s| s.to_string()).collect(),
            model: Some(self.model.clone()),
        };

        debug!(
            "External embedding request: {} texts to {}",
            texts.len(),
            self.endpoint
        );

        let mut req_builder = self
            .client
            .post(&self.endpoint)
            .header("Content-Type", "application/json")
            .json(&request);

        // Add custom headers
        for (key, value) in &self.headers {
            req_builder = req_builder.header(key.as_str(), value.as_str());
        }

        let response = req_builder.send().await?;

        let status = response.status();
        if !status.is_success() {
            let error_text = response.text().await.unwrap_or_default();
            warn!("External API error: {} - {}", status, error_text);
            return Err(anyhow!("External API error {}: {}", status, error_text));
        }

        let response: ExternalResponse = response.json().await?;

        if response.embeddings.len() != texts.len() {
            return Err(anyhow!(
                "Embedding count mismatch: got {}, expected {}",
                response.embeddings.len(),
                texts.len()
            ));
        }

        // Validate dimensions
        for (i, embedding) in response.embeddings.iter().enumerate() {
            if embedding.len() != self.dimensions {
                return Err(anyhow!(
                    "Dimension mismatch for embedding {}: got {}, expected {}",
                    i,
                    embedding.len(),
                    self.dimensions
                ));
            }
        }

        let embeddings: Vec<EmbeddingResult> = response
            .embeddings
            .into_iter()
            .zip(texts.iter())
            .map(|(vector, &text)| EmbeddingResult {
                vector,
                text: text.to_string(),
                token_count: None,
            })
            .collect();

        let duration = start.elapsed();
        debug!(
            "External embedding complete: {} embeddings in {:?}",
            embeddings.len(),
            duration
        );

        Ok(EmbeddingBatch::new(embeddings, duration))
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }

    fn name(&self) -> &str {
        "external"
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn max_batch_size(&self) -> usize {
        self.batch_size
    }
}

#[derive(Debug, Serialize)]
struct ExternalRequest {
    texts: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ExternalResponse {
    embeddings: Vec<Vec<f32>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_provider() {
        let provider = ExternalProvider::new("http://localhost:8000/embed", 384).unwrap();
        assert_eq!(provider.dimensions(), 384);
        assert_eq!(provider.name(), "external");
    }

    #[test]
    fn test_empty_endpoint_fails() {
        let result = ExternalProvider::new("", 384);
        assert!(result.is_err());
    }

    #[test]
    fn test_with_headers() {
        let provider = ExternalProvider::new("http://localhost:8000/embed", 384)
            .unwrap()
            .with_bearer_token("my-token")
            .with_header("X-Custom", "value");

        assert!(provider.headers.contains_key("Authorization"));
        assert!(provider.headers.contains_key("X-Custom"));
    }

    #[test]
    fn test_with_model() {
        let provider = ExternalProvider::new("http://localhost:8000/embed", 384)
            .unwrap()
            .with_model("my-custom-model");

        assert_eq!(provider.model(), "my-custom-model");
    }

    #[test]
    fn test_with_batch_size() {
        let provider = ExternalProvider::new("http://localhost:8000/embed", 384)
            .unwrap()
            .with_batch_size(64);

        assert_eq!(provider.max_batch_size(), 64);
    }
}
