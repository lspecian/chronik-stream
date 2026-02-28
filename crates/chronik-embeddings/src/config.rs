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
    /// OpenAI supports up to 2048 texts per call; 256 balances latency and throughput.
    pub batch_size: usize,

    /// Number of concurrent in-flight embedding batches.
    /// Higher values increase throughput but also API rate limit pressure.
    pub embedding_concurrency: usize,

    /// Maximum embedding retries on failure.
    pub max_retries: u32,

    /// Vector quantization mode (VO-3). Controls memory vs. recall trade-off.
    pub quantization: QuantizationMode,

    /// Re-ranker configuration (VO-4). Cross-encoder re-ranking for quality improvement.
    pub reranker: crate::reranker::RerankerConfig,

    /// VO-5: Active model ID for search. When a search request doesn't specify a model,
    /// this model's index is used. Default: `"default"` (uses the topic's primary model).
    pub active_model: String,

    /// VO-6: Number of dimensions used for HNSW graph search.
    /// When less than `embedding.dimensions`, vectors are stored at full dimensionality
    /// but the HNSW graph is built on truncated (Matryoshka) dimensions for faster search.
    /// Default: 0 (use full `embedding.dimensions`).
    pub search_dimensions: usize,

    /// VO-6: Enable adaptive two-phase retrieval.
    /// Phase 1: Search HNSW at `search_dimensions` for k×3 candidates.
    /// Phase 2: Re-score candidates using full-dimension vectors, return top-k.
    /// Only effective when `search_dimensions < embedding.dimensions`.
    pub adaptive_retrieval: bool,
}

impl Default for VectorSearchConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            embedding: EmbeddingModelConfig::default(),
            field: "value".to_string(),
            hnsw: HnswConfig::default(),
            batch_size: 256,
            embedding_concurrency: 4,
            max_retries: 3,
            quantization: QuantizationMode::F32,
            reranker: crate::reranker::RerankerConfig::default(),
            active_model: "default".to_string(),
            search_dimensions: 0, // 0 = use full embedding.dimensions
            adaptive_retrieval: false,
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
            if result.batch_size < 1 || result.batch_size > 2048 {
                return Err(anyhow!("batch_size must be 1-2048, got {}", result.batch_size));
            }
        }

        // vector.embedding_concurrency (or CHRONIK_EMBEDDING_CONCURRENCY env var)
        if let Some(v) = config.get("vector.embedding_concurrency") {
            result.embedding_concurrency = v.parse()
                .map_err(|_| anyhow!("Invalid embedding_concurrency: {}", v))?;
        }
        if let Ok(v) = std::env::var("CHRONIK_EMBEDDING_CONCURRENCY") {
            if let Ok(c) = v.parse::<usize>() {
                result.embedding_concurrency = c;
            }
        }
        if result.embedding_concurrency < 1 || result.embedding_concurrency > 32 {
            return Err(anyhow!("embedding_concurrency must be 1-32, got {}", result.embedding_concurrency));
        }

        // vector.max_retries
        if let Some(v) = config.get("vector.max_retries") {
            result.max_retries = v.parse()
                .map_err(|_| anyhow!("Invalid max_retries: {}", v))?;
        }

        // vector.quantization (VO-3)
        if let Some(v) = config.get("vector.quantization") {
            result.quantization = v.parse()?;
        }

        // vector.reranker.* (VO-4)
        result.reranker = crate::reranker::RerankerConfig::from_topic_config(config)?;

        // vector.active_model (VO-5)
        if let Some(v) = config.get("vector.active_model") {
            result.active_model = v.clone();
        }

        // vector.search_dimensions (VO-6) — 0 means use full embedding.dimensions
        if let Some(v) = config.get("vector.search_dimensions") {
            result.search_dimensions = v.parse()
                .map_err(|_| anyhow!("Invalid search_dimensions: {}", v))?;
            if result.search_dimensions > 0 && result.search_dimensions > result.embedding.dimensions {
                return Err(anyhow!(
                    "search_dimensions ({}) cannot exceed embedding dimensions ({})",
                    result.search_dimensions, result.embedding.dimensions
                ));
            }
        }

        // vector.adaptive_retrieval (VO-6)
        if let Some(v) = config.get("vector.adaptive_retrieval") {
            result.adaptive_retrieval = v.eq_ignore_ascii_case("true");
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
            "vector.embedding_concurrency",
            "vector.max_retries",
            "vector.hnsw.m",
            "vector.hnsw.ef_construction",
            "vector.hnsw.ef_search",
            "vector.hnsw.metric",
            "vector.quantization",
            "vector.reranker.enabled",
            "vector.reranker.provider",
            "vector.reranker.model",
            "vector.reranker.endpoint",
            "vector.reranker.api_key",
            "vector.reranker.overretrieval_factor",
            "vector.active_model",
            "vector.search_dimensions",
            "vector.adaptive_retrieval",
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

/// Vector quantization mode (VO-3).
///
/// Controls how vectors are stored in memory and on disk.
/// Lower precision reduces memory but may decrease recall slightly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum QuantizationMode {
    /// Full 32-bit float (6,144 bytes per 1536-dim vector). No quality loss.
    #[default]
    F32,
    /// Half-precision 16-bit float (3,072 bytes per 1536-dim vector). ~99.9% recall.
    F16,
    /// 8-bit unsigned int with scalar quantization (1,536 bytes per 1536-dim vector). ~97-98% recall.
    Uint8,
}

