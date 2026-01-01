//! Vector search configuration.
//!
//! Configuration is extracted from topic config HashMap keys prefixed with `vector.`.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Vector search configuration for a topic.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorSearchConfig {
    /// Whether vector search is enabled for this topic.
    pub enabled: bool,

    /// Embedding model configuration.
    pub embedding: EmbeddingModelConfig,

    /// Field to embed (e.g., "value", "key", or JSON path like "$.message.text").
    pub field: String,

    /// HNSW index configuration.
    pub hnsw: HnswConfig,

    /// Batch size for embedding API calls.
    pub batch_size: usize,

    /// Maximum embedding retries on failure.
    pub max_retries: u32,
}

impl Default for VectorSearchConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            embedding: EmbeddingModelConfig::default(),
            field: "value".to_string(),
            hnsw: HnswConfig::default(),
            batch_size: 32,
            max_retries: 3,
        }
    }
}

impl VectorSearchConfig {
    /// Parse vector search configuration from topic config HashMap.
    ///
    /// Keys are expected to be prefixed with `vector.`, e.g.:
    /// - `vector.enabled` = "true"
    /// - `vector.provider` = "openai"
    /// - `vector.model` = "text-embedding-3-small"
    pub fn from_topic_config(config: &HashMap<String, String>) -> Result<Self> {
        let mut result = Self::default();

        // vector.enabled
        if let Some(v) = config.get("vector.enabled") {
            result.enabled = v.eq_ignore_ascii_case("true");
        }

        // vector.field
        if let Some(v) = config.get("vector.field") {
            result.field = v.clone();
        }

        // Embedding configuration
        result.embedding = EmbeddingModelConfig::from_topic_config(config)?;

        // HNSW configuration
        result.hnsw = HnswConfig::from_topic_config(config)?;

        // vector.batch_size
        if let Some(v) = config.get("vector.batch_size") {
            result.batch_size = v.parse()
                .map_err(|_| anyhow!("Invalid batch_size: {}", v))?;
            if result.batch_size < 1 || result.batch_size > 1000 {
                return Err(anyhow!("batch_size must be 1-1000, got {}", result.batch_size));
            }
        }

        // vector.max_retries
        if let Some(v) = config.get("vector.max_retries") {
            result.max_retries = v.parse()
                .map_err(|_| anyhow!("Invalid max_retries: {}", v))?;
        }

        Ok(result)
    }

    /// Validate the configuration.
    pub fn validate(&self) -> Result<()> {
        if self.field.is_empty() {
            return Err(anyhow!("vector.field cannot be empty"));
        }

        self.embedding.validate()?;
        self.hnsw.validate()?;

        Ok(())
    }

    /// Get all valid vector config keys for validation.
    pub fn valid_keys() -> &'static [&'static str] {
        &[
            "vector.enabled",
            "vector.field",
            "vector.provider",
            "vector.model",
            "vector.endpoint",
            "vector.dimensions",
            "vector.batch_size",
            "vector.max_retries",
            "vector.hnsw.m",
            "vector.hnsw.ef_construction",
            "vector.hnsw.ef_search",
            "vector.hnsw.metric",
        ]
    }
}

/// Embedding model configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingModelConfig {
    /// Provider type.
    pub provider: EmbeddingProvider,

    /// Model name (e.g., "text-embedding-3-small" for OpenAI).
    pub model: String,

    /// Custom endpoint URL (for External provider).
    pub endpoint: Option<String>,

    /// Vector dimensions (auto-detected for known models).
    pub dimensions: usize,
}

impl Default for EmbeddingModelConfig {
    fn default() -> Self {
        Self {
            provider: EmbeddingProvider::OpenAI,
            model: "text-embedding-3-small".to_string(),
            endpoint: None,
            dimensions: 1536,
        }
    }
}

impl EmbeddingModelConfig {
    /// Parse from topic config.
    pub fn from_topic_config(config: &HashMap<String, String>) -> Result<Self> {
        let mut result = Self::default();

        // vector.provider
        if let Some(v) = config.get("vector.provider") {
            result.provider = v.parse()?;
        }

        // vector.model
        if let Some(v) = config.get("vector.model") {
            result.model = v.clone();
            // Auto-detect dimensions for known models
            result.dimensions = known_model_dimensions(&result.model)
                .unwrap_or(result.dimensions);
        }

        // vector.endpoint
        if let Some(v) = config.get("vector.endpoint") {
            result.endpoint = Some(v.clone());
        }

        // vector.dimensions (override auto-detection)
        if let Some(v) = config.get("vector.dimensions") {
            result.dimensions = v.parse()
                .map_err(|_| anyhow!("Invalid dimensions: {}", v))?;
        }

        Ok(result)
    }

