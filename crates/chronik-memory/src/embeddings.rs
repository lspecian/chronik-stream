//! Pluggable text-embedding providers used by [`crate::lifecycle::SemanticDedup`].
//!
//! Phase 2 (AMS-2.4) ships:
//! - [`Embedder`] trait — `async fn embed(&self, text: &str) -> Result<Vec<f32>>`
//! - [`OpenAIEmbedder`] — calls OpenAI's `/v1/embeddings` (also works against
//!   any OpenAI-compatible endpoint via `with_base_url`).
//!
//! Phase 3 (AMS-3.6, behind `feature = "local-embed"`) will add a local BGE /
//! mxbai embedder via `candle-core`. The trait is the only seam — switching
//! providers is a one-liner at the call site.

use crate::error::{MemoryError, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Async free-form text completion. Used by [`crate::recall::RecallBuilder`]
/// when the HyDE channel (AMS-2.5) is enabled — the recall layer asks the
/// generator for a hypothetical answer to the user's query, then embeds it
/// and runs a vector search.
///
/// Object-safe so callers can hold `Arc<dyn TextGenerator>` without a type
/// parameter on their state.
#[async_trait]
pub trait TextGenerator: Send + Sync + std::fmt::Debug {
    /// Generate a completion for `prompt`. Implementations should keep the
    /// response short (a sentence or two) — HyDE benefits from concise,
    /// concrete hypotheticals, not full essays.
    async fn complete(&self, prompt: &str) -> Result<String>;

    /// Provider identifier — used in logging / metrics labels.
    fn id(&self) -> &str;
}

/// Async text embedder. Implementations should be cheap to clone — the
/// caller may invoke `embed` concurrently across many texts.
#[async_trait]
pub trait Embedder: Send + Sync + std::fmt::Debug {
    /// Embed a single text into a fixed-dimension vector.
    async fn embed(&self, text: &str) -> Result<Vec<f32>>;

    /// Embed multiple texts. Default impl just calls [`embed`](Self::embed) in
    /// sequence; providers with batched APIs (OpenAI) override for throughput.
    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut out = Vec::with_capacity(texts.len());
        for t in texts {
            out.push(self.embed(t).await?);
        }
        Ok(out)
    }

    /// Provider identifier — used in logging / metrics labels.
    fn id(&self) -> &str;
}

/// Cosine similarity between two equal-dimension vectors. Returns 0.0 if
/// either vector is the zero vector (avoids NaN).
///
/// Pure function — provider-agnostic — used by [`crate::lifecycle::SemanticDedup`].
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for i in 0..a.len() {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

// ============================================================================
// OpenAIEmbedder
// ============================================================================

const OPENAI_DEFAULT_BASE: &str = "https://api.openai.com";
const OPENAI_DEFAULT_MODEL: &str = "text-embedding-3-small";

/// Calls OpenAI's `/v1/embeddings` (or any OpenAI-compatible endpoint).
#[derive(Debug, Clone)]
pub struct OpenAIEmbedder {
    api_key: String,
    model: String,
    base_url: String,
    http: reqwest::Client,
    id: String,
}

impl OpenAIEmbedder {
    /// Build with an OpenAI API key. Default model `text-embedding-3-small`.
    pub fn new(api_key: impl Into<String>) -> Self {
        let model = OPENAI_DEFAULT_MODEL.to_string();
        let id = format!("openai-embedder:{model}");
        Self {
            api_key: api_key.into(),
            model,
            base_url: OPENAI_DEFAULT_BASE.to_string(),
            http: reqwest::Client::new(),
            id,
        }
    }

    /// Override the model id (e.g. `text-embedding-3-large`).
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self.id = format!("openai-embedder:{}", self.model);
        self
    }

    /// Override the base URL — useful for vLLM or LM Studio embedding servers.
    pub fn with_base_url(mut self, base: impl Into<String>) -> Self {
        self.base_url = base.into();
        self
    }
}

#[async_trait]
impl Embedder for OpenAIEmbedder {
    fn id(&self) -> &str {
        &self.id
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let mut v = self.embed_batch(&[text.to_string()]).await?;
        v.pop()
            .ok_or_else(|| MemoryError::Provider("openai embed: empty response".into()))
    }

    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }
        let url = format!("{}/v1/embeddings", self.base_url.trim_end_matches('/'));
        let body = serde_json::json!({
            "model": self.model,
            "input": texts,
        });
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.api_key)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let txt = resp.text().await.unwrap_or_default();
            return Err(MemoryError::Provider(format!(
                "openai embeddings returned {status}: {txt}"
            )));
        }
        let parsed: EmbeddingsResponse = resp.json().await?;
        Ok(parsed.data.into_iter().map(|d| d.embedding).collect())
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct EmbeddingsResponse {
    data: Vec<EmbeddingItem>,
}