impl std::str::FromStr for QuantizationMode {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            "f32" | "float32" | "none" => Ok(Self::F32),
            "f16" | "float16" | "half" => Ok(Self::F16),
            "uint8" | "u8" | "int8" | "sq8" => Ok(Self::Uint8),
            _ => Err(anyhow!("Invalid quantization: {}. Valid: f32, f16, uint8", s)),
        }
    }
}

impl std::fmt::Display for QuantizationMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::F32 => write!(f, "f32"),
            Self::F16 => write!(f, "f16"),
            Self::Uint8 => write!(f, "uint8"),
        }
    }
}

impl QuantizationMode {
    /// Bytes per scalar element.
    pub fn bytes_per_element(&self) -> usize {
        match self {
            Self::F32 => 4,
            Self::F16 => 2,
            Self::Uint8 => 1,
        }
    }

    /// Bytes per vector at the given dimensionality.
    pub fn bytes_per_vector(&self, dimensions: usize) -> usize {
        dimensions * self.bytes_per_element()
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
        // VO-1: Verify new defaults
        assert_eq!(config.batch_size, 256);
        assert_eq!(config.embedding_concurrency, 4);
    }

    #[test]
    fn test_batch_size_range() {
        // Max allowed is 2048
        let mut topic_config = HashMap::new();
        topic_config.insert("vector.batch_size".to_string(), "2048".to_string());
        let config = VectorSearchConfig::from_topic_config(&topic_config).unwrap();
        assert_eq!(config.batch_size, 2048);

        // Over max should fail
        let mut topic_config = HashMap::new();
        topic_config.insert("vector.batch_size".to_string(), "2049".to_string());
        assert!(VectorSearchConfig::from_topic_config(&topic_config).is_err());
    }

    #[test]
    fn test_embedding_concurrency_config() {
        let mut topic_config = HashMap::new();
        topic_config.insert("vector.embedding_concurrency".to_string(), "8".to_string());
        let config = VectorSearchConfig::from_topic_config(&topic_config).unwrap();
        assert_eq!(config.embedding_concurrency, 8);

        // Over max (32) should fail
        let mut topic_config = HashMap::new();
        topic_config.insert("vector.embedding_concurrency".to_string(), "33".to_string());
        assert!(VectorSearchConfig::from_topic_config(&topic_config).is_err());
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
    fn test_quantization_mode_parsing() {
        assert_eq!("f32".parse::<QuantizationMode>().unwrap(), QuantizationMode::F32);
        assert_eq!("none".parse::<QuantizationMode>().unwrap(), QuantizationMode::F32);
        assert_eq!("f16".parse::<QuantizationMode>().unwrap(), QuantizationMode::F16);
        assert_eq!("half".parse::<QuantizationMode>().unwrap(), QuantizationMode::F16);
        assert_eq!("uint8".parse::<QuantizationMode>().unwrap(), QuantizationMode::Uint8);
        assert_eq!("sq8".parse::<QuantizationMode>().unwrap(), QuantizationMode::Uint8);
        assert!("invalid".parse::<QuantizationMode>().is_err());
    }

    #[test]
    fn test_quantization_config_from_topic() {
        let mut topic_config = HashMap::new();
        topic_config.insert("vector.quantization".to_string(), "uint8".to_string());
        let config = VectorSearchConfig::from_topic_config(&topic_config).unwrap();
        assert_eq!(config.quantization, QuantizationMode::Uint8);
    }

    #[test]
    fn test_quantization_bytes_per_vector() {
        assert_eq!(QuantizationMode::F32.bytes_per_vector(1536), 6144);
        assert_eq!(QuantizationMode::F16.bytes_per_vector(1536), 3072);
        assert_eq!(QuantizationMode::Uint8.bytes_per_vector(1536), 1536);
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

    #[test]
    fn test_active_model_default() {
        let config = VectorSearchConfig::default();
        assert_eq!(config.active_model, "default");
    }

    #[test]
    fn test_active_model_from_topic_config() {
        let mut topic_config = HashMap::new();
        topic_config.insert("vector.active_model".to_string(), "text-embedding-3-large".to_string());
        let config = VectorSearchConfig::from_topic_config(&topic_config).unwrap();
        assert_eq!(config.active_model, "text-embedding-3-large");
    }

    #[test]
    fn test_search_dimensions_default() {
        let config = VectorSearchConfig::default();
        assert_eq!(config.search_dimensions, 0);
        assert!(!config.adaptive_retrieval);
    }

    #[test]
    fn test_search_dimensions_from_topic_config() {
        let mut topic_config = HashMap::new();
        topic_config.insert("vector.search_dimensions".to_string(), "512".to_string());
        topic_config.insert("vector.adaptive_retrieval".to_string(), "true".to_string());
        let config = VectorSearchConfig::from_topic_config(&topic_config).unwrap();
        assert_eq!(config.search_dimensions, 512);
        assert!(config.adaptive_retrieval);
    }

    #[test]
    fn test_search_dimensions_exceeds_embedding_dims() {
        let mut topic_config = HashMap::new();
        // Default embedding dims = 1536
        topic_config.insert("vector.search_dimensions".to_string(), "2000".to_string());
        let result = VectorSearchConfig::from_topic_config(&topic_config);
        assert!(result.is_err());
    }
}