    /// Validate the configuration.
    pub fn validate(&self) -> Result<()> {
        match self.provider {
            EmbeddingProvider::External => {
                if self.endpoint.is_none() {
                    return Err(anyhow!("External provider requires vector.endpoint"));
                }
            }
            EmbeddingProvider::Local => {
                #[cfg(not(feature = "local-models"))]
                return Err(anyhow!(
                    "Local provider requires 'local-models' feature. Rebuild with --features local-models"
                ));
            }
            EmbeddingProvider::OpenAI => {
                // OpenAI validation happens at runtime (API key check)
            }
        }

        if self.dimensions < 1 || self.dimensions > 4096 {
            return Err(anyhow!("dimensions must be 1-4096, got {}", self.dimensions));
        }

        Ok(())
    }
}

/// Embedding provider type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum EmbeddingProvider {
    /// OpenAI Embeddings API.
    #[default]
    OpenAI,
    /// External HTTP endpoint.
    External,
    /// Local ONNX model.
    Local,
}

impl std::str::FromStr for EmbeddingProvider {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            "openai" => Ok(Self::OpenAI),
            "external" => Ok(Self::External),
            "local" => Ok(Self::Local),
            _ => Err(anyhow!("Invalid provider: {}. Valid: openai, external, local", s)),
        }
    }
}

impl std::fmt::Display for EmbeddingProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OpenAI => write!(f, "openai"),
            Self::External => write!(f, "external"),
            Self::Local => write!(f, "local"),
        }
    }
}

/// HNSW index configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HnswConfig {
    /// Number of bi-directional links per node.
    pub m: usize,

    /// Size of the dynamic candidate list during construction.
    pub ef_construction: usize,

    /// Size of the dynamic candidate list during search.
    pub ef_search: usize,

    /// Distance metric.
    pub metric: DistanceMetric,
}

impl Default for HnswConfig {
    fn default() -> Self {
        Self {
            m: 16,
            ef_construction: 100,
            ef_search: 50,
            metric: DistanceMetric::Cosine,
        }
    }
}

impl HnswConfig {
    /// Parse from topic config.
    pub fn from_topic_config(config: &HashMap<String, String>) -> Result<Self> {
        let mut result = Self::default();

        // vector.hnsw.m
        if let Some(v) = config.get("vector.hnsw.m") {
            result.m = v.parse()
                .map_err(|_| anyhow!("Invalid hnsw.m: {}", v))?;
        }

        // vector.hnsw.ef_construction
        if let Some(v) = config.get("vector.hnsw.ef_construction") {
            result.ef_construction = v.parse()
                .map_err(|_| anyhow!("Invalid hnsw.ef_construction: {}", v))?;
        }

        // vector.hnsw.ef_search
        if let Some(v) = config.get("vector.hnsw.ef_search") {
            result.ef_search = v.parse()
                .map_err(|_| anyhow!("Invalid hnsw.ef_search: {}", v))?;
        }

        // vector.hnsw.metric
        if let Some(v) = config.get("vector.hnsw.metric") {
            result.metric = v.parse()?;
        }

        Ok(result)
    }

    /// Validate the configuration.
    pub fn validate(&self) -> Result<()> {
        if self.m < 2 || self.m > 100 {
            return Err(anyhow!("hnsw.m must be 2-100, got {}", self.m));
        }

        if self.ef_construction < self.m {
            return Err(anyhow!(
                "hnsw.ef_construction ({}) must be >= hnsw.m ({})",
                self.ef_construction, self.m
            ));
        }

        if self.ef_search < 1 {
            return Err(anyhow!("hnsw.ef_search must be >= 1"));
        }

        Ok(())
    }
}

/// Distance metric for vector similarity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DistanceMetric {
    /// Euclidean (L2) distance.
    Euclidean,
    /// Cosine similarity (default, best for text embeddings).
    #[default]
    Cosine,
    /// Inner product (dot product).
    InnerProduct,
}