#[derive(Debug, Deserialize, Serialize)]
struct EmbeddingItem {
    embedding: Vec<f32>,
    #[allow(dead_code)]
    #[serde(default)]
    index: Option<usize>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// In-memory test embedder — returns deterministic vectors keyed off the
    /// SHA-256 prefix of the input. Useful for unit tests of dedup logic
    /// where we just need a stable embedding without an HTTP call.
    #[derive(Debug, Clone)]
    pub struct MockEmbedder {
        pub dim: usize,
    }

    #[async_trait]
    impl Embedder for MockEmbedder {
        fn id(&self) -> &str {
            "mock-embedder"
        }
        async fn embed(&self, text: &str) -> Result<Vec<f32>> {
            use sha2::{Digest, Sha256};
            let h = Sha256::digest(text.as_bytes());
            let mut v = Vec::with_capacity(self.dim);
            for i in 0..self.dim {
                v.push(h[i % h.len()] as f32 / 255.0);
            }
            // Normalize so cosine sim with itself is 1.
            let n = (v.iter().map(|x| x * x).sum::<f32>()).sqrt();
            if n > 0.0 {
                v.iter_mut().for_each(|x| *x /= n);
            }
            Ok(v)
        }
    }

    #[test]
    fn cosine_self_is_one() {
        let v = vec![0.6, 0.0, 0.8];
        let s = cosine_similarity(&v, &v);
        assert!((s - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_orthogonal_is_zero() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        assert!(cosine_similarity(&a, &b).abs() < 1e-6);
    }

    #[test]
    fn cosine_handles_zero_vector() {
        let a = vec![0.0, 0.0, 0.0];
        let b = vec![1.0, 0.0, 0.0];
        assert_eq!(cosine_similarity(&a, &b), 0.0);
    }

    #[test]
    fn cosine_handles_size_mismatch() {
        let a = vec![1.0, 0.0];
        let b = vec![1.0, 0.0, 0.0];
        assert_eq!(cosine_similarity(&a, &b), 0.0);
    }

    #[tokio::test]
    async fn mock_embedder_is_deterministic() {
        let e = MockEmbedder { dim: 16 };
        let a = e.embed("hello").await.unwrap();
        let b = e.embed("hello").await.unwrap();
        assert_eq!(a, b);
        let c = e.embed("world").await.unwrap();
        assert_ne!(a, c);
    }

    #[tokio::test]
    async fn openai_embedder_happy_path() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .and(header("authorization", "Bearer test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [
                    {"embedding": [0.1f32, 0.2, 0.3], "index": 0},
                    {"embedding": [0.4f32, 0.5, 0.6], "index": 1}
                ],
                "model": OPENAI_DEFAULT_MODEL
            })))
            .mount(&server)
            .await;

        let e = OpenAIEmbedder::new("test-key").with_base_url(server.uri());
        let out = e
            .embed_batch(&["hello".to_string(), "world".to_string()])
            .await
            .unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], vec![0.1, 0.2, 0.3]);
    }

    #[tokio::test]
    async fn openai_embedder_4xx_becomes_provider_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(401).set_body_string("{\"error\":\"x\"}"))
            .mount(&server)
            .await;
        let e = OpenAIEmbedder::new("bad").with_base_url(server.uri());
        let err = e.embed("hi").await.unwrap_err();
        assert!(matches!(err, MemoryError::Provider(_)));
    }

    #[tokio::test]
    async fn openai_embedder_empty_batch() {
        let e = OpenAIEmbedder::new("k").with_base_url("http://127.0.0.1:1");
        let v = e.embed_batch(&[]).await.unwrap();
        assert!(v.is_empty());
    }

    #[test]
    fn id_includes_model_name() {
        let e = OpenAIEmbedder::new("k");
        assert!(e.id().contains(OPENAI_DEFAULT_MODEL));
        let big = e.with_model("text-embedding-3-large");
        assert!(big.id().contains("text-embedding-3-large"));
    }
}