impl std::str::FromStr for DistanceMetric {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            "euclidean" | "l2" => Ok(Self::Euclidean),
            "cosine" => Ok(Self::Cosine),
            "innerproduct" | "inner_product" | "dot" => Ok(Self::InnerProduct),
            _ => Err(anyhow!("Invalid metric: {}. Valid: euclidean, cosine, innerproduct", s)),
        }
    }
}

impl std::fmt::Display for DistanceMetric {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Euclidean => write!(f, "euclidean"),
            Self::Cosine => write!(f, "cosine"),
            Self::InnerProduct => write!(f, "innerproduct"),
        }
    }
}

/// Get dimensions for known embedding models.
fn known_model_dimensions(model: &str) -> Option<usize> {
    match model {
        // OpenAI models
        "text-embedding-3-small" => Some(1536),
        "text-embedding-3-large" => Some(3072),
        "text-embedding-ada-002" => Some(1536),

        // Sentence Transformers (common local models)
        "all-MiniLM-L6-v2" => Some(384),
        "all-mpnet-base-v2" => Some(768),
        "paraphrase-multilingual-MiniLM-L12-v2" => Some(384),

        // Cohere
        "embed-english-v3.0" => Some(1024),
        "embed-english-light-v3.0" => Some(384),
        "embed-multilingual-v3.0" => Some(1024),

        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = VectorSearchConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.field, "value");
        assert_eq!(config.embedding.provider, EmbeddingProvider::OpenAI);
    }

    #[test]
    fn test_from_topic_config() {
        let mut topic_config = HashMap::new();
        topic_config.insert("vector.enabled".to_string(), "true".to_string());
        topic_config.insert("vector.provider".to_string(), "openai".to_string());
        topic_config.insert("vector.model".to_string(), "text-embedding-3-small".to_string());
        topic_config.insert("vector.field".to_string(), "$.message.text".to_string());

        let config = VectorSearchConfig::from_topic_config(&topic_config).unwrap();
        assert!(config.enabled);
        assert_eq!(config.field, "$.message.text");
        assert_eq!(config.embedding.dimensions, 1536);
    }

    #[test]
    fn test_hnsw_config() {
        let mut topic_config = HashMap::new();
        topic_config.insert("vector.hnsw.m".to_string(), "32".to_string());
        topic_config.insert("vector.hnsw.ef_construction".to_string(), "200".to_string());
        topic_config.insert("vector.hnsw.metric".to_string(), "cosine".to_string());

        let config = HnswConfig::from_topic_config(&topic_config).unwrap();
        assert_eq!(config.m, 32);
        assert_eq!(config.ef_construction, 200);
        assert_eq!(config.metric, DistanceMetric::Cosine);
    }

    #[test]
    fn test_provider_parsing() {
        assert_eq!("openai".parse::<EmbeddingProvider>().unwrap(), EmbeddingProvider::OpenAI);
        assert_eq!("external".parse::<EmbeddingProvider>().unwrap(), EmbeddingProvider::External);
        assert_eq!("local".parse::<EmbeddingProvider>().unwrap(), EmbeddingProvider::Local);
    }

    #[test]
    fn test_metric_parsing() {
        assert_eq!("euclidean".parse::<DistanceMetric>().unwrap(), DistanceMetric::Euclidean);
        assert_eq!("l2".parse::<DistanceMetric>().unwrap(), DistanceMetric::Euclidean);
        assert_eq!("cosine".parse::<DistanceMetric>().unwrap(), DistanceMetric::Cosine);
        assert_eq!("dot".parse::<DistanceMetric>().unwrap(), DistanceMetric::InnerProduct);
    }

    #[test]
    fn test_known_model_dimensions() {
        assert_eq!(known_model_dimensions("text-embedding-3-small"), Some(1536));
        assert_eq!(known_model_dimensions("all-MiniLM-L6-v2"), Some(384));
        assert_eq!(known_model_dimensions("unknown-model"), None);
    }

    #[test]
    fn test_validate_hnsw() {
        let mut config = HnswConfig::default();
        assert!(config.validate().is_ok());

        config.m = 1; // Too low
        assert!(config.validate().is_err());

        config.m = 16;
        config.ef_construction = 10; // Less than m
        assert!(config.validate().is_err());
    }
}
